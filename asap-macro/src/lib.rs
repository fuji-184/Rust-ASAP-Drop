// asap-macro/src/lib.rs
//
// Proc macro yang mengubah:
//
//   asap!(expr)
//
// menjadi:
//
//   ::asap_runtime::AsapOwned::new(expr)
//
// Sesederhana itu di sisi macro — "magic" ada di MIR pass
// yang berjalan di asap-driver.

use proc_macro::TokenStream;
use quote::quote;
use syn::{parse_macro_input, Expr};

/// Wrap sebuah expression dengan ASAP drop semantics.
///
/// Variable yang dibungkus `asap!()` akan di-drop segera setelah
/// last use (atau setelah semua borrow selesai), bukan di akhir scope.
///
/// # Contoh
///
/// ```rust
/// use asap_macro::asap;
///
/// fn example() {
///     let a = asap!(vec![1, 2, 3]);     // Vec di-drop segera setelah last use
///     let b = asap!(String::from("hi")); // String di-drop segera setelah last use
///
///     println!("{:?}", *a);  // use a
///     // a di-drop di sini (bukan di akhir fungsi)
///
///     println!("{}", *b);    // use b
///     // b di-drop di sini
///
///     // ... kode lain tanpa a dan b di memory ...
/// }
/// ```
///
/// # Borrow Safety
///
/// Jika ada borrow yang masih aktif, drop ditunda sampai borrow selesai:
///
/// ```rust
/// let a = asap!(vec![1, 2, 3]);
/// let r = &*a;           // borrow aktif
/// println!("{:?}", r);   // last use of borrow
/// // a di-drop DI SINI, setelah borrow r selesai
/// ```
#[proc_macro]
pub fn asap(input: TokenStream) -> TokenStream {
    let expr = parse_macro_input!(input as Expr);

    // Validasi: beberapa expression tidak masuk akal untuk di-wrap
    match &expr {
        // asap!(42) — primitive tidak punya destructor, tidak berguna
        // tapi kita allow saja, hanya buang-buang type wrapper
        // (MIR pass akan skip karena tidak implement Drop)
        Expr::Lit(_) => {
            // Tetap wrap supaya API konsisten, MIR pass akan skip
        }
        // asap!({ ... block ... }) — valid, wrap seluruh block
        Expr::Block(_) => {}
        // Kasus normal: function call, struct init, method chain, dll
        _ => {}
    }

    let expanded = quote! {
        // Gunakan path absolut agar tidak perlu user import manual
        ::asap_runtime::AsapOwned::new(#expr)
    };

    expanded.into()
}

/// Variant untuk closure — berguna kalau construction-nya expensive
/// dan ingin lazy:
///
/// ```rust
/// let a = asap_lazy!(|| expensive_computation());
/// ```
///
/// Ini equivalent dengan `asap!(expensive_computation())` tapi
/// membuat intent lebih eksplisit.
#[proc_macro]
pub fn asap_lazy(input: TokenStream) -> TokenStream {
    let expr = parse_macro_input!(input as Expr);

    let expanded = quote! {
        // Evaluate closure langsung — lazy hanya soal keterbacaan
        ::asap_runtime::AsapOwned::new((#expr)())
    };

    expanded.into()
}
