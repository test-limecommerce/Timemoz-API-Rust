//! Aturan murni `overtime-approvals.service.ts` (tanpa DB): galat bernama, validasi lapis
//! kedua, penolakan perintah per karyawan, resolusi target, dan perakitan hasil.

use std::collections::HashMap;

use axum::http::StatusCode;
use chrono::NaiveDate;
use mz_core::{ApiError, ApiResult};
use serde_json::{Value, json};

use super::dto::{
    MAX_DURATION_MINUTES, MAX_ORDER_USERS, MIN_DURATION_MINUTES, MIN_ORDER_USERS, OrderInput,
    REASON_MAX_LENGTH,
};
use super::entity::{OrderRefusal, OrderRefusalCode, QuotaEntry};
use crate::calendar::{DayInfo, LeaveCoverage};
use crate::hr::common::dto::{as_js_integer, is_uuid_any, js_trim};
use crate::hr::common::eligibility::{
    OvertimeEmployeeRow, is_eligible_overtime_employee, overtime_employee_rank,
};
use crate::hr::common::enums::OvertimeRequestStatus;
use crate::hr::common::errors::overtime_error;
pub use crate::hr::common::errors::{not_found_error, state_conflict_error};
use crate::hr::common::mapper::{OvertimeRequestEntity, normal_sec_live};
use crate::math::{QuotaEvaluation, exceeds_day};
use crate::zone::{date_key, parse_date_key};

// ── galat bernama ────────────────────────────────────────────────────────────

/// `403 OVERTIME_NOT_APPROVER`.
pub fn not_approver() -> ApiError {
    overtime_error(
        StatusCode::FORBIDDEN,
        "OVERTIME_NOT_APPROVER",
        "Anda bukan approver lembur di perusahaan ini.",
        None,
    )
}

/// `403 OVERTIME_DISABLED`.
pub fn disabled() -> ApiError {
    overtime_error(
        StatusCode::FORBIDDEN,
        "OVERTIME_DISABLED",
        "Fitur lembur sedang dinonaktifkan.",
        None,
    )
}

/// Skenario 9: `403 OVERTIME_SELF_DECISION`.
pub fn self_decision() -> ApiError {
    overtime_error(
        StatusCode::FORBIDDEN,
        "OVERTIME_SELF_DECISION",
        "Pengajuan lembur milik sendiri harus diputuskan approver lain.",
        None,
    )
}

/// T10: `409 OVERTIME_DEADLINE_PASSED`.
pub fn deadline_passed() -> ApiError {
    overtime_error(
        StatusCode::CONFLICT,
        "OVERTIME_DEADLINE_PASSED",
        "Batas waktu memutuskan pengajuan lembur ini sudah lewat.",
        None,
    )
}

/// Skenario 2: `422 OVERTIME_ORDER_PAST_DATE` + `details.localToday`.
pub fn order_past_date(local_today: NaiveDate) -> ApiError {
    overtime_error(
        StatusCode::UNPROCESSABLE_ENTITY,
        "OVERTIME_ORDER_PAST_DATE",
        "Perintah lembur hanya untuk hari ini atau tanggal ke depan.",
        Some(json!({ "localToday": date_key(local_today) })),
    )
}

/// `400 Tanggal "from" tidak boleh setelah "to".`
pub fn range_reversed() -> ApiError {
    ApiError::bad_request("Tanggal \"from\" tidak boleh setelah \"to\".")
}

// ── validasi lapis kedua ─────────────────────────────────────────────────────

/// UUID unik tanpa beda huruf (`key` = huruf kecil), urutan input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderUuid {
    pub raw: String,
    pub key: String,
}

/// `ValidatedOrder`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedOrder {
    pub uuids: Vec<OrderUuid>,
    pub date: NaiveDate,
    pub requested_sec: i64,
    pub reason: String,
}

/// `parseOrderUuids(userIds)`: 1–100 UUID (versi apa pun, `UUID_RE`), lalu dilipat tanpa
/// beda huruf. `None` = bukan array string.
pub fn parse_order_uuids(user_ids: Option<&[String]>) -> ApiResult<Vec<OrderUuid>> {
    let Some(ids) = user_ids.filter(|ids| {
        (MIN_ORDER_USERS..=MAX_ORDER_USERS).contains(&ids.len())
            && ids.iter().all(|u| is_uuid_any(u))
    }) else {
        return Err(ApiError::bad_request(format!(
            "userIds harus berisi {MIN_ORDER_USERS}–{MAX_ORDER_USERS} UUID yang valid"
        )));
    };
    let mut out: Vec<OrderUuid> = Vec::new();
    for raw in ids {
        let key = raw.to_lowercase();
        if out.iter().any(|u| u.key == key) {
            continue;
        }
        out.push(OrderUuid {
            raw: raw.clone(),
            key,
        });
    }
    Ok(out)
}

/// `parseOrderDateKey(dateKey)`.
pub fn parse_order_date(date: Option<&str>) -> ApiResult<NaiveDate> {
    date.and_then(parse_date_key)
        .ok_or_else(|| ApiError::bad_request("date harus tanggal YYYY-MM-DD yang valid"))
}

/// `parseOrderRequestedSec(minutes)`: bilangan bulat 1–1440 → detik.
pub fn parse_order_requested_sec(minutes: Option<i64>) -> ApiResult<i64> {
    match minutes {
        Some(m) if (MIN_DURATION_MINUTES..=MAX_DURATION_MINUTES).contains(&m) => Ok(m * 60),
        _ => Err(ApiError::bad_request(format!(
            "durationMinutes harus bilangan bulat {MIN_DURATION_MINUTES}–{MAX_DURATION_MINUTES}"
        ))),
    }
}

fn string_array(v: Option<&Value>) -> Option<Vec<String>> {
    v?.as_array()?
        .iter()
        .map(|x| x.as_str().map(str::to_owned))
        .collect()
}

/// `validateOrder(dto)` (lapis kedua; DTO sudah menolak dengan 400). Panjang `reason`
/// dihitung seperti `String.length` (unit UTF-16).
pub fn validate_order(input: &OrderInput) -> ApiResult<ValidatedOrder> {
    let ids = string_array(input.user_ids.as_ref());
    let uuids = parse_order_uuids(ids.as_deref())?;
    let date = parse_order_date(input.date.as_ref().and_then(Value::as_str))?;
    let requested_sec =
        parse_order_requested_sec(input.duration_minutes.as_ref().and_then(as_js_integer))?;
    let reason = input
        .reason
        .as_ref()
        .and_then(Value::as_str)
        .map(|s| js_trim(s).to_owned())
        .unwrap_or_default();
    let len = reason.encode_utf16().count();
    if len == 0 || len > REASON_MAX_LENGTH {
        return Err(ApiError::bad_request(format!(
            "reason wajib diisi (maks {REASON_MAX_LENGTH} karakter)"
        )));
    }
    Ok(ValidatedOrder {
        uuids,
        date,
        requested_sec,
        reason,
    })
}

// ── penolakan perintah per karyawan ─────────────────────────────────────────

/// `orderEmployeeRefusal(employee, callerRank)`: kelayakan → rank (D3=(b): rank target ≤
/// rank pemberi perintah; rank pemberi tak dikenal = tolak).
pub fn order_employee_refusal(
    employee: Option<&OvertimeEmployeeRow>,
    caller_rank: Option<u8>,
) -> Option<OrderRefusalCode> {
    if !is_eligible_overtime_employee(employee) {
        return Some(OrderRefusalCode::OVERTIME_EMPLOYEE_NOT_ELIGIBLE);
    }
    match (caller_rank, overtime_employee_rank(employee)) {
        (Some(caller), Some(target)) if target <= caller => None,
        _ => Some(OrderRefusalCode::ROLE_TOO_HIGH),
    }
}

/// `orderDayRefusal(day, requestedSec)`: cuti sehari penuh, lalu FX-7.3 (lembur + jam
/// normal setelah cuti > 24 jam).
pub fn order_day_refusal(day: &DayInfo, requested_sec: i64) -> Option<OrderRefusalCode> {
    if day.leave == LeaveCoverage::Full {
        return Some(OrderRefusalCode::OVERTIME_EMPLOYEE_ON_LEAVE);
    }
    if exceeds_day(requested_sec, normal_sec_live(day)) {
        return Some(OrderRefusalCode::OVERTIME_DURATION_EXCEEDS_DAY);
    }
    None
}

/// Satu karyawan yang ter-resolve dari `userIds` (`OrderTarget`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderTarget {
    /// uuid persis seperti di request (untuk `refused[].userId`).
    pub request_uuid: String,
    pub user_id: i32,
}

/// Bagian murni `resolveOrderTargets`: SELF (uuid pemanggil), uuid bukan anggota company
/// (→ NOT_ELIGIBLE), lalu target terurut `users.id` naik. `members` = `(id, uuid)` anggota
/// company (status keanggotaan apa pun) untuk uuid selain pemanggil.
pub fn resolve_targets(
    uuids: &[OrderUuid],
    caller_id: i32,
    caller_uuid: &str,
    members: &[(i32, String)],
) -> (HashMap<String, OrderRefusalCode>, Vec<OrderTarget>) {
    let mut refusals = HashMap::new();
    let caller_key = caller_uuid.to_lowercase();
    let mut targets = Vec::new();
    for u in uuids {
        if u.key == caller_key {
            refusals.insert(u.raw.clone(), OrderRefusalCode::OVERTIME_ORDER_SELF);
            continue;
        }
        match members
            .iter()
            .find(|(_, uuid)| uuid.to_lowercase() == u.key)
        {
            None => {
                refusals.insert(
                    u.raw.clone(),
                    OrderRefusalCode::OVERTIME_EMPLOYEE_NOT_ELIGIBLE,
                );
            }
            Some((id, _)) if *id == caller_id => {
                refusals.insert(u.raw.clone(), OrderRefusalCode::OVERTIME_ORDER_SELF);
            }
            Some((id, _)) => targets.push(OrderTarget {
                request_uuid: u.raw.clone(),
                user_id: *id,
            }),
        }
    }
    // Sort stabil, seperti `Array.prototype.sort`.
    targets.sort_by_key(|t| t.user_id);
    (refusals, targets)
}

/// Rakit `{ created, refused }` mengikuti urutan input `uuids`.
pub fn assemble_order_result(
    uuids: &[OrderUuid],
    created: &HashMap<String, OvertimeRequestEntity>,
    refusals: &HashMap<String, OrderRefusalCode>,
) -> (Vec<OvertimeRequestEntity>, Vec<OrderRefusal>) {
    let mut created_list = Vec::new();
    let mut refused = Vec::new();
    for u in uuids {
        if let Some(entity) = created.get(&u.raw) {
            created_list.push(entity.clone());
            continue;
        }
        if let Some(code) = refusals.get(&u.raw) {
            refused.push(OrderRefusal {
                user_id: u.raw.clone(),
                code: *code,
                message: code.message(),
            });
        }
    }
    (created_list, refused)
}

/// Rakit entri `GET /approvals/quota` mengikuti urutan input. Entri tanpa evaluasi dan
/// tanpa penolakan → galat 500 (Node: `throw new Error`).
pub fn assemble_quota_entries(
    uuids: &[OrderUuid],
    quotas: &HashMap<String, QuotaEvaluation>,
    refusals: &HashMap<String, OrderRefusalCode>,
) -> ApiResult<Vec<QuotaEntry>> {
    uuids
        .iter()
        .map(|u| {
            let quota = quotas.get(&u.raw);
            let refusal = refusals.get(&u.raw).copied();
            if quota.is_none() && refusal.is_none() {
                return Err(ApiError::internal(anyhow::anyhow!(
                    "Evaluasi kuota tak ada untuk {}",
                    u.raw
                )));
            }
            Ok(QuotaEntry {
                user_id: u.raw.clone(),
                quota: if refusal.is_some() {
                    None
                } else {
                    quota.cloned()
                },
                refusal,
            })
        })
        .collect()
}

// ── daftar ───────────────────────────────────────────────────────────────────

/// `ListMode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListMode {
    Pending,
    History,
}

/// Status filter `listWhere`: isi → unik (urutan pertama); kosong → `[PENDING_APPROVAL]`
/// untuk Menunggu, tanpa filter untuk riwayat.
pub fn list_statuses(
    status: Option<&[OvertimeRequestStatus]>,
    mode: ListMode,
) -> Option<Vec<OvertimeRequestStatus>> {
    match status {
        Some(list) if !list.is_empty() => {
            let mut out: Vec<OvertimeRequestStatus> = Vec::new();
            for s in list {
                if !out.contains(s) {
                    out.push(*s);
                }
            }
            Some(out)
        }
        _ => match mode {
            ListMode::Pending => Some(vec![OvertimeRequestStatus::PENDING_APPROVAL]),
            ListMode::History => None,
        },
    }
}

/// `Math.ceil(total / limit)`.
pub fn total_pages(total: i64, limit: i64) -> i64 {
    if limit <= 0 {
        return 0;
    }
    (total + limit - 1).div_euclid(limit)
}

#[cfg(test)]
mod tests {
    //! Kasus murni `overtime-approvals.service.spec.ts`.
    #![allow(clippy::indexing_slicing, clippy::unwrap_used, clippy::expect_used)]
    use super::*;
    use crate::calendar::DayType;
    use crate::hr::common::eligibility::EmployeeRole;
    use mz_core::enums::BaseRole;

    fn uuid_of(n: u32) -> String {
        format!("00000000-0000-4000-8000-{n:012}")
    }

    fn emp(code: &str, is_system: bool, base: Option<BaseRole>) -> OvertimeEmployeeRow {
        OvertimeEmployeeRow {
            id: 1,
            uuid: uuid_of(1),
            name: "Budi".into(),
            username: "budi@x.id".into(),
            role: Some(EmployeeRole {
                code: code.into(),
                is_system,
                base_role: base,
                status: 1,
            }),
        }
    }

    const WORKDAY: DayInfo = DayInfo {
        day_type: DayType::Workday,
        base_normal_sec: 28_800,
        daily_limit_sec: 14_400,
        pattern_work_days: 5,
        leave: LeaveCoverage::None,
    };

    fn input(v: Value) -> OrderInput {
        OrderInput {
            user_ids: v.get("userIds").cloned(),
            date: v.get("date").cloned(),
            duration_minutes: v.get("durationMinutes").cloned(),
            reason: v.get("reason").cloned(),
        }
    }

    #[test]
    fn layer_two_validation() {
        let ok = validate_order(&input(json!({
            "userIds": [uuid_of(1), uuid_of(1).to_uppercase(), uuid_of(2)],
            "date": "2026-10-23", "durationMinutes": 90, "reason": "  Stock  "
        })))
        .unwrap();
        assert_eq!(ok.uuids.len(), 2);
        assert_eq!(ok.requested_sec, 5_400);
        assert_eq!(ok.reason, "Stock");
        let base = json!({"userIds": [uuid_of(1)], "date": "2026-10-23", "durationMinutes": 90, "reason": "x"});
        let many: Vec<String> = (0..101).map(|i| uuid_of(100 + i)).collect();
        for (k, v) in [
            ("userIds", json!([])),
            ("userIds", json!(["bukan-uuid"])),
            ("userIds", json!("x")),
            ("userIds", json!(many)),
            ("date", json!("2026-02-30")),
            ("date", json!("23-10-2026")),
            ("durationMinutes", json!(0)),
            ("durationMinutes", json!(1441)),
            ("durationMinutes", json!(1.5)),
            ("reason", json!("   ")),
            ("reason", json!("x".repeat(501))),
            // 300 emoji: lolos DTO (validator.js), ditolak lapis kedua (`.length` 600).
            ("reason", json!("😀".repeat(300))),
        ] {
            let mut b = base.clone();
            b[k] = v.clone();
            let err = validate_order(&input(b)).err();
            assert!(
                matches!(err, Some(ApiError::Message { status, .. }) if status == StatusCode::BAD_REQUEST),
                "{k}={v}"
            );
        }
        // UUID versi lain diterima lapis kedua (`UUID_RE` tanpa versi).
        assert!(
            parse_order_uuids(Some(&["6ba7b810-9dad-11d1-80b4-00c04fd430c8".to_owned()])).is_ok()
        );
        let hundred: Vec<String> = (0..100).map(|i| uuid_of(100 + i)).collect();
        assert_eq!(parse_order_uuids(Some(&hundred)).unwrap().len(), 100);
    }

    #[test]
    fn employee_refusal_rank_rules() {
        // Pemanggil MANAGER (rank 2).
        let manager = mz_core::enums::rank_of("MANAGER");
        assert_eq!(
            order_employee_refusal(Some(&emp("STAFF", true, None)), manager),
            None
        );
        assert_eq!(
            order_employee_refusal(Some(&emp("MANAGER", true, None)), manager),
            None
        );
        assert_eq!(
            order_employee_refusal(Some(&emp("ADMIN", true, None)), manager),
            Some(OrderRefusalCode::ROLE_TOO_HIGH)
        );
        // ADMIN boleh ADMIN & MANAGER, bukan SUPER_ADMIN.
        let admin = mz_core::enums::rank_of("ADMIN");
        assert_eq!(
            order_employee_refusal(Some(&emp("ADMIN", true, None)), admin),
            None
        );
        assert_eq!(
            order_employee_refusal(Some(&emp("SUPER_ADMIN", true, None)), admin),
            Some(OrderRefusalCode::ROLE_TOO_HIGH)
        );
        // Role custom → base role.
        assert_eq!(
            order_employee_refusal(Some(&emp("KASIR", false, Some(BaseRole::ADMIN))), manager),
            Some(OrderRefusalCode::ROLE_TOO_HIGH)
        );
        assert_eq!(
            order_employee_refusal(Some(&emp("KASIR", false, Some(BaseRole::STAFF))), manager),
            None
        );
        // Rank pemanggil tak dikenal → tolak.
        assert_eq!(
            order_employee_refusal(Some(&emp("STAFF", true, None)), mz_core::enums::rank_of("")),
            Some(OrderRefusalCode::ROLE_TOO_HIGH)
        );
        // GUEST / tanpa baris → tak layak (sebelum rank).
        assert_eq!(
            order_employee_refusal(Some(&emp("GUEST", true, None)), admin),
            Some(OrderRefusalCode::OVERTIME_EMPLOYEE_NOT_ELIGIBLE)
        );
        assert_eq!(
            order_employee_refusal(None, admin),
            Some(OrderRefusalCode::OVERTIME_EMPLOYEE_NOT_ELIGIBLE)
        );
    }

    #[test]
    fn day_refusal_rules() {
        assert_eq!(order_day_refusal(&WORKDAY, 3_600), None);
        let full = DayInfo {
            leave: LeaveCoverage::Full,
            ..WORKDAY
        };
        assert_eq!(
            order_day_refusal(&full, 60),
            Some(OrderRefusalCode::OVERTIME_EMPLOYEE_ON_LEAVE)
        );
        // FX-7.3: 16 jam + 8 jam normal = 24 jam (lolos); +1 menit ditolak.
        assert_eq!(order_day_refusal(&WORKDAY, 57_600), None);
        assert_eq!(
            order_day_refusal(&WORKDAY, 57_660),
            Some(OrderRefusalCode::OVERTIME_DURATION_EXCEEDS_DAY)
        );
        // Hari libur (normal 0): 24 jam lolos.
        let holiday = DayInfo {
            day_type: DayType::Holiday,
            base_normal_sec: 0,
            ..WORKDAY
        };
        assert_eq!(order_day_refusal(&holiday, 86_400), None);
        // Cuti setengah hari: normal 4 jam.
        let half = DayInfo {
            leave: LeaveCoverage::Half,
            ..WORKDAY
        };
        assert_eq!(order_day_refusal(&half, 72_000), None);
    }

    #[test]
    fn resolve_targets_rules() {
        let uuids = parse_order_uuids(Some(&[
            uuid_of(9),
            uuid_of(3).to_uppercase(),
            uuid_of(1),
            uuid_of(5),
            uuid_of(3),
        ]))
        .unwrap();
        assert_eq!(uuids.len(), 4);
        let members = vec![(30, uuid_of(3)), (10, uuid_of(1)), (9, uuid_of(9))];
        let (refusals, targets) = resolve_targets(&uuids, 9, &uuid_of(9).to_uppercase(), &members);
        assert_eq!(
            refusals.get(&uuid_of(9)),
            Some(&OrderRefusalCode::OVERTIME_ORDER_SELF)
        );
        assert_eq!(
            refusals.get(&uuid_of(5)),
            Some(&OrderRefusalCode::OVERTIME_EMPLOYEE_NOT_ELIGIBLE)
        );
        assert_eq!(
            targets,
            vec![
                OrderTarget {
                    request_uuid: uuid_of(1),
                    user_id: 10
                },
                OrderTarget {
                    request_uuid: uuid_of(3).to_uppercase(),
                    user_id: 30
                },
            ]
        );
    }

    #[test]
    fn list_status_defaults() {
        use OvertimeRequestStatus::*;
        assert_eq!(
            list_statuses(None, ListMode::Pending),
            Some(vec![PENDING_APPROVAL])
        );
        assert_eq!(
            list_statuses(Some(&[]), ListMode::Pending),
            Some(vec![PENDING_APPROVAL])
        );
        assert_eq!(list_statuses(None, ListMode::History), None);
        assert_eq!(
            list_statuses(Some(&[APPROVED, REJECTED, APPROVED]), ListMode::History),
            Some(vec![APPROVED, REJECTED])
        );
        assert_eq!(total_pages(0, 20), 0);
        assert_eq!(total_pages(21, 20), 2);
        assert_eq!(total_pages(20, 20), 1);
    }

    #[test]
    fn order_result_follows_input_order() {
        let uuids = parse_order_uuids(Some(&[uuid_of(2), uuid_of(1), uuid_of(3)])).unwrap();
        let mut refusals = HashMap::new();
        refusals.insert(uuid_of(1), OrderRefusalCode::OVERTIME_DATE_TAKEN);
        let (created, refused) = assemble_order_result(&uuids, &HashMap::new(), &refusals);
        assert!(created.is_empty());
        assert_eq!(
            refused,
            vec![OrderRefusal {
                user_id: uuid_of(1),
                code: OrderRefusalCode::OVERTIME_DATE_TAKEN,
                message: "Karyawan sudah punya pengajuan lembur aktif di tanggal ini."
            }]
        );
        assert_eq!(
            serde_json::to_value(&refused[0]).unwrap(),
            json!({"userId": uuid_of(1), "code": "OVERTIME_DATE_TAKEN",
                   "message": "Karyawan sudah punya pengajuan lembur aktif di tanggal ini."})
        );
        // Kuota: entri tanpa evaluasi & tanpa penolakan → 500.
        assert!(assemble_quota_entries(&uuids, &HashMap::new(), &refusals).is_err());
        let entries = assemble_quota_entries(&uuids[1..2], &HashMap::new(), &refusals).unwrap();
        assert_eq!(
            serde_json::to_value(&entries).unwrap(),
            json!([{"userId": uuid_of(1), "quota": null, "refusal": "OVERTIME_DATE_TAKEN"}])
        );
    }

    #[test]
    fn past_date_error_body() {
        use axum::response::IntoResponse;
        let res = order_past_date(parse_date_key("2026-10-23").unwrap()).into_response();
        assert_eq!(res.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }
}
