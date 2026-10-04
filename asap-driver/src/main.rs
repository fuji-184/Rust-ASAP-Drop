// asap-driver/src/main.rs
//
// Binary pengganti rustc yang inject AsapDropPass ke pipeline.
// Dipakai via: RUSTC_WRAPPER=asap-driver cargo +nightly build

#![feature(rustc_private)]

extern crate rustc_data_structures;
extern crate rustc_driver;
extern crate rustc_interface;
extern crate rustc_middle;
extern crate rustc_session;
extern crate rustc_span;

mod pass;

use rustc_data_structures::steal::Steal;
use rustc_driver::{Callbacks, Compilation};
use rustc_interface::interface::{Compiler, Config};
use rustc_middle::mir::Body;
use rustc_middle::ty::TyCtxt;
use rustc_middle::util::Providers;
use rustc_session::Session;
use rustc_span::def_id::LocalDefId;

use std::sync::OnceLock;

struct AsapCallbacks;

/// Query `mir_built` asli, disimpan saat `config()` dipanggil.
/// Dipakai oleh `asap_mir_built` untuk menghitung MIR lalu memutasinya.
type MirBuiltFn = for<'tcx> fn(TyCtxt<'tcx>, LocalDefId) -> &'tcx Steal<Body<'tcx>>;
static ORIG_MIR_BUILT: OnceLock<MirBuiltFn> = OnceLock::new();
/// `override_queries` sebelumnya (kalau ada), agar tetap dipanggil.
static PREV_OVERRIDE: OnceLock<Option<fn(&Session, &mut Providers)>> = OnceLock::new();

/// Pengganti query `mir_built`: hitung MIR seperti biasa, lalu terapkan
/// transform ASAP (sisipkan early Drop) sebelum dikembalikan ke pipeline.
/// Karena ini berjalan SEBELUM borrowck, transform yang tidak sound akan
/// ditolak borrowck sebagai compile error (fail-safe), bukan silent UB.
fn asap_mir_built<'tcx>(tcx: TyCtxt<'tcx>, def: LocalDefId) -> &'tcx Steal<Body<'tcx>> {
    let orig = *ORIG_MIR_BUILT.get().expect("ORIG_MIR_BUILT not set");
    let steal = orig(tcx, def);

    // Dua fase: analisis read-only dulu, baru mutasi. Kalau analisis panic
    // (mis. query belum siap), tidak ada yang termutasi — aman di-skip.
    let plan = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let body = steal.borrow();
        pass::plan_asap_transform(tcx, &body)
    }));

    let plan = match plan {
        Ok(p) => p,
        Err(_) => return steal,
    };
    if plan.is_empty() {
        return steal;
    }

    let applied = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut body = steal.risky_hack_borrow_mut();
        pass::apply_asap_plan(&mut body, &plan)
    }));
    if applied.is_err() {
        eprintln!("[asap] transform panicked, MIR may be partially modified");
    }
    steal
}

/// Callback `override_queries`: bungkus `mir_built` dengan versi ASAP.
fn asap_override_queries(_sess: &Session, providers: &mut Providers) {
    if let Some(prev) = PREV_OVERRIDE.get().copied().flatten() {
        prev(_sess, providers);
    }
    let _ = ORIG_MIR_BUILT.set(providers.queries.mir_built);
    providers.queries.mir_built = asap_mir_built;
}

impl Callbacks for AsapCallbacks {
    fn config(&mut self, config: &mut Config) {
        let _ = PREV_OVERRIDE.set(config.override_queries);
        config.override_queries = Some(asap_override_queries);
        // Query override kami harus berjalan untuk tiap fungsi di tiap build.
        // Incremental cache bisa menyajikan MIR lama tanpa memanggil provider
        // kami (transform ter-skip diam-diam), jadi matikan incremental.
        if config.opts.incremental.is_some() {
            eprintln!("[asap] note: disabling incremental compilation");
            config.opts.incremental = None;
        }
    }

    fn after_analysis<'tcx>(
        &mut self,
        _compiler: &Compiler,
        _tcx: TyCtxt<'tcx>,
    ) -> Compilation {
        // Analisis/logging per-fungsi terjadi di asap_mir_built (lazy, saat
        // tiap MIR dibangun). Di sini tidak ada kerjaan tambahan.
        Compilation::Continue
    }
}

fn main() -> std::process::ExitCode {
    let args: Vec<String> = std::env::args().collect();
    // Protokol RUSTC_WRAPPER: cargo memanggil
    //   asap-driver <real-rustc-path> <rustc-args...>
    // sedangkan run_compiler mengharapkan slice dengan argv[0] = path rustc.
    // Jadi buang argv[0] (wrapper) bila argv[1] terlihat seperti path rustc.
    let compiler_args: Vec<String> = if args.len() > 1
        && (args[1].contains("rustc") || args[1].ends_with("rustc.exe"))
    {
        args[1..].to_vec()
    } else {
        args
    };
    rustc_driver::catch_with_exit_code(|| {
        rustc_driver::run_compiler(&compiler_args, &mut AsapCallbacks)
    })
}
