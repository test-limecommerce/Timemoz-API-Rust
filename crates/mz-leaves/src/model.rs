//! Baris `leaveRequest` + relasi (`user`, `reviewedBy`, `approver`, `leaveTypeConfig`) dan
//! bentuk JSON-nya: kolom skalar urutan schema Prisma, lalu relasi urutan `include`
//! (selalu subset berurutan `user, reviewedBy, approver, leaveTypeConfig`).

use chrono::{NaiveDate, NaiveDateTime};
use mz_core::time::JsDate;
use mz_leave::enums::{LeaveDayType, LeaveType};
use mz_leave::js::{ser_f64, ser_opt_f64};
use serde::Serialize;
use sqlx::{PgExecutor, Postgres, QueryBuilder};

use crate::util::date_js;

/// SELECT dasar (alias `lr`); tambahkan `WHERE ...` sendiri.
pub const REQUEST_SELECT: &str = r#"SELECT lr.id, lr.uuid::text AS uuid, lr."userId" AS user_id,
    lr."companyId" AS company_id, lr."leaveType"::text AS leave_type, lr."leaveTypeId" AS leave_type_id,
    lr."startDate" AS start_date, lr."endDate" AS end_date,
    lr."startDayType"::text AS start_day_type, lr."endDayType"::text AS end_day_type,
    lr."totalDays" AS total_days, lr.reason, lr."documentKey" AS document_key,
    lr.status::text AS status, lr."rejectionReason" AS rejection_reason,
    lr."revisionNote" AS revision_note, lr."approvalNote" AS approval_note,
    lr."balanceBefore" AS balance_before, lr."balanceAfter" AS balance_after,
    lr."reviewedById" AS reviewed_by_id, lr."reviewedAt" AS reviewed_at,
    lr."approverId" AS approver_id, lr."createdAt" AS created_at, lr."updatedAt" AS updated_at,
    u.name AS u_name, u.username AS u_username,
    rb.name AS rb_name,
    ap.name AS ap_name, ap.username AS ap_username,
    lt.id AS lt_id, lt."companyId" AS lt_company_id, lt.name AS lt_name, lt.code AS lt_code,
    lt."deductBalance" AS lt_deduct_balance, lt."balanceSource"::text AS lt_balance_source,
    lt."requiresDocument" AS lt_requires_document, lt."annualQuota" AS lt_annual_quota,
    lt."isActive" AS lt_is_active, lt."isDefault" AS lt_is_default,
    lt."createdAt" AS lt_created_at, lt."updatedAt" AS lt_updated_at
  FROM "leaveRequest" lr
  JOIN users u ON u.id = lr."userId"
  LEFT JOIN users rb ON rb.id = lr."reviewedById"
  LEFT JOIN users ap ON ap.id = lr."approverId"
  LEFT JOIN "companyLeaveType" lt ON lt.id = lr."leaveTypeId""#;

/// Satu baris [`REQUEST_SELECT`].
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct RequestRow {
    pub id: i32,
    pub uuid: String,
    pub user_id: i32,
    pub company_id: i32,
    pub leave_type: Option<String>,
    pub leave_type_id: Option<i32>,
    pub start_date: NaiveDate,
    pub end_date: NaiveDate,
    pub start_day_type: String,
    pub end_day_type: String,
    pub total_days: f64,
    pub reason: String,
    pub document_key: Option<String>,
    pub status: String,
    pub rejection_reason: Option<String>,
    pub revision_note: Option<String>,
    pub approval_note: Option<String>,
    pub balance_before: Option<f64>,
    pub balance_after: Option<f64>,
    pub reviewed_by_id: Option<i32>,
    pub reviewed_at: Option<NaiveDateTime>,
    pub approver_id: Option<i32>,
    pub created_at: NaiveDateTime,
    pub updated_at: Option<NaiveDateTime>,
    pub u_name: String,
    pub u_username: String,
    pub rb_name: Option<String>,
    pub ap_name: Option<String>,
    pub ap_username: Option<String>,
    pub lt_id: Option<i32>,
    pub lt_company_id: Option<i32>,
    pub lt_name: Option<String>,
    pub lt_code: Option<String>,
    pub lt_deduct_balance: Option<bool>,
    pub lt_balance_source: Option<String>,
    pub lt_requires_document: Option<bool>,
    pub lt_annual_quota: Option<i32>,
    pub lt_is_active: Option<bool>,
    pub lt_is_default: Option<bool>,
    pub lt_created_at: Option<NaiveDateTime>,
    pub lt_updated_at: Option<NaiveDateTime>,
}

/// `companyLeaveType` (semua kolom skalar).
#[derive(Debug, Clone, PartialEq, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct LeaveTypeConfig {
    pub id: i32,
    pub company_id: i32,
    pub name: String,
    pub code: String,
    pub deduct_balance: bool,
    pub balance_source: String,
    pub requires_document: bool,
    pub annual_quota: Option<i32>,
    pub is_active: bool,
    pub is_default: bool,
    pub created_at: JsDate,
    pub updated_at: Option<JsDate>,
}

/// Kolom `companyLeaveType` (alias `lt`) untuk [`LeaveTypeConfig`].
pub const LEAVE_TYPE_COLUMNS: &str = r#"lt.id, lt."companyId" AS company_id, lt.name, lt.code,
    lt."deductBalance" AS deduct_balance, lt."balanceSource"::text AS balance_source,
    lt."requiresDocument" AS requires_document, lt."annualQuota" AS annual_quota,
    lt."isActive" AS is_active, lt."isDefault" AS is_default, lt."createdAt" AS created_at,
    lt."updatedAt" AS updated_at"#;

/// `{ id, name, username }`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UserRef {
    pub id: i32,
    pub name: String,
    pub username: String,
}

/// `{ id, name }`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UserMini {
    pub id: i32,
    pub name: String,
}

/// Relasi yang di-`include`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Include {
    pub user: bool,
    pub reviewed_by: bool,
    pub approver: bool,
    pub leave_type: bool,
}

impl Include {
    /// `user, reviewedBy, approver, leaveTypeConfig` (dasbor: list/get).
    pub const ALL: Self = Self {
        user: true,
        reviewed_by: true,
        approver: true,
        leave_type: true,
    };
    /// `user, leaveTypeConfig` (approve/reject/needs-revision, kalender).
    pub const USER_TYPE: Self = Self {
        user: true,
        reviewed_by: false,
        approver: false,
        leave_type: true,
    };
    /// `user, approver, leaveTypeConfig` (pengajuan mobile).
    pub const SUBMIT: Self = Self {
        user: true,
        reviewed_by: false,
        approver: true,
        leave_type: true,
    };
    /// `reviewedBy, approver, leaveTypeConfig` (mobile list/get/revise).
    pub const MOBILE: Self = Self {
        user: false,
        reviewed_by: true,
        approver: true,
        leave_type: true,
    };
    /// Tanpa relasi (mobile cancel).
    pub const NONE: Self = Self {
        user: false,
        reviewed_by: false,
        approver: false,
        leave_type: false,
    };
}

/// JSON `leaveRequest` + relasi.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestView {
    pub id: i32,
    pub uuid: String,
    pub user_id: i32,
    pub company_id: i32,
    pub leave_type: Option<String>,
    pub leave_type_id: Option<i32>,
    pub start_date: JsDate,
    pub end_date: JsDate,
    pub start_day_type: String,
    pub end_day_type: String,
    #[serde(serialize_with = "ser_f64")]
    pub total_days: f64,
    pub reason: String,
    pub document_key: Option<String>,
    pub status: String,
    pub rejection_reason: Option<String>,
    pub revision_note: Option<String>,
    pub approval_note: Option<String>,
    #[serde(serialize_with = "ser_opt_f64")]
    pub balance_before: Option<f64>,
    #[serde(serialize_with = "ser_opt_f64")]
    pub balance_after: Option<f64>,
    pub reviewed_by_id: Option<i32>,
    pub reviewed_at: Option<JsDate>,
    pub approver_id: Option<i32>,
    pub created_at: JsDate,
    pub updated_at: Option<JsDate>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<UserRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reviewed_by: Option<Option<UserMini>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approver: Option<Option<UserRef>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub leave_type_config: Option<Option<LeaveTypeConfig>>,
}

impl RequestRow {
    /// `leaveTypeConfig` (null bila tidak ada).
    pub fn leave_type_config(&self) -> Option<LeaveTypeConfig> {
        Some(LeaveTypeConfig {
            id: self.lt_id?,
            company_id: self.lt_company_id?,
            name: self.lt_name.clone()?,
            code: self.lt_code.clone()?,
            deduct_balance: self.lt_deduct_balance?,
            balance_source: self.lt_balance_source.clone()?,
            requires_document: self.lt_requires_document?,
            annual_quota: self.lt_annual_quota,
            is_active: self.lt_is_active?,
            is_default: self.lt_is_default?,
            created_at: JsDate(self.lt_created_at?),
            updated_at: self.lt_updated_at.map(JsDate),
        })
    }

    pub fn start_day_type(&self) -> LeaveDayType {
        self.start_day_type
            .parse()
            .unwrap_or(LeaveDayType::FULL_DAY)
    }

    pub fn end_day_type(&self) -> LeaveDayType {
        self.end_day_type.parse().unwrap_or(LeaveDayType::FULL_DAY)
    }

    pub fn legacy_type(&self) -> Option<LeaveType> {
        self.leave_type.as_deref().and_then(|t| t.parse().ok())
    }

    /// `leaveTypeConfig?.name ?? leaveType ?? fallback`.
    pub fn type_name_or(&self, fallback: &str) -> String {
        self.lt_name
            .clone()
            .or_else(|| self.leave_type.clone())
            .unwrap_or_else(|| fallback.to_owned())
    }

    pub fn view(&self, inc: Include) -> RequestView {
        RequestView {
            id: self.id,
            uuid: self.uuid.clone(),
            user_id: self.user_id,
            company_id: self.company_id,
            leave_type: self.leave_type.clone(),
            leave_type_id: self.leave_type_id,
            start_date: date_js(self.start_date),
            end_date: date_js(self.end_date),
            start_day_type: self.start_day_type.clone(),
            end_day_type: self.end_day_type.clone(),
            total_days: self.total_days,
            reason: self.reason.clone(),
            document_key: self.document_key.clone(),
            status: self.status.clone(),
            rejection_reason: self.rejection_reason.clone(),
            revision_note: self.revision_note.clone(),
            approval_note: self.approval_note.clone(),
            balance_before: self.balance_before,
            balance_after: self.balance_after,
            reviewed_by_id: self.reviewed_by_id,
            reviewed_at: self.reviewed_at.map(JsDate),
            approver_id: self.approver_id,
            created_at: JsDate(self.created_at),
            updated_at: self.updated_at.map(JsDate),
            user: inc.user.then(|| UserRef {
                id: self.user_id,
                name: self.u_name.clone(),
                username: self.u_username.clone(),
            }),
            reviewed_by: inc.reviewed_by.then(|| {
                self.reviewed_by_id
                    .zip(self.rb_name.clone())
                    .map(|(id, name)| UserMini { id, name })
            }),
            approver: inc.approver.then(|| {
                match (self.approver_id, &self.ap_name, &self.ap_username) {
                    (Some(id), Some(name), Some(username)) => Some(UserRef {
                        id,
                        name: name.clone(),
                        username: username.clone(),
                    }),
                    _ => None,
                }
            }),
            leave_type_config: inc.leave_type.then(|| self.leave_type_config()),
        }
    }
}

/// Satu baris berdasarkan `id`.
pub async fn fetch_by_id(exec: impl PgExecutor<'_>, id: i32) -> sqlx::Result<Option<RequestRow>> {
    sqlx::query_as(&format!("{REQUEST_SELECT} WHERE lr.id = $1"))
        .bind(id)
        .fetch_optional(exec)
        .await
}

/// `QueryBuilder` berawalan [`REQUEST_SELECT`] + ` WHERE `.
pub fn select_where() -> QueryBuilder<'static, Postgres> {
    QueryBuilder::new(format!("{REQUEST_SELECT} WHERE "))
}

/// Visibilitas pengajuan bagi reviewer (`leave-visibility.ts`): hanya approver yang
/// ditugaskan; approver kebijakan juga melihat pengajuan tanpa approver.
#[derive(Debug, Clone, Copy)]
pub struct Visibility {
    pub user_id: i32,
    pub policy_approver: bool,
}

impl Visibility {
    /// `leaveVisibilityWhere(prisma, userId, companyId)`.
    pub async fn load(
        exec: impl PgExecutor<'_>,
        user_id: i32,
        company_id: i32,
    ) -> sqlx::Result<Self> {
        Ok(Self {
            user_id,
            policy_approver: mz_leave::policy::is_policy_approver(exec, user_id, company_id)
                .await?,
        })
    }

    /// `approverSql(alias, ...)`.
    pub fn push(&self, qb: &mut QueryBuilder<'_, Postgres>, alias: &str) {
        mz_leave::policy::push_approver_sql(qb, alias, self.user_id, self.policy_approver);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row() -> RequestRow {
        let at = mz_core::time::parse_js_date("2026-10-01T03:04:05.678Z").unwrap_or_default();
        RequestRow {
            id: 7,
            uuid: "9c9dd5b0-645e-499d-9c3e-d6cfc2f4ac55".into(),
            user_id: 3,
            company_id: 2,
            leave_type: None,
            leave_type_id: Some(4),
            start_date: mz_leave::day_count::parse_date_key("2026-10-05").unwrap_or_default(),
            end_date: mz_leave::day_count::parse_date_key("2026-10-06").unwrap_or_default(),
            start_day_type: "FULL_DAY".into(),
            end_day_type: "HALF_DAY_MORNING".into(),
            total_days: 1.5,
            reason: "x".into(),
            document_key: None,
            status: "PENDING".into(),
            rejection_reason: None,
            revision_note: None,
            approval_note: None,
            balance_before: Some(12.0),
            balance_after: None,
            reviewed_by_id: None,
            reviewed_at: None,
            approver_id: Some(9),
            created_at: at,
            updated_at: Some(at),
            u_name: "Ani".into(),
            u_username: "ani@x.id".into(),
            rb_name: None,
            ap_name: Some("Budi".into()),
            ap_username: Some("budi@x.id".into()),
            lt_id: Some(4),
            lt_company_id: Some(2),
            lt_name: Some("Cuti".into()),
            lt_code: Some("C".into()),
            lt_deduct_balance: Some(true),
            lt_balance_source: Some("ANNUAL".into()),
            lt_requires_document: Some(false),
            lt_annual_quota: None,
            lt_is_active: Some(true),
            lt_is_default: Some(false),
            lt_created_at: Some(at),
            lt_updated_at: None,
        }
    }

    #[test]
    fn view_key_order() {
        let v = serde_json::to_string(&row().view(Include::SUBMIT)).unwrap_or_default();
        assert_eq!(
            v,
            r#"{"id":7,"uuid":"9c9dd5b0-645e-499d-9c3e-d6cfc2f4ac55","userId":3,"companyId":2,"leaveType":null,"leaveTypeId":4,"startDate":"2026-10-05T00:00:00.000Z","endDate":"2026-10-06T00:00:00.000Z","startDayType":"FULL_DAY","endDayType":"HALF_DAY_MORNING","totalDays":1.5,"reason":"x","documentKey":null,"status":"PENDING","rejectionReason":null,"revisionNote":null,"approvalNote":null,"balanceBefore":12,"balanceAfter":null,"reviewedById":null,"reviewedAt":null,"approverId":9,"createdAt":"2026-10-01T03:04:05.678Z","updatedAt":"2026-10-01T03:04:05.678Z","user":{"id":3,"name":"Ani","username":"ani@x.id"},"approver":{"id":9,"name":"Budi","username":"budi@x.id"},"leaveTypeConfig":{"id":4,"companyId":2,"name":"Cuti","code":"C","deductBalance":true,"balanceSource":"ANNUAL","requiresDocument":false,"annualQuota":null,"isActive":true,"isDefault":false,"createdAt":"2026-10-01T03:04:05.678Z","updatedAt":null}}"#
        );
        let m = serde_json::to_value(row().view(Include::MOBILE)).unwrap_or_default();
        assert_eq!(m.get("reviewedBy"), Some(&serde_json::Value::Null));
        assert!(m.get("user").is_none());
        let n = serde_json::to_value(row().view(Include::NONE)).unwrap_or_default();
        assert!(n.get("leaveTypeConfig").is_none());
    }
}
