// asap-macro/src/lib.rs
//
// Proc macro yang mengubah:
//
//   asap!(expr)
//
// menjadi:
//
//   ::asap_runtime::__asap_identity(expr)
//
// PENTING: tipe kembaliannya T ASLI (bukan wrapper), sehingga user code
// tidak berubah sama sekali — by-value, borrow, dan method call semuanya
// native. "Magic"-nya ada di MIR pass (asap-driver) yang mendeteksi local
// yang diinisialisasi lewat Call ke `__asap_identity` dan memindahkan
// Drop-nya lebih awal.

use proc_macro::TokenStream;
use quote::quote;
use syn::{parse_macro_input, Expr};

/// Tandai sebuah expression dengan ASAP drop semantics.
///
/// Variable yang dibungkus `asap!()` akan di-drop segera setelah
/// last use (atau setelah semua borrow selesai), bukan di akhir scope.
/// Tipe variable TETAP tipe aslinya — tidak ada wrapper:
///
/// ```rust,ignore
/// use asap_macro::asap;
///
/// fn takes_ownership(v: Vec<i32>) { /* ... */ }
/// fn borrows(v: &Vec<i32>) { /* ... */ }
///
/// fn example() {
///     let a = asap!(vec![1, 2, 3]); // a: Vec<i32>, bukan wrapper!
///
///     takes_ownership(a.clone()); // by-value biasa
///     borrows(&a);                // borrow biasa
///
///     let sum: i32 = a.iter().sum(); // last use
///     // a di-drop di sini (bukan di akhir fungsi)
/// }
/// ```
///
/// # Borrow Safety
///
/// Jika ada borrow yang masih aktif, drop ditunda sampai borrow selesai:
///
/// ```rust,ignore
/// let a = asap!(vec![1, 2, 3]);
/// let r = &a;            // borrow aktif
/// println!("{:?}", r);   // last use of borrow
/// // a di-drop DI SINI, setelah borrow r selesai
/// ```
#[proc_macro]
pub fn asap(input: TokenStream) -> TokenStream {
    let expr = parse_macro_input!(input as Expr);

    let expanded = quote! {
        // Path absolut agar user tidak perlu import manual.
        // Identity call ini yang dideteksi MIR pass sebagai marker.
        ::asap_runtime::__asap_identity(#expr)
    };

    expanded.into()
}

/// Variant untuk closure — berguna kalau construction-nya expensive
/// dan ingin lazy:
///
/// ```rust,ignore
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
        ::asap_runtime::__asap_identity((#expr)())
    };

    expanded.into()
}
