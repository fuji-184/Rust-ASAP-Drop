// asap-runtime/src/lib.rs
//
// AsapOwned<T> adalah wrapper transparan di atas T.
//
// Dua peran:
//   1. MARKER: MIR pass scan semua local variables, cari yang type-nya
//              AsapOwned<_>. Hanya mereka yang dapat ASAP treatment.
//
//   2. DEREF COERCION: Semua operasi pada AsapOwned<T> di-forward ke T,
//              sehingga user tidak terasa bedanya:
//              let a = asap!(vec![10]);
//              a.push(20);   ← bekerja seperti biasa via DerefMut

use std::ops::{Deref, DerefMut};
use std::fmt;

/// Wrapper marker untuk ASAP drop.
/// Zero-cost: tidak ada overhead runtime, tidak ada extra allocation.
///
/// # Jangan dibuat langsung
/// Gunakan macro `asap!()` dari crate `asap-macro`.
///
/// ```
/// let a = asap!(vec![10, 20, 30]);
/// let b = asap!(String::from("hello"));
/// let c = asap!(MyStruct::new());
/// ```
#[repr(transparent)]  // layout identik dengan T, zero overhead
pub struct AsapOwned<T> {
    // Field ini SENGAJA bernama `__asap_inner` dengan prefix khusus
    // agar MIR pass bisa identify field access ke wrapper ini
    // (membedakan dari field biasa).
    pub __asap_inner: T,
}

impl<T> AsapOwned<T> {
    /// Buat AsapOwned baru. Dipanggil oleh macro asap!().
    #[inline(always)]
    pub fn new(value: T) -> Self {
        AsapOwned { __asap_inner: value }
    }

    /// Ambil inner value, consume wrapper.
    /// MIR pass akan treat ini sebagai "last use" juga.
    #[inline(always)]
    pub fn into_inner(self) -> T {
        // Tidak bisa `self.__asap_inner` langsung karena Self: Drop (E0509).
        // Gunakan ManuallyDrop + ptr::read agar tidak double-drop.
        let this = std::mem::ManuallyDrop::new(self);
        unsafe { std::ptr::read(&this.__asap_inner) }
    }
}

// ── Deref: AsapOwned<T> transparan terhadap T ─────────────────────────────

impl<T> Deref for AsapOwned<T> {
    type Target = T;

    #[inline(always)]
    fn deref(&self) -> &T {
        &self.__asap_inner
    }
}

impl<T> DerefMut for AsapOwned<T> {
    #[inline(always)]
    fn deref_mut(&mut self) -> &mut T {
        &mut self.__asap_inner
    }
}

// ── Drop: ketika MIR pass TIDAK aktif, behavior default tetap correct ─────

impl<T> Drop for AsapOwned<T> {
    fn drop(&mut self) {
        // Inner value (T) akan di-drop otomatis oleh Rust karena ia field struct.
        // Kita tidak perlu melakukan apapun secara eksplisit di sini.
        //
        // Ketika MIR pass AKTIF:
        //   Pass akan mengganti/memindahkan Drop terminator ini lebih awal.
        //
        // Ketika MIR pass TIDAK aktif (compile biasa):
        //   Drop ini jalan di akhir scope seperti biasa — behavior tetap correct,
        //   hanya tidak ASAP. Ini penting: plugin bersifat optimization, bukan
        //   correctness requirement.
    }
}

// ── Traits forwarding ─────────────────────────────────────────────────────

impl<T: fmt::Debug> fmt::Debug for AsapOwned<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Tampilkan seperti T biasa, bukan "AsapOwned { ... }"
        // sehingga debugging tidak terasa beda
        fmt::Debug::fmt(&self.__asap_inner, f)
    }
}

impl<T: fmt::Display> fmt::Display for AsapOwned<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.__asap_inner, f)
    }
}

impl<T: Clone> Clone for AsapOwned<T> {
    fn clone(&self) -> Self {
        AsapOwned::new(self.__asap_inner.clone())
    }
}

impl<T: PartialEq> PartialEq for AsapOwned<T> {
    fn eq(&self, other: &Self) -> bool {
        self.__asap_inner == other.__asap_inner
    }
}

impl<T: Eq> Eq for AsapOwned<T> {}

// ── BorrowInfo: metadata untuk smart drop timing ─────────────────────────
//
// Catatan penting: struct ini tidak dipakai di runtime.
// Ini adalah "hint" untuk MIR pass, yang membaca tipe ini
// dalam MIR type system dan tahu cara memperlakukannya.
//
// Analogi: seperti PhantomData<T> — ada di type level, hilang di runtime.

/// Marker trait: tipe ini adalah ASAP-managed.
/// MIR pass mencari implementor trait ini untuk menemukan kandidat.
pub trait AsapManaged {}
impl<T> AsapManaged for AsapOwned<T> {}
