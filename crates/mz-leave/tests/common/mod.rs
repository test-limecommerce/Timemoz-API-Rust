//! Fixture bersama uji integrasi `mz-leave`: setiap uji membuat company + user sendiri
//! (tidak menyentuh company fixture template), jadi uji boleh jalan paralel.

#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use axum::Router;
use chrono::{NaiveDate, NaiveDateTime};
use mz_core::AppState;
use mz_testkit::TestApp;

pub fn no_routes(_: &AppState) -> Router {
    Router::new()
}

pub fn at(s: &str) -> NaiveDateTime {
    mz_core::time::parse_js_date(s).unwrap()
}

pub fn d(s: &str) -> NaiveDate {
    mz_leave::day_count::parse_date_key(s).unwrap()
}

pub struct Co {
    pub id: i32,
    pub users: Vec<i32>,
}

impl Co {
    pub fn user(&self) -> i32 {
        self.users[0]
    }
}

fn uniq() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// Company baru (zona `tz`) dengan `n` user aktif yang bergabung `joined` (UTC).
pub async fn company(app: &TestApp, tz: &str, n: usize, joined: &str) -> Co {
    let id: i32 = app
        .scalar(&format!(
            r#"INSERT INTO company (name, status, "timeZone") VALUES ('Leave {}', 1, '{tz}') RETURNING id"#,
            uniq()
        ))
        .await;
    let mut users = Vec::new();
    for i in 0..n {
        let uid = add_user(app, id, &format!("User {i}"), joined).await;
        users.push(uid);
    }
    Co { id, users }
}

pub async fn add_user(app: &TestApp, company_id: i32, name: &str, joined: &str) -> i32 {
    let uid: i32 = app
        .scalar(&format!(
            r#"INSERT INTO users (username, password, name, status)
               VALUES ('leave-{}@test.local', 'x', '{name}', 1) RETURNING id"#,
            uniq()
        ))
        .await;
    app.exec(&format!(
        r#"INSERT INTO "userCompany" ("userId", "companyId", status, "idleTimeInSecond", "isDefault", "createdAt")
           VALUES ({uid}, {company_id}, 1, 180, true, '{joined}')"#
    ))
    .await;
    uid
}

/// `INSERT INTO "leavePolicy"` dengan kolom tambahan (`cols`, `vals` SQL mentah).
pub async fn policy(app: &TestApp, company_id: i32, cols: &str, vals: &str) -> i32 {
    let (c, v) = if cols.is_empty() {
        (String::new(), String::new())
    } else {
        (format!(", {cols}"), format!(", {vals}"))
    };
    app.scalar(&format!(
        r#"INSERT INTO "leavePolicy" ("companyId"{c}) VALUES ({company_id}{v}) RETURNING id"#
    ))
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn balance(
    app: &TestApp,
    co: i32,
    user: i32,
    year: i32,
    total: f64,
    used: f64,
    carry: f64,
    carry_used: f64,
) -> i32 {
    app.scalar(&format!(
        r#"INSERT INTO "leaveBalance" ("userId", "companyId", year, "annualLeaveTotal", "annualLeaveUsed",
             "carryOverDays", "carryOverUsed")
           VALUES ({user}, {co}, {year}, {total}, {used}, {carry}, {carry_used}) RETURNING id"#
    ))
    .await
}

pub async fn holiday(app: &TestApp, co: i32, name: &str, date: &str, deducts: bool) -> i32 {
    app.scalar(&format!(
        r#"INSERT INTO "companyHoliday" ("companyId", name, date, type, "deductsLeave")
           VALUES ({co}, '{name}', '{date}', 'JOINT_LEAVE', {deducts}) RETURNING id"#
    ))
    .await
}

pub async fn leave_type(app: &TestApp, co: i32, deduct: bool, source: &str) -> i32 {
    app.scalar(&format!(
        r#"INSERT INTO "companyLeaveType" ("companyId", name, code, "deductBalance", "balanceSource")
           VALUES ({co}, 'Cuti {source}', 'C{}', {deduct}, '{source}') RETURNING id"#,
        &uniq()[..8]
    ))
    .await
}

/// Pengajuan cuti (status/tipe apa adanya).
#[allow(clippy::too_many_arguments)]
pub async fn request(
    app: &TestApp,
    co: i32,
    user: i32,
    type_id: Option<i32>,
    start: &str,
    end: &str,
    total: f64,
    status: &str,
    reviewed_at: Option<&str>,
) -> i32 {
    let t = type_id.map_or("NULL".to_owned(), |t| t.to_string());
    let r = reviewed_at.map_or("NULL".to_owned(), |r| format!("'{r}'"));
    app.scalar(&format!(
        r#"INSERT INTO "leaveRequest" ("userId", "companyId", "leaveTypeId", "startDate", "endDate",
             "totalDays", reason, status, "reviewedAt")
           VALUES ({user}, {co}, {t}, '{start}', '{end}', {total}, 'alasan', '{status}', {r}) RETURNING id"#
    ))
    .await
}

/// Pola 6 hari (Minggu libur, Sabtu hari pendek).
pub async fn six_day_pattern(app: &TestApp, co: i32, effective_from: &str, created_at: &str) {
    app.exec(&format!(
        r#"INSERT INTO "companyWorkPattern" ("companyId", "workDays", "offDays", "shortDay", "effectiveFrom", "createdAt")
           VALUES ({co}, 6, ARRAY[7], 6, '{effective_from}', '{created_at}')"#
    ))
    .await;
}

pub async fn f(app: &TestApp, sql: &str) -> f64 {
    app.scalar::<f64>(sql).await
}

/// `(field, delta, source, dedupeKey, intendedDelta, reason)` riwayat saldo user, urut id.
pub async fn adjustments(
    app: &TestApp,
    user: i32,
) -> Vec<(String, f64, String, Option<String>, Option<f64>, String)> {
    sqlx::query_as(
        r#"SELECT field, delta, source::text, "dedupeKey", "intendedDelta", reason
           FROM "leaveBalanceAdjustment" WHERE "userId" = $1 ORDER BY id"#,
    )
    .bind(user)
    .fetch_all(app.db())
    .await
    .unwrap()
}
