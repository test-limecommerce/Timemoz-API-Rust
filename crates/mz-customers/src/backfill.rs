//! Port `src/customers/customer-backfill.plan.ts` — bagian murni skrip backfill customer dari data
//! pemilik toko (`ownerName`, `phone`): pengelompokan toko menjadi customer tanpa panggilan DB.

use std::collections::HashMap;

use crate::phone::normalize_store_phone;

/// `BackfillStoreRow`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackfillStoreRow {
    pub id: i32,
    pub owner_name: String,
    pub phone: Option<String>,
    pub created_by_id: i32,
}

/// `BackfillCustomerRow`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackfillCustomerRow {
    pub id: i32,
    pub code: String,
    pub name: String,
    pub phone: Option<String>,
}

/// `BackfillGroup`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackfillGroup {
    pub key: String,
    /// Ejaan dari toko ber-id terkecil di kelompok, dipotong ke lebar kolom.
    pub name: String,
    pub phone: Option<String>,
    pub created_by_id: i32,
    /// Customer lama dengan kunci yang sama (dipakai ulang, bukan dikembar).
    pub existing: Option<BackfillCustomerRow>,
    pub store_ids: Vec<i32>,
}

/// `BackfillPlan`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BackfillPlan {
    pub groups: Vec<BackfillGroup>,
    /// `ownerName` kosong/spasi saja.
    pub skipped_blank_name: Vec<BackfillStoreRow>,
    /// Dua customer lama berbagi satu kunci — dilaporkan, tidak dirapikan.
    pub ambiguous: Vec<BackfillCustomerRow>,
}

/// Batas `customer.name` di schema (unit UTF-16, seperti `String.slice`).
const NAME_MAX_LENGTH: usize = 160;

/// `\s` JS untuk `trim()`/`replace(/\s+/g, ' ')` — sama dengan `jsfmt::trim`.
fn is_js_space(c: char) -> bool {
    matches!(
        c,
        '\t' | '\n' | '\u{000B}' | '\u{000C}' | '\r' | ' ' | '\u{00A0}' | '\u{1680}' | '\u{2000}'
            ..='\u{200A}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{202F}'
                | '\u{205F}'
                | '\u{3000}'
                | '\u{FEFF}'
    )
}

/// `.trim().replace(/\s+/g, ' ')`.
fn squash(value: &str) -> String {
    let mut out = String::new();
    let mut pending_space = false;
    for c in value.chars() {
        if is_js_space(c) {
            pending_space = !out.is_empty();
        } else {
            if pending_space {
                out.push(' ');
                pending_space = false;
            }
            out.push(c);
        }
    }
    out
}

/// `isBlankOwnerName`.
pub fn is_blank_owner_name(value: Option<&str>) -> bool {
    value.is_none_or(|v| v.chars().all(is_js_space))
}

/// `String.prototype.slice(0, n)` dalam unit UTF-16 (memotong pasangan surrogat ikut JS: separuh
/// pasangan tidak mungkin di `String` Rust, jadi pasangan yang terpotong dibuang seluruhnya).
fn slice_utf16(s: &str, max: usize) -> String {
    let mut units = 0;
    let mut out = String::new();
    for c in s.chars() {
        units += c.len_utf16();
        if units > max {
            break;
        }
        out.push(c);
    }
    out
}

/// `canonicalCustomerName`: spasi dirapatkan, dipotong 160, huruf besar/kecil apa adanya.
pub fn canonical_customer_name(owner_name: &str) -> String {
    slice_utf16(&squash(owner_name), NAME_MAX_LENGTH)
}

/// `dedupeKey`: `(ownerName case-insensitive & spasi rapat, phone ternormalisasi)` dengan pemisah
/// `\u{1F}`. Nomor kosong = kelompoknya sendiri.
pub fn dedupe_key(owner_name: Option<&str>, phone: Option<&str>) -> String {
    let name = squash(owner_name.unwrap_or_default()).to_lowercase();
    format!(
        "{name}\u{1F}{}",
        normalize_store_phone(phone).unwrap_or_default()
    )
}

/// `planCustomerGroups`. `stores` HARUS terurut id menaik.
pub fn plan_customer_groups(
    stores: &[BackfillStoreRow],
    existing_customers: &[BackfillCustomerRow],
) -> BackfillPlan {
    let mut existing_by_key: HashMap<String, BackfillCustomerRow> = HashMap::new();
    let mut ambiguous = Vec::new();

    // Id menaik: baris tertua menang bila dua customer lama berbagi kunci.
    let mut sorted: Vec<&BackfillCustomerRow> = existing_customers.iter().collect();
    sorted.sort_by_key(|c| c.id);
    for customer in sorted {
        let key = dedupe_key(Some(&customer.name), customer.phone.as_deref());
        if existing_by_key.contains_key(&key) {
            ambiguous.push(customer.clone());
            continue;
        }
        existing_by_key.insert(key, customer.clone());
    }

    let mut groups: Vec<BackfillGroup> = Vec::new();
    let mut index_by_key: HashMap<String, usize> = HashMap::new();
    let mut skipped_blank_name = Vec::new();

    for store in stores {
        if is_blank_owner_name(Some(&store.owner_name)) {
            skipped_blank_name.push(store.clone());
            continue;
        }
        let key = dedupe_key(Some(&store.owner_name), store.phone.as_deref());
        if let Some(&i) = index_by_key.get(&key) {
            if let Some(group) = groups.get_mut(i) {
                group.store_ids.push(store.id);
            }
            continue;
        }
        index_by_key.insert(key.clone(), groups.len());
        groups.push(BackfillGroup {
            name: canonical_customer_name(&store.owner_name),
            phone: normalize_store_phone(store.phone.as_deref()),
            created_by_id: store.created_by_id,
            existing: existing_by_key.get(&key).cloned(),
            store_ids: vec![store.id],
            key,
        });
    }

    BackfillPlan {
        groups,
        skipped_blank_name,
        ambiguous,
    }
}

/// `nextCustomerSequence`: nomor urut berikutnya untuk `<PREFIX>-0001` (`^\d{1,9}$` pada sufiks).
pub fn next_customer_sequence(existing_codes: &[String], prefix: &str) -> i64 {
    let marker = format!("{prefix}-");
    let mut max = 0_i64;
    for code in existing_codes {
        let Some(suffix) = code.strip_prefix(&marker) else {
            continue;
        };
        if !(1..=9).contains(&suffix.len()) || !suffix.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        if let Ok(n) = suffix.parse::<i64>() {
            max = max.max(n);
        }
    }
    max + 1
}

/// `formatCustomerCode`: `<PREFIX>-` + urutan berpad nol 4 digit.
pub fn format_customer_code(prefix: &str, sequence: i64) -> String {
    format!("{prefix}-{sequence:04}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(id: i32, owner: &str, phone: Option<&str>, created_by: i32) -> BackfillStoreRow {
        BackfillStoreRow {
            id,
            owner_name: owner.to_owned(),
            phone: phone.map(str::to_owned),
            created_by_id: created_by,
        }
    }
    fn customer(id: i32, code: &str, name: &str, phone: Option<&str>) -> BackfillCustomerRow {
        BackfillCustomerRow {
            id,
            code: code.to_owned(),
            name: name.to_owned(),
            phone: phone.map(str::to_owned),
        }
    }

    // ── dedupeKey ──
    #[test]
    fn key_merges_case_differences() {
        assert_eq!(
            dedupe_key(Some("PT Sumber Makmur"), Some("0812")),
            dedupe_key(Some("pt sumber makmur"), Some("0812"))
        );
    }

    #[test]
    fn key_merges_extra_whitespace() {
        assert_eq!(
            dedupe_key(Some("  PT  Sumber   Makmur "), Some("0812")),
            dedupe_key(Some("PT Sumber Makmur"), Some("0812"))
        );
    }

    #[test]
    fn key_merges_phone_formats_via_normalize_store_phone() {
        assert_eq!(
            dedupe_key(Some("Pak Slamet"), Some("081234567890")),
            dedupe_key(Some("Pak Slamet"), Some("+6281234567890"))
        );
        assert_eq!(
            dedupe_key(Some("Pak Slamet"), Some("0812-3456-7890")),
            dedupe_key(Some("Pak Slamet"), Some("+6281234567890"))
        );
    }

    #[test]
    fn key_separates_same_name_different_phone() {
        assert_ne!(
            dedupe_key(Some("Pak Slamet"), Some("0811")),
            dedupe_key(Some("Pak Slamet"), Some("0822"))
        );
    }

    #[test]
    fn key_separates_phone_from_no_phone() {
        assert_ne!(
            dedupe_key(Some("Pak Slamet"), None),
            dedupe_key(Some("Pak Slamet"), Some("0812"))
        );
    }

    #[test]
    fn key_separator_cannot_collide_with_name_spaces() {
        assert_ne!(
            dedupe_key(Some("a b"), None),
            dedupe_key(Some("a"), Some("b"))
        );
    }

    // ── canonicalCustomerName ──
    #[test]
    fn canonical_name_squashes_spaces_keeps_case() {
        assert_eq!(
            canonical_customer_name("  PT  Sumber   Makmur "),
            "PT Sumber Makmur"
        );
    }

    #[test]
    fn canonical_name_truncates_to_160() {
        assert_eq!(
            canonical_customer_name(&"x".repeat(200)).chars().count(),
            160
        );
    }

    // ── planCustomerGroups ──
    #[test]
    fn same_name_and_phone_collapse_into_one_customer() {
        let plan = plan_customer_groups(
            &[
                store(1, "PT Sumber Makmur", Some("081234567890"), 1),
                store(2, "pt sumber makmur", Some("+6281234567890"), 1),
                store(3, "PT Sumber Makmur", Some("0812-3456-7890"), 1),
            ],
            &[],
        );
        assert_eq!(plan.groups.len(), 1);
        assert_eq!(plan.groups[0].store_ids, vec![1, 2, 3]);
    }

    #[test]
    fn same_name_different_phone_stays_apart() {
        let plan = plan_customer_groups(
            &[
                store(1, "Pak Slamet", Some("0811"), 1),
                store(2, "Pak Slamet", Some("0822"), 1),
            ],
            &[],
        );
        assert_eq!(plan.groups.len(), 2);
    }

    #[test]
    fn spelling_and_creator_come_from_smallest_store_id() {
        let plan = plan_customer_groups(
            &[
                store(5, "PT Sumber Makmur", Some("0812"), 77),
                store(9, "pt sumber makmur", Some("0812"), 88),
            ],
            &[],
        );
        assert_eq!(plan.groups[0].name, "PT Sumber Makmur");
        assert_eq!(plan.groups[0].created_by_id, 77);
    }

    #[test]
    fn phone_is_stored_normalized() {
        let plan = plan_customer_groups(&[store(1, "Pak Slamet", Some("081234567890"), 1)], &[]);
        assert_eq!(plan.groups[0].phone.as_deref(), Some("+6281234567890"));
    }

    #[test]
    fn blank_owner_names_are_skipped_and_reported() {
        let plan = plan_customer_groups(
            &[
                store(1, "", None, 1),
                store(2, "   ", None, 1),
                store(3, "Pak Slamet", Some("0812"), 1),
            ],
            &[],
        );
        assert_eq!(plan.groups.len(), 1);
        let ids: Vec<i32> = plan.skipped_blank_name.iter().map(|s| s.id).collect();
        assert_eq!(ids, vec![1, 2]);
    }

    #[test]
    fn reuses_existing_customer_with_same_key() {
        let plan = plan_customer_groups(
            &[store(1, "PT Sumber Makmur", Some("081234567890"), 1)],
            &[customer(
                500,
                "CUST-0001",
                "pt sumber makmur",
                Some("+6281234567890"),
            )],
        );
        assert_eq!(plan.groups.len(), 1);
        assert_eq!(plan.groups[0].existing.as_ref().map(|c| c.id), Some(500));
    }

    #[test]
    fn does_not_reuse_customer_with_different_phone() {
        let plan = plan_customer_groups(
            &[store(1, "PT Sumber Makmur", Some("0811"), 1)],
            &[customer(
                500,
                "CUST-0001",
                "PT Sumber Makmur",
                Some("+62822"),
            )],
        );
        assert!(plan.groups[0].existing.is_none());
    }

    #[test]
    fn duplicate_existing_customers_resolve_to_smallest_id_and_report_rest() {
        let plan = plan_customer_groups(
            &[store(1, "PT Sumber Makmur", Some("0812"), 1)],
            &[
                customer(900, "CUST-0009", "PT Sumber Makmur", Some("+62812")),
                customer(500, "CUST-0001", "pt sumber makmur", Some("+62812")),
            ],
        );
        assert_eq!(plan.groups[0].existing.as_ref().map(|c| c.id), Some(500));
        let amb: Vec<i32> = plan.ambiguous.iter().map(|c| c.id).collect();
        assert_eq!(amb, vec![900]);
    }

    #[test]
    fn keeps_store_order_inside_group() {
        let plan = plan_customer_groups(
            &[
                store(3, "Pak Slamet", Some("0812"), 1),
                store(1, "Bu Sri", Some("0813"), 1),
                store(7, "Pak Slamet", Some("0812"), 1),
            ],
            &[],
        );
        let g = plan.groups.iter().find(|g| g.name == "Pak Slamet");
        assert_eq!(g.map(|g| g.store_ids.clone()), Some(vec![3, 7]));
    }

    // ── penomoran kode ──
    #[test]
    fn sequence_starts_at_one() {
        assert_eq!(next_customer_sequence(&[], "CUST"), 1);
        assert_eq!(
            next_customer_sequence(&["LEGACY-A".into(), "MJ-001".into()], "CUST"),
            1
        );
    }

    #[test]
    fn sequence_continues_from_highest_not_count() {
        let codes = ["CUST-0001", "CUST-0042", "CUST-0007"].map(String::from);
        assert_eq!(next_customer_sequence(&codes, "CUST"), 43);
    }

    #[test]
    fn sequence_ignores_other_prefixes() {
        let codes = ["OTHER-9999", "CUST-0003"].map(String::from);
        assert_eq!(next_customer_sequence(&codes, "CUST"), 4);
    }

    #[test]
    fn sequence_ignores_non_numeric_and_overlong_suffix() {
        let codes = ["CUST-12AB", "CUST-1234567890", "CUST-0002"].map(String::from);
        assert_eq!(next_customer_sequence(&codes, "CUST"), 3);
    }

    #[test]
    fn code_is_zero_padded() {
        assert_eq!(format_customer_code("CUST", 1), "CUST-0001");
        assert_eq!(format_customer_code("CUST", 12345), "CUST-12345");
    }
}
