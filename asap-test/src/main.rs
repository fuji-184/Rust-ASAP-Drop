// asap-test/src/main.rs
//
// Test untuk membuktikan bahwa:
//   1. asap!(x) di-drop lebih awal dari let x biasa
//   2. let x biasa tetap di-drop di akhir scope (tidak terpengaruh)
//   3. Borrow aktif menunda drop sampai borrow selesai
//   4. Mix asap dan non-asap dalam satu scope bekerja benar

use asap_macro::asap;
use std::sync::{Arc, Mutex};

// ── Helper: resource yang log kapan ia di-create/drop ─────────────────────

struct Tracked {
    name: &'static str,
    log: Arc<Mutex<Vec<String>>>,
}

impl Tracked {
    fn new(name: &'static str, log: &Arc<Mutex<Vec<String>>>) -> Self {
        log.lock().unwrap().push(format!("  CREATE {name}"));
        Tracked { name, log: log.clone() }
    }
    fn use_it(&self) {
        self.log.lock().unwrap().push(format!("  USE    {}", self.name));
    }
}

impl Drop for Tracked {
    fn drop(&mut self) {
        self.log.lock().unwrap().push(format!("  DROP   {}", self.name));
    }
}

// ─────────────────────────────────────────────────────────────────────────
// Test 1: Fine-grained control — mix asap dan non-asap dalam satu scope
// ─────────────────────────────────────────────────────────────────────────

fn test_mix(log: &Arc<Mutex<Vec<String>>>) {
    log.lock().unwrap().push("[test_mix]".into());

    let normal = Tracked::new("normal", log);     // drop di akhir scope
    let early  = asap!(Tracked::new("early", log)); // drop setelah last use

    normal.use_it();
    early.use_it();   // ← last use of `early`
    // dengan plugin: `early` di-drop SEKARANG

    normal.use_it();  // normal masih hidup
    log.lock().unwrap().push("  --- middle ---".into());
    // di sini: early sudah tidak ada, normal masih ada

} // normal di-drop di sini (akhir scope)

// Expected output:
//   CREATE normal
//   CREATE early
//   USE    normal
//   USE    early
//   DROP   early    ← ASAP!
//   USE    normal
//   --- middle ---
//   DROP   normal   ← normal scope

// ─────────────────────────────────────────────────────────────────────────
// Test 2: Borrow yang aktif menunda drop
// ─────────────────────────────────────────────────────────────────────────

fn test_borrow_delays_drop(log: &Arc<Mutex<Vec<String>>>) {
    log.lock().unwrap().push("[test_borrow_delays_drop]".into());

    let a = asap!(Tracked::new("A", log));

    a.use_it();         // last direct use of a

    let r = &*a;        // borrow a — drop harus ditunda!
    r.use_it();         // last use of borrow r
    // r expired di sini

    // dengan plugin: a di-drop di sini (setelah r expired)
    // BUKAN di akhir scope

    log.lock().unwrap().push("  --- after borrow ---".into());
}

// Expected output:
//   CREATE A
//   USE    A
//   USE    A       ← via borrow r
//   DROP   A       ← setelah borrow r expired, bukan di akhir scope
//   --- after borrow ---

// ─────────────────────────────────────────────────────────────────────────
// Test 3: Chained — beberapa asap variable, masing-masing drop sendiri
// ─────────────────────────────────────────────────────────────────────────

fn test_chained(log: &Arc<Mutex<Vec<String>>>) {
    log.lock().unwrap().push("[test_chained]".into());

    let conn   = asap!(Tracked::new("conn",   log));
    let query  = asap!(Tracked::new("query",  log));
    let report = asap!(Tracked::new("report", log));

    conn.use_it();
    // DROP conn setelah ini

    query.use_it();
    // DROP query setelah ini

    report.use_it();
    // DROP report setelah ini

    log.lock().unwrap().push("  --- all freed ---".into());
    // tanpa asap: semua baru di-drop di sini, urutan terbalik
}

// Expected output dengan asap:
//   CREATE conn, CREATE query, CREATE report
//   USE conn   → DROP conn
//   USE query  → DROP query
//   USE report → DROP report
//   --- all freed ---

// ─────────────────────────────────────────────────────────────────────────
// Test 4: asap dengan Vec — tipe standar library
// ─────────────────────────────────────────────────────────────────────────

fn test_with_vec() -> usize {
    // Contoh dari requirement kamu: asap!(vec![10])
    let a = asap!(vec![10, 20, 30]);
    let b = vec![1, 2, 3];           // normal, tidak asap

    let sum_a: usize = a.iter().sum();
    // a di-drop setelah ini (dengan plugin)

    let sum_b: usize = b.iter().sum();
    // b di-drop di akhir scope

    sum_a + sum_b
}

// ─────────────────────────────────────────────────────────────────────────
// Test 5: asap! tidak mempengaruhi return value
// ─────────────────────────────────────────────────────────────────────────

fn test_return_safety(log: &Arc<Mutex<Vec<String>>>) -> String {
    log.lock().unwrap().push("[test_return_safety]".into());

    let temp = asap!(Tracked::new("temp", log));
    temp.use_it();
    // temp di-drop setelah ini

    let result = String::from("result value");
    // result tidak asap, di-move sebagai return value
    result // ← tidak di-drop, di-move keluar fungsi
}

// ─────────────────────────────────────────────────────────────────────────
// Test 6: Mutex lock — use case yang paling umum untuk ASAP drop
// ─────────────────────────────────────────────────────────────────────────

fn test_mutex_early_unlock() {
    let data = Arc::new(Mutex::new(vec![1, 2, 3]));

    // Tanpa asap: lock tertahan sampai akhir scope
    // Dengan asap: lock dilepas segera setelah last use
    let guard = asap!(data.lock().unwrap());
    let _len = guard.len(); // last use of guard
    // DROP guard di sini → mutex unlock ASAP
    // Thread lain bisa langsung akses

    // ... pekerjaan panjang yang tidak butuh lock ...
    let _ = (0..1000).sum::<i32>();
    // Tanpa asap: lock masih tertahan sampai sini
    // Dengan asap: sudah dilepas jauh lebih awal ↑
}

// ─────────────────────────────────────────────────────────────────────────
// Main
// ─────────────────────────────────────────────────────────────────────────

fn main() {
    let log = Arc::new(Mutex::new(Vec::<String>::new()));

    test_mix(&log);
    test_borrow_delays_drop(&log);
    test_chained(&log);
    test_mutex_early_unlock();

    let vec_sum = test_with_vec();
    let ret = test_return_safety(&log);

    println!("=== ASAP Drop Event Log ===\n");
    for line in log.lock().unwrap().iter() {
        println!("{line}");
    }

    println!("\nvec_sum = {vec_sum}");
    println!("returned = {ret}");

    println!("\n=== Expected (dengan plugin aktif) ===");
    println!("test_mix:");
    println!("  CREATE normal → CREATE early → USE normal → USE early");
    println!("  → DROP early  ← ASAP drop!");
    println!("  → USE normal → middle → DROP normal");
    println!();
    println!("test_borrow_delays_drop:");
    println!("  CREATE A → USE A → USE A(via borrow)");
    println!("  → DROP A  ← drop ditunda sampai borrow expired");
    println!("  → after borrow");
    println!();
    println!("test_chained:");
    println!("  Setiap variable di-drop langsung setelah use-nya masing-masing");
}
