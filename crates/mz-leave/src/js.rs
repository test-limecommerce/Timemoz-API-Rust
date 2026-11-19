//! Semantik angka JavaScript yang dipakai modul cuti: `Math.round`, `round2`
//! (`Math.round(n * 100) / 100`), dan serialisasi `Float` Prisma (`12`, bukan `12.0`).

use serde::Serializer;

/// `Math.round(x)`: setengah dibulatkan ke +∞ (`Math.round(-0.5) === -0`, `2.5 → 3`).
/// Tidak memakai `x + 0.5` supaya `0.49999999999999994` tetap 0 seperti JS.
pub fn js_round(x: f64) -> f64 {
    if !x.is_finite() {
        return x;
    }
    let f = x.floor();
    let r = if x - f >= 0.5 { f + 1.0 } else { f };
    // Hindari `-0` di JSON (`JSON.stringify(-0) === "0"`).
    if r == 0.0 { 0.0 } else { r }
}

/// `round2(n) = Math.round(n * 100) / 100` (salinan di beberapa service Node).
pub fn round2(n: f64) -> f64 {
    js_round(n * 100.0) / 100.0
}

/// `serialize_with` untuk kolom `Float`: angka bulat tanpa `.0` seperti `JSON.stringify`.
pub fn ser_f64<S: Serializer>(v: &f64, s: S) -> Result<S::Ok, S::Error> {
    serde::Serialize::serialize(&mz_core::jsfmt::num(*v), s)
}

/// [`ser_f64`] untuk `Float?`.
pub fn ser_opt_f64<S: Serializer>(v: &Option<f64>, s: S) -> Result<S::Ok, S::Error> {
    match v {
        Some(x) => ser_f64(x, s),
        None => s.serialize_none(),
    }
}

/// `String(n)` untuk angka di teks alasan (`terpotong 0.5 dari 1 hari`).
pub fn num_str(n: f64) -> String {
    mz_core::jsfmt::num_string(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rounds_like_math_round() {
        assert_eq!(js_round(2.5), 3.0);
        assert_eq!(js_round(-2.5), -2.0);
        assert_eq!(js_round(-0.5), 0.0);
        assert_eq!(js_round(0.499_999_999_999_999_94), 0.0);
        assert_eq!(round2(2.675), 2.68);
        assert_eq!(round2(0.1 + 0.2), 0.3);
        assert_eq!(round2(11.5 - 12.0), -0.5);
    }

    #[test]
    fn serializes_without_trailing_zero() {
        #[derive(serde::Serialize)]
        struct S {
            #[serde(serialize_with = "ser_f64")]
            a: f64,
            #[serde(serialize_with = "ser_opt_f64")]
            b: Option<f64>,
            #[serde(serialize_with = "ser_opt_f64")]
            c: Option<f64>,
        }
        let v = serde_json::to_string(&S {
            a: 12.0,
            b: Some(0.5),
            c: None,
        })
        .unwrap_or_default();
        assert_eq!(v, r#"{"a":12,"b":0.5,"c":null}"#);
        assert_eq!(num_str(0.5), "0.5");
        assert_eq!(num_str(1.0), "1");
    }
}
