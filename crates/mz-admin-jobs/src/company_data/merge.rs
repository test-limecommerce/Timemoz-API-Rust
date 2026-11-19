//! Port `src/admin-jobs/company-data/merge.ts`: gabungkan data satu company dari Timemoz LAMA
//! (`LEGACY_DATABASE_URL`) ke database tujuan berdasarkan uuid, dalam SATU transaksi
//! (dry run = ROLLBACK, apply = COMMIT). Seluruh SQL sama dengan Node.
//!
//! Hanya jalur admin job (`--from-db`, tanpa `--tracking-only`/`--user`/`--skip-resync`) yang
//! di-port; sumber folder (`export-company.js`) tetap milik skrip Node.

use std::time::Instant;

use serde_json::{Map, Value, json};
use sqlx::postgres::PgConnectOptions;
use sqlx::{Connection, PgConnection, PgPool, Row as _};

use super::backup::{pad_end, pad_start};
use super::errors::{CdResult, Code, Log, fail};
use super::pg::q;
use super::scope::Row;

pub const MERGE_BATCH: i64 = 5000;
pub const UNLIMITED_RETENTION_DAYS: i32 = 36500;
const UPDATE_DETAIL_LIMIT: i64 = 50;

/// `MergeParams` (jalur admin job).
#[derive(Debug, Clone)]
pub struct MergeParams {
    pub legacy_company_id: i32,
    pub apply: bool,
    pub prefer_legacy: bool,
    pub unlimited_retention: bool,
    pub accept_retention: bool,
    pub since: Option<String>,
    pub until: Option<String>,
    pub expect_target_company_id: Option<i32>,
    pub target_host: Option<String>,
    pub legacy_host: Option<String>,
    /// Kunci advisory yang ditahan sepanjang transaksi merge (lihat restore).
    pub lock_key: Option<String>,
}

/// `isValidMergeDate(value)`: `YYYY-MM-DD` yang ada di kalender.
pub fn is_valid_merge_date(value: &str) -> bool {
    let b = value.as_bytes();
    b.len() == 10
        && b.iter().enumerate().all(|(i, c)| match i {
            4 | 7 => *c == b'-',
            _ => c.is_ascii_digit(),
        })
        && chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d").is_ok()
}

/// `validateMergeParams` (bagian jalur admin job).
pub fn validate(p: &MergeParams) -> CdResult<()> {
    if p.legacy_company_id < 1 {
        return fail(
            Code::InvalidParams,
            "--from-db harus id company (bilangan bulat > 0)",
        );
    }
    if let Some(s) = &p.since
        && !is_valid_merge_date(s)
    {
        return fail(
            Code::InvalidParams,
            "--since harus tanggal YYYY-MM-DD yang valid",
        );
    }
    if let Some(u) = &p.until
        && !is_valid_merge_date(u)
    {
        return fail(
            Code::InvalidParams,
            "--until harus tanggal YYYY-MM-DD yang valid",
        );
    }
    if let (Some(s), Some(u)) = (&p.since, &p.until)
        && s > u
    {
        return fail(Code::InvalidParams, "--since tidak boleh sesudah --until");
    }
    if p.expect_target_company_id.is_some_and(|e| e < 1) {
        return fail(
            Code::InvalidParams,
            "expectTargetCompanyId harus bilangan bulat > 0.",
        );
    }
    Ok(())
}

// ── Koneksi ──────────────────────────────────────────────────────────────────

/// `pgClientConfig(envName)`: semantik SSL libpq (require/prefer tanpa CA = terenkripsi tanpa
/// verifikasi; `verify-*` wajib CA dari `sslrootcert` atau `PGSSLROOTCERT`).
pub fn pg_connect_options(
    env_name: &str,
    raw: Option<&str>,
    pgsslrootcert: Option<&str>,
) -> CdResult<PgConnectOptions> {
    let Some(raw) = raw.filter(|s| !s.trim().is_empty()) else {
        return fail(
            Code::PgConfig,
            format!("{env_name} tidak di-set (pakai: node --env-file=.env ...)"),
        );
    };
    let mut url = url::Url::parse(raw)?;
    let pairs: Vec<(String, String)> = url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    let sslmode = pairs
        .iter()
        .find(|(k, _)| k == "sslmode")
        .map(|(_, v)| v.clone());
    let root_cert = pairs
        .iter()
        .find(|(k, _)| k == "sslrootcert")
        .map(|(_, v)| v.clone())
        .or_else(|| pgsslrootcert.filter(|s| !s.is_empty()).map(str::to_owned));
    let kept: Vec<&(String, String)> = pairs
        .iter()
        .filter(|(k, _)| !matches!(k.as_str(), "schema" | "sslmode" | "sslrootcert"))
        .collect();
    if kept.is_empty() {
        url.set_query(None);
    } else {
        url.query_pairs_mut()
            .clear()
            .extend_pairs(kept.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    }
    let base = mz_core::db::connect_options(url.as_str())?;
    use sqlx::postgres::PgSslMode;
    let Some(mode) = sslmode.filter(|m| m != "disable") else {
        return Ok(base.ssl_mode(PgSslMode::Disable));
    };
    match root_cert {
        None => {
            if mode == "require" || mode == "prefer" {
                return Ok(base.ssl_mode(PgSslMode::Require));
            }
            fail(
                Code::PgConfig,
                format!(
                    "{env_name} memakai sslmode={mode}, yang mewajibkan CA server.\nUnduh ca-certificate.crt dari panel DigitalOcean (Database > Connection Details),\nlalu jalankan dengan PGSSLROOTCERT=/path/ca-certificate.crt."
                ),
            )
        }
        Some(ca) => {
            if std::fs::metadata(&ca).is_err() {
                return fail(
                    Code::PgConfig,
                    format!("CA tidak terbaca ({ca}): berkas tidak ada"),
                );
            }
            Ok(base.ssl_mode(PgSslMode::VerifyFull).ssl_root_cert(ca))
        }
    }
}

/// Host (`host:port`) URL untuk log (`new URL(env).host`).
pub fn url_host(raw: &str) -> Option<String> {
    let u = url::Url::parse(raw).ok()?;
    let host = u.host_str()?;
    Some(match u.port() {
        Some(p) => format!("{host}:{p}"),
        None => host.to_owned(),
    })
}

// ── Staging ──────────────────────────────────────────────────────────────────

const TS: &str = "timestamp(3)";

/// `STAGE_COLS` (urutan tabel = urutan objek Node).
pub fn stage_cols() -> Vec<(&'static str, Vec<(&'static str, &'static str)>)> {
    let base = || {
        vec![
            ("id", "int"),
            ("uuid", "uuid"),
            ("createdAt", TS),
            ("updatedAt", TS),
            ("status", "int"),
        ]
    };
    let with = |extra: &[(&'static str, &'static str)]| {
        let mut v = base();
        v.extend_from_slice(extra);
        v
    };
    vec![
        (
            "company",
            with(&[
                ("name", "text"),
                ("telp", "text"),
                ("timeZone", "text"),
                ("dailyReportSentAt", TS),
                ("isAlllowDeleteScreenshot", "boolean"),
                ("fixedHourNotification", "int"),
                ("isAgencyMode", "boolean"),
            ]),
        ),
        (
            "users",
            with(&[
                ("username", "text"),
                ("password", "text"),
                ("name", "text"),
                ("sentAt", TS),
            ]),
        ),
        ("projectType", with(&[("code", "text")])),
        (
            "project",
            with(&[
                ("code", "text"),
                ("name", "text"),
                ("screenshotPerHour", "int"),
                ("projectTypeId", "int"),
                ("startDate", "date"),
                ("totalHour", "int"),
            ]),
        ),
        (
            "projectCompany",
            with(&[("projectId", "int"), ("companyId", "int")]),
        ),
        (
            "projectUser",
            with(&[("projectId", "int"), ("userId", "int")]),
        ),
        (
            "userCompany",
            with(&[
                ("userId", "int"),
                ("companyId", "int"),
                ("isScreenshotActive", "boolean"),
                ("isAllowManualTime", "boolean"),
                ("isBlur", "boolean"),
                ("isAllowEditTime", "boolean"),
                ("screenshotPerHour", "int"),
                ("idleTimeInSecond", "int"),
                ("isDefault", "boolean"),
            ]),
        ),
        (
            "userAccessLevel",
            with(&[
                ("userId", "int"),
                ("accessLevelId", "int"),
                ("companyId", "int"),
            ]),
        ),
        (
            "tracking",
            with(&[
                ("takenDate", "date"),
                ("isAbsent", "boolean"),
                ("userId", "int"),
                ("totalTakenInSecond", "int"),
            ]),
        ),
        (
            "trackingTimeline",
            with(&[
                ("note", "text"),
                ("isOffline", "boolean"),
                ("startAt", TS),
                ("endAt", TS),
                ("trackingId", "int"),
                ("projectId", "int"),
            ]),
        ),
        (
            "screenshot",
            with(&[
                ("fileFullpath", "text"),
                ("isBlur", "boolean"),
                ("takenAt", TS),
                ("trackingTimelineId", "int"),
                ("projectId", "int"),
            ]),
        ),
    ]
}

fn stg(name: &str) -> String {
    q(&format!("_lg_{name}"))
}

async fn exec(db: &mut PgConnection, sql: &str) -> CdResult<u64> {
    Ok(sqlx::query(sql)
        .persistent(false)
        .execute(&mut *db)
        .await?
        .rows_affected())
}

async fn create_staging(db: &mut PgConnection) -> CdResult<()> {
    for (name, cols) in stage_cols() {
        let defs = cols
            .iter()
            .map(|(c, t)| format!("{} {t}", q(c)))
            .collect::<Vec<_>>()
            .join(", ");
        exec(
            db,
            &format!(
                "CREATE TEMP TABLE {} ({defs}, \"newId\" int, \"matchedBy\" text,\n         \"skip\" text, \"touched\" boolean NOT NULL DEFAULT false) ON COMMIT DROP",
                stg(name)
            ),
        )
        .await?;
        exec(db, &format!("CREATE INDEX ON {} (\"id\")", stg(name))).await?;
        exec(db, &format!("CREATE INDEX ON {} (\"uuid\")", stg(name))).await?;
    }
    Ok(())
}

/// `makeStager(db, name, batch)`.
struct Stager {
    sql: String,
    cols: Vec<&'static str>,
    buf: Vec<Value>,
    count: u64,
}

impl Stager {
    fn new(name: &str, cols: &[(&'static str, &'static str)]) -> Self {
        let col_list = cols
            .iter()
            .map(|(c, _)| q(c))
            .collect::<Vec<_>>()
            .join(", ");
        let rec_def = cols
            .iter()
            .map(|(c, t)| format!("{} {t}", q(c)))
            .collect::<Vec<_>>()
            .join(", ");
        Self {
            sql: format!(
                "INSERT INTO {} ({col_list})\n               SELECT {col_list} FROM jsonb_to_recordset($1::jsonb) AS t({rec_def})",
                stg(name)
            ),
            cols: cols.iter().map(|(c, _)| *c).collect(),
            buf: Vec::new(),
            count: 0,
        }
    }

    async fn flush(&mut self, db: &mut PgConnection) -> CdResult<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let rows = std::mem::take(&mut self.buf);
        let n = rows.len();
        sqlx::query(&self.sql)
            .bind(Value::Array(rows))
            .persistent(false)
            .execute(&mut *db)
            .await?;
        self.count += u64::try_from(n).unwrap_or(0);
        Ok(())
    }

    async fn push(&mut self, db: &mut PgConnection, row: &Row) -> CdResult<()> {
        let picked: Map<String, Value> = self
            .cols
            .iter()
            .map(|c| ((*c).to_owned(), row.get(*c).cloned().unwrap_or(Value::Null)))
            .collect();
        self.buf.push(Value::Object(picked));
        if i64::try_from(self.buf.len()).unwrap_or(i64::MAX) >= MERGE_BATCH {
            self.flush(db).await?;
        }
        Ok(())
    }
}

/// Kondisi rentang takenDate untuk alias tracking `t` (parameter ke-`from` = since,
/// ke-`from+1` = until).
fn in_range(t: &str, from: usize) -> String {
    format!(
        "((${from}::date IS NULL OR {t}.\"takenDate\" >= ${from}::date)\n    AND (${n}::date IS NULL OR {t}.\"takenDate\" <= ${n}::date))",
        n = from + 1
    )
}

/// `dbSource`: baca DB lama dalam SATU snapshot `REPEATABLE READ READ ONLY`.
struct DbSource {
    legacy: PgConnection,
    company_id: i32,
    since: Option<String>,
    until: Option<String>,
}

impl DbSource {
    fn scope(name: &str) -> String {
        let projects = r#"SELECT "projectId" FROM "projectCompany" WHERE "companyId" = $1"#;
        let trackings = format!(
            "SELECT t.\"id\" FROM \"tracking\" t WHERE {}\n                       AND ($4::text IS NULL OR t.\"userId\" IN (SELECT u.\"id\" FROM \"users\" u\n                             WHERE lower(u.\"username\") = lower($4::text) OR u.\"uuid\"::text = $4::text))",
            in_range("t", 2)
        );
        let timelines = format!(
            "SELECT \"id\" FROM \"trackingTimeline\"\n                      WHERE \"projectId\" IN ({projects}) AND \"trackingId\" IN ({trackings})"
        );
        match name {
            "company" => r#""id" = $1"#.to_owned(),
            "userCompany" | "userAccessLevel" | "projectCompany" => {
                r#""companyId" = $1"#.to_owned()
            }
            "users" => r#""id" IN (SELECT "userId" FROM "userCompany" WHERE "companyId" = $1
                     UNION SELECT "userId" FROM "userAccessLevel" WHERE "companyId" = $1)"#
                .to_owned(),
            "project" => format!("\"id\" IN ({projects})"),
            "projectType" => "TRUE".to_owned(),
            "projectUser" => format!("\"projectId\" IN ({projects})"),
            "trackingTimeline" => format!("\"id\" IN ({timelines})"),
            "tracking" => format!(
                "\"id\" IN (SELECT \"trackingId\" FROM \"trackingTimeline\"\n                         WHERE \"projectId\" IN ({projects}) AND \"trackingId\" IN ({trackings}))"
            ),
            "screenshot" => {
                format!("\"projectId\" IN ({projects}) AND \"trackingTimelineId\" IN ({timelines})")
            }
            _ => "FALSE".to_owned(),
        }
    }

    /// `eachRow(name)`: keyset per id. Seluruh baris tabel dikembalikan sebagai satu daftar
    /// per halaman ke `on_page`.
    async fn page(&mut self, name: &str, last_id: i64) -> CdResult<Vec<Row>> {
        let sql = format!(
            "SELECT to_jsonb(x) AS \"r\"\n           FROM {} x, (SELECT $1::int, $2::date, $3::date, $4::text) _p\n          WHERE {} AND x.\"id\" > $5::int ORDER BY x.\"id\" LIMIT $6::int",
            q(name),
            Self::scope(name)
        );
        let rows = sqlx::query(&sql)
            .bind(self.company_id)
            .bind(self.since.as_deref())
            .bind(self.until.as_deref())
            .bind(Option::<&str>::None)
            .bind(i32::try_from(last_id).unwrap_or(i32::MAX))
            .bind(i32::try_from(MERGE_BATCH).unwrap_or(i32::MAX))
            .persistent(false)
            .fetch_all(&mut self.legacy)
            .await?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let v: Value = r.try_get("r")?;
            if let Value::Object(m) = v {
                out.push(m);
            }
        }
        Ok(out)
    }
}

fn cols_of(
    cols: &[(&'static str, Vec<(&'static str, &'static str)>)],
    name: &str,
) -> Vec<(&'static str, &'static str)> {
    cols.iter()
        .find(|(n, _)| *n == name)
        .map(|(_, c)| c.clone())
        .unwrap_or_default()
}

fn key(r: &Row, k: &str) -> String {
    r.get(k).map(Value::to_string).unwrap_or_default()
}

fn by_company(r: &Row, legacy_company_id: i64) -> bool {
    r.get("companyId").and_then(Value::as_i64) == Some(legacy_company_id)
}

/// `loadBackup(db, source, ...)`.
async fn load_backup(
    db: &mut PgConnection,
    source: &mut DbSource,
    log: &dyn Log,
) -> CdResult<(i64, Map<String, Value>)> {
    let cols = stage_cols();
    let mut loaded = Map::new();

    // companies()
    let mut companies = Vec::new();
    let mut last = 0i64;
    loop {
        let page = source.page("company", last).await?;
        let n = page.len();
        last = page
            .last()
            .and_then(|r| r.get("id"))
            .and_then(Value::as_i64)
            .unwrap_or(last);
        companies.extend(page);
        if i64::try_from(n).unwrap_or(0) < MERGE_BATCH {
            break;
        }
    }
    if companies.is_empty() {
        return fail(
            Code::LegacySource,
            format!("Company {} tidak ada di DB lama", source.company_id),
        );
    }
    if companies.len() != 1 {
        return fail(
            Code::LegacySource,
            "company.json harus berisi tepat satu company",
        );
    }
    let legacy_company_id = companies
        .first()
        .and_then(|c| c.get("id"))
        .and_then(Value::as_i64)
        .unwrap_or_default();

    let mut user_ids = std::collections::HashSet::<String>::new();
    let mut project_ids = std::collections::HashSet::<String>::new();
    let mut tracking_ids = std::collections::HashSet::<String>::new();
    let mut timeline_ids = std::collections::HashSet::<String>::new();
    let mut seen_tracking = std::collections::HashSet::<String>::new();

    for name in [
        "company",
        "userCompany",
        "userAccessLevel",
        "projectCompany",
        "users",
        "project",
        "projectType",
        "projectUser",
        "trackingTimeline",
        "tracking",
        "screenshot",
    ] {
        let mut stager = Stager::new(name, &cols_of(&cols, name));
        let mut seen = 0u64;
        let mut last = 0i64;
        loop {
            let page = source.page(name, last).await?;
            let n = page.len();
            last = page
                .last()
                .and_then(|r| r.get("id"))
                .and_then(Value::as_i64)
                .unwrap_or(last);
            for row in page {
                seen += 1;
                let keep = match name {
                    "company" | "projectType" => true,
                    "userCompany" | "userAccessLevel" => {
                        by_company(&row, legacy_company_id) && {
                            user_ids.insert(key(&row, "userId"));
                            true
                        }
                    }
                    "projectCompany" => {
                        by_company(&row, legacy_company_id) && {
                            project_ids.insert(key(&row, "projectId"));
                            true
                        }
                    }
                    "users" => user_ids.contains(&key(&row, "id")),
                    "project" => project_ids.contains(&key(&row, "id")),
                    "projectUser" => project_ids.contains(&key(&row, "projectId")),
                    "trackingTimeline" => {
                        if project_ids.contains(&key(&row, "projectId")) {
                            tracking_ids.insert(key(&row, "trackingId"));
                            timeline_ids.insert(key(&row, "id"));
                            true
                        } else {
                            false
                        }
                    }
                    "tracking" => {
                        let id = key(&row, "id");
                        if tracking_ids.contains(&id) && !seen_tracking.contains(&id) {
                            seen_tracking.insert(id);
                            true
                        } else {
                            false
                        }
                    }
                    "screenshot" => {
                        project_ids.contains(&key(&row, "projectId"))
                            && timeline_ids.contains(&key(&row, "trackingTimelineId"))
                    }
                    _ => false,
                };
                if keep {
                    stager.push(db, &row).await?;
                }
            }
            if i64::try_from(n).unwrap_or(0) < MERGE_BATCH {
                break;
            }
        }
        stager.flush(db).await?;
        log.line(&format!(
            "  {} {} dari {seen} baris",
            pad_end(name, 17),
            pad_start(&stager.count.to_string(), 9)
        ));
        loaded.insert(name.to_owned(), Value::from(stager.count));
    }

    if source.since.is_some() || source.until.is_some() {
        let mut removed = Vec::new();
        for (label, sql) in [
            (
                "screenshot",
                format!(
                    "DELETE FROM {} s USING {} tt, {} t\n          WHERE tt.\"id\" = s.\"trackingTimelineId\" AND t.\"id\" = tt.\"trackingId\"\n            AND NOT {}",
                    stg("screenshot"),
                    stg("trackingTimeline"),
                    stg("tracking"),
                    in_range("t", 1)
                ),
            ),
            (
                "trackingTimeline",
                format!(
                    "DELETE FROM {} tt USING {} t\n          WHERE t.\"id\" = tt.\"trackingId\" AND NOT {}",
                    stg("trackingTimeline"),
                    stg("tracking"),
                    in_range("t", 1)
                ),
            ),
            (
                "tracking",
                format!(
                    "DELETE FROM {} t WHERE NOT {}",
                    stg("tracking"),
                    in_range("t", 1)
                ),
            ),
        ] {
            let n = sqlx::query(&sql)
                .bind(source.since.as_deref())
                .bind(source.until.as_deref())
                .persistent(false)
                .execute(&mut *db)
                .await?
                .rows_affected();
            removed.push(format!("{label} {n}"));
        }
        log.line(&format!(
            "  di luar rentang {} .. {} (dibuang): {}",
            source.since.as_deref().unwrap_or("awal"),
            source.until.as_deref().unwrap_or("akhir"),
            removed.join(", ")
        ));
    }

    for (name, _) in &cols {
        exec(db, &format!("ANALYZE {}", stg(name))).await?;
    }
    Ok((legacy_company_id, loaded))
}

// ── Merge generik ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Hook {
    Company,
    ProjectType,
    Users,
    UserCompany,
    UserAccessLevel,
    Tracking,
    None,
}

/// `MergeSpec`.
struct Spec {
    name: &'static str,
    fks: &'static [(&'static str, &'static str)],
    sync: &'static [&'static str],
    insert_only: &'static [(&'static str, &'static str)],
    no_insert: Option<&'static str>,
    updatable_matches: &'static [&'static str],
    win_rule: Option<&'static str>,
    label: Option<&'static str>,
    conflict_list: Option<&'static str>,
    hook: Hook,
}

const fn spec(name: &'static str) -> Spec {
    Spec {
        name,
        fks: &[],
        sync: &[],
        insert_only: &[],
        no_insert: None,
        updatable_matches: &["uuid"],
        win_rule: None,
        label: None,
        conflict_list: None,
        hook: Hook::None,
    }
}

/// `buildSpecs(ctx)` (urutan = urutan topologis).
fn specs() -> Vec<Spec> {
    vec![
        Spec {
            sync: &[
                "name",
                "telp",
                "timeZone",
                "isAlllowDeleteScreenshot",
                "fixedHourNotification",
                "isAgencyMode",
                "status",
            ],
            no_insert: Some("company tidak ada di tujuan"),
            hook: Hook::Company,
            ..spec("company")
        },
        Spec {
            no_insert: Some("projectType (code) tidak ada di tujuan"),
            hook: Hook::ProjectType,
            ..spec("projectType")
        },
        Spec {
            sync: &["username", "password", "name", "status", "sentAt"],
            updatable_matches: &["uuid", "username"],
            win_rule: Some(
                r#"NOT EXISTS (SELECT 1 FROM "userCompany" o
                  WHERE o."userId" = p."id" AND o."isDefault" AND o."status" = 1
                    AND o."companyId" <> $2::int)"#,
            ),
            label: Some(r#"p."username""#),
            conflict_list: Some(
                r#"p."username" || ' (default: ' || (SELECT c."name" FROM "userCompany" o
                       JOIN "company" c ON c."id" = o."companyId"
                      WHERE o."userId" = p."id" AND o."isDefault" AND o."status" = 1 LIMIT 1) || ')'"#,
            ),
            hook: Hook::Users,
            ..spec("users")
        },
        Spec {
            fks: &[("projectTypeId", "projectType")],
            sync: &[
                "code",
                "name",
                "screenshotPerHour",
                "projectTypeId",
                "startDate",
                "status",
                "totalHour",
            ],
            ..spec("project")
        },
        Spec {
            fks: &[("projectId", "project"), ("companyId", "company")],
            sync: &["projectId", "companyId", "status"],
            ..spec("projectCompany")
        },
        Spec {
            fks: &[("projectId", "project"), ("userId", "users")],
            sync: &["projectId", "userId", "status"],
            ..spec("projectUser")
        },
        Spec {
            fks: &[("userId", "users"), ("companyId", "company")],
            sync: &[
                "userId",
                "companyId",
                "status",
                "isScreenshotActive",
                "isAllowManualTime",
                "isBlur",
                "isAllowEditTime",
                "screenshotPerHour",
                "idleTimeInSecond",
            ],
            insert_only: &[("isDefault", r#"s."insDefault""#)],
            conflict_list: Some(
                r#"(SELECT u."username" FROM "users" u WHERE u."id" = p."userId")"#,
            ),
            hook: Hook::UserCompany,
            ..spec("userCompany")
        },
        Spec {
            fks: &[("userId", "users"), ("companyId", "company")],
            sync: &["userId", "companyId", "accessLevelId", "status"],
            hook: Hook::UserAccessLevel,
            ..spec("userAccessLevel")
        },
        Spec {
            fks: &[("userId", "users")],
            sync: &["isAbsent", "totalTakenInSecond", "status"],
            label: Some(
                r#"(SELECT u."username" FROM "users" u WHERE u."id" = p."userId") || ' ' || p."takenDate"::text"#,
            ),
            insert_only: &[
                ("userId", r#"s."n_userId""#),
                ("takenDate", r#"s."takenDate""#),
            ],
            hook: Hook::Tracking,
            ..spec("tracking")
        },
        Spec {
            fks: &[("trackingId", "tracking"), ("projectId", "project")],
            sync: &[
                "note",
                "isOffline",
                "startAt",
                "endAt",
                "trackingId",
                "projectId",
                "status",
            ],
            label: Some(
                r#"(SELECT u."username" || ' ' || t."takenDate"::text FROM "tracking" t
                 JOIN "users" u ON u."id" = t."userId" WHERE t."id" = p."trackingId")"#,
            ),
            ..spec("trackingTimeline")
        },
        Spec {
            fks: &[
                ("trackingTimelineId", "trackingTimeline"),
                ("projectId", "project"),
            ],
            sync: &[
                "fileFullpath",
                "isBlur",
                "takenAt",
                "trackingTimelineId",
                "projectId",
                "status",
            ],
            ..spec("screenshot")
        },
    ]
}

fn fk_col(col: &str) -> String {
    q(&format!("n_{col}"))
}

fn src_expr(spec: &Spec, col: &str) -> String {
    if spec.fks.iter().any(|(c, _)| *c == col) {
        format!("s.{}", fk_col(col))
    } else {
        format!("s.{}", q(col))
    }
}

const LEGACY_STAMP: &str = r#"COALESCE(s."updatedAt", s."createdAt")"#;
const TARGET_STAMP: &str = r#"COALESCE(p."updatedAt", p."createdAt")"#;

/// `MergeTableStats`.
#[derive(Debug, Clone, Default)]
struct Stats {
    inserted: u64,
    updated: u64,
    conflict: i64,
    total: i64,
    by_uuid: i64,
    by_fallback: i64,
    skipped_insert: Option<u64>,
    skips: Vec<(String, i64)>,
    update_rows: Option<Vec<String>>,
    conflict_rows: Option<Vec<String>>,
}

impl Stats {
    fn to_json(&self) -> Value {
        let mut m = Map::new();
        m.insert("inserted".into(), json!(self.inserted));
        m.insert("updated".into(), json!(self.updated));
        m.insert("conflict".into(), json!(self.conflict));
        if let Some(r) = &self.update_rows {
            m.insert("updateRows".into(), json!(r));
        }
        if let Some(r) = &self.conflict_rows {
            m.insert("conflictRows".into(), json!(r));
        }
        if let Some(n) = self.skipped_insert {
            m.insert("skippedInsert".into(), json!(n));
        }
        m.insert("total".into(), json!(self.total));
        m.insert("byUuid".into(), json!(self.by_uuid));
        m.insert("byFallback".into(), json!(self.by_fallback));
        m.insert(
            "skips".into(),
            Value::Array(
                self.skips
                    .iter()
                    .map(|(s, n)| json!({ "skip": s, "n": n }))
                    .collect(),
            ),
        );
        Value::Object(m)
    }
}

struct Ctx {
    target_company_id: Option<i32>,
    expect_target_company_id: Option<i32>,
}

async fn map_foreign_keys(db: &mut PgConnection, spec: &Spec) -> CdResult<()> {
    for (col, r) in spec.fks {
        exec(
            db,
            &format!(
                "ALTER TABLE {} ADD COLUMN {} int",
                stg(spec.name),
                fk_col(col)
            ),
        )
        .await?;
        exec(
            db,
            &format!(
                "UPDATE {} s SET {} = r.\"newId\"\n         FROM {} r WHERE r.\"id\" = s.{}",
                stg(spec.name),
                fk_col(col),
                stg(r),
                q(col)
            ),
        )
        .await?;
        sqlx::query(&format!(
            "UPDATE {} SET \"skip\" = $1\n         WHERE \"skip\" IS NULL AND {} IS NULL",
            stg(spec.name),
            fk_col(col)
        ))
        .bind(format!("{col} tak terpetakan ke {r}"))
        .persistent(false)
        .execute(&mut *db)
        .await?;
    }
    Ok(())
}

async fn match_by_uuid(db: &mut PgConnection, spec: &Spec) -> CdResult<()> {
    exec(
        db,
        &format!(
            "UPDATE {} s SET \"newId\" = p.\"id\", \"matchedBy\" = 'uuid'\n       FROM {} p WHERE p.\"uuid\" = s.\"uuid\"",
            stg(spec.name),
            q(spec.name)
        ),
    )
    .await?;
    Ok(())
}

/// Kueri dengan parameter `$1 = matchKinds` (+ `$2 = targetCompanyId` bila ada `winRule`).
fn bind_params<'q>(
    query: sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments>,
    spec: &Spec,
    ctx: &Ctx,
) -> sqlx::query::Query<'q, sqlx::Postgres, sqlx::postgres::PgArguments> {
    let kinds: Vec<String> = spec
        .updatable_matches
        .iter()
        .map(|s| (*s).to_owned())
        .collect();
    let query = query.bind(kinds);
    if spec.win_rule.is_some() {
        query.bind(ctx.target_company_id)
    } else {
        query
    }
}

async fn update_matched(
    db: &mut PgConnection,
    spec: &Spec,
    p: &MergeParams,
    stats: &mut Stats,
    ctx: &Ctx,
) -> CdResult<()> {
    if spec.sync.is_empty() {
        return Ok(());
    }
    let differs = spec
        .sync
        .iter()
        .map(|c| format!("p.{} IS DISTINCT FROM {}", q(c), src_expr(spec, c)))
        .collect::<Vec<_>>()
        .join(" OR ");
    let cond = format!(
        "p.\"id\" = s.\"newId\" AND s.\"skip\" IS NULL\n      AND s.\"matchedBy\" = ANY($1::text[]) AND ({differs})"
    );
    let base = format!("FROM {} s WHERE {cond}", stg(spec.name));
    let default_wins = format!("{LEGACY_STAMP} >= {TARGET_STAMP}");
    let wins = spec.win_rule.unwrap_or(&default_wins);

    let row = bind_params(
        sqlx::query(&format!(
            "SELECT COUNT(*) FILTER (WHERE NOT {wins})::int AS \"conflict\"\n       FROM {} p, {} s WHERE {cond}",
            q(spec.name),
            stg(spec.name)
        )),
        spec,
        ctx,
    )
    .persistent(false)
    .fetch_one(&mut *db)
    .await?;
    let conflict: i32 = row.try_get("conflict")?;
    stats.conflict = if p.prefer_legacy {
        0
    } else {
        i64::from(conflict)
    };

    let mut sets: Vec<String> = spec
        .sync
        .iter()
        .map(|c| format!("{} = {}", q(c), src_expr(spec, c)))
        .collect();
    sets.push(format!(
        "\"updatedAt\" = GREATEST({LEGACY_STAMP}, p.\"updatedAt\")"
    ));
    let guard = if p.prefer_legacy {
        String::new()
    } else {
        format!("AND {wins}")
    };

    let diffs = spec
        .sync
        .iter()
        .map(|c| {
            let show = |v: &str| {
                if *c == "password" {
                    "'***'".to_owned()
                } else {
                    format!("COALESCE({v}::text, '∅')")
                }
            };
            format!(
                "CASE WHEN p.{qc} IS DISTINCT FROM {src}\n                THEN '{c}: ' || {a} || ' -> ' || {b} END",
                qc = q(c),
                src = src_expr(spec, c),
                a = show(&format!("p.{}", q(c))),
                b = show(&src_expr(spec, c))
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    let detail = bind_params(
        sqlx::query(&format!(
            "SELECT s.\"uuid\"::text AS \"uuid\", {label} AS \"label\",\n            array_to_string(ARRAY_REMOVE(ARRAY[{diffs}], NULL), ' | ') AS \"diff\"\n       FROM {} p, {} s WHERE {cond} {guard}\n      ORDER BY s.\"id\" LIMIT {UPDATE_DETAIL_LIMIT}",
            q(spec.name),
            stg(spec.name),
            label = spec.label.unwrap_or("NULL"),
        )),
        spec,
        ctx,
    )
    .persistent(false)
    .fetch_all(&mut *db)
    .await?;
    let mut update_rows = Vec::new();
    for r in detail {
        let uuid: Option<String> = r.try_get("uuid")?;
        let label: Option<String> = r.try_get::<Option<String>, _>("label").unwrap_or(None);
        let diff: Option<String> = r.try_get("diff")?;
        update_rows.push(format!(
            "{}{}: {}",
            uuid.unwrap_or_else(|| "null".into()),
            label
                .filter(|l| !l.is_empty())
                .map(|l| format!(" ({l})"))
                .unwrap_or_default(),
            diff.unwrap_or_else(|| "null".into())
        ));
    }
    stats.update_rows = Some(update_rows);

    let res = bind_params(
        sqlx::query(&format!(
            "WITH upd AS (\n       UPDATE {} p SET {} {base} {guard}\n       RETURNING p.\"id\")\n     UPDATE {} s SET \"touched\" = true FROM upd WHERE s.\"newId\" = upd.\"id\"",
            q(spec.name),
            sets.join(", "),
            stg(spec.name)
        )),
        spec,
        ctx,
    )
    .persistent(false)
    .execute(&mut *db)
    .await?;
    if let Some(list) = spec.conflict_list
        && stats.conflict > 0
        && !p.prefer_legacy
    {
        let rows = bind_params(
            sqlx::query(&format!(
                "SELECT {list} AS \"label\" FROM {} p, {} s\n        WHERE {cond} AND NOT {wins} ORDER BY 1",
                q(spec.name),
                stg(spec.name)
            )),
            spec,
            ctx,
        )
        .persistent(false)
        .fetch_all(&mut *db)
        .await?;
        stats.conflict_rows = Some(
            rows.iter()
                .map(|r| {
                    r.try_get::<Option<String>, _>("label")
                        .ok()
                        .flatten()
                        .unwrap_or_else(|| "null".into())
                })
                .collect(),
        );
    }
    stats.updated = res.rows_affected();
    Ok(())
}

async fn insert_missing(db: &mut PgConnection, spec: &Spec, stats: &mut Stats) -> CdResult<()> {
    if let Some(reason) = spec.no_insert {
        let n = sqlx::query(&format!(
            "UPDATE {} SET \"skip\" = $1 WHERE \"newId\" IS NULL AND \"skip\" IS NULL",
            stg(spec.name)
        ))
        .bind(reason)
        .persistent(false)
        .execute(&mut *db)
        .await?
        .rows_affected();
        stats.skipped_insert = Some(n);
        return Ok(());
    }
    let mut cols: Vec<&str> = vec!["uuid", "createdAt", "updatedAt"];
    cols.extend(spec.sync.iter());
    cols.extend(spec.insert_only.iter().map(|(c, _)| *c));
    let mut vals: Vec<String> = vec![
        "s.\"uuid\"".into(),
        "s.\"createdAt\"".into(),
        "s.\"updatedAt\"".into(),
    ];
    vals.extend(spec.sync.iter().map(|c| src_expr(spec, c)));
    vals.extend(spec.insert_only.iter().map(|(_, e)| (*e).to_owned()));
    let n = exec(
        db,
        &format!(
            "WITH ins AS (\n       INSERT INTO {} ({})\n       SELECT {} FROM {} s\n        WHERE s.\"newId\" IS NULL AND s.\"skip\" IS NULL\n        ORDER BY s.\"id\"\n       RETURNING \"id\", \"uuid\")\n     UPDATE {} s\n        SET \"newId\" = ins.\"id\", \"matchedBy\" = 'insert', \"touched\" = true\n       FROM ins WHERE s.\"uuid\" = ins.\"uuid\"",
            q(spec.name),
            cols.iter().map(|c| q(c)).collect::<Vec<_>>().join(", "),
            vals.join(", "),
            stg(spec.name),
            stg(spec.name)
        ),
    )
    .await?;
    stats.inserted = n;
    Ok(())
}

async fn count_summary(db: &mut PgConnection, spec: &Spec, stats: &mut Stats) -> CdResult<()> {
    let row = sqlx::query(&format!(
        "SELECT COUNT(*)::int AS \"total\",\n            COUNT(*) FILTER (WHERE \"matchedBy\" = 'uuid')::int AS \"byUuid\",\n            COUNT(*) FILTER (WHERE \"matchedBy\" NOT IN ('uuid', 'insert'))::int AS \"byFallback\"\n       FROM {}",
        stg(spec.name)
    ))
    .persistent(false)
    .fetch_one(&mut *db)
    .await?;
    stats.total = i64::from(row.try_get::<i32, _>("total")?);
    stats.by_uuid = i64::from(row.try_get::<i32, _>("byUuid")?);
    stats.by_fallback = i64::from(row.try_get::<i32, _>("byFallback")?);
    let skips = sqlx::query(&format!(
        "SELECT \"skip\", COUNT(*)::int AS \"n\" FROM {}\n      WHERE \"skip\" IS NOT NULL GROUP BY 1 ORDER BY 2 DESC",
        stg(spec.name)
    ))
    .persistent(false)
    .fetch_all(&mut *db)
    .await?;
    stats.skips = skips
        .iter()
        .map(|r| {
            (
                r.try_get::<String, _>("skip").unwrap_or_default(),
                i64::from(r.try_get::<i32, _>("n").unwrap_or(0)),
            )
        })
        .collect();
    Ok(())
}

async fn after_match(db: &mut PgConnection, spec: &Spec, ctx: &mut Ctx) -> CdResult<()> {
    match spec.hook {
        Hook::Company => {
            let new_id: Option<i32> =
                sqlx::query_scalar(&format!("SELECT \"newId\" FROM {}", stg("company")))
                    .persistent(false)
                    .fetch_optional(&mut *db)
                    .await?
                    .flatten();
            let Some(new_id) = new_id.filter(|n| *n != 0) else {
                return fail(
                    Code::MergeTargetMissing,
                    "Company (uuid) tidak ditemukan di database tujuan — script ini hanya merge.",
                );
            };
            ctx.target_company_id = Some(new_id);
            if let Some(expect) = ctx.expect_target_company_id
                && new_id != expect
            {
                return fail(
                    Code::MergeTargetMismatch,
                    format!(
                        "Company lama cocok (uuid) dengan company #{new_id} di tujuan, bukan #{expect} yang dipilih."
                    ),
                );
            }
            exec(
                db,
                &format!(
                    "UPDATE \"company\" p SET \"dailyReportSentAt\" = s.\"dailyReportSentAt\"::time\n             FROM {} s\n            WHERE p.\"id\" = s.\"newId\"\n              AND p.\"dailyReportSentAt\" IS DISTINCT FROM s.\"dailyReportSentAt\"::time\n              AND {LEGACY_STAMP} > {TARGET_STAMP}",
                    stg("company")
                ),
            )
            .await?;
        }
        Hook::ProjectType => {
            exec(
                db,
                &format!(
                    "UPDATE {} s SET \"newId\" = p.\"id\", \"matchedBy\" = 'code'\n             FROM \"projectType\" p WHERE s.\"newId\" IS NULL AND p.\"code\" = s.\"code\"",
                    stg("projectType")
                ),
            )
            .await?;
        }
        Hook::Users => {
            exec(
                db,
                &format!(
                    "UPDATE {} s SET \"newId\" = p.\"id\", \"matchedBy\" = 'username'\n             FROM \"users\" p WHERE s.\"newId\" IS NULL AND p.\"username\" = s.\"username\"",
                    stg("users")
                ),
            )
            .await?;
            exec(
                db,
                &format!(
                    "UPDATE {} s SET \"skip\" = 'username dipakai user lain di tujuan'\n            WHERE s.\"skip\" IS NULL AND s.\"newId\" IS NOT NULL\n              AND EXISTS (SELECT 1 FROM \"users\" o\n                           WHERE o.\"username\" = s.\"username\" AND o.\"id\" <> s.\"newId\")",
                    stg("users")
                ),
            )
            .await?;
        }
        Hook::UserAccessLevel => {
            let t = stg("userAccessLevel");
            exec(
                db,
                &format!("ALTER TABLE {t} ADD COLUMN \"n_accessLevelId\" int"),
            )
            .await?;
            exec(
                db,
                &format!(
                    "UPDATE {t} s SET \"n_accessLevelId\" = a.\"id\"\n             FROM \"accessLevel\" a WHERE a.\"id\" = s.\"accessLevelId\" AND a.\"companyId\" IS NULL"
                ),
            )
            .await?;
            exec(
                db,
                &format!(
                    "UPDATE {t} SET \"skip\" = 'accessLevelId bukan role bawaan di tujuan'\n            WHERE \"skip\" IS NULL AND \"n_accessLevelId\" IS NULL"
                ),
            )
            .await?;
            exec(
                db,
                &format!(
                    "UPDATE {t} s SET \"skip\" = 'sudah ada role aktif lain di tujuan'\n            WHERE s.\"skip\" IS NULL AND s.\"newId\" IS NOT NULL AND s.\"status\" = 1\n              AND EXISTS (SELECT 1 FROM \"userAccessLevel\" o\n                           WHERE o.\"userId\" = s.\"n_userId\" AND o.\"companyId\" = s.\"n_companyId\"\n                             AND o.\"status\" = 1 AND o.\"id\" <> s.\"newId\")\n              AND EXISTS (SELECT 1 FROM \"userAccessLevel\" p\n                           WHERE p.\"id\" = s.\"newId\" AND p.\"status\" <> 1)"
                ),
            )
            .await?;
        }
        Hook::Tracking => {
            exec(
                db,
                &format!(
                    "UPDATE {} s SET \"newId\" = p.\"id\", \"matchedBy\" = 'userDate'\n             FROM \"tracking\" p\n            WHERE s.\"newId\" IS NULL AND s.\"skip\" IS NULL\n              AND p.\"userId\" = s.\"n_userId\" AND p.\"takenDate\" = s.\"takenDate\"",
                    stg("tracking")
                ),
            )
            .await?;
        }
        Hook::UserCompany | Hook::None => {}
    }
    Ok(())
}

async fn before_insert(db: &mut PgConnection, spec: &Spec) -> CdResult<()> {
    match spec.hook {
        Hook::UserCompany => {
            let t = stg("userCompany");
            exec(
                db,
                &format!("ALTER TABLE {t} ADD COLUMN \"insDefault\" boolean"),
            )
            .await?;
            exec(
                db,
                &format!(
                    "UPDATE {t} s SET \"insDefault\" =\n              s.\"isDefault\"\n              AND s.\"id\" = (SELECT MAX(x.\"id\") FROM {t} x\n                             WHERE x.\"n_userId\" = s.\"n_userId\" AND x.\"isDefault\"\n                               AND x.\"newId\" IS NULL AND x.\"skip\" IS NULL)\n              AND NOT EXISTS (SELECT 1 FROM \"userCompany\" d\n                               WHERE d.\"userId\" = s.\"n_userId\" AND d.\"isDefault\")\n            WHERE s.\"newId\" IS NULL AND s.\"skip\" IS NULL"
                ),
            )
            .await?;
        }
        Hook::UserAccessLevel => {
            let t = stg("userAccessLevel");
            exec(
                db,
                &format!(
                    "UPDATE {t} s SET \"skip\" = 'sudah ada role aktif lain di tujuan'\n            WHERE s.\"skip\" IS NULL AND s.\"newId\" IS NULL AND s.\"status\" = 1\n              AND (EXISTS (SELECT 1 FROM \"userAccessLevel\" o\n                            WHERE o.\"userId\" = s.\"n_userId\" AND o.\"companyId\" = s.\"n_companyId\"\n                              AND o.\"status\" = 1)\n                OR s.\"id\" <> (SELECT MAX(x.\"id\") FROM {t} x\n                               WHERE x.\"n_userId\" = s.\"n_userId\" AND x.\"n_companyId\" = s.\"n_companyId\"\n                                 AND x.\"status\" = 1 AND x.\"newId\" IS NULL))"
                ),
            )
            .await?;
        }
        _ => {}
    }
    Ok(())
}

async fn merge_table(
    db: &mut PgConnection,
    spec: &Spec,
    p: &MergeParams,
    ctx: &mut Ctx,
) -> CdResult<Stats> {
    let mut stats = Stats::default();
    map_foreign_keys(db, spec).await?;
    match_by_uuid(db, spec).await?;
    after_match(db, spec, ctx).await?;
    update_matched(db, spec, p, &mut stats, ctx).await?;
    before_insert(db, spec).await?;
    insert_missing(db, spec, &mut stats).await?;
    count_summary(db, spec, &mut stats).await?;
    Ok(stats)
}

// ── Pasca-merge ──────────────────────────────────────────────────────────────

async fn recompute_merged_tracking_totals(db: &mut PgConnection) -> CdResult<u64> {
    exec(
        db,
        &format!(
            "UPDATE \"tracking\" t SET \"totalTakenInSecond\" = agg.\"seconds\"\n       FROM (SELECT tt.\"trackingId\",\n                    COALESCE(SUM(EXTRACT(EPOCH FROM (tt.\"endAt\" - tt.\"startAt\")))\n                      FILTER (WHERE tt.\"status\" = 1), 0)::int AS \"seconds\"\n               FROM \"trackingTimeline\" tt\n              WHERE tt.\"trackingId\" IN (SELECT \"newId\" FROM {}\n                                         WHERE \"matchedBy\" = 'userDate')\n              GROUP BY tt.\"trackingId\") agg\n      WHERE t.\"id\" = agg.\"trackingId\"\n        AND t.\"totalTakenInSecond\" IS DISTINCT FROM agg.\"seconds\"",
            stg("tracking")
        ),
    )
    .await
}

async fn affected_dates(db: &mut PgConnection) -> CdResult<Vec<String>> {
    Ok(sqlx::query_scalar(&format!(
        "SELECT DISTINCT to_char(t.\"takenDate\", 'YYYY-MM-DD') AS \"d\"\n       FROM \"tracking\" t\n      WHERE t.\"id\" IN (SELECT s.\"n_trackingId\" FROM {} s WHERE s.\"touched\")\n         OR t.\"id\" IN (SELECT s.\"newId\" FROM {} s WHERE s.\"touched\")\n      ORDER BY 1",
        stg("trackingTimeline"),
        stg("tracking")
    ))
    .persistent(false)
    .fetch_all(&mut *db)
    .await?)
}

/// `resyncDate(db, date)` — WAJIB identik dengan `tasks.service.ts#runSyncForDate`.
async fn resync_date(db: &mut PgConnection, date: &str) -> CdResult<()> {
    sqlx::query(
        r#"INSERT INTO "dailyUserProjectSummary" ("reportDate", "userId", "projectId", "companyId", "totalSeconds")
     SELECT $1::date, "userId", "projectId", "companyId", SUM(seconds)::int
       FROM (
         SELECT DISTINCT ON (tt.id)
           t."userId", tt."projectId", uc."companyId",
           EXTRACT(EPOCH FROM (tt."endAt" - tt."startAt"))::int as seconds
         FROM "trackingTimeline" tt
         JOIN "tracking" t ON tt."trackingId" = t.id
         JOIN "userCompany" uc ON t."userId" = uc."userId" AND uc."status" = 1 AND uc."isDefault" = true
         WHERE t."takenDate" = $1::date AND tt.status = 1
       ) AS unique_timeline
      GROUP BY "userId", "projectId", "companyId"
      ORDER BY "userId", "projectId", "companyId"
     ON CONFLICT ("userId", "projectId", "reportDate")
     DO UPDATE SET "totalSeconds" = EXCLUDED."totalSeconds", "companyId" = EXCLUDED."companyId""#,
    )
    .bind(date)
    .persistent(false)
    .execute(&mut *db)
    .await?;
    sqlx::query(
        r#"WITH unique_timeline AS (
       SELECT DISTINCT ON (tt.id)
         tt."projectId", pc."companyId",
         EXTRACT(EPOCH FROM (tt."endAt" - tt."startAt"))::int as seconds
       FROM "trackingTimeline" tt
       JOIN "tracking" t ON tt."trackingId" = t.id
       JOIN "projectCompany" pc ON tt."projectId" = pc."projectId"
       WHERE t."takenDate" = $1::date AND tt.status = 1
     ),
     project_upsert AS (
       INSERT INTO "dailyProjectSummary" ("reportDate", "projectId", "companyId", "totalSeconds")
       SELECT $1::date, "projectId", "companyId", SUM(seconds)::int
         FROM unique_timeline
        GROUP BY "projectId", "companyId"
        ORDER BY "projectId", "companyId"
       ON CONFLICT ("projectId", "companyId", "reportDate")
       DO UPDATE SET "totalSeconds" = EXCLUDED."totalSeconds"
       RETURNING 1
     ),
     company_upsert AS (
       INSERT INTO "dailyCompanySummary" ("reportDate", "companyId", "totalSeconds")
       SELECT $1::date, "companyId", SUM(seconds)::int
         FROM unique_timeline
        GROUP BY "companyId"
        ORDER BY "companyId"
       ON CONFLICT ("companyId", "reportDate")
       DO UPDATE SET "totalSeconds" = EXCLUDED."totalSeconds"
       RETURNING 1
     )
     SELECT (SELECT COUNT(*) FROM project_upsert), (SELECT COUNT(*) FROM company_upsert)"#,
    )
    .bind(date)
    .persistent(false)
    .execute(&mut *db)
    .await?;
    Ok(())
}

/// `targetOnlyCounts(db, companyId, range, legacyUserId)`.
async fn target_only_counts(
    db: &mut PgConnection,
    company_id: i32,
    p: &MergeParams,
) -> CdResult<IndexMapCounts> {
    let row = sqlx::query(&format!(
        "SELECT\n       (SELECT COUNT(*) FROM \"userCompany\" p WHERE p.\"companyId\" = $1\n          AND NOT EXISTS (SELECT 1 FROM {uc} s WHERE s.\"uuid\" = p.\"uuid\"))::int AS \"userCompany\",\n       (SELECT COUNT(*) FROM \"userAccessLevel\" p WHERE p.\"companyId\" = $1\n          AND NOT EXISTS (SELECT 1 FROM {ual} s WHERE s.\"uuid\" = p.\"uuid\"))::int AS \"userAccessLevel\",\n       (SELECT COUNT(*) FROM \"projectCompany\" p WHERE p.\"companyId\" = $1\n          AND NOT EXISTS (SELECT 1 FROM {pc} s WHERE s.\"uuid\" = p.\"uuid\"))::int AS \"projectCompany\",\n       (SELECT COUNT(*) FROM \"trackingTimeline\" p\n          JOIN \"tracking\" t ON t.\"id\" = p.\"trackingId\"\n         WHERE p.\"projectId\" IN (SELECT \"newId\" FROM {pr})\n           AND {range}\n           AND ($4::int IS NULL OR t.\"userId\" = (SELECT \"newId\" FROM {us} WHERE \"id\" = $4::int))\n           AND NOT EXISTS (SELECT 1 FROM {tt} s WHERE s.\"uuid\" = p.\"uuid\"))::int AS \"trackingTimeline\"",
        uc = stg("userCompany"),
        ual = stg("userAccessLevel"),
        pc = stg("projectCompany"),
        pr = stg("project"),
        us = stg("users"),
        tt = stg("trackingTimeline"),
        range = in_range("t", 2)
    ))
    .bind(company_id)
    .bind(p.since.as_deref())
    .bind(p.until.as_deref())
    .bind(Option::<i32>::None)
    .persistent(false)
    .fetch_one(&mut *db)
    .await?;
    let mut out = Vec::new();
    for k in [
        "userCompany",
        "userAccessLevel",
        "projectCompany",
        "trackingTimeline",
    ] {
        out.push((k, i64::from(row.try_get::<i32, _>(k)?)));
    }
    Ok(out)
}

type IndexMapCounts = Vec<(&'static str, i64)>;

/// `util.format('…:', obj)` untuk objek empat kunci (selalu melewati `breakLength` 80).
fn inspect_counts(counts: &IndexMapCounts) -> String {
    let body = counts
        .iter()
        .map(|(k, v)| format!("  {k}: {v}"))
        .collect::<Vec<_>>()
        .join(",\n");
    format!("{{\n{body}\n}}")
}

async fn check_retention(
    db: &mut PgConnection,
    company_id: i32,
    p: &MergeParams,
    log: &dyn Log,
) -> CdResult<Option<(i32, i32)>> {
    let days: Option<Option<i32>> = sqlx::query_scalar(
        r#"SELECT "screenshotRetentionDays" AS "days" FROM "company" WHERE "id" = $1"#,
    )
    .bind(company_id)
    .persistent(false)
    .fetch_optional(&mut *db)
    .await?;
    let Some(days) = days.flatten().filter(|d| *d > 0 && *d < 36500) else {
        return Ok(None);
    };
    if p.unlimited_retention {
        sqlx::query(
            r#"UPDATE "company" SET "screenshotRetentionDays" = $2, "updatedAt" = NOW() WHERE "id" = $1"#,
        )
        .bind(company_id)
        .bind(UNLIMITED_RETENTION_DAYS)
        .persistent(false)
        .execute(&mut *db)
        .await?;
        log.line(&format!(
            "screenshotRetentionDays: {days} -> {UNLIMITED_RETENTION_DAYS} (unlimited)"
        ));
        return Ok(Some((days, UNLIMITED_RETENTION_DAYS)));
    }
    let msg = format!(
        "screenshotRetentionDays company tujuan = {days}. Cron purge (02:15) akan menghapus object GCS screenshot impor yang lebih tua dari itu — object yang SAMA masih dipakai aplikasi lama (bucket sama). Pakai --unlimited-retention, atau --accept-retention kalau memang mau dihapus."
    );
    if !p.accept_retention {
        return fail(Code::RetentionLimited, msg);
    }
    log.line(&format!("PERINGATAN: {msg}"));
    Ok(Some((days, days)))
}

/// `mergeLegacyCompany(deps, params, log)` dengan sumber DB lama, dalam SATU transaksi di
/// database tujuan (`target`); `legacy_options` = koneksi `LEGACY_DATABASE_URL`.
pub async fn merge_legacy_company(
    target: &PgPool,
    legacy_options: &PgConnectOptions,
    p: &MergeParams,
    log: &dyn Log,
) -> CdResult<Value> {
    validate(p)?;
    log.line(&format!(
        "Tujuan : {}",
        p.target_host.as_deref().unwrap_or("-")
    ));
    log.line(&format!(
        "Sumber : DB lama {}, company {}",
        p.legacy_host.as_deref().unwrap_or("-"),
        p.legacy_company_id
    ));
    log.line(&format!(
        "Mode   : {}",
        if p.apply {
            "APPLY (commit)"
        } else {
            "DRY RUN (rollback)"
        }
    ));
    log.line("Tabel  : semua");
    log.line(&format!(
        "Rentang: takenDate {} .. {}",
        p.since.as_deref().unwrap_or("awal"),
        p.until.as_deref().unwrap_or("akhir")
    ));
    log.line("User   : semua\n");

    // Klien dibuka per job dan SELALU ditutup (juga saat merge gagal); transaksi tujuan
    // di-ROLLBACK otomatis bila `tx` dibuang karena error.
    let mut tx = target.begin().await?;
    sqlx::query("SET LOCAL statement_timeout = 0")
        .persistent(false)
        .execute(&mut *tx)
        .await?;
    if let Some(key) = &p.lock_key {
        mz_core::db::advisory_xact_lock(&mut tx, key).await?;
    }
    let legacy = PgConnection::connect_with(legacy_options).await?;
    let mut source = DbSource {
        legacy,
        company_id: p.legacy_company_id,
        since: p.since.clone(),
        until: p.until.clone(),
    };
    let result = run_in_tx(&mut tx, &mut source, p, log).await;
    let _ = source.legacy.close().await;
    let result = result?;
    if p.apply {
        tx.commit().await?;
        log.line("\nCOMMIT selesai.");
    } else {
        tx.rollback().await?;
        log.line("\nDRY RUN — semua perubahan di-rollback. Jalankan ulang dengan --apply.");
    }
    Ok(result)
}

async fn run_in_tx(
    db: &mut PgConnection,
    source: &mut DbSource,
    p: &MergeParams,
    log: &dyn Log,
) -> CdResult<Value> {
    create_staging(db).await?;

    log.line("Memuat backup ke staging:");
    sqlx::query("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .persistent(false)
        .execute(&mut source.legacy)
        .await?;
    let loaded = load_backup(db, source, log).await;
    // Read-only; gagal di sini (koneksi putus) jangan menutupi error asal.
    let _ = sqlx::query("ROLLBACK")
        .persistent(false)
        .execute(&mut source.legacy)
        .await;
    let (legacy_company_id, _loaded) = loaded?;

    log.line("\nMerge:");
    let mut ctx = Ctx {
        target_company_id: None,
        expect_target_company_id: p.expect_target_company_id,
    };
    let mut report = Map::new();
    let mut retention: Option<(i32, i32)> = None;
    for spec in specs() {
        let t0 = Instant::now();
        let s = merge_table(db, &spec, p, &mut ctx).await?;
        if spec.name == "company" {
            retention =
                check_retention(db, ctx.target_company_id.unwrap_or_default(), p, log).await?;
        }
        log.line(&format!(
            "  {} total {} | uuid {} | cadangan {} | insert {} | update {} | konflik {} | {:.1}s",
            pad_end(spec.name, 17),
            pad_start(&s.total.to_string(), 7),
            pad_start(&s.by_uuid.to_string(), 7),
            pad_start(&s.by_fallback.to_string(), 5),
            pad_start(&s.inserted.to_string(), 7),
            pad_start(&s.updated.to_string(), 6),
            pad_start(&s.conflict.to_string(), 5),
            t0.elapsed().as_secs_f64()
        ));
        for (skip, n) in &s.skips {
            log.line(&format!("      skip {n}: {skip}"));
        }
        for c in s.conflict_rows.iter().flatten() {
            log.line(&format!("      konflik: {c}"));
        }
        let detail = s.update_rows.as_ref().map_or(0, Vec::len);
        for u in s.update_rows.iter().flatten() {
            log.line(&format!("      update: {u}"));
        }
        let detail = u64::try_from(detail).unwrap_or(0);
        if s.updated > detail {
            log.line(&format!(
                "      update: ... {} baris lagi",
                s.updated - detail
            ));
        }
        report.insert(spec.name.to_owned(), s.to_json());
    }

    let recomputed = recompute_merged_tracking_totals(db).await?;
    log.line(&format!(
        "\ntracking digabung (userId+takenDate), total dihitung ulang: {recomputed}"
    ));

    let target_company_id = ctx.target_company_id.unwrap_or_default();
    let only_target = target_only_counts(db, target_company_id, p).await?;
    log.line(&format!(
        "Hanya ada di tujuan (tidak disentuh): {}",
        inspect_counts(&only_target)
    ));

    let dates = affected_dates(db).await?;
    log.line(&format!(
        "\nResync daily summary untuk {} tanggal...",
        dates.len()
    ));
    for d in &dates {
        resync_date(db, d).await?;
    }

    let target_only: Map<String, Value> = only_target
        .iter()
        .map(|(k, v)| ((*k).to_owned(), Value::from(*v)))
        .collect();
    Ok(json!({
        "committed": p.apply,
        "targetCompanyId": target_company_id,
        "legacyCompanyId": legacy_company_id,
        "legacyUserId": null,
        "tables": report,
        "retention": retention.map(|(before, after)| json!({ "before": before, "after": after })),
        "recomputedTrackingTotals": recomputed,
        "targetOnly": target_only,
        "affectedDates": dates,
        "resynced": true,
    }))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn params() -> MergeParams {
        MergeParams {
            legacy_company_id: 4,
            apply: false,
            prefer_legacy: false,
            unlimited_retention: false,
            accept_retention: false,
            since: None,
            until: None,
            expect_target_company_id: None,
            target_host: None,
            legacy_host: None,
            lock_key: None,
        }
    }

    fn msg(p: &MergeParams) -> Option<String> {
        validate(p).err().map(|e| {
            format!(
                "{}: {e}",
                super::super::errors::code_of(&e).map_or("?", Code::as_str)
            )
        })
    }

    #[test]
    fn validasi_seperti_cli_lama() {
        let mut p = params();
        p.since = Some("2026-10-12".into());
        p.until = Some("2026-10-01".into());
        assert_eq!(
            msg(&p).as_deref(),
            Some("INVALID_PARAMS: --since tidak boleh sesudah --until")
        );
        let mut p = params();
        p.since = Some("2026-02-30".into());
        assert_eq!(
            msg(&p).as_deref(),
            Some("INVALID_PARAMS: --since harus tanggal YYYY-MM-DD yang valid")
        );
        let mut p = params();
        p.legacy_company_id = 0;
        assert_eq!(
            msg(&p).as_deref(),
            Some("INVALID_PARAMS: --from-db harus id company (bilangan bulat > 0)")
        );
        assert_eq!(msg(&params()), None);
        assert!(is_valid_merge_date("2024-02-29"));
        assert!(!is_valid_merge_date("2023-02-29"));
        assert!(!is_valid_merge_date("2024-2-29"));
    }

    #[test]
    fn pg_config_seperti_node() {
        let err = pg_connect_options("DATABASE_URL", None, None).unwrap_err();
        assert_eq!(super::super::errors::code_of(&err), Some(Code::PgConfig));
        assert!(
            pg_connect_options(
                "DATABASE_URL",
                Some("postgresql://u:p@h:25060/db?schema=public&sslmode=require"),
                None
            )
            .is_ok()
        );
        assert!(
            pg_connect_options("X", Some("postgresql://u:p@h/db?sslmode=disable"), None).is_ok()
        );
        let err = pg_connect_options("X", Some("postgresql://u:p@h/db?sslmode=verify-full"), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("mewajibkan CA server"), "{err}");
        assert_eq!(
            url_host("postgresql://u:p@h:25060/db").as_deref(),
            Some("h:25060")
        );
    }

    #[test]
    fn format_target_only_seperti_util_format() {
        let s = inspect_counts(&vec![
            ("userCompany", 0),
            ("userAccessLevel", 0),
            ("projectCompany", 0),
            ("trackingTimeline", 0),
        ]);
        assert_eq!(
            s,
            "{\n  userCompany: 0,\n  userAccessLevel: 0,\n  projectCompany: 0,\n  trackingTimeline: 0\n}"
        );
    }

    #[test]
    fn in_range_parameter() {
        assert!(in_range("t", 2).contains("$2::date IS NULL"));
        assert!(in_range("t", 2).contains("$3::date"));
    }
}
