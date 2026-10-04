// pass.rs
//
// Inti dari ASAP Drop:
//
//  1. Scan locals yang bertipe AsapOwned<T>       → "candidates"
//  2. Taint flow-insensitive: local apa pun yang nilainya diturunkan dari
//     candidate (`&*a`, hasil `Deref::deref(a)`, salinan ref, ...) dicatat
//     sebagai turunan candidate. Ini menangani rantai Deref yang tidak
//     terlihat sebagai `Ref` langsung di MIR.
//  3. Untuk tiap candidate, cari last_use = lokasi terakhir candidate
//     ATAU turunannya dipakai (mencakup FakeRead/PlaceMention/asm/intrinsic).
//     StorageDead SENGAJA tidak dipakai: local bernama (mis. `let r = &*a`)
//     di-StorageDead di akhir fungsi, jauh setelah borrow-nya mati menurut
//     NLL. last_use + validasi borrowck sudah cukup dan tepat.
//  4. safe_drop_point = last_use
//  5. Split basic block di safe_drop_point, sisipkan Drop terminator
//  6. Netralkan scope drop lama (Drop → Goto) → tidak double-drop
//
// Cara kerja dengan compiler (nightly 1.97):
//  - main.rs membungkus query `mir_built` via `override_queries`.
//    `plan_asap_transform` dipanggil SEBELUM borrowck, sehingga transform
//    yang tidak sound ditolak borrowck sebagai compile error (fail-safe),
//    bukan silent UB.
//  - Guard tambahan: semua use harus terurut SEBELUM titik drop menurut
//    CFG (`Location::is_predecessor_of`), dan titik drop harus mencapai
//    scope drop lama. Kasus loop/branch yang meragukan di-skip (fallback
//    ke drop normal di akhir scope).
//  - Kasus move-out (`Move` langsung dari candidate, mis. `into_inner`)
//    di-skip: value sudah pindah, early drop tidak ada gunanya.
//
// Catatan kompatibilitas nightly:
//  - `MirPass` tidak lagi publik → pass ini struct biasa.
//  - `Rvalue::Use` kini 2 field; varian `CheckedBinaryOp`/`AddressOf`/`Len`
//    sudah dihapus; pola `box (...)` dihindari (tanpa feature gate).
//  - `BasicBlockData` non-exhaustive → konstruksi via `new_stmts`.
//  - `body.basic_blocks` adalah `BasicBlocks` → mutasi via `as_mut()`.
//  - Konstruksi `Drop` wajib isi field baru `drop: None, async_fut: None`.

#![allow(dead_code)]

use rustc_middle::{
    mir::{BasicBlock, BasicBlockData, Body, Local, Location, Operand, Place, Rvalue,
        StatementKind, Terminator, TerminatorKind, UnwindAction},
    ty::{TyCtxt, TyKind},
};

use std::collections::{HashMap, HashSet};

/// Satu early-drop yang akan diterapkan, plus cara menyisipkannya.
#[derive(Debug, Clone, Copy)]
pub struct AsapDrop {
    local: Local,
    /// Lokasi use terakhir (untuk log/guard).
    after: Location,
    old_block: BasicBlock,
    point: InsertPoint,
}

#[derive(Debug, Clone, Copy)]
enum InsertPoint {
    /// Split `after.block` tepat setelah statement `after`.
    AfterStmt,
    /// Sisipkan block drop baru antara `pred` dan successor normalnya
    /// (dipakai bila use terakhir adalah terminator seperti `Call`,
    /// yang tidak bisa di-split). Ditentukan saat apply lewat successor
    /// SAAT ITU ( chaining otomatis bila beberapa drop berbagi pred).
    BeforeSucc { pred: BasicBlock },
}

// ── Entry: susun rencana transform untuk satu body ─────────────────────────

pub fn plan_asap_transform<'a>(tcx: TyCtxt<'a>, body: &Body<'a>) -> Vec<AsapDrop> {
    let candidates = find_asap_candidates(tcx, body);
    if candidates.is_empty() {
        return Vec::new();
    }

    // Taint flow-insensitive: local -> set candidate sumber.
    // Menangani `&*a` langsung, rantai `Deref::deref(a)`, dan reborrow.
    let taint = compute_taint(body, &candidates);

    let mut analysis = analyze_body(body, &candidates, &taint);
    for info in analysis.values_mut() {
        info.safe_drop_point = info.last_use;
    }

    let mut plan = Vec::new();
    for (&local, info) in &analysis {
        eprintln!(
            "[asap]   {:?}: last_use={:?}, scope_drop={:?}, moved_out={}",
            local, info.last_use, info.existing_drop_block, info.moved_out
        );
        let (Some(after), Some(old_block)) = (info.safe_drop_point, info.existing_drop_block)
        else {
            eprintln!("[asap]   skip {:?}: no use or no scope drop found", local);
            continue;
        };
        if info.moved_out {
            eprintln!("[asap]   skip {:?}: moved out, early drop pointless", local);
            continue;
        }

        // Tentukan titik sisip.
        let after_len = body.basic_blocks[after.block].statements.len();
        let (point, drop_exec_loc) = if after.statement_index < after_len {
            (InsertPoint::AfterStmt, after)
        } else {
            // Use terakhir adalah terminator (mis. Call `use_it()`):
            // sisipkan di awal successor normalnya.
            let term = body.basic_blocks[after.block].terminator();
            let Some(succ) = single_normal_succ(term) else {
                eprintln!("[asap]   skip {:?}: last use has no single successor", local);
                continue;
            };
            // Blok cleanup dan non-cleanup tidak boleh dicampur.
            if body.basic_blocks[after.block].is_cleanup
                != body.basic_blocks[succ].is_cleanup
            {
                eprintln!("[asap]   skip {:?}: cleanup mismatch", local);
                continue;
            }
            let start = Location { block: succ, statement_index: 0 };
            (InsertPoint::BeforeSucc { pred: after.block }, start)
        };

        // Semua use harus terurut sebelum titik eksekusi drop.
        let ordered = info.uses.iter().all(|&u| {
            same_location(u, drop_exec_loc) || u.is_predecessor_of(drop_exec_loc, body)
        });
        if !ordered {
            eprintln!("[asap]   skip {:?}: use order unclear (loop/branch?)", local);
            continue;
        }
        // Titik eksekusi drop harus mencapai scope drop lama.
        let drop_loc = Location {
            block: old_block,
            statement_index: body.basic_blocks[old_block].statements.len(),
        };
        if !(same_location(drop_exec_loc, drop_loc)
            || drop_exec_loc.is_predecessor_of(drop_loc, body))
        {
            eprintln!("[asap]   skip {:?}: drop point cannot reach scope drop", local);
            continue;
        }

        eprintln!(
            "[asap] early-drop {:?} at {:?} (scope drop at {:?})",
            local, drop_exec_loc, old_block
        );
        plan.push(AsapDrop { local, after, old_block, point });
    }

    // BeforeSucc dulu (chaining via successor saat-itu), lalu AfterStmt
    // menurun agar split tidak menggeser lokasi rencana lain.
    plan.sort_by(|a, b| {
        fn rank(p: &AsapDrop) -> u8 {
            match p.point {
                InsertPoint::BeforeSucc { .. } => 0,
                InsertPoint::AfterStmt => 1,
            }
        }
        (rank(a), std::cmp::Reverse((a.after.block.as_u32(), a.after.statement_index)))
            .cmp(&(rank(b), std::cmp::Reverse((b.after.block.as_u32(), b.after.statement_index))))
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
pub fn apply_asap_plan(body: &mut Body<'_>, plan: &[AsapDrop]) {
    for item in plan {
        match item.point {
            InsertPoint::AfterStmt => {
                let new_bb = insert_drop_after(body, item.local, item.after);
                // Kalau split terjadi di block yang sama dengan scope drop
                // lama, drop lama sudah ikut pindah ke block baru.
                let stale =
                    if item.old_block == item.after.block { new_bb } else { item.old_block };
                neutralize_scope_drop(body, stale);
                eprintln!("[asap]   applied early drop of {:?} at {:?}", item.local, item.after);
            }
            InsertPoint::BeforeSucc { pred } => {
                insert_drop_before_succ(body, item.local, pred);
                neutralize_scope_drop(body, item.old_block);
                eprintln!(
                    "[asap]   applied early drop of {:?} after {:?}",
                    item.local, item.after
                );
            }
        }
    }
}

/// Sisipkan block drop baru antara `pred` dan successor normalnya SAAT INI
/// (bukan yang tercatat — chaining otomatis bila berbagi pred).
/// Return block drop baru.
fn insert_drop_before_succ(
    body: &mut Body<'_>,
    local: Local,
    pred: BasicBlock,
) -> BasicBlock {
    let blocks = body.basic_blocks.as_mut();
    let source_info = blocks[pred].terminator().source_info;
    let is_cleanup = blocks[pred].is_cleanup;

    // Successor saat ini (sudah di-guard single-succ saat plan).
    let succ = single_normal_succ(blocks[pred].terminator())
        .expect("BeforeSucc pred lost its single successor");

    // Block drop baru: tanpa statement, langsung Drop → succ.
    let drop_bb = blocks.push(BasicBlockData::new_stmts(
        Vec::new(),
        Some(Terminator {
            source_info,
            kind: TerminatorKind::Drop {
                place: Place::from(local),
                target: succ,
                unwind: UnwindAction::Continue,
                replace: false,
                drop: None,
                async_fut: None,
            },
        }),
        is_cleanup,
    ));

    // Arahkan successor normal pred ke block drop baru.
    // (Hanya successor normal; edge unwind/cleanup dibiarkan.)
    let term = blocks[pred].terminator_mut();
    match &mut term.kind {
        TerminatorKind::Call { target: Some(t), .. } => *t = drop_bb,
        TerminatorKind::Goto { target } => *target = drop_bb,
        TerminatorKind::Assert { target, .. } => *target = drop_bb,
        TerminatorKind::Drop { target, .. } => *target = drop_bb,
        TerminatorKind::FalseEdge { real_target, .. } => *real_target = drop_bb,
        TerminatorKind::FalseUnwind { real_target, .. } => *real_target = drop_bb,
        _ => unreachable!("BeforeSucc pred lost its single successor"),
    }

    drop_bb
}

// ── Step 1: Temukan locals bertipe AsapOwned<T> ───────────────────────────

fn find_asap_candidates<'a>(tcx: TyCtxt<'a>, body: &Body<'a>) -> HashSet<Local> {
    let mut candidates = HashSet::new();

    for (local, decl) in body.local_decls.iter_enumerated() {
        // Skip return value (_0) dan args
        if local.as_u32() == 0 || local.as_u32() <= body.arg_count as u32 {
            continue;
        }

        let ty = decl.ty;

        // Cek apakah tipe ini adalah AsapOwned<_>
        if let TyKind::Adt(adt_def, _) = ty.kind() {
            let name = tcx.item_name(adt_def.did());
            if name.as_str() == "AsapOwned" {
                // Double-check: crate-nya adalah asap_runtime
                let crate_name = tcx.crate_name(adt_def.did().krate);
                if crate_name.as_str() == "asap_runtime" {
                    candidates.insert(local);
                    eprintln!("[asap]   Found candidate: {:?} : {:?}", local, ty);
                }
            }
        }
    }

    candidates
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
    /// Lokasi terakhir candidate (atau turunannya) dipakai
    last_use: Option<Location>,
    /// max = last_use (lihat catatan di header)
    safe_drop_point: Option<Location>,
    /// Block tempat scope drop lama berada
    existing_drop_block: Option<BasicBlock>,
    /// True jika candidate di-move-out langsung (skip transform)
    moved_out: bool,
    /// Semua lokasi use (untuk guard urutan CFG)
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
            safe_drop_point: None,
            existing_drop_block: None,
            moved_out: false,
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
                    note_place_use(place.local, false, loc, &ctx, &mut result);
                }
                StatementKind::SetDiscriminant { place, .. } => {
                    note_place_use(place.local, false, loc, &ctx, &mut result);
                }
                StatementKind::StorageLive(_) | StatementKind::StorageDead(_) => {
                    // Storage sengaja diabaikan: StorageDead local bernama
                    // (mis. `r` di `let r = &*a`) terjadi di akhir fungsi,
                    // jauh setelah borrow mati menurut NLL. Liveness borrow
                    // sudah diwakili oleh uses (termasuk FakeRead).
                }
                StatementKind::PlaceMention(place) => {
                    note_place_use(place.local, false, loc, &ctx, &mut result);
                }
                StatementKind::AscribeUserType(inner, _) => {
                    let (place, _proj) = &**inner;
                    note_place_use(place.local, false, loc, &ctx, &mut result);
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
                    note_place_use(place.local, false, loc, &ctx, &mut result);
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
                        // Scope drop candidate: BUKAN use, dicatat terpisah.
                        if let Some(analysis) = result.get_mut(&place.local) {
                            analysis.existing_drop_block = Some(bb);
                        }
                    } else {
                        // Drop dari value turunan = pemakaian turunan itu.
                        note_place_use(place.local, false, loc, &ctx, &mut result);
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

/// Catat pemakaian place berakar di `place_local` untuk semua candidate
/// sumbernya (langsung bila local itu sendiri candidate, tak langsung bila
/// turunan tainted). `is_move` menandai move-out bila akarnya candidate.
fn note_place_use(
    place_local: Local,
    is_move: bool,
    loc: Location,
    ctx: &Ctx<'_>,
    result: &mut HashMap<Local, LocalAnalysis>,
) {
    let Some(srcs) = ctx.taint.get(&place_local) else {
        return;
    };
    // Clone kecil: hindari pinjam `taint` sambil mutasi `result`.
    // (taint dan result objek berbeda, tapi borrow checker butuh ini
    //  karena keduanya diakses via ctx/result — sebenarnya aman langsung;
    //  clone untuk kejelasan.)
    let srcs: Vec<Local> = srcs.iter().copied().collect();
    for source in srcs {
        if let Some(analysis) = result.get_mut(&source) {
            if is_move && place_local == source {
                analysis.moved_out = true;
            }
            analysis.last_use = Some(match analysis.last_use {
                None => loc,
                Some(prev) => location_max(Some(prev), Some(loc)).unwrap(),
            });
            analysis.uses.push(loc);
        }
    }
}

// ── Step 4: Mutasi — sisipkan early Drop, netralkan scope drop lama ───────

/// Split basic block di `after_loc` dan sisipkan Drop terminator.
///
/// ```
/// Sebelum:
///   bb_N: [s0, s1, ..., s_k(drop_point), s_k+1, ..., s_n] → orig_term
///
/// Setelah:
///   bb_N:    [s0, s1, ..., s_k] → Drop(local) → bb_new
///   bb_new:  [s_k+1, ..., s_n] → orig_term
/// ```
/// Return block baru. Kalau block yang di-split memuat scope drop lama
/// sebagai terminatornya, drop lama ikut pindah ke block baru.
fn insert_drop_after(body: &mut Body<'_>, local: Local, after_loc: Location) -> BasicBlock {
    let bb = after_loc.block;
    let split_idx = after_loc.statement_index + 1; // split SETELAH statement ini

    let blocks = body.basic_blocks.as_mut();
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

    new_bb
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
            note_place_use(place.local, false, loc, ctx, result);
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
        Operand::Move(place) => note_place_use(place.local, true, loc, ctx, result),
        Operand::Copy(place) => note_place_use(place.local, false, loc, ctx, result),
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
