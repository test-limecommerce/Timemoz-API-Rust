//! Bagian murni `store-visit-plans.service.ts` / `mobile-store-visit-plans.service.ts`:
//! validasi tanggal & rentang, plafon tulis, status turunan (§2.1), klasifikasi kepatuhan (§4.3),
//! urutan daftar mobile (§4.4). Seluruh tanggal lewat [`crate::plan_dates`] (UTC polos).

use std::collections::{HashMap, HashSet};

use axum::http::StatusCode;
use chrono::NaiveDate;
use mz_core::jsfmt::num;
use mz_core::{ApiError, ApiResult};
use serde_json::{Value, json};

use crate::enums::StoreVisitPlanStatus;
use crate::plan_dates::{
    expand_plan_date_range, from_plan_date, is_plan_date, plan_date_diff_days, to_plan_date,
};
use crate::plans::dto::{CreatePlans, PlanStoreItem};

/// §4.1, §6 — rentang maksimum endpoint baca.
pub const MAX_RANGE_DAYS: i64 = 92;
/// §4.6 — `PLAN_DATE_TOO_FAR`.
pub const MAX_FUTURE_DAYS: i64 = 365;
/// §6 — `PLAN_BATCH_TOO_LARGE`.
pub const MAX_BATCH_ROWS: usize = 2000;
/// §6 — `PLAN_DAILY_LIMIT`.
pub const MAX_PLANS_PER_DAY: usize = 50;

fn bad(code: &str, message: impl Into<String>) -> ApiError {
    ApiError::coded(StatusCode::BAD_REQUEST, code, message)
}

fn bad_details(code: &str, message: impl Into<String>, details: Value) -> ApiError {
    ApiError::body(
        StatusCode::BAD_REQUEST,
        json!({ "code": code, "message": message.into(), "details": details }),
    )
}

/// `assertPlanDate(value, field)`: 400 `INVALID_DATE_RANGE` untuk tanggal di luar kalender.
pub fn assert_plan_date(value: Option<&str>, field: &str) -> ApiResult<String> {
    match value {
        Some(v) if is_plan_date(v) => Ok(v.to_owned()),
        _ => Err(bad(
            "INVALID_DATE_RANGE",
            format!("{field} is not a valid calendar date"),
        )),
    }
}

/// Rentang tervalidasi (`resolveRange`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanRangeOk {
    pub start_date: String,
    pub end_date: String,
    pub gte: NaiveDate,
    pub lte: NaiveDate,
}

/// §4.1 `resolveRange`: wajib, urut, ≤ 92 hari.
pub fn resolve_range(start: Option<&str>, end: Option<&str>) -> ApiResult<PlanRangeOk> {
    let start_date = assert_plan_date(start, "startDate")?;
    let end_date = assert_plan_date(end, "endDate")?;
    let span = plan_date_diff_days(&start_date, &end_date).unwrap_or(0);
    if span < 0 {
        return Err(bad(
            "INVALID_DATE_RANGE",
            "startDate must be on or before endDate",
        ));
    }
    if span + 1 > MAX_RANGE_DAYS {
        return Err(bad(
            "INVALID_DATE_RANGE",
            format!("Date range is limited to {MAX_RANGE_DAYS} days"),
        ));
    }
    let (Some(gte), Some(lte)) = (to_plan_date(&start_date), to_plan_date(&end_date)) else {
        return Err(bad(
            "INVALID_DATE_RANGE",
            "startDate is not a valid calendar date",
        ));
    };
    Ok(PlanRangeOk {
        start_date,
        end_date,
        gte,
        lte,
    })
}

/// §0.9, §4.6 `assertWritableDates`: hari ini atau sesudahnya, ≤ 365 hari ke depan.
pub fn assert_writable_dates(dates: &[String], today: &str) -> ApiResult<()> {
    let past: Vec<&String> = dates.iter().filter(|d| d.as_str() < today).collect();
    if !past.is_empty() {
        return Err(bad_details(
            "PLAN_DATE_IN_PAST",
            "Visit plans cannot be scheduled before today",
            json!({ "dates": past.iter().take(10).collect::<Vec<_>>(), "today": today }),
        ));
    }
    let too_far: Vec<&String> = dates
        .iter()
        .filter(|d| plan_date_diff_days(today, d).is_some_and(|n| n > MAX_FUTURE_DAYS))
        .collect();
    if !too_far.is_empty() {
        return Err(bad_details(
            "PLAN_DATE_TOO_FAR",
            format!("Visit plans cannot be scheduled more than {MAX_FUTURE_DAYS} days ahead"),
            json!({ "dates": too_far.iter().take(10).collect::<Vec<_>>(), "today": today }),
        ));
    }
    Ok(())
}

/// §4.1 `resolveRequestedDates`: `dates` atau `range`, tidak dua-duanya, tidak kosong.
pub fn resolve_requested_dates(dto: &CreatePlans) -> ApiResult<Vec<String>> {
    let has_dates = dto.dates.as_ref().is_some_and(|d| !d.is_empty());
    let has_range = dto.range.is_some();
    if has_dates == has_range {
        return Err(bad(
            "INVALID_DATE_RANGE",
            "Send either dates or range, not both and not neither",
        ));
    }
    if let Some(dates) = dto.dates.as_ref().filter(|d| !d.is_empty()) {
        let mut d: Vec<String> = dates.clone();
        d.sort();
        d.dedup();
        for x in &d {
            assert_plan_date(Some(x), "dates")?;
        }
        return Ok(d);
    }
    let range = dto.range.clone().unwrap_or_default();
    let r = resolve_range(range.start_date.as_deref(), range.end_date.as_deref())?;
    let dates = expand_plan_date_range(&r.start_date, &r.end_date, range.weekdays.as_deref());
    if dates.is_empty() {
        return Err(bad(
            "INVALID_DATE_RANGE",
            "range.weekdays matched no date inside the range",
        ));
    }
    Ok(dates)
}

/// §6 `PLAN_BATCH_TOO_LARGE` — sebelum query apa pun.
pub fn assert_batch_size(dates: usize, stores: usize) -> ApiResult<()> {
    if dates * stores > MAX_BATCH_ROWS {
        return Err(bad_details(
            "PLAN_BATCH_TOO_LARGE",
            format!("One request cannot create more than {MAX_BATCH_ROWS} visit plans"),
            json!({ "dates": dates, "stores": stores, "rows": dates * stores }),
        ));
    }
    Ok(())
}

/// 409 `PLAN_DUPLICATE` (maks 20 rincian).
pub fn duplicate_plans(duplicates: &[(String, String)]) -> ApiError {
    let d: Vec<Value> = duplicates
        .iter()
        .take(20)
        .map(|(date, store)| json!({ "date": date, "storeId": store }))
        .collect();
    ApiError::body(
        StatusCode::CONFLICT,
        json!({
            "code": "PLAN_DUPLICATE",
            "message": "Some of these stores are already scheduled for this salesperson on these dates",
            "details": { "duplicates": d },
        }),
    )
}

/// 400 `PLAN_DAILY_LIMIT` (`details.dates` maks 10).
pub fn daily_limit(overflow: &[(String, usize)]) -> ApiError {
    let d: Vec<Value> = overflow
        .iter()
        .take(10)
        .map(|(date, total)| json!({ "date": date, "total": total }))
        .collect();
    bad_details(
        "PLAN_DAILY_LIMIT",
        format!("A salesperson cannot have more than {MAX_PLANS_PER_DAY} visit plans on one date"),
        json!({ "dates": d, "limit": MAX_PLANS_PER_DAY }),
    )
}

/// §6 `assertBulkFits`: `PLAN_DUPLICATE` (bila `skipExisting=false`) lalu `PLAN_DAILY_LIMIT`
/// atas keadaan SETELAH request ini. `store_ids` = uuid → id toko.
pub fn assert_bulk_fits(
    dates: &[String],
    items: &[PlanStoreItem],
    store_ids: &HashMap<String, i32>,
    existing_by_date: &HashMap<String, HashSet<i32>>,
    skip_existing: bool,
) -> ApiResult<()> {
    let empty = HashSet::new();
    let mut duplicates = Vec::new();
    let mut overflow = Vec::new();
    for date in dates {
        let already = existing_by_date.get(date).unwrap_or(&empty);
        let mut fresh = 0;
        for item in items {
            let id = store_ids.get(&item.store_id).copied().unwrap_or_default();
            if already.contains(&id) {
                duplicates.push((date.clone(), item.store_id.clone()));
                continue;
            }
            fresh += 1;
        }
        let total = already.len() + fresh;
        if total > MAX_PLANS_PER_DAY {
            overflow.push((date.clone(), total));
        }
    }
    if !skip_existing && !duplicates.is_empty() {
        return Err(duplicate_plans(&duplicates));
    }
    if !overflow.is_empty() {
        return Err(daily_limit(&overflow));
    }
    Ok(())
}

/// Ringkasan toko di detail galat (`{ id, code, name }`).
pub fn store_ref(uuid: &str, code: &str, name: &str) -> Value {
    json!({ "id": uuid, "code": code, "name": name })
}

/// 409 `PLAN_ALREADY_FULFILLED`.
pub fn already_fulfilled(stores: Vec<Value>) -> ApiError {
    ApiError::body(
        StatusCode::CONFLICT,
        json!({
            "code": "PLAN_ALREADY_FULFILLED",
            "message": "Some of these visit plans have already been fulfilled by a visit and cannot be removed",
            "details": { "stores": stores },
        }),
    )
}

/// 409 `PLAN_VISIT_TYPE_LOCKED`.
pub fn visit_type_locked(stores: Vec<Value>) -> ApiError {
    ApiError::body(
        StatusCode::CONFLICT,
        json!({
            "code": "PLAN_VISIT_TYPE_LOCKED",
            "message": "Visit type cannot be changed after the plan has been fulfilled",
            "details": { "stores": stores },
        }),
    )
}

/// 404 `PLAN_NOT_FOUND` (update/delete).
pub fn plan_not_found() -> ApiError {
    ApiError::coded(
        StatusCode::NOT_FOUND,
        "PLAN_NOT_FOUND",
        "Visit plan not found",
    )
}

/// 404 `STORE_VISIT_PLAN_NOT_FOUND` (`GET :uuid`).
pub fn store_visit_plan_not_found() -> ApiError {
    ApiError::coded(
        StatusCode::NOT_FOUND,
        "STORE_VISIT_PLAN_NOT_FOUND",
        "Store visit plan not found",
    )
}

/// Kunjungan "hidup" mengklaim jadwal (`visit && state !== cancelled`).
pub fn is_fulfilled(visit_state: Option<&str>) -> bool {
    visit_state.is_some_and(|s| s != "cancelled")
}

/// §2.1 `toPlanStatus`.
pub fn plan_status(
    visit_state: Option<&str>,
    visit_date: NaiveDate,
    today: &str,
) -> StoreVisitPlanStatus {
    if is_fulfilled(visit_state) {
        return StoreVisitPlanStatus::fulfilled;
    }
    if from_plan_date(visit_date).as_str() < today {
        StoreVisitPlanStatus::missed
    } else {
        StoreVisitPlanStatus::pending
    }
}

// ───────────────────────── kepatuhan (§4.3) ─────────────────────────

/// Lima keadaan baris jadwal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComplianceClass {
    Excluded,
    Fulfilled,
    Missed,
    PendingActive,
    PendingInactive,
}

/// `classifyComplianceRow`.
pub fn classify(
    store_deleted: bool,
    store_active: bool,
    visit_state: Option<&str>,
    visit_date: NaiveDate,
    today: &str,
    user_active: bool,
) -> ComplianceClass {
    if store_deleted || !store_active {
        return ComplianceClass::Excluded;
    }
    if is_fulfilled(visit_state) {
        return ComplianceClass::Fulfilled;
    }
    if from_plan_date(visit_date).as_str() < today {
        return ComplianceClass::Missed;
    }
    if user_active {
        ComplianceClass::PendingActive
    } else {
        ComplianceClass::PendingInactive
    }
}

/// Angka mentah satu sel laporan.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Bucket {
    pub planned: i64,
    pub fulfilled: i64,
    pub missed: i64,
    pub excluded: i64,
    pub unplanned: i64,
}

impl Bucket {
    /// `applyComplianceClass`.
    pub fn apply(&mut self, cls: ComplianceClass) {
        match cls {
            ComplianceClass::Excluded => self.excluded += 1,
            ComplianceClass::Fulfilled => {
                self.planned += 1;
                self.fulfilled += 1;
            }
            ComplianceClass::Missed => {
                self.planned += 1;
                self.missed += 1;
            }
            ComplianceClass::PendingActive => self.planned += 1,
            ComplianceClass::PendingInactive => {}
        }
    }

    pub fn add(&mut self, o: &Bucket) {
        self.planned += o.planned;
        self.fulfilled += o.fulfilled;
        self.missed += o.missed;
        self.excluded += o.excluded;
        self.unplanned += o.unplanned;
    }

    /// `toComplianceCounts` (urutan kunci Node; `complianceRate` 4 desimal, `null` bila penyebut 0).
    pub fn counts(&self) -> serde_json::Map<String, Value> {
        let denominator = self.fulfilled + self.missed;
        #[allow(clippy::cast_precision_loss)]
        let rate = if denominator == 0 {
            Value::Null
        } else {
            num((self.fulfilled as f64 / denominator as f64 * 10000.0).round() / 10000.0)
        };
        let mut m = serde_json::Map::new();
        m.insert("planned".into(), self.planned.into());
        m.insert("fulfilled".into(), self.fulfilled.into());
        m.insert("missed".into(), self.missed.into());
        m.insert(
            "pending".into(),
            (self.planned - self.fulfilled - self.missed).into(),
        );
        m.insert("excluded".into(), self.excluded.into());
        m.insert("unplanned".into(), self.unplanned.into());
        m.insert("complianceRate".into(), rate);
        m
    }
}

/// Baris laporan `{ key, ...counts }`.
pub fn compliance_row(key: Value, b: &Bucket) -> Value {
    let mut m = serde_json::Map::new();
    m.insert("key".into(), key);
    m.extend(b.counts());
    Value::Object(m)
}

// ───────────────────────── mobile (§4.4) ─────────────────────────

/// Kunci urut daftar harian mobile: jarak (bila ada koordinat), `sortOrder` (NULL terakhir), nama.
pub fn mobile_compare(
    has_coords: bool,
    a: (Option<i64>, Option<i32>, &str),
    b: (Option<i64>, Option<i32>, &str),
) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    if has_coords {
        let d = a.0.unwrap_or(0).cmp(&b.0.unwrap_or(0));
        if d != Ordering::Equal {
            return d;
        }
    }
    let sa = a.1.map_or(i64::MAX, i64::from);
    let sb = b.1.map_or(i64::MAX, i64::from);
    if sa != sb {
        return sa.cmp(&sb);
    }
    mz_core::jsfmt::collate_id(a.2, b.2)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plans::dto::PlanRange;

    fn d(s: &str) -> NaiveDate {
        to_plan_date(s).unwrap()
    }

    fn body(e: ApiError) -> Value {
        match e {
            ApiError::Body { body, .. } => body,
            other => panic!("{other:?}"),
        }
    }

    /// "menolak rentang terbalik, lebih dari 92 hari, dan tanggal yang tidak ada".
    #[test]
    fn range_rules() {
        let e = body(resolve_range(Some("2026-08-31"), Some("2026-08-01")).unwrap_err());
        assert_eq!(e["message"], "startDate must be on or before endDate");
        let e = body(resolve_range(Some("2026-01-01"), Some("2026-04-03")).unwrap_err());
        assert_eq!(e["message"], "Date range is limited to 92 days");
        assert!(resolve_range(Some("2026-01-01"), Some("2026-04-02")).is_ok());
        let e = body(resolve_range(Some("2026-02-31"), Some("2026-03-01")).unwrap_err());
        assert_eq!(
            e,
            json!({"code":"INVALID_DATE_RANGE","message":"startDate is not a valid calendar date"})
        );
    }

    /// "menolak tanggal lampau dan tanggal lebih dari 365 hari ke depan".
    #[test]
    fn writable_dates() {
        let e = body(assert_writable_dates(&["2026-08-19".into()], "2026-08-20").unwrap_err());
        assert_eq!(e["code"], "PLAN_DATE_IN_PAST");
        assert_eq!(
            e["details"],
            json!({"dates":["2026-08-19"],"today":"2026-08-20"})
        );
        let e = body(assert_writable_dates(&["2027-08-21".into()], "2026-08-20").unwrap_err());
        assert_eq!(e["code"], "PLAN_DATE_TOO_FAR");
        assert!(
            assert_writable_dates(&["2027-08-20".into(), "2026-08-20".into()], "2026-08-20")
                .is_ok()
        );
    }

    fn create(dates: Option<Vec<&str>>, range: Option<PlanRange>) -> CreatePlans {
        CreatePlans {
            user_id: String::new(),
            dates: dates.map(|v| v.into_iter().map(str::to_owned).collect()),
            range,
            stores: vec![],
            visit_type_id: None,
            skip_existing: true,
        }
    }

    /// "menolak request tanpa dates maupun range, dan yang mengirim dua-duanya" + ekspansi weekdays.
    #[test]
    fn requested_dates() {
        let both = create(Some(vec!["2026-08-20"]), Some(PlanRange::default()));
        assert_eq!(
            body(resolve_requested_dates(&both).unwrap_err())["message"],
            "Send either dates or range, not both and not neither"
        );
        assert!(resolve_requested_dates(&create(None, None)).is_err());
        let r = create(Some(vec!["2026-08-21", "2026-08-20", "2026-08-21"]), None);
        assert_eq!(
            resolve_requested_dates(&r).unwrap(),
            vec!["2026-08-20", "2026-08-21"]
        );
        let range = PlanRange {
            start_date: Some("2026-08-17".into()),
            end_date: Some("2026-08-23".into()),
            weekdays: Some(vec![1, 3]),
        };
        assert_eq!(
            resolve_requested_dates(&create(None, Some(range))).unwrap(),
            vec!["2026-08-17", "2026-08-19"]
        );
        let none = PlanRange {
            start_date: Some("2026-08-20".into()),
            end_date: Some("2026-08-21".into()),
            weekdays: Some(vec![0]),
        };
        assert_eq!(
            body(resolve_requested_dates(&create(None, Some(none))).unwrap_err())["message"],
            "range.weekdays matched no date inside the range"
        );
    }

    /// "PLAN_BATCH_TOO_LARGE dihitung sebelum query apa pun".
    #[test]
    fn batch_size() {
        assert!(assert_batch_size(50, 40).is_ok());
        let e = body(assert_batch_size(51, 40).unwrap_err());
        assert_eq!(e["details"], json!({"dates":51,"stores":40,"rows":2040}));
    }

    fn items(n: usize) -> (Vec<PlanStoreItem>, HashMap<String, i32>) {
        let items: Vec<PlanStoreItem> = (0..n)
            .map(|i| PlanStoreItem {
                store_id: format!("s{i}"),
                ..Default::default()
            })
            .collect();
        let ids = (0..n)
            .map(|i| (format!("s{i}"), i32::try_from(i).unwrap() + 1))
            .collect();
        (items, ids)
    }

    /// "PLAN_DAILY_LIMIT dihitung dari keadaan SETELAH request ini" + "baris yang sudah ada tidak
    /// dihitung dua kali" + "skipExisting=false menolak 409 PLAN_DUPLICATE".
    #[test]
    fn bulk_fits() {
        let dates = vec!["2026-08-20".to_owned()];
        let (it, ids) = items(10);
        let existing: HashMap<String, HashSet<i32>> =
            [("2026-08-20".to_owned(), (100..141).collect())]
                .into_iter()
                .collect();
        let e = body(assert_bulk_fits(&dates, &it, &ids, &existing, true).unwrap_err());
        assert_eq!(e["code"], "PLAN_DAILY_LIMIT");
        assert_eq!(
            e["details"]["dates"],
            json!([{"date":"2026-08-20","total":51}])
        );
        let existing: HashMap<String, HashSet<i32>> =
            [("2026-08-20".to_owned(), (1..=10).chain(100..140).collect())]
                .into_iter()
                .collect();
        assert!(assert_bulk_fits(&dates, &it, &ids, &existing, true).is_ok());
        let e = body(assert_bulk_fits(&dates, &it, &ids, &existing, false).unwrap_err());
        assert_eq!(e["code"], "PLAN_DUPLICATE");
        assert_eq!(e["details"]["duplicates"].as_array().unwrap().len(), 10);
    }

    /// "menurunkan status dari kunjungan dan tanggal" + "kunjungan yang dibatalkan tidak dihitung".
    #[test]
    fn status_derivation() {
        let today = "2026-08-20";
        assert_eq!(
            plan_status(Some("completed"), d("2026-08-01"), today),
            StoreVisitPlanStatus::fulfilled
        );
        assert_eq!(
            plan_status(Some("active"), d("2026-08-25"), today),
            StoreVisitPlanStatus::fulfilled
        );
        assert_eq!(
            plan_status(Some("cancelled"), d("2026-08-19"), today),
            StoreVisitPlanStatus::missed
        );
        assert_eq!(
            plan_status(None, d("2026-08-20"), today),
            StoreVisitPlanStatus::pending
        );
    }

    /// §4.3 klasifikasi + rate.
    #[test]
    fn compliance() {
        let today = "2026-08-20";
        assert_eq!(
            classify(true, true, Some("completed"), d("2026-08-01"), today, true),
            ComplianceClass::Excluded
        );
        assert_eq!(
            classify(false, false, None, d("2026-08-01"), today, true),
            ComplianceClass::Excluded
        );
        assert_eq!(
            classify(false, true, Some("completed"), d("2026-08-25"), today, true),
            ComplianceClass::Fulfilled
        );
        assert_eq!(
            classify(false, true, None, d("2026-08-19"), today, false),
            ComplianceClass::Missed
        );
        assert_eq!(
            classify(false, true, None, d("2026-08-20"), today, false),
            ComplianceClass::PendingInactive
        );
        let mut b = Bucket::default();
        b.apply(ComplianceClass::PendingActive);
        assert_eq!(b.counts()["complianceRate"], Value::Null);
        assert_eq!(b.counts()["pending"], json!(1));
        b.apply(ComplianceClass::Fulfilled);
        b.apply(ComplianceClass::Fulfilled);
        b.apply(ComplianceClass::Missed);
        b.apply(ComplianceClass::Excluded);
        b.unplanned = 2;
        let row = compliance_row(json!({"id": null, "name": "Tanpa region"}), &b);
        assert_eq!(
            row,
            json!({"key":{"id":null,"name":"Tanpa region"},"planned":4,"fulfilled":2,"missed":1,"pending":1,
                   "excluded":1,"unplanned":2,"complianceRate":0.6667})
        );
        b.missed = 0;
        assert_eq!(b.counts()["complianceRate"], json!(1));
    }

    /// Urutan mobile: jarak → sortOrder (NULL terakhir) → nama.
    #[test]
    fn mobile_ordering() {
        use std::cmp::Ordering;
        assert_eq!(
            mobile_compare(false, (None, Some(1), "B"), (None, None, "A")),
            Ordering::Less
        );
        assert_eq!(
            mobile_compare(false, (None, None, "B"), (None, None, "A")),
            Ordering::Greater
        );
        assert_eq!(
            mobile_compare(true, (Some(10), Some(5), "A"), (Some(5), None, "Z")),
            Ordering::Greater
        );
        assert_eq!(
            mobile_compare(true, (Some(5), Some(2), "Z"), (Some(5), Some(3), "A")),
            Ordering::Less
        );
    }
}
