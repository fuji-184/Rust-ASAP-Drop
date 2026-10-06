// pass.rs
//
// Inti dari ASAP Drop (semantik setara NLL, per-path):
//
//  1. Scan locals yang diinisialisasi via `asap_runtime::__asap_identity`
//     (hasil macro `asap!`)                        → "candidates"
//     + PROMOSI: owned locals yang diturunkan dari candidate
//     (mis. `let cloned = early.clone()`) ikut jadi kandidat (fixpoint).
//  2. Taint flow-insensitive: local apa pun yang nilainya diturunkan dari
//     candidate (`&a`, hasil call yang menerima borrow candidate, salinan
//     ref, ...) dicatat sebagai turunan candidate.
//  3. Untuk tiap candidate, kumpulkan USES (borrow-uses) dan MOVES (direct).
//  4. Titik sisip (bisa LEBIH DARI SATU per candidate):
//       (a) Setelah tiap per-path-last BORROW-use: use yang tidak mencapai
//           use lain (cabang berbeda → dua titik, lurus → satu titik).
//       (b) Setelah tiap MOVE langsung ("drop setelah move terakhir").
//           Drop di sini selalu no-op kondisional (value sudah pindah),
//           jadi aman; borrowck menolak bila ada use-after-move.
//     Tiap titik: guard urutan (titik tidak boleh mencapai use mana pun;
//     use di cabang disjoint yang tak terjangkau diabaikan), guard loop,
//     guard move (titik borrow tak boleh mencapai Move), titik harus
//     mendominasi ≥1 situs scope drop. Hoist maju (lewat blok tanpa use,
//     hanya single-successor) agar `if { use }` mendarat di JOIN.
//     Titik identik dari anchor berbeda di-dedupe (satu drop cukup).
//  5. Netralkan SELEKTIF: hanya situs scope drop yang didominasi ≥1 titik
//     sisip. Path lain (return-diverge, path yang me-move) memakai drop
//     aslinya yang tetap utuh.
//  6. Split langsung di lokasi tervalidasi (AfterStmt split / split di
//     awal successor). TIDAK PERNAH edge-insert: guard memeriksa blok,
//     sisipan harus di blok yang sama (dulu mismatch ini menyebabkan
//     cabang hilang + drop ganda).
//
// Fail-safe: mutasi di `mir_built` (SEBELUM borrowck). Penempatan yang
// melanggar aturan borrow/move ditolak sebagai compile error, bukan UB.
//
// Catatan kompatibilitas nightly:
//  - `MirPass` tidak lagi publik → pass ini struct biasa.
//  - `Rvalue::Use` kini 2 field; varian `CheckedBinaryOp`/`AddressOf`/`Len`
//    sudah dihapus; pola `box (...)` dihindari (tanpa feature gate).
//  - `BasicBlockData` non-exhaustive → konstruksi via `new_stmts`.
//  - `body.basic_blocks` adalah `BasicBlocks` → mutasi via `as_mut()`.
//  - Konstruksi `Drop` wajib isi field baru `drop: None, async_fut: None`.

#![allow(dead_code)]

use rustc_data_structures::graph::dominators::Dominators;
use rustc_middle::{
    mir::{BasicBlock, BasicBlockData, Body, Local, Location, Operand, Place, Rvalue,
        StatementKind, Terminator, TerminatorKind, UnwindAction},
    ty::{TyCtxt, TyKind},
};

use std::collections::{HashMap, HashSet};

/// Satu early-drop yang akan diterapkan: split blok di `exec`
/// (`exec.statement_index` dalam `0..=len`, `== len` = tepat sebelum
/// terminator) dan sisipkan Drop; netralkan `sites`.
/// Selalu split di lokasi tervalidasi — TIDAK PERNAH edge-insert
/// (edge-insert + guard blok-level terbukti salah: false path mencapai
/// blok tanpa lewat edge insert → leak).
#[derive(Debug, Clone)]
pub struct AsapDrop {
    local: Local,
    /// Lokasi anchor (use/move terakhir di path-nya, untuk log).
    after: Location,
    /// Lokasi eksekusi drop (hasil hoist, untuk split + sort).
    exec: Location,
    /// Subset situs scope drop yang didominasi titik ini → dinetralkan.
    sites: Vec<BasicBlock>,
}

// ── Entry: susun rencana transform untuk satu body ─────────────────────────

pub fn plan_asap_transform<'a>(tcx: TyCtxt<'a>, body: &Body<'a>) -> Vec<AsapDrop> {
    let mut candidates = find_asap_candidates(tcx, body);
    if candidates.is_empty() {
        return Vec::new();
    }

    // Taint flow-insensitive: local -> set candidate sumber.
    // Lalu PROMOSI: local owned (bukan reference) yang nilainya diturunkan
    // dari candidate (mis. `let cloned = early.clone()`) ikut menjadi
    // kandidat — mereka value independen yang juga boleh di-drop awal.
    // Iterasi sampai fixpoint (promosi hanya menambah, berhimpit di #locals).
    //
    // Catatan return: TIDAK ada skip khusus. `return early` (move) tidak
    // punya successor (titik move di-skip struktural); `return &x` dari
    // local sudah E0515 dengan/tanpa driver; value-return murni tidak
    // memperpanjang borrow. Tracking jalan sampai last use sesuai NLL.
    let mut taint = compute_taint(body, &candidates);
    loop {
        let mut grown = false;
        for (local, srcs) in &taint {
            if candidates.contains(local) || srcs.is_empty() {
                continue;
            }
            // Skip return value (_0) dan args; skip reference (borrow temps
            // bukan owned value — liveness mereka diatur via candidate asal).
            if local.as_u32() == 0 || local.as_u32() <= body.arg_count as u32 {
                continue;
            }
            if matches!(body.local_decls[*local].ty.kind(), TyKind::Ref(..)) {
                continue;
            }
            eprintln!("[asap]   promote {:?} to candidate (derives from {:?})", local, srcs);
            candidates.insert(*local);
            grown = true;
        }
        if !grown {
            break;
        }
        taint = compute_taint(body, &candidates);
    }

    let analysis = analyze_body(body, &candidates, &taint);

    // Dominator dipakai guard titik sisip.
    let dom: &Dominators<BasicBlock> = body.basic_blocks.dominators();

    let mut plan = Vec::new();
    for (&local, info) in &analysis {
        eprintln!(
            "[asap]   {:?}: borrow_uses={:?}, moves={:?}, scope_drops={:?}, moved_out={}",
            local, info.borrow_uses, info.moves, info.existing_drop_blocks, info.moved_out
        );
        if info.existing_drop_blocks.is_empty() {
            eprintln!("[asap]   skip {:?}: no scope drop found", local);
            continue;
        }
        if info.borrow_uses.is_empty() && info.moves.is_empty() {
            eprintln!("[asap]   skip {:?}: no use found", local);
            continue;
        }

        // Anchor borrow: per-path-last uses (use yang tidak mencapai use
        // lain — cabang berbeda menghasilkan beberapa anchor).
        let mut anchors: Vec<(Location, bool)> = Vec::new(); // (loc, is_move)
        for &u in &info.borrow_uses {
            let dominated_by_later = info.borrow_uses.iter().any(|&v| {
                !same_location(u, v) && u.is_predecessor_of(v, body)
            });
            if !dominated_by_later {
                anchors.push((u, false));
            }
        }
        // Anchor move: setiap Move langsung ("drop setelah move terakhir",
        // satu per lokasi move — no-op kondisional bila sudah pindah).
        for &m in &info.moves {
            anchors.push((m, true));
        }

        for (after, is_move_anchor) in anchors {
            // Posisi awal: tepat setelah anchor.
            let after_len = body.basic_blocks[after.block].statements.len();
            let mut drop_exec_loc = if after.statement_index < after_len {
                Location { block: after.block, statement_index: after.statement_index + 1 }
            } else {
                // Anchor di posisi terminator (mis. Call):
                // mulai dari awal successor normalnya.
                let term = body.basic_blocks[after.block].terminator();
                let Some(succ) = single_normal_succ(term) else {
                    eprintln!("[asap]   skip {:?} at {:?}: no single successor", local, after);
                    continue;
                };
                // Blok cleanup dan non-cleanup tidak boleh dicampur.
                if body.basic_blocks[after.block].is_cleanup
                    != body.basic_blocks[succ].is_cleanup
                {
                    eprintln!("[asap]   skip {:?} at {:?}: cleanup mismatch", local, after);
                    continue;
                }
                Location { block: succ, statement_index: 0 }
            };

            // Hoist: maju ke depan selama titik belum mendominasi situs
            // mana pun. Tujuannya: kasus `if { use }` (tanpa else) mendarat
            // di JOIN (mendominasi scope drop), bukan tertahan di dalam
            // cabang (yang akan leak di path lain). Maksimalitas anchor
            // menjamin tidak ada use yang dilewati (kecuali via loop,
            // yang digagalkan guard). Berhenti (gagal) di: cabang
            // multi-successor, cleanup mismatch, blok berulang (loop),
            // atau mencapai situs itu sendiri (nol manfaat).
            //
            // Titik final SELALU di-split langsung (bukan edge-insert):
            // guard dan sisipan mengacu ke lokasi yang sama sehingga
            // tidak ada path yang lolos tanpa drop.
            let mut visited = HashSet::new();
            let placed: Option<Vec<BasicBlock>> = loop {
                // Guard urutan: titik eksekusi TIDAK BOLEH mencapai use mana
                // pun (itu berarti use-after-drop, atau loop-back).
                // Use yang tidak terjangkau dari titik ini — mis. use di
                // cabang lain yang disjoint — tidak relevan dan diabaikan
                // (path-nya dijamin oleh guard dominansi di bawah).
                let ordered = info.borrow_uses.iter().all(|&u| {
                    same_location(u, drop_exec_loc)
                        || !drop_exec_loc.is_predecessor_of(u, body)
                });
                if !ordered {
                    eprintln!("[asap]     hoist {:?}: stop, reaches a later use", drop_exec_loc);
                    break None;
                }
                // Guard move: titik borrow TIDAK boleh mencapai Move mana
                // pun (drop-then-move = borrowck error). Titik move
                // dikecualikan (drop setelah move = no-op yang aman).
                if !is_move_anchor {
                    let hits_move = info.moves.iter().any(|&m| {
                        drop_exec_loc.is_predecessor_of(m, body)
                            && !same_location(drop_exec_loc, m)
                    });
                    if hits_move {
                        eprintln!("[asap]     hoist {:?}: stop, reaches a move", drop_exec_loc);
                        break None;
                    }
                }
                // Situs yang didominasi titik ini (yang akan dinetralkan).
                let mut sites = Vec::new();
                let mut zero_benefit = false;
                for &site in &info.existing_drop_blocks {
                    let site_loc = Location {
                        block: site,
                        statement_index: body.basic_blocks[site].statements.len(),
                    };
                    if same_location(drop_exec_loc, site_loc) {
                        zero_benefit = true;
                        break;
                    }
                    if drop_exec_loc.dominates(site_loc, dom) {
                        sites.push(site);
                    }
                }
                if zero_benefit || !sites.is_empty() {
                    break if sites.is_empty() { None } else { Some(sites) };
                }
                // Maju satu langkah.
                let len = body.basic_blocks[drop_exec_loc.block].statements.len();
                if drop_exec_loc.statement_index < len {
                    // Maju dalam blok.
                    drop_exec_loc.statement_index += 1;
                    continue;
                }
                // Ujung blok: hanya lanjut lewat single successor.
                if !visited.insert(drop_exec_loc.block) {
                    eprintln!("[asap]     hoist {:?}: stop, loop (self)", drop_exec_loc);
                    break None; // loop
                }
                let term = body.basic_blocks[drop_exec_loc.block].terminator();
                let Some(succ) = single_normal_succ(term) else {
                    eprintln!("[asap]     hoist {:?}: stop, no single succ", drop_exec_loc);
                    break None; // cabang/return/yield/asm: berhenti
                };
                if body.basic_blocks[drop_exec_loc.block].is_cleanup
                    != body.basic_blocks[succ].is_cleanup
                {
                    eprintln!("[asap]     hoist {:?}: stop, cleanup mismatch", drop_exec_loc);
                    break None;
                }
                if !visited.insert(succ) {
                    eprintln!("[asap]     hoist {:?}: stop, loop (succ)", drop_exec_loc);
                    break None; // loop
                }
                drop_exec_loc = Location { block: succ, statement_index: 0 };
            };
            let Some(sites) = placed else {
                eprintln!("[asap]   skip {:?} at {:?}: no sound drop point", local, after);
                continue;
            };

            eprintln!(
                "[asap] early-drop {:?} at {:?} (neutralize {:?})",
                local, drop_exec_loc, sites
            );
            plan.push(AsapDrop { local, after, exec: drop_exec_loc, sites });
        }
    }

    // Dedupe: beberapa anchor (mis. use di kedua cabang) bisa hoist ke
    // titik eksekusi yang SAMA (mis. awal join). Satu drop di sana sudah
    // mencakup semua path tersebut — sisipan ganda = drop ganda.
    plan.sort_by(|a, b| {
        (a.local.as_u32(), a.exec.block.as_u32(), a.exec.statement_index).cmp(&(
            b.local.as_u32(),
            b.exec.block.as_u32(),
            b.exec.statement_index,
        ))
    });
    plan.dedup_by(|a, b| {
        a.local == b.local
            && a.exec.block == b.exec.block
            && a.exec.statement_index == b.exec.statement_index
    });
    // Merge situs: item duplikat yang dibuang mungkin membawa situs yang
    // tidak dimiliki item yang dipertahankan. Karena dedupe di atas hanya
    // membuang item dengan exec IDENTIK, situsnya (hasil guard dominansi
    // dari titik yang sama) juga identik — tidak ada yang hilang.

    // Sortir menurun berdasar posisi eksekusi agar split tidak menggeser
    // lokasi rencana lain dalam blok yang sama.
    plan.sort_by(|a, b| {
        std::cmp::Reverse((a.exec.block.as_u32(), a.exec.statement_index))
            .cmp(&std::cmp::Reverse((b.exec.block.as_u32(), b.exec.statement_index)))
    });
    plan
}

/// Successor normal tunggal dari sebuah terminator, atau None bila tidak
/// ada / ambigu (SwitchInt) / tidak didukung (Yield: coroutine + early drop
/// = kombinasi belum didukung; InlineAsm: multi-target).
fn single_normal_succ(term: &Terminator<'_>) -> Option<BasicBlock> {
    match &term.kind {
        TerminatorKind::Call { target: Some(t), .. } => Some(*t),
        TerminatorKind::Goto { target } => Some(*target),
        TerminatorKind::Assert { target, .. } => Some(*target),
        TerminatorKind::Drop { target, .. } => Some(*target),
        TerminatorKind::FalseEdge { real_target, .. } => Some(*real_target),
        TerminatorKind::FalseUnwind { real_target, .. } => Some(*real_target),
        _ => None,
    }
}

/// Terapkan rencana ke body yang sudah di-borrow mutabel.
/// Dipanggil setelah `plan_asap_transform` dari `asap_mir_built`.
///
/// Urutan penting: netralkan SEMUA scope drop dulu (tidak menggeser
/// index), baru sisipkan early drops. Kalau dibalik, split bisa
/// memindahkan terminator sehingga index situs basi menunjuk block
/// yang salah (pernah menyebabkan drop ganda + cabang hilang).
pub fn apply_asap_plan(body: &mut Body<'_>, plan: &[AsapDrop]) {
    // Fase 1: netralkan situs scope drop yang didominasi titik sisip.
    // (Tidak menggeser index; situs yang tidak didominasi tetap utuh
    // untuk path lain — mis. path return atau path yang me-move.)
    for item in plan {
        for &site in &item.sites {
            neutralize_scope_drop(body, site);
        }
    }
    // Fase 2: sisipkan early drops, menurun berdasar posisi eksekusi
    // (split di blok ber-index besar dulu agar tidak menggeser yang lain).
    for item in plan {
        insert_drop_at(body, item.local, item.exec.block, item.exec.statement_index);
        eprintln!("[asap]   applied early drop of {:?} at {:?}", item.local, item.exec);
    }
}

// ── Step 1: Temukan locals kandidat ASAP ─────────────────────────────────
//
// Kandidat = destination dari Call ke `asap_runtime::__asap_identity`
// (hasil macro `asap!`). Tipe local adalah T ASLI.

fn find_asap_candidates<'a>(tcx: TyCtxt<'a>, body: &Body<'a>) -> HashSet<Local> {
    let mut candidates = HashSet::new();

    for bb_data in body.basic_blocks.iter() {
        let Some(term) = &bb_data.terminator else {
            continue;
        };
        let TerminatorKind::Call { func, destination, .. } = &term.kind else {
            continue;
        };
        if !is_asap_identity_call(tcx, func) {
            continue;
        }
        let dest = destination.local;
        // Skip return value (_0) dan args
        if dest.as_u32() == 0 || dest.as_u32() <= body.arg_count as u32 {
            continue;
        }
        if candidates.insert(dest) {
            eprintln!(
                "[asap]   Found candidate: {:?} : {:?}",
                dest, body.local_decls[dest].ty
            );
        }
    }

    candidates
}

/// True bila operand fungsi adalah Call ke `asap_runtime::__asap_identity`.
fn is_asap_identity_call<'a>(tcx: TyCtxt<'a>, func: &Operand<'a>) -> bool {
    let Operand::Constant(c) = func else {
        return false;
    };
    let ty = c.const_.ty();
    if let TyKind::FnDef(def_id, _args) = ty.kind() {
        tcx.item_name(*def_id).as_str() == "__asap_identity"
            && tcx.crate_name(def_id.krate).as_str() == "asap_runtime"
    } else {
        false
    }
}

// ── Step 2: Taint — local apa yang diturunkan dari candidate ──────────────
//
// Taint bersifat flow-INSENSITIVE (over-approx): kalau benar, drop jadi
// lebih lambat (kehilangan optimasi), tidak pernah lebih awal (sound).
// Yang tidak tertaint tapi seharusnya → ditangkap borrowck (fail-safe).

/// Map: local -> set candidate sumber. Candidate memetakan ke dirinya sendiri.
fn compute_taint(body: &Body<'_>, candidates: &HashSet<Local>) -> HashMap<Local, HashSet<Local>> {
    let mut taint: HashMap<Local, HashSet<Local>> = candidates
        .iter()
        .map(|&c| (c, HashSet::from([c])))
        .collect();

    // Fixpoint: definisi bisa muncul setelah pemakaian dalam urutan layout
    // (mis. temp yang dipakai di block awal didefinisikan di ... — jarang,
    // tapi loop murah untuk body kecil).
    loop {
        let mut changed = false;

        for (_bb, bb_data) in body.basic_blocks.iter_enumerated() {
            for stmt in &bb_data.statements {
                let StatementKind::Assign(assign) = &stmt.kind else {
                    continue;
                };
                let (lhs, rvalue) = &**assign;
                let srcs = rvalue_sources(rvalue, &taint);
                if !srcs.is_empty() {
                    let entry = taint.entry(lhs.local).or_default();
                    let before = entry.len();
                    entry.extend(srcs);
                    changed |= entry.len() != before;
                }
            }

            if let Some(term) = &bb_data.terminator {
                match &term.kind {
                    TerminatorKind::Call { func, args, destination, .. } => {
                        let mut srcs = operand_sources(func, &taint);
                        for arg in args.iter() {
                            srcs.extend(operand_sources(&arg.node, &taint));
                        }
                        if !srcs.is_empty() {
                            let entry = taint.entry(destination.local).or_default();
                            let before = entry.len();
                            entry.extend(srcs);
                            changed |= entry.len() != before;
                        }
                    }
                    _ => {}
                }
            }
        }

        if !changed {
            break;
        }
    }

    for (local, srcs) in &taint {
        if !candidates.contains(local) {
            eprintln!("[asap]   {:?} derives from {:?}", local, srcs);
        }
    }
    taint
}

/// Kumpulan candidate sumber yang disebut di sebuah rvalue.
fn rvalue_sources(rvalue: &Rvalue<'_>, taint: &HashMap<Local, HashSet<Local>>) -> HashSet<Local> {
    fn op_sources(
        out: &mut HashSet<Local>,
        o: &Operand<'_>,
        taint: &HashMap<Local, HashSet<Local>>,
    ) {
        out.extend(operand_sources(o, taint));
    }
    fn place_sources(
        out: &mut HashSet<Local>,
        p: &Place<'_>,
        taint: &HashMap<Local, HashSet<Local>>,
    ) {
        if let Some(s) = taint.get(&p.local) {
            out.extend(s.iter().copied());
        }
    }
    let mut out = HashSet::new();
    match rvalue {
        Rvalue::Use(o, _) | Rvalue::Cast(_, o, _) | Rvalue::UnaryOp(_, o)
        | Rvalue::Repeat(o, _) | Rvalue::WrapUnsafeBinder(o, _) => op_sources(&mut out, o, taint),
        Rvalue::BinaryOp(_, ops) => {
            let (l, r) = &**ops;
            op_sources(&mut out, l, taint);
            op_sources(&mut out, r, taint);
        }
        Rvalue::Ref(_, _, p) | Rvalue::RawPtr(_, p) | Rvalue::Discriminant(p)
        | Rvalue::CopyForDeref(p) => place_sources(&mut out, p, taint),
        Rvalue::Aggregate(_, ops) => {
            for o in ops {
                op_sources(&mut out, o, taint);
            }
        }
        Rvalue::ThreadLocalRef(_) => {}
    }
    out
}

fn operand_sources(op: &Operand<'_>, taint: &HashMap<Local, HashSet<Local>>) -> HashSet<Local> {
    match op {
        Operand::Move(p) | Operand::Copy(p) => {
            taint.get(&p.local).cloned().unwrap_or_default()
        }
        Operand::Constant(_) | Operand::RuntimeChecks(_) => HashSet::new(),
    }
}

// ── Step 3: Analisis uses — lokasi terakhir tiap candidate dipakai ────────

#[derive(Debug)]
struct LocalAnalysis {
    /// Lokasi terakhir candidate (atau turunannya) dipakai (termasuk moves)
    last_use: Option<Location>,
    /// Lokasi terakhir NON-MOVE use (untuk log)
    last_borrow_use: Option<Location>,
    /// Lokasi-lokasi Move langsung dari candidate
    moves: Vec<Location>,
    /// SEMUA block tempat scope drop berada (cabang = banyak situs)
    existing_drop_blocks: Vec<BasicBlock>,
    /// True jika candidate di-move-out langsung (untuk log)
    moved_out: bool,
    /// Lokasi use non-move, unik (untuk guard urutan + anchor borrow)
    borrow_uses: Vec<Location>,
    /// Semua lokasi use incl. moves, unik (untuk guard loop)
    uses: Vec<Location>,
}

struct Ctx<'a> {
    candidates: &'a HashSet<Local>,
    /// local -> set candidate sumber (flow-insensitive)
    taint: &'a HashMap<Local, HashSet<Local>>,
}

fn analyze_body(
    body: &Body<'_>,
    candidates: &HashSet<Local>,
    taint: &HashMap<Local, HashSet<Local>>,
) -> HashMap<Local, LocalAnalysis> {
    let mut result: HashMap<Local, LocalAnalysis> = candidates
        .iter()
        .map(|&l| (l, LocalAnalysis {
            last_use: None,
            last_borrow_use: None,
            moves: Vec::new(),
            existing_drop_blocks: Vec::new(),
            moved_out: false,
            borrow_uses: Vec::new(),
            uses: Vec::new(),
        }))
        .collect();

    let ctx = Ctx { candidates, taint };

    for (bb, bb_data) in body.basic_blocks.iter_enumerated() {
        for (stmt_idx, stmt) in bb_data.statements.iter().enumerate() {
            let loc = Location { block: bb, statement_index: stmt_idx };
            match &stmt.kind {
                StatementKind::Assign(assign) => {
                    let (_lhs, rvalue) = &**assign;
                    visit_rvalue(rvalue, loc, &ctx, &mut result);
                }
                StatementKind::FakeRead(inner) => {
                    let (_cause, place) = &**inner;
                    note_place_use(place, false, loc, &ctx, &mut result);
                }
                StatementKind::SetDiscriminant { place, .. } => {
                    note_place_use(place, false, loc, &ctx, &mut result);
                }
                StatementKind::StorageLive(_) | StatementKind::StorageDead(_) => {
                    // Storage sengaja diabaikan: StorageDead local bernama
                    // (mis. `r` di `let r = &*a`) terjadi di akhir fungsi,
                    // jauh setelah borrow mati menurut NLL. Liveness borrow
                    // sudah diwakili oleh uses (termasuk FakeRead).
                }
                StatementKind::PlaceMention(place) => {
                    note_place_use(place, false, loc, &ctx, &mut result);
                }
                StatementKind::AscribeUserType(inner, _) => {
                    let (place, _proj) = &**inner;
                    note_place_use(place, false, loc, &ctx, &mut result);
                }
                StatementKind::Intrinsic(inner) => {
                    use rustc_middle::mir::NonDivergingIntrinsic as I;
                    match &**inner {
                        I::Assume(op) => visit_operand(op, loc, &ctx, &mut result),
                        I::CopyNonOverlapping(c) => {
                            visit_operand(&c.src, loc, &ctx, &mut result);
                            visit_operand(&c.dst, loc, &ctx, &mut result);
                            visit_operand(&c.count, loc, &ctx, &mut result);
                        }
                    }
                }
                StatementKind::ConstEvalCounter
                | StatementKind::Nop
                | StatementKind::Coverage(_) => {}
                StatementKind::BackwardIncompatibleDropHint { place, .. } => {
                    note_place_use(place, false, loc, &ctx, &mut result);
                }
            }
        }

        if let Some(term) = &bb_data.terminator {
            let loc = Location {
                block: bb,
                statement_index: bb_data.statements.len(),
            };
            match &term.kind {
                TerminatorKind::Call { func, args, .. } => {
                    visit_operand(func, loc, &ctx, &mut result);
                    for arg in args.iter() {
                        visit_operand(&arg.node, loc, &ctx, &mut result);
                    }
                }
                TerminatorKind::TailCall { func, args, .. } => {
                    visit_operand(func, loc, &ctx, &mut result);
                    for arg in args.iter() {
                        visit_operand(&arg.node, loc, &ctx, &mut result);
                    }
                }
                TerminatorKind::Drop { place, .. } => {
                    if ctx.candidates.contains(&place.local) {
                        // Scope drop candidate: BUKAN use. Satu candidate
                        // bisa punya BANYAK situs (tiap exit path cabang).
                        // Blok cleanup DIKECUALIKAN: mereka untuk path unwind
                        // (tetap dibutuhkan bila panic terjadi sebelum/sesudah
                        // early drop; elaborasi membuatnya kondisional).
                        // Mereka juga TIDAK BOLEH dinetralkan.
                        if !bb_data.is_cleanup {
                            if let Some(analysis) = result.get_mut(&place.local) {
                                if !analysis.existing_drop_blocks.contains(&bb) {
                                    analysis.existing_drop_blocks.push(bb);
                                }
                            }
                        }
                    } else {
                        // Drop dari value turunan = pemakaian turunan itu.
                        note_place_use(place, false, loc, &ctx, &mut result);
                    }
                }
                TerminatorKind::SwitchInt { discr, .. } => {
                    visit_operand(discr, loc, &ctx, &mut result);
                }
                TerminatorKind::Assert { cond, .. } => {
                    visit_operand(cond, loc, &ctx, &mut result);
                }
                TerminatorKind::Yield { value, .. } => {
                    visit_operand(value, loc, &ctx, &mut result);
                }
                TerminatorKind::InlineAsm { operands, .. } => {
                    use rustc_middle::mir::InlineAsmOperand as O;
                    for op in operands {
                        match op {
                            O::In { value, .. } => visit_operand(value, loc, &ctx, &mut result),
                            O::Out { .. } => {}
                            O::InOut { in_value, .. } => {
                                visit_operand(in_value, loc, &ctx, &mut result)
                            }
                            O::Const { .. } | O::SymFn { .. } | O::SymStatic { .. } => {}
                            // Label asm = target block, bukan value.
                            O::Label { .. } => {}
                        }
                    }
                }
                TerminatorKind::Goto { .. }
                | TerminatorKind::UnwindResume
                | TerminatorKind::UnwindTerminate(_)
                | TerminatorKind::Return
                | TerminatorKind::Unreachable
                | TerminatorKind::CoroutineDrop
                | TerminatorKind::FalseEdge { .. }
                | TerminatorKind::FalseUnwind { .. } => {}
            }
        }
    }

    result
}

/// Catat pemakaian place untuk semua candidate sumbernya (langsung bila
/// local itu sendiri candidate, tak langsung bila turunan tainted).
/// `is_move` + akar candidate = move-out (dicatat di `moves`);
/// pemakaian lain (termasuk move dari temp `&`) = borrow-use.
fn note_place_use(
    place: &Place<'_>,
    is_move: bool,
    loc: Location,
    ctx: &Ctx<'_>,
    result: &mut HashMap<Local, LocalAnalysis>,
) {
    let place_local = place.local;
    let Some(srcs) = ctx.taint.get(&place_local) else {
        return;
    };
    // Clone kecil: hindari pinjam `taint` sambil mutasi `result`.
    let srcs: Vec<Local> = srcs.iter().copied().collect();
    for source in srcs {
        if let Some(analysis) = result.get_mut(&source) {
            let direct_move = is_move && place_local == source;
            if direct_move {
                analysis.moved_out = true;
                push_unique(&mut analysis.moves, loc);
            } else {
                analysis.last_borrow_use = Some(match analysis.last_borrow_use {
                    None => loc,
                    Some(prev) => location_max(Some(prev), Some(loc)).unwrap(),
                });
                push_unique(&mut analysis.borrow_uses, loc);
            }
            analysis.last_use = Some(match analysis.last_use {
                None => loc,
                Some(prev) => location_max(Some(prev), Some(loc)).unwrap(),
            });
            push_unique(&mut analysis.uses, loc);
        }
    }
}

fn push_unique(v: &mut Vec<Location>, loc: Location) {
    if !v.contains(&loc) {
        v.push(loc);
    }
}

// ── Step 4: Mutasi — sisipkan early Drop, netralkan scope drop lama ───────

/// Split basic block di `split_idx` dan sisipkan Drop terminator.
///
/// ```
/// Sebelum (split_idx = k+1):
///   bb_N: [s0, s1, ..., s_k, s_k+1, ..., s_n] → orig_term
///
/// Setelah:
///   bb_N:    [s0, s1, ..., s_k] → Drop(local) → bb_new
///   bb_new:  [s_k+1, ..., s_n] → orig_term
/// ```
/// Return block baru.
fn insert_drop_at(body: &mut Body<'_>, local: Local, bb: BasicBlock, split_idx: usize) {
    let blocks = body.basic_blocks.as_mut();
    assert!(
        split_idx <= blocks[bb].statements.len(),
        "split_idx out of bounds"
    );
    let source_info = blocks[bb].terminator().source_info;
    let is_cleanup = blocks[bb].is_cleanup;

    // Kumpulkan tail statements dan terminator lama
    let tail_stmts = blocks[bb].statements[split_idx..].to_vec();
    let orig_terminator = blocks[bb].terminator().clone();

    // Block baru: tail + original terminator. `push` mengembalikan index-nya.
    let new_bb =
        blocks.push(BasicBlockData::new_stmts(tail_stmts, Some(orig_terminator), is_cleanup));

    // Truncate block lama: buang tail statements, ganti terminator dengan Drop
    blocks[bb].statements.truncate(split_idx);
    blocks[bb].terminator = Some(Terminator {
        source_info,
        kind: TerminatorKind::Drop {
            place: Place::from(local),
            target: new_bb,
            unwind: UnwindAction::Continue,
            replace: false,
            drop: None,
            async_fut: None,
        },
    });
}

/// Ganti Drop terminator di `block` dengan Goto ke target-nya.
/// Ini "menghapus" drop lama yang sudah kita pindahkan lebih awal.
fn neutralize_scope_drop(body: &mut Body<'_>, block: BasicBlock) {
    let blocks = body.basic_blocks.as_mut();
    let target = {
        let term = blocks[block].terminator();
        match term.kind {
            TerminatorKind::Drop { target, .. } => target,
            _ => return, // sudah bukan drop, skip
        }
    };

    let source_info = blocks[block].terminator().source_info;
    blocks[block].terminator = Some(Terminator {
        source_info,
        kind: TerminatorKind::Goto { target },
    });

    eprintln!("[asap]   Neutralized scope drop at {:?}", block);
}

// ── Helpers ───────────────────────────────────────────────────────────────

fn visit_rvalue(
    rvalue: &Rvalue<'_>,
    loc: Location,
    ctx: &Ctx<'_>,
    result: &mut HashMap<Local, LocalAnalysis>,
) {
    match rvalue {
        Rvalue::Use(op, _) | Rvalue::Cast(_, op, _) | Rvalue::UnaryOp(_, op) => {
            visit_operand(op, loc, ctx, result);
        }
        Rvalue::Repeat(op, _) | Rvalue::WrapUnsafeBinder(op, _) => {
            visit_operand(op, loc, ctx, result);
        }
        Rvalue::BinaryOp(_, ops) => {
            let (l, r) = &**ops;
            visit_operand(l, loc, ctx, result);
            visit_operand(r, loc, ctx, result);
        }
        // Borrow: &*candidate — bukan "last use", tapi awal borrow
        // (di-track via taint + StorageDead, bukan di sini).
        Rvalue::Ref(..) | Rvalue::RawPtr(..) => {}
        Rvalue::Aggregate(_, operands) => {
            for op in operands {
                visit_operand(op, loc, ctx, result);
            }
        }
        Rvalue::Discriminant(place) | Rvalue::CopyForDeref(place) => {
            note_place_use(place, false, loc, ctx, result);
        }
        Rvalue::ThreadLocalRef(_) => {}
    }
}

fn visit_operand(
    op: &Operand<'_>,
    loc: Location,
    ctx: &Ctx<'_>,
    result: &mut HashMap<Local, LocalAnalysis>,
) {
    match op {
        Operand::Move(place) => note_place_use(place, true, loc, ctx, result),
        Operand::Copy(place) => note_place_use(place, false, loc, ctx, result),
        Operand::Constant(_) | Operand::RuntimeChecks(_) => {}
    }
}

fn same_location(a: Location, b: Location) -> bool {
    a.block == b.block && a.statement_index == b.statement_index
}

/// Bandingkan dua Location: mana yang "lebih akhir"?
/// Simplified: block index lebih besar = lebih akhir.
/// (Guard CFG `is_predecessor_of` di plan memastikan kasus
/// loop/branch yang menipu tetap di-skip.)
fn location_max(a: Option<Location>, b: Option<Location>) -> Option<Location> {
    match (a, b) {
        (None, x) | (x, None) => x,
        (Some(la), Some(lb)) => {
            if la.block > lb.block {
                Some(la)
            } else if lb.block > la.block {
                Some(lb)
            } else if la.statement_index >= lb.statement_index {
                Some(la)
            } else {
                Some(lb)
            }
        }
    }
}
