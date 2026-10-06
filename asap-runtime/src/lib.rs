// asap-runtime/src/lib.rs
//
// Marker untuk ASAP drop: `__asap_identity`, dipanggil oleh macro `asap!()`.
//
// Value KEMBALI DENGAN TIPE ASLINYA (T), sehingga user code tidak berubah
// sama sekali: by-value, borrow, dan method call semuanya native.
// MIR pass (asap-driver) mendeteksi local yang diinisialisasi lewat Call
// ke fungsi ini dan memindahkan Drop-nya lebih awal.

/// Marker identitas untuk ASAP drop. Dipanggil oleh macro `asap!()`.
///
/// Zero-cost: langsung di-inline, tipe kembaliannya T (bukan wrapper),
/// sehingga tidak ada perbedaan tipe di user code.
#[inline(always)]
pub fn __asap_identity<T>(x: T) -> T {
    x
}
