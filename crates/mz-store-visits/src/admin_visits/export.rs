//! Port `src/store-visits/store-visits-export.generator.ts` (§7.6): kolom export kunjungan +
//! kebijakan kolom TOKO (§6.3: `hidden` membuang kolom, mode lain mengosongkan sel), CSV
//! di-stream per 500 baris (kursor `id`), XLSX utuh di memori (sheet `StoreVisits`).

use std::collections::HashMap;

use axum::body::{Body, Bytes};
use mz_core::AppState;
use mz_table_fields::policy::CensoredColumns;
use mz_table_fields::registry::FieldVisibilityMode;
use sqlx::{Postgres, QueryBuilder};

use super::service::{
    VISIT_COLUMNS, VISIT_FROM, VISIT_OPTIONAL_JOINS, VisitFilter, VisitRow, photo_flags,
};
use crate::export::{Cell, build_xlsx, csv_header_line, csv_line, format_date_tz};

/// `BATCH_SIZE`.
pub const BATCH_SIZE: i64 = 500;

/// Konteks per baris (`RowContext`).
pub struct RowContext<'a> {
    pub time_zone: &'a str,
    pub photos_count: usize,
    pub photo_kinds: String,
}

/// Satu kolom export: judul, kunci registry `stores` (atau `None`), sel.
pub struct ExportColumn {
    pub header: &'static str,
    pub column_key: Option<&'static str>,
    pub cell: fn(&VisitRow, &RowContext<'_>) -> Cell,
}

fn s(v: &str) -> Cell {
    Cell::Text(v.to_owned())
}
fn so(v: Option<&str>) -> Cell {
    Cell::s(v)
}
fn n(v: f64) -> Cell {
    Cell::Num(v)
}
fn ni(v: Option<i32>) -> Cell {
    Cell::n(v.map(f64::from))
}
fn b(v: Option<bool>) -> Cell {
    match v {
        None => Cell::blank(),
        Some(true) => s("Yes"),
        Some(false) => s("No"),
    }
}

/// `formatDuration(seconds)`.
pub fn format_duration(seconds: Option<i32>) -> String {
    let Some(sec) = seconds else {
        return String::new();
    };
    // `Math.floor` + `%` JS (nilai durasi tidak pernah negatif).
    let h = sec.div_euclid(3600);
    let m = sec.rem_euclid(3600).div_euclid(60);
    let r = sec % 60;
    if h > 0 {
        format!("{h}h {m}m {r}s")
    } else if m > 0 {
        format!("{m}m {r}s")
    } else {
        format!("{r}s")
    }
}

macro_rules! col {
    ($h:literal, $k:expr, $f:expr) => {
        ExportColumn {
            header: $h,
            column_key: $k,
            cell: $f,
        }
    };
}

/// `EXPORT_COLUMNS` (urutan berkas).
pub const EXPORT_COLUMNS: &[ExportColumn] = &[
    col!("id", None, |v, _| s(&v.uuid)),
    col!("visitNumber", None, |v, _| s(&v.visit_number)),
    col!("salesId", None, |v, _| s(&v.sales_uuid)),
    col!("salesName", None, |v, _| s(&v.sales_name)),
    col!("salesUsername", None, |v, _| s(&v.sales_username)),
    col!("storeId", None, |v, _| s(&v.store_uuid)),
    col!("storeCode", Some("code"), |v, _| s(&v.store_code)),
    col!("storeName", Some("name"), |v, _| s(&v.store_name)),
    col!("storeOwnerName", Some("ownerName"), |v, _| s(
        &v.store_owner_name
    )),
    // Snapshot region saat check-in — bukan kolom `group` toko, tidak pernah disensor.
    col!("region", None, |v, _| so(v.group_name.as_deref())),
    col!("state", None, |v, _| s(&v.state)),
    col!("status", None, |v, _| so(v.status.as_deref())),
    col!("checkInAt", None, |v, c| s(&format_date_tz(
        Some(v.check_in_at),
        c.time_zone
    ))),
    col!("checkInLat", None, |v, _| n(v.check_in_lat)),
    col!("checkInLng", None, |v, _| n(v.check_in_lng)),
    col!("checkInAccuracyMeters", None, |v, _| n(v.check_in_accuracy)),
    col!("checkInDistanceMeters", None, |v, _| ni(Some(
        v.check_in_distance
    ))),
    col!("checkInMethod", None, |v, _| s(&v.check_in_method)),
    col!("checkInGpsValidation", None, |v, _| s(
        &v.check_in_gps_validation
    )),
    col!("checkInIsMock", None, |v, _| b(Some(v.check_in_is_mock))),
    col!("checkInIsOverride", None, |v, _| b(Some(
        v.check_in_is_override
    ))),
    col!("checkInOverrideReason", None, |v, _| so(v
        .check_in_override_reason
        .as_deref())),
    col!("checkOutAt", None, |v, c| s(&format_date_tz(
        v.check_out_at,
        c.time_zone
    ))),
    col!("checkOutDistanceMeters", None, |v, _| ni(
        v.check_out_distance
    )),
    col!("checkOutGpsValidation", None, |v, _| so(v
        .check_out_gps_validation
        .as_deref())),
    col!("checkOutIsMock", None, |v, _| b(v.check_out_is_mock)),
    col!("durationSeconds", None, |v, _| s(&format_duration(
        v.duration_seconds
    ))),
    col!("note", None, |v, _| so(v.note.as_deref())),
    col!("tags", None, |v, _| s(&v.tags.join("|"))),
    col!("photosCount", None, |_, c| n(f64::from(
        u32::try_from(c.photos_count).unwrap_or(u32::MAX)
    ))),
    col!("photoKinds", None, |_, c| s(&c.photo_kinds)),
    col!("hasGpsIssue", None, |v, _| b(Some(v.has_gps_issue))),
    col!("createdAt", None, |v, c| s(&format_date_tz(
        Some(v.created_at),
        c.time_zone
    ))),
    col!("planned", None, |v, _| b(Some(v.plan_id.is_some()))),
];

/// `columnsFor(censored)`: `hidden` dibuang.
pub fn columns_for(censored: &CensoredColumns) -> Vec<&'static ExportColumn> {
    EXPORT_COLUMNS
        .iter()
        .filter(|c| {
            c.column_key
                .is_none_or(|k| censored.get(k) != Some(&FieldVisibilityMode::Hidden))
        })
        .collect()
}

/// Judul kolom untuk satu pembaca.
pub fn headers(columns: &[&'static ExportColumn]) -> Vec<&'static str> {
    columns.iter().map(|c| c.header).collect()
}

/// `toRowCells`: kolom tersensor (masked/revealable) → sel kosong.
pub fn row_cells(
    columns: &[&'static ExportColumn],
    visit: &VisitRow,
    photos: &[(String, bool)],
    time_zone: &str,
    censored: &CensoredColumns,
) -> Vec<Cell> {
    let (count, kinds) =
        crate::photos::summarize_photos(photos.iter().map(|(k, c)| (k.as_str(), *c)));
    let ctx = RowContext {
        time_zone,
        photos_count: count,
        photo_kinds: kinds.join("|"),
    };
    columns
        .iter()
        .map(|c| {
            if c.column_key.is_some_and(|k| censored.contains_key(k)) {
                Cell::blank()
            } else {
                (c.cell)(visit, &ctx)
            }
        })
        .collect()
}

/// Satu batch export (`id > after`, urut `id`). `limit = None` = semua (xlsx).
async fn batch(
    db: &sqlx::PgPool,
    filter: &VisitFilter,
    after: i32,
    limit: Option<i64>,
) -> mz_core::ApiResult<Vec<VisitRow>> {
    let mut qb = QueryBuilder::<Postgres>::new(VISIT_COLUMNS);
    qb.push(VISIT_FROM).push(VISIT_OPTIONAL_JOINS);
    filter.push_where(&mut qb, true);
    qb.push(" AND v.id > ")
        .push_bind(after)
        .push(" ORDER BY v.id ASC");
    if let Some(l) = limit {
        qb.push(" LIMIT ").push_bind(l);
    }
    Ok(qb.build_query_as().fetch_all(db).await?)
}

/// `buildStoreVisitsCsvStream`: BOM + judul, lalu satu batch 500 baris per tarikan.
pub fn csv_stream(
    state: AppState,
    filter: VisitFilter,
    time_zone: String,
    censored: CensoredColumns,
) -> Body {
    let columns = columns_for(&censored);
    let header = csv_header_line(&headers(&columns));
    enum Step {
        Header,
        Rows(i32),
        Done,
    }
    let stream = futures::stream::unfold(Step::Header, move |step| {
        let (state, filter, tz, censored, columns, header) = (
            state.clone(),
            filter.clone(),
            time_zone.clone(),
            censored.clone(),
            columns.clone(),
            header.clone(),
        );
        async move {
            match step {
                Step::Done => None,
                Step::Header => Some((
                    Ok::<Bytes, std::io::Error>(Bytes::from(header)),
                    Step::Rows(0),
                )),
                Step::Rows(after) => {
                    let rows = match batch(&state.db, &filter, after, Some(BATCH_SIZE)).await {
                        Ok(r) => r,
                        Err(e) => {
                            tracing::error!("store-visits csv export batch failed: {e:?}");
                            return Some((Err(std::io::Error::other("export failed")), Step::Done));
                        }
                    };
                    let last = rows.last().map(|r| r.id)?;
                    let ids: Vec<i32> = rows.iter().map(|r| r.id).collect();
                    let flags = match photo_flags(&state.db, &ids).await {
                        Ok(f) => f,
                        Err(e) => {
                            tracing::error!("store-visits csv export photos failed: {e:?}");
                            return Some((Err(std::io::Error::other("export failed")), Step::Done));
                        }
                    };
                    let empty = Vec::new();
                    let mut chunk = String::new();
                    for r in &rows {
                        let photos = flags.get(&r.id).unwrap_or(&empty);
                        chunk.push_str(&csv_line(&row_cells(&columns, r, photos, &tz, &censored)));
                    }
                    let next = if i64::try_from(rows.len()).unwrap_or(i64::MAX) < BATCH_SIZE {
                        Step::Done
                    } else {
                        Step::Rows(last)
                    };
                    Some((Ok(Bytes::from(chunk)), next))
                }
            }
        }
    });
    // `.fuse()`: CompressionLayer bisa mem-poll body lagi sesudah stream selesai; `unfold` panic bila itu terjadi.
    Body::from_stream(futures::StreamExt::fuse(stream))
}

/// `buildStoreVisitsXlsx`: seluruh workbook di memori (pemanggil sudah memplafon 5.000 baris).
pub async fn build_visits_xlsx(
    state: &AppState,
    filter: &VisitFilter,
    time_zone: &str,
    censored: &CensoredColumns,
) -> mz_core::ApiResult<Vec<u8>> {
    let columns = columns_for(censored);
    let rows = batch(&state.db, filter, 0, None).await?;
    let ids: Vec<i32> = rows.iter().map(|r| r.id).collect();
    let flags: HashMap<i32, Vec<(String, bool)>> = photo_flags(&state.db, &ids).await?;
    let empty = Vec::new();
    let cells: Vec<Vec<Cell>> = rows
        .iter()
        .map(|r| {
            row_cells(
                &columns,
                r,
                flags.get(&r.id).unwrap_or(&empty),
                time_zone,
                censored,
            )
        })
        .collect();
    build_xlsx("StoreVisits", &headers(&columns), &cells)
        .map_err(|e| mz_core::ApiError::internal(anyhow::anyhow!("{e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mz_core::time::JsDate;

    fn visit() -> VisitRow {
        let mut v = super::super::service::tests_support::visit();
        v.state = "completed".into();
        v.status = Some("success".into());
        v.note = Some("Restock rak depan".into());
        v.tags = vec!["restock".into()];
        v.duration_seconds = Some(1440);
        v.check_in_at = JsDate::from_millis(1_785_553_200_000).unwrap();
        v.check_out_at = JsDate::from_millis(1_785_554_640_000);
        v.check_out_distance = Some(28);
        v.check_out_gps_validation = Some("valid".into());
        v.check_out_is_mock = Some(false);
        v.store_owner_name = "Pak Slamet".into();
        v.group_name = Some("Jogja Barat".into());
        v
    }

    fn censored(pairs: &[(&str, FieldVisibilityMode)]) -> CensoredColumns {
        pairs.iter().map(|(k, m)| ((*k).to_owned(), *m)).collect()
    }

    fn line(c: &CensoredColumns) -> (Vec<&'static str>, Vec<Cell>) {
        let cols = columns_for(c);
        let photos = vec![("store".to_owned(), true), ("selfie".to_owned(), false)];
        (
            headers(&cols),
            row_cells(&cols, &visit(), &photos, "Asia/Jakarta", c),
        )
    }

    #[test]
    fn documented_column_order() {
        let (h, cells) = line(&CensoredColumns::new());
        assert_eq!(
            h.join(","),
            "id,visitNumber,salesId,salesName,salesUsername,storeId,storeCode,storeName,storeOwnerName,region,state,status,checkInAt,checkInLat,checkInLng,checkInAccuracyMeters,checkInDistanceMeters,checkInMethod,checkInGpsValidation,checkInIsMock,checkInIsOverride,checkInOverrideReason,checkOutAt,checkOutDistanceMeters,checkOutGpsValidation,checkOutIsMock,durationSeconds,note,tags,photosCount,photoKinds,hasGpsIssue,createdAt,planned"
        );
        let at = |name: &str| cells[h.iter().position(|x| *x == name).unwrap()].clone();
        assert_eq!(at("storeOwnerName"), Cell::Text("Pak Slamet".into()));
        assert_eq!(at("region"), Cell::Text("Jogja Barat".into()));
        assert_eq!(at("planned"), Cell::Text("Yes".into()));
        assert_eq!(at("photosCount"), Cell::Num(1.0));
        assert_eq!(at("photoKinds"), Cell::Text("store".into()));
        assert_eq!(at("durationSeconds"), Cell::Text("24m 0s".into()));
        assert_eq!(
            at("checkInAt"),
            Cell::Text("2026-08-01 10:00:00 WIB".into())
        );
        assert_eq!(at("checkOutIsMock"), Cell::Text("No".into()));
    }

    #[test]
    fn hidden_drops_column_masked_blanks_cell() {
        let (h, cells) = line(&censored(&[("ownerName", FieldVisibilityMode::Hidden)]));
        assert!(!h.contains(&"storeOwnerName"));
        assert!(!cells.iter().any(|c| *c == Cell::Text("Pak Slamet".into())));
        for mode in [FieldVisibilityMode::Masked, FieldVisibilityMode::Revealable] {
            let (h, cells) = line(&censored(&[("ownerName", mode)]));
            let i = h.iter().position(|x| *x == "storeOwnerName").unwrap();
            assert_eq!(cells[i], Cell::blank());
        }
    }

    #[test]
    fn group_censor_does_not_touch_region_and_store_id_stays() {
        let (h, cells) = line(&censored(&[("group", FieldVisibilityMode::Hidden)]));
        let i = h.iter().position(|x| *x == "region").unwrap();
        assert_eq!(cells[i], Cell::Text("Jogja Barat".into()));
        assert!(h.contains(&"storeId"));
    }

    #[test]
    fn unplanned_and_no_region() {
        let mut v = visit();
        v.plan_id = None;
        v.group_name = None;
        let cols = columns_for(&CensoredColumns::new());
        let cells = row_cells(&cols, &v, &[], "UTC", &CensoredColumns::new());
        assert_eq!(cells.last(), Some(&Cell::Text("No".into())));
        assert_eq!(cells[9], Cell::blank());
    }

    #[test]
    fn durations() {
        assert_eq!(format_duration(None), "");
        assert_eq!(format_duration(Some(59)), "59s");
        assert_eq!(format_duration(Some(3725)), "1h 2m 5s");
    }
}
