//! Pintu belakang search & sort (`search-sort-policy.ts`, §6.4): kolom bermode selain `visible`
//! dikeluarkan dari klausa search dan ditolak sebagai `sort`. Murni; siap dipakai route
//! sales-tracker (belum ada pemakai di crate ini).

use serde_json::json;

use crate::error::ReqError;
use crate::policy::CensoredColumns;
use crate::registry::column_of;

/// Kode error §6.4 untuk `sort` ke kolom yang tersensor.
pub const FIELD_NOT_SORTABLE: &str = "FIELD_NOT_SORTABLE";

/// Cabang `search` yang boleh ikut. Hasil BOLEH kosong; pemanggil tetap memasang `OR []`
/// (tidak cocok baris mana pun), bukan membuang klausanya.
pub fn allowed_search_branches<T>(
    branches: impl IntoIterator<Item = (&'static str, T)>,
    censored: &CensoredColumns,
) -> Vec<T> {
    branches
        .into_iter()
        .filter(|(key, _)| !censored.contains_key(*key))
        .map(|(_, clause)| clause)
        .collect()
}

/// `400 FIELD_NOT_SORTABLE` bila kolom registry yang diminta sebagai `sort` tersensor.
/// `column_key = None` = diurutkan bukan menurut kolom terdaftar (tidak ada yang bocor).
pub fn assert_sortable_column(
    table_key: &str,
    column_key: Option<&str>,
    censored: &CensoredColumns,
) -> Result<(), ReqError> {
    let Some(column_key) = column_key else {
        return Ok(());
    };
    let Some(mode) = censored.get(column_key) else {
        return Ok(());
    };
    let label = column_of(table_key, column_key).map_or(column_key, |c| c.label);
    // Urutan kunci = literal Node: code, message, details{tableKey, field, mode, label}.
    Err(ReqError::body(
        http::StatusCode::BAD_REQUEST,
        json!({
            "code": FIELD_NOT_SORTABLE,
            "message": format!("Column \"{column_key}\" is not sortable for your role"),
            "details": {
                "tableKey": table_key,
                "field": column_key,
                "mode": mode.as_str(),
                "label": label,
            },
        }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::FieldVisibilityMode;

    #[test]
    fn censored_branches_are_dropped() {
        let mut c = CensoredColumns::new();
        c.insert("address".into(), FieldVisibilityMode::Masked);
        let kept = allowed_search_branches([("name", 1), ("address", 2), ("ownerName", 3)], &c);
        assert_eq!(kept, vec![1, 3]);
        assert!(allowed_search_branches([("address", 2)], &c).is_empty());
    }

    #[test]
    fn sort_rejects_censored_column() {
        let mut c = CensoredColumns::new();
        c.insert("lastVisitAt".into(), FieldVisibilityMode::Hidden);
        assert!(assert_sortable_column("stores", Some("createdAt"), &c).is_ok());
        assert!(assert_sortable_column("stores", None, &c).is_ok());
        let err = assert_sortable_column("stores", Some("lastVisitAt"), &c).unwrap_err();
        let (status, body) = crate::error::parts(&err);
        assert_eq!(status, http::StatusCode::BAD_REQUEST);
        assert!(
            body.starts_with(r#"{"code":"FIELD_NOT_SORTABLE","message":"Column \"lastVisitAt\""#)
        );
        assert!(body.contains(r#""mode":"hidden","label":"Kunjungan Terakhir""#));
    }
}
