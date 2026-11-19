//! Port `src/users/random-password.ts` — password acak (CSPRNG) dengan minimal satu
//! huruf besar, kecil, angka, dan simbol; karakter mirip (I/l/1, O/0) tidak dipakai.

use rand::Rng;

pub const UPPER: &str = "ABCDEFGHJKLMNPQRSTUVWXYZ";
pub const LOWER: &str = "abcdefghijkmnopqrstuvwxyz";
pub const DIGIT: &str = "23456789";
pub const SYMBOL: &str = "!@#$%^&*?-_";

fn pick(chars: &[u8], rng: &mut impl Rng) -> char {
    chars
        .get(rng.random_range(0..chars.len()))
        .copied()
        .map_or('a', char::from)
}

/// `generateRandomPassword(length = 14)`; panjang minimum 8.
pub fn generate_random_password(length: usize) -> String {
    let mut rng = rand::rng();
    let all = [UPPER, LOWER, DIGIT, SYMBOL].concat();
    let mut chars: Vec<char> = vec![
        pick(UPPER.as_bytes(), &mut rng),
        pick(LOWER.as_bytes(), &mut rng),
        pick(DIGIT.as_bytes(), &mut rng),
        pick(SYMBOL.as_bytes(), &mut rng),
    ];
    for _ in 0..length.max(8).saturating_sub(chars.len()) {
        chars.push(pick(all.as_bytes(), &mut rng));
    }
    // Shuffle seperti Node: ambil acak lalu buang, agar posisi karakter wajib tak tertebak.
    let mut shuffled = String::with_capacity(chars.len());
    while !chars.is_empty() {
        shuffled.push(chars.swap_remove(rng.random_range(0..chars.len())));
    }
    shuffled
}

/// Default Node: 14 karakter.
pub fn generate_default_random_password() -> String {
    generate_random_password(14)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn length_and_minimum() {
        assert_eq!(generate_default_random_password().len(), 14);
        assert_eq!(generate_random_password(3).len(), 8);
        assert_eq!(generate_random_password(30).len(), 30);
    }

    #[test]
    fn charset_and_required_classes() {
        for _ in 0..200 {
            let p = generate_random_password(14);
            assert!(p.chars().any(|c| UPPER.contains(c)), "{p}");
            assert!(p.chars().any(|c| LOWER.contains(c)), "{p}");
            assert!(p.chars().any(|c| DIGIT.contains(c)), "{p}");
            assert!(p.chars().any(|c| SYMBOL.contains(c)), "{p}");
            assert!(p.chars().all(|c| !"IlO01".contains(c)), "{p}");
        }
    }
}
