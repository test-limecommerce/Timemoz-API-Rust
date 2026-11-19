//! Route tulis `/v1/divisions*`: create, update, ganti peran anggota, keluarkan anggota, hapus.
//!
//! Aturan seluruh berkas (sama dengan `divisions.service.ts`): setiap tulis memakai SATU
//! transaksi, pernyataan pertama `lockDivisionsCompany` (kunci advisory `divisions:company:{id}`,
//! `mz_account::division_membership::lock_divisions_company`), audit dibuang bila rollback, tidak
//! ada query laporan/waktu di dalam transaksi; sesudah commit, detail dibaca ulang.
//!
//! Cache: Node TIDAK memanggil `DivisionGateService.invalidate` dari endpoint ini (gerbang hanya
//! menyimpan kolom company `divisionsEnabled`/modul/paket; keanggotaan dan jangkauan Leader dibaca
//! tiap request tanpa cache, FR-021), jadi tidak ada invalidasi lokal maupun pesan
//! `mz:cache-invalidate` yang perlu dikirim. Lihat `plan/deviations/mz-divisions.md`.

use std::collections::HashSet;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use mz_account::division_membership::lock_divisions_company;
use mz_audit::{AuditHandle, Lookup, MemberEffect, Model, Row, Touched};
use mz_core::extract::{ValidatedJson, ValidatedQuery};
use mz_core::time::now_naive;
use mz_core::uuid_rules::is_uuid;
use mz_core::{ApiError, AppState, RequestCtx};
use serde::Serialize;
use sqlx::{PgConnection, Postgres, QueryBuilder};

use crate::consts::*;
use crate::dto::{
    CreateBody, CreateInput, Field, MemberInput, MemberRole, Members, RoleBody, UpdateBody,
    UpdateInput, VersionQuery, parse_create, parse_role, parse_update, parse_version_query,
};
use crate::entities::{DeleteEntity, DeleteResponse, DetailResponse, Person};
use crate::errors::*;
use crate::extract::{UuidPair, UuidParam};
use crate::queries::*;
use crate::read::get_detail;
use crate::scope::can_lead_division;

/// Galat di dalam transaksi: galat SQL ditahan mentah supaya `mapUniqueViolation` bisa melihat
/// constraint-nya sebelum diubah menjadi 409 generik.
enum TxError {
    Div(DivError),
    Sql(sqlx::Error),
}

impl From<DivError> for TxError {
    fn from(e: DivError) -> Self {
        Self::Div(e)
    }
}

impl From<sqlx::Error> for TxError {
    fn from(e: sqlx::Error) -> Self {
        Self::Sql(e)
    }
}

type TxResult<T> = Result<T, TxError>;

/// `throw this.mapUniqueViolation(e) ?? e`.
fn into_div(e: TxError) -> DivError {
    match e {
        TxError::Div(d) => d,
        TxError::Sql(s) => map_unique_violation(&s).unwrap_or_else(|| s.into()),
    }
}

/// Karyawan aktif company hasil `resolveActiveEmployees`.
#[derive(Debug, Clone)]
pub struct Employee {
    pub id: i32,
    pub uuid: String,
    pub name: String,
}

/// Divisi hidup yang dibaca ulang di bawah lock.
struct Locked {
    id: i32,
    uuid: String,
    name: String,
}

/// Baris `divisionMember` hidup (status user apa pun).
#[derive(Debug, Clone, Copy)]
pub struct MemberRowRef {
    pub id: i32,
    pub user_id: i32,
    pub role: MemberRole,
}

fn role_of(s: &str) -> MemberRole {
    MemberRole::parse(s).unwrap_or(MemberRole::Member)
}

// ───────────────────────── validasi (T023) ─────────────────────────

/// Anggota yang sudah dinormalisasi: uuid huruf kecil, urutan masukan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Normalized {
    pub members: Vec<(String, MemberRole)>,
    pub leader_uuids: Vec<String>,
}

/// `normalizeMembers`: duplikat DULU (422 `details.userUuids`), baru "minimal satu Leader".
pub fn normalize_members(members: &[MemberInput]) -> DivResult<Normalized> {
    let normalized: Vec<(String, MemberRole)> = members
        .iter()
        .map(|m| (m.user_uuid.to_lowercase(), m.role))
        .collect();
    let mut seen: HashSet<&str> = HashSet::new();
    let mut duplicates: Vec<String> = Vec::new();
    for (uuid, _) in &normalized {
        if !seen.insert(uuid.as_str()) && !duplicates.contains(uuid) {
            duplicates.push(uuid.clone());
        }
    }
    if !duplicates.is_empty() {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct D {
            user_uuids: Vec<String>,
        }
        return Err(coded_details(
            StatusCode::UNPROCESSABLE_ENTITY,
            DIVISION_DUPLICATE_MEMBER,
            "The same employee is listed more than once",
            D {
                user_uuids: duplicates,
            },
        ));
    }
    let leader_uuids: Vec<String> = normalized
        .iter()
        .filter(|(_, r)| *r == MemberRole::Leader)
        .map(|(u, _)| u.clone())
        .collect();
    if leader_uuids.is_empty() {
        return Err(coded(
            StatusCode::UNPROCESSABLE_ENTITY,
            DIVISION_LEADER_REQUIRED,
            "A division needs at least one Leader",
        ));
    }
    Ok(Normalized {
        members: normalized,
        leader_uuids,
    })
}

/// Satu query: karyawan aktif company untuk `uuids`. Yang tak ditemukan → 422
/// `DIVISION_MEMBER_NOT_IN_COMPANY` dengan `details.userUuids` dalam URUTAN MASUKAN.
async fn resolve_active_employees(
    conn: &mut PgConnection,
    company: i32,
    uuids: &[String],
) -> TxResult<std::collections::HashMap<String, Employee>> {
    let mut wanted: Vec<String> = Vec::new();
    for u in uuids {
        let l = u.to_lowercase();
        if !wanted.contains(&l) {
            wanted.push(l);
        }
    }
    let mut result = std::collections::HashMap::new();
    if wanted.is_empty() {
        return Ok(result);
    }
    let parsed: Vec<uuid::Uuid> = wanted
        .iter()
        .filter_map(|u| uuid::Uuid::parse_str(u).ok())
        .collect();
    let rows: Vec<(i32, String, String)> = sqlx::query_as(&format!(
        r#"SELECT u.id, u.uuid::text, u.name FROM users u
           WHERE u.uuid = ANY($2) AND {ACTIVE_EMPLOYEE_SQL}"#
    ))
    .bind(company)
    .bind(&parsed)
    .fetch_all(&mut *conn)
    .await?;
    for (id, uuid, name) in rows {
        result.insert(uuid.to_lowercase(), Employee { id, uuid, name });
    }
    let missing: Vec<String> = wanted
        .iter()
        .filter(|u| !result.contains_key(*u))
        .cloned()
        .collect();
    if !missing.is_empty() {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct D {
            user_uuids: Vec<String>,
        }
        let message = if missing.len() == 1 {
            "1 employee is not an active employee of this company".to_owned()
        } else {
            format!(
                "{} employees are not active employees of this company",
                missing.len()
            )
        };
        return Err(coded_details(
            StatusCode::UNPROCESSABLE_ENTITY,
            DIVISION_MEMBER_NOT_IN_COMPANY,
            &message,
            D {
                user_uuids: missing,
            },
        )
        .into());
    }
    Ok(result)
}

/// FR-033: setiap Leader harus punya role efektif yang boleh memimpin; 422
/// `DIVISION_LEADER_NOT_ELIGIBLE` dengan SEMUA yang ditolak di `details` (urutan masukan).
async fn assert_leaders_eligible(
    conn: &mut PgConnection,
    company: i32,
    leaders: &[Employee],
) -> TxResult<()> {
    if leaders.is_empty() {
        return Ok(());
    }
    let ids: Vec<i32> = leaders.iter().map(|l| l.id).collect();
    let codes = role_codes_in_company(&mut *conn, company, &ids).await?;
    let offenders: Vec<&Employee> = leaders
        .iter()
        .filter(|l| !can_lead_division(code_of(&codes, l.id)))
        .collect();
    let Some(first) = offenders.first() else {
        return Ok(());
    };
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct D<'a> {
        user_uuid: &'a str,
        name: &'a str,
    }
    let message = if offenders.len() == 1 {
        format!(
            "{} cannot be a Leader because their role is Guest",
            first.name
        )
    } else {
        format!(
            "{} employees cannot be Leaders because their role is Guest",
            offenders.len()
        )
    };
    let details: Vec<D<'_>> = offenders
        .iter()
        .map(|l| D {
            user_uuid: &l.uuid,
            name: &l.name,
        })
        .collect();
    Err(coded_details(
        StatusCode::UNPROCESSABLE_ENTITY,
        DIVISION_LEADER_NOT_ELIGIBLE,
        &message,
        details,
    )
    .into())
}

/// FR-004: nama unik per company tanpa membedakan huruf, hanya di antara divisi hidup.
/// Pola di-escape karena Prisma menerjemahkan `equals` + `insensitive` menjadi ILIKE.
async fn assert_name_free(
    conn: &mut PgConnection,
    company: i32,
    name: &str,
    exclude: Option<i32>,
) -> TxResult<()> {
    let clash: Option<i32> = sqlx::query_scalar(
        r#"SELECT id FROM division
           WHERE "companyId" = $1 AND "deletedAt" IS NULL AND name ILIKE $2
             AND ($3::int IS NULL OR id <> $3)
           LIMIT 1"#,
    )
    .bind(company)
    .bind(escape_like_pattern(name))
    .bind(exclude)
    .fetch_optional(&mut *conn)
    .await?;
    if clash.is_some() {
        return Err(name_taken().into());
    }
    Ok(())
}

// ───────────────────────── helper tulis ─────────────────────────

/// Lock company, baca ulang divisi hidup → 404; `version` beda → 409 DIVISION_STALE (T3).
async fn lock_and_load(
    conn: &mut PgConnection,
    company: i32,
    division_uuid: &str,
    expected_version: i64,
) -> TxResult<Locked> {
    lock_divisions_company(conn, company).await?;
    let row: Option<(i32, String, String, i32)> = sqlx::query_as(
        r#"SELECT id, uuid::text, name, version FROM division
           WHERE uuid = $1::uuid AND "companyId" = $2 AND "deletedAt" IS NULL
           LIMIT 1"#,
    )
    .bind(division_uuid.to_lowercase())
    .bind(company)
    .fetch_optional(&mut *conn)
    .await?;
    let Some((id, uuid, name, version)) = row else {
        return Err(division_not_found().into());
    };
    if i64::from(version) != expected_version {
        return Err(stale().into());
    }
    Ok(Locked { id, uuid, name })
}

struct LiveMemberRef {
    row: MemberRowRef,
    user: Employee,
}

/// Baris anggota hidup `userUuid` di divisi ini, user-nya karyawan aktif company; selain itu 404.
async fn find_live_member(
    conn: &mut PgConnection,
    company: i32,
    division_id: i32,
    user_uuid: &str,
) -> TxResult<LiveMemberRef> {
    let row: Option<(i32, i32, String, String, String)> = sqlx::query_as(&format!(
        r#"SELECT dm.id, dm."userId", dm.role::text, u.uuid::text, u.name
           FROM "divisionMember" dm
           JOIN users u ON u.id = dm."userId"
           WHERE dm."companyId" = $1 AND dm."divisionId" = $2 AND dm."deletedAt" IS NULL
             AND u.uuid = $3::uuid AND {ACTIVE_EMPLOYEE_SQL}
           ORDER BY dm.id DESC
           LIMIT 1"#
    ))
    .bind(company)
    .bind(division_id)
    .bind(user_uuid.to_lowercase())
    .fetch_optional(&mut *conn)
    .await?;
    let Some((id, user_id, role, uuid, name)) = row else {
        return Err(member_not_found().into());
    };
    Ok(LiveMemberRef {
        row: MemberRowRef {
            id,
            user_id,
            role: role_of(&role),
        },
        user: Employee {
            id: user_id,
            uuid,
            name,
        },
    })
}

/// Divisi punya Division Leader: ADA baris LEADER hidup, karyawan aktif, role efektif boleh
/// memimpin. `exclude_row` = baris yang sedang diturunkan/dikeluarkan (D5). Dibaca di bawah lock.
async fn has_division_leader(
    conn: &mut PgConnection,
    company: i32,
    division_id: i32,
    exclude_row: Option<i32>,
) -> TxResult<bool> {
    let leaders: Vec<i32> = sqlx::query_scalar(&format!(
        r#"SELECT dm."userId" FROM "divisionMember" dm
           JOIN users u ON u.id = dm."userId"
           WHERE dm."companyId" = $1 AND dm."divisionId" = $2 AND dm.role::text = 'LEADER'
             AND dm."deletedAt" IS NULL AND {ACTIVE_EMPLOYEE_SQL}
             AND ($3::int IS NULL OR dm.id <> $3)
           ORDER BY dm.id ASC"#
    ))
    .bind(company)
    .bind(division_id)
    .bind(exclude_row)
    .fetch_all(&mut *conn)
    .await?;
    if leaders.is_empty() {
        return Ok(false);
    }
    let codes = role_codes_in_company(&mut *conn, company, &leaders).await?;
    Ok(leaders
        .iter()
        .any(|u| can_lead_division(code_of(&codes, *u))))
}

/// D5 / FR-010: menurunkan atau mengeluarkan Leader hanya bila masih ada Leader LAIN → 422.
async fn assert_another_leader(
    conn: &mut PgConnection,
    company: i32,
    division_id: i32,
    target_row: i32,
    message: &str,
) -> TxResult<()> {
    if !has_division_leader(conn, company, division_id, Some(target_row)).await? {
        return Err(leader_required(message).into());
    }
    Ok(())
}

async fn live_rows_of(
    conn: &mut PgConnection,
    company: i32,
    division_id: i32,
) -> TxResult<Vec<MemberRowRef>> {
    let rows: Vec<(i32, i32, String)> = sqlx::query_as(
        r#"SELECT id, "userId", role::text FROM "divisionMember"
           WHERE "companyId" = $1 AND "divisionId" = $2 AND "deletedAt" IS NULL
           ORDER BY id ASC"#,
    )
    .bind(company)
    .bind(division_id)
    .fetch_all(&mut *conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|(id, user_id, role)| MemberRowRef {
            id,
            user_id,
            role: role_of(&role),
        })
        .collect())
}

/// Diff full replace (murni):
///
/// - user + peran sama → dibiarkan; user tidak ada di `desired` → dicabut;
/// - peran berubah → dicabut, lalu dibuat ulang; baris kembar user yang sama → yang kedua dicabut.
///
/// Baris baru: Leader dulu (urutan id = urutan tampilan audit).
pub fn diff_members(
    current: &[MemberRowRef],
    desired: &[(i32, MemberRole)],
) -> (Vec<i32>, Vec<(i32, MemberRole)>) {
    let mut kept: HashSet<i32> = HashSet::new();
    let mut revoke = Vec::new();
    for row in current {
        let same = desired
            .iter()
            .any(|(u, r)| *u == row.user_id && *r == row.role);
        if same && !kept.contains(&row.user_id) {
            kept.insert(row.user_id);
        } else {
            revoke.push(row.id);
        }
    }
    let added: Vec<(i32, MemberRole)> = desired
        .iter()
        .filter(|(u, _)| !kept.contains(u))
        .copied()
        .collect();
    let create = added
        .iter()
        .filter(|(_, r)| *r == MemberRole::Leader)
        .chain(added.iter().filter(|(_, r)| *r != MemberRole::Leader))
        .copied()
        .collect();
    (revoke, create)
}

/// `updateMany({ where: { id: { in }, divisionId, deletedAt: null }, data: { deletedAt } })`
/// atas `divisionMember` (audit keanggotaan: anggota keluar). Kosong → tanpa query.
async fn revoke_rows(
    conn: &mut PgConnection,
    audit: &AuditHandle,
    division_id: i32,
    ids: &[i32],
) -> TxResult<()> {
    if ids.is_empty() {
        return Ok(());
    }
    let lookup = {
        let ids = ids.to_vec();
        Lookup::filter(move |qb| {
            qb.push("id = ANY(").push_bind(ids.clone());
            qb.push(r#") AND "divisionId" = "#).push_bind(division_id);
            qb.push(r#" AND "deletedAt" IS NULL"#);
        })
    };
    let pre = audit
        .membership_pre(
            Model::DivisionMember,
            MemberEffect::Remove,
            Touched::Where(lookup),
        )
        .await;
    sqlx::query(
        r#"UPDATE "divisionMember" SET "deletedAt" = $3
           WHERE id = ANY($1) AND "divisionId" = $2 AND "deletedAt" IS NULL"#,
    )
    .bind(ids)
    .bind(division_id)
    .bind(now_naive())
    .execute(&mut *conn)
    .await?;
    audit.record_membership(pre).await;
    Ok(())
}

/// `createMany` baris anggota baru (audit keanggotaan: anggota masuk). Kosong → tanpa query.
async fn create_rows(
    conn: &mut PgConnection,
    audit: &AuditHandle,
    company: i32,
    division_id: i32,
    actor: i32,
    rows: &[(i32, MemberRole)],
) -> TxResult<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let data: Vec<Row> = rows
        .iter()
        .map(|(uid, role)| {
            Row::new()
                .with("companyId", company)
                .with("divisionId", division_id)
                .with("userId", *uid)
                .with("role", role.as_str())
                .with("createdById", actor)
        })
        .collect();
    let pre = audit
        .membership_pre(
            Model::DivisionMember,
            MemberEffect::Add,
            Touched::Rows(data),
        )
        .await;
    let mut qb = QueryBuilder::<Postgres>::new(
        r#"INSERT INTO "divisionMember" ("companyId", "divisionId", "userId", role, "createdById") "#,
    );
    qb.push_values(rows.iter(), |mut b, (uid, role)| {
        b.push_bind(company)
            .push_bind(division_id)
            .push_bind(*uid)
            .push_bind(role.as_str())
            .push_unseparated(r#"::"DivisionMemberRole""#)
            .push_bind(actor);
    });
    qb.build().execute(&mut *conn).await?;
    audit.record_membership(pre).await;
    Ok(())
}

/// Ganti peran = cabut + buat ulang. SEMUA pencabutan sebelum pembuatan apa pun (T9).
async fn replace_rows(
    conn: &mut PgConnection,
    audit: &AuditHandle,
    company: i32,
    division_id: i32,
    actor: i32,
    revoke: &[i32],
    create: &[(i32, MemberRole)],
) -> TxResult<()> {
    revoke_rows(conn, audit, division_id, revoke).await?;
    create_rows(conn, audit, company, division_id, actor, create).await
}

/// Naikkan `version` + `updatedAt` (T3), sekaligus nama/deskripsi bila diberikan
/// (`division.update({ data, select: { id: true } })`).
async fn bump_version(
    conn: &mut PgConnection,
    audit: &AuditHandle,
    division_id: i32,
    name: Option<&str>,
    description: &Field<String>,
) -> TxResult<()> {
    let pre = audit
        .pre_read(Model::Division, Lookup::Id(division_id))
        .await;
    let mut qb = QueryBuilder::<Postgres>::new("UPDATE division SET ");
    if let Some(n) = name {
        qb.push("name = ").push_bind(n.to_owned()).push(", ");
    }
    match description {
        Field::Absent => {}
        Field::Null => {
            qb.push("description = NULL, ");
        }
        Field::Val(d) => {
            qb.push("description = ").push_bind(d.clone()).push(", ");
        }
    }
    qb.push(r#"version = version + 1, "updatedAt" = "#)
        .push_bind(now_naive())
        .push(" WHERE id = ")
        .push_bind(division_id);
    qb.build().execute(&mut *conn).await?;
    audit
        .record_update(Model::Division, pre, &Row::new().with("id", division_id))
        .await;
    Ok(())
}

// ───────────────────────── service ─────────────────────────

fn members_or_crash(m: &Members) -> DivResult<&[MemberInput]> {
    match m {
        Members::List(l) => Ok(l),
        // `null.map(...)` → TypeError di Node → 500.
        _ => Err(internal("members is not iterable")),
    }
}

/// Node: src/divisions/divisions.service.ts:186 `create`.
pub async fn create(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    dto: &CreateInput,
) -> DivResult<DetailResponse> {
    let company = open_company(state, user_id).await?;
    let uuid = create_tx(state, audit, user_id, company, dto)
        .await
        .map_err(into_div)?;
    // Di luar transaksi: detail memuat angka waktu (query laporan).
    get_detail(state, user_id, &uuid).await
}

async fn create_tx(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    company: i32,
    dto: &CreateInput,
) -> TxResult<String> {
    // Transaksi rollback tidak boleh meninggalkan baris audit.
    let discard = audit.discard_guard();
    let mut tx = state.db.begin().await?;
    // 1. Lock dulu (T4).
    lock_divisions_company(&mut tx, company).await?;
    // 2. Validasi: tidak ada yang ditulis sebelum semuanya lolos.
    assert_name_free(&mut tx, company, &dto.name, None).await?;
    let norm = normalize_members(members_or_crash(&dto.members)?)?;
    let uuids: Vec<String> = norm.members.iter().map(|(u, _)| u.clone()).collect();
    let employees = resolve_active_employees(&mut tx, company, &uuids).await?;
    let leaders: Vec<Employee> = norm
        .leader_uuids
        .iter()
        .filter_map(|u| employees.get(u).cloned())
        .collect();
    assert_leaders_eligible(&mut tx, company, &leaders).await?;

    // 3. Divisinya (`create` dengan `select: { id, uuid }`).
    let (division_id, division_uuid): (i32, String) = sqlx::query_as(
        r#"INSERT INTO division ("companyId", name, description, version, "createdById")
           VALUES ($1, $2, $3, 0, $4) RETURNING id, uuid::text"#,
    )
    .bind(company)
    .bind(&dto.name)
    .bind(&dto.description)
    .bind(user_id)
    .fetch_one(&mut *tx)
    .await?;
    audit
        .record_create(
            Model::Division,
            &Row::new()
                .with("id", division_id)
                .with("uuid", division_uuid.as_str()),
        )
        .await;

    // 4. Anggotanya, Leader lebih dulu (urutan id = urutan tampilan audit).
    let resolved: Vec<(i32, MemberRole)> = norm
        .members
        .iter()
        .filter_map(|(u, r)| employees.get(u).map(|e| (e.id, *r)))
        .collect();
    let ordered: Vec<(i32, MemberRole)> = resolved
        .iter()
        .filter(|(_, r)| *r == MemberRole::Leader)
        .chain(resolved.iter().filter(|(_, r)| *r != MemberRole::Leader))
        .copied()
        .collect();
    create_rows(&mut tx, audit, company, division_id, user_id, &ordered).await?;

    tx.commit().await?;
    discard.disarm();
    Ok(division_uuid)
}

/// Node: src/divisions/divisions.service.ts:806 `update`.
pub async fn update(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    division_uuid: &str,
    dto: &UpdateInput,
) -> DivResult<DetailResponse> {
    let company = open_company(state, user_id).await?;
    // Uuid dari luar tidak dipercaya: bukan uuid → 404.
    if !is_uuid(division_uuid) {
        return Err(division_not_found());
    }
    let uuid = update_tx(state, audit, user_id, company, division_uuid, dto)
        .await
        .map_err(into_div)?;
    get_detail(state, user_id, &uuid).await
}

async fn update_tx(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    company: i32,
    division_uuid: &str,
    dto: &UpdateInput,
) -> TxResult<String> {
    let discard = audit.discard_guard();
    let mut tx = state.db.begin().await?;
    // 1. Lock + baca ulang + cek version (T3).
    let division = lock_and_load(&mut tx, company, division_uuid, dto.version).await?;

    // 2. Nama (T4). Nama identik tidak dicek; beda huruf saja tetap dicek (dirinya dikecualikan).
    let new_name: Option<&str> = match &dto.name {
        Field::Absent => None,
        // `assertNameFree(null)` → TypeError di Node → 500.
        Field::Null => return Err(internal("name is null").into()),
        Field::Val(n) => Some(n.as_str()),
    };
    if let Some(n) = new_name
        && n != division.name
    {
        assert_name_free(&mut tx, company, n, Some(division.id)).await?;
    }

    match &dto.members {
        Members::Absent => {
            // 3. FR-008: menyimpan divisi tanpa Division Leader ditolak (dibaca di bawah lock).
            if !has_division_leader(&mut tx, company, division.id, None).await? {
                return Err(leader_required(
                    "This division has no Leader; choose a Leader before saving",
                )
                .into());
            }
        }
        members => {
            // 4. Validasi persis seperti create.
            let norm = normalize_members(members_or_crash(members)?)?;
            let uuids: Vec<String> = norm.members.iter().map(|(u, _)| u.clone()).collect();
            let employees = resolve_active_employees(&mut tx, company, &uuids).await?;
            let leaders: Vec<Employee> = norm
                .leader_uuids
                .iter()
                .filter_map(|u| employees.get(u).cloned())
                .collect();
            assert_leaders_eligible(&mut tx, company, &leaders).await?;

            // 5. Diff terhadap baris hidup divisi ini (status user APA PUN). Baris orang yang
            //    sama di divisi lain tidak disentuh (A1 D6).
            let desired: Vec<(i32, MemberRole)> = norm
                .members
                .iter()
                .filter_map(|(u, r)| employees.get(u).map(|e| (e.id, *r)))
                .collect();
            let current = live_rows_of(&mut tx, company, division.id).await?;
            let (revoke, create) = diff_members(&current, &desired);
            replace_rows(
                &mut tx,
                audit,
                company,
                division.id,
                user_id,
                &revoke,
                &create,
            )
            .await?;
        }
    }

    // 6. Nama/deskripsi + version (selalu naik bila berhasil).
    bump_version(&mut tx, audit, division.id, new_name, &dto.description).await?;

    tx.commit().await?;
    discard.disarm();
    Ok(division.uuid)
}

/// Node: src/divisions/divisions.service.ts:918 `setMemberRole`.
pub async fn set_member_role(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    division_uuid: &str,
    member_uuid: &str,
    role: MemberRole,
    version: i64,
) -> DivResult<DetailResponse> {
    let company = open_company(state, user_id).await?;
    if !is_uuid(division_uuid) {
        return Err(division_not_found());
    }
    if !is_uuid(member_uuid) {
        return Err(member_not_found());
    }
    let uuid = set_role_tx(
        state,
        audit,
        user_id,
        company,
        division_uuid,
        member_uuid,
        role,
        version,
    )
    .await
    .map_err(into_div)?;
    get_detail(state, user_id, &uuid).await
}

#[allow(clippy::too_many_arguments)]
async fn set_role_tx(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    company: i32,
    division_uuid: &str,
    member_uuid: &str,
    role: MemberRole,
    version: i64,
) -> TxResult<String> {
    let discard = audit.discard_guard();
    let mut tx = state.db.begin().await?;
    let division = lock_and_load(&mut tx, company, division_uuid, version).await?;
    let target = find_live_member(&mut tx, company, division.id, member_uuid).await?;

    // No-op terdokumentasi: tidak ada tulis, tidak ada kenaikan version.
    if target.row.role == role {
        tx.commit().await?;
        discard.disarm();
        return Ok(division.uuid);
    }

    if role == MemberRole::Member {
        // D5 / T2′: dihitung di bawah lock, target dikecualikan.
        assert_another_leader(
            &mut tx,
            company,
            division.id,
            target.row.id,
            "Appoint another Leader before demoting the last one",
        )
        .await?;
    } else {
        // D4: promosi. FR-033 sebelum menulis apa pun.
        assert_leaders_eligible(&mut tx, company, std::slice::from_ref(&target.user)).await?;
    }

    replace_rows(
        &mut tx,
        audit,
        company,
        division.id,
        user_id,
        &[target.row.id],
        &[(target.row.user_id, role)],
    )
    .await?;
    bump_version(&mut tx, audit, division.id, None, &Field::Absent).await?;

    tx.commit().await?;
    discard.disarm();
    Ok(division.uuid)
}

/// Node: src/divisions/divisions.service.ts:984 `removeMember`.
pub async fn remove_member(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    division_uuid: &str,
    member_uuid: &str,
    version: i64,
) -> DivResult<DetailResponse> {
    let company = open_company(state, user_id).await?;
    if !is_uuid(division_uuid) {
        return Err(division_not_found());
    }
    if !is_uuid(member_uuid) {
        return Err(member_not_found());
    }
    let uuid = remove_member_tx(state, audit, company, division_uuid, member_uuid, version)
        .await
        .map_err(into_div)?;
    get_detail(state, user_id, &uuid).await
}

async fn remove_member_tx(
    state: &AppState,
    audit: &AuditHandle,
    company: i32,
    division_uuid: &str,
    member_uuid: &str,
    version: i64,
) -> TxResult<String> {
    let discard = audit.discard_guard();
    let mut tx = state.db.begin().await?;
    let division = lock_and_load(&mut tx, company, division_uuid, version).await?;
    let target = find_live_member(&mut tx, company, division.id, member_uuid).await?;
    if target.row.role == MemberRole::Leader {
        // FR-010 / D5 / T2′.
        assert_another_leader(
            &mut tx,
            company,
            division.id,
            target.row.id,
            "Appoint another Leader before removing the last one",
        )
        .await?;
    }
    revoke_rows(&mut tx, audit, division.id, &[target.row.id]).await?;
    bump_version(&mut tx, audit, division.id, None, &Field::Absent).await?;
    tx.commit().await?;
    discard.disarm();
    Ok(division.uuid)
}

/// Node: src/divisions/divisions.service.ts:1034 `remove`.
pub async fn remove(
    state: &AppState,
    audit: &AuditHandle,
    user_id: i32,
    division_uuid: &str,
    version: i64,
) -> DivResult<DeleteResponse> {
    let company = open_company(state, user_id).await?;
    if !is_uuid(division_uuid) {
        return Err(division_not_found());
    }
    remove_tx(state, audit, company, division_uuid, version)
        .await
        .map_err(into_div)
}

type RemoveRow = (i32, i32, String, String, String, i32, bool);

async fn remove_tx(
    state: &AppState,
    audit: &AuditHandle,
    company: i32,
    division_uuid: &str,
    version: i64,
) -> TxResult<DeleteResponse> {
    let discard = audit.discard_guard();
    let mut tx = state.db.begin().await?;
    let division = lock_and_load(&mut tx, company, division_uuid, version).await?;

    // Anggota dibaca SEKALI di dalam transaksi (status user apa pun).
    let rows: Vec<RemoveRow> = sqlx::query_as(
        r#"SELECT dm.id, dm."userId", dm.role::text, u.uuid::text, u.name, u.status,
                  EXISTS (SELECT 1 FROM "userCompany" uc
                          WHERE uc."userId" = u.id AND uc."companyId" = $1 AND uc.status = 1)
           FROM "divisionMember" dm
           JOIN users u ON u.id = dm."userId"
           WHERE dm."companyId" = $1 AND dm."divisionId" = $2 AND dm."deletedAt" IS NULL
           ORDER BY dm.id ASC"#,
    )
    .bind(company)
    .bind(division.id)
    .fetch_all(&mut *tx)
    .await?;
    let active_rows: Vec<&RemoveRow> = rows.iter().filter(|r| r.5 == ACTIVE && r.6).collect();
    // Baris kembar user yang sama (hanya mungkin di dev tanpa partial index).
    let removed_members = active_rows
        .iter()
        .map(|r| r.1)
        .collect::<HashSet<i32>>()
        .len();

    let leader_rows: Vec<&&RemoveRow> = active_rows.iter().filter(|r| r.2 == "LEADER").collect();
    let leader_ids: Vec<i32> = leader_rows.iter().map(|r| r.1).collect();
    let codes = role_codes_in_company(&mut *tx, company, &leader_ids).await?;
    let mut seen: HashSet<i32> = HashSet::new();
    let mut leader_list: Vec<&RemoveRow> = leader_rows
        .iter()
        .map(|r| **r)
        .filter(|r| seen.insert(r.1) && can_lead_division(code_of(&codes, r.1)))
        .collect();
    leader_list.sort_by(|a, b| locale_compare(&a.4, &b.4).then(a.1.cmp(&b.1)));
    let leaders: Vec<Person> = leader_list
        .iter()
        .map(|r| Person::new(&r.3, &r.4))
        .collect();

    // Anggota dulu (semua baris hidup, status user apa pun), lalu divisinya.
    let all_ids: Vec<i32> = rows.iter().map(|r| r.0).collect();
    revoke_rows(&mut tx, audit, division.id, &all_ids).await?;
    let pre = audit
        .pre_read(Model::Division, Lookup::Id(division.id))
        .await;
    let now = now_naive();
    sqlx::query(
        r#"UPDATE division SET "deletedAt" = $2, version = version + 1, "updatedAt" = $2
           WHERE id = $1"#,
    )
    .bind(division.id)
    .bind(now)
    .execute(&mut *tx)
    .await?;
    audit
        .record_update(Model::Division, pre, &Row::new().with("id", division.id))
        .await;
    tx.commit().await?;
    discard.disarm();

    Ok(DeleteResponse {
        data: DeleteEntity {
            uuid: division.uuid,
            removed_members,
            leader: leaders.first().cloned(),
            leaders,
        },
    })
}

// ───────────────────────── handler ─────────────────────────

/// Node: src/divisions/divisions.controller.ts:139 `create`
pub async fn h_create(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    ValidatedJson(body): ValidatedJson<CreateBody>,
) -> DivResult<(StatusCode, Json<DetailResponse>)> {
    let dto = parse_create(&body.0).map_err(ApiError::Validation)?;
    let res = create(&state, &audit, ctx.require_user_id()?, &dto).await?;
    Ok((StatusCode::CREATED, Json(res)))
}

/// Node: src/divisions/divisions.controller.ts:176 `update`
pub async fn h_update(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    UuidParam(uuid): UuidParam,
    ValidatedJson(body): ValidatedJson<UpdateBody>,
) -> DivResult<Json<DetailResponse>> {
    let dto = parse_update(&body.0).map_err(ApiError::Validation)?;
    Ok(Json(
        update(&state, &audit, ctx.require_user_id()?, &uuid, &dto).await?,
    ))
}

/// Node: src/divisions/divisions.controller.ts:213 `setMemberRole`
pub async fn h_set_member_role(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    UuidPair(uuid, user_uuid): UuidPair,
    ValidatedJson(body): ValidatedJson<RoleBody>,
) -> DivResult<Json<DetailResponse>> {
    let dto = parse_role(&body.0).map_err(ApiError::Validation)?;
    Ok(Json(
        set_member_role(
            &state,
            &audit,
            ctx.require_user_id()?,
            &uuid,
            &user_uuid,
            dto.role,
            dto.version,
        )
        .await?,
    ))
}

/// Node: src/divisions/divisions.controller.ts:245 `removeMember`
pub async fn h_remove_member(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    UuidPair(uuid, user_uuid): UuidPair,
    ValidatedQuery(q): ValidatedQuery<VersionQuery>,
) -> DivResult<Json<DetailResponse>> {
    let version = parse_version_query(&q.0).map_err(ApiError::Validation)?;
    Ok(Json(
        remove_member(
            &state,
            &audit,
            ctx.require_user_id()?,
            &uuid,
            &user_uuid,
            version,
        )
        .await?,
    ))
}

/// Node: src/divisions/divisions.controller.ts:277 `remove`
pub async fn h_remove(
    State(state): State<AppState>,
    ctx: RequestCtx,
    audit: AuditHandle,
    UuidParam(uuid): UuidParam,
    ValidatedQuery(q): ValidatedQuery<VersionQuery>,
) -> DivResult<Json<DeleteResponse>> {
    let version = parse_version_query(&q.0).map_err(ApiError::Validation)?;
    Ok(Json(
        remove(&state, &audit, ctx.require_user_id()?, &uuid, version).await?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "3f2504e0-4f89-41d3-9a0c-0305e82c3301";
    const B: &str = "9b1deb4d-3b7d-4bad-9bdd-2b0d7b3dcb6d";

    fn m(u: &str, role: MemberRole) -> MemberInput {
        MemberInput {
            user_uuid: u.into(),
            role,
        }
    }

    fn body_of(e: DivError) -> (u16, String) {
        match e {
            DivError::Body { status, body } => (status.as_u16(), body.to_string()),
            other => (other.status().as_u16(), String::new()),
        }
    }

    #[test]
    fn normalize_lowercases_and_keeps_order() {
        let upper = A.to_uppercase();
        let n = normalize_members(&[m(&upper, MemberRole::Leader), m(B, MemberRole::Member)]);
        let Ok(n) = n else {
            return assert!(n.is_ok());
        };
        assert_eq!(n.members[0].0, A);
        assert_eq!(n.leader_uuids, vec![A.to_owned()]);
    }

    #[test]
    fn duplicates_come_before_leader_check_and_keep_key_order() {
        // Dua baris sama sebagai LEADER + MEMBER: duplikat menang atas hitungan Leader.
        let r = normalize_members(&[
            m(A, MemberRole::Leader),
            m(&A.to_uppercase(), MemberRole::Member),
        ]);
        let (status, body) = body_of(r.err().unwrap_or_else(|| internal("x")));
        assert_eq!(status, 422);
        assert_eq!(
            body,
            format!(
                r#"{{"code":"DIVISION_DUPLICATE_MEMBER","message":"The same employee is listed more than once","details":{{"userUuids":["{A}"]}}}}"#
            )
        );
    }

    #[test]
    fn leader_required_when_no_leader() {
        let r = normalize_members(&[m(A, MemberRole::Member)]);
        let (status, _) = body_of(r.err().unwrap_or_else(|| internal("x")));
        assert_eq!(status, 422);
        assert!(normalize_members(&[]).is_err());
    }

    fn row(id: i32, user_id: i32, role: MemberRole) -> MemberRowRef {
        MemberRowRef { id, user_id, role }
    }

    #[test]
    fn diff_keeps_same_revokes_missing_and_recreates_role_changes() {
        use MemberRole::{Leader, Member};
        let current = [row(1, 10, Leader), row(2, 11, Member), row(3, 12, Member)];
        // 10 tetap Leader; 11 naik jadi Leader; 12 hilang; 13 baru Member; 14 baru Leader.
        let desired = [(10, Leader), (11, Leader), (13, Member), (14, Leader)];
        let (revoke, create) = diff_members(&current, &desired);
        assert_eq!(revoke, vec![2, 3]);
        // Leader baru dulu, lalu Member (urutan id = urutan tampilan).
        assert_eq!(create, vec![(11, Leader), (14, Leader), (13, Member)]);
    }

    #[test]
    fn diff_revokes_duplicate_live_rows_of_same_user() {
        use MemberRole::Leader;
        let current = [row(1, 10, Leader), row(2, 10, Leader)];
        let (revoke, create) = diff_members(&current, &[(10, Leader)]);
        assert_eq!(revoke, vec![2]);
        assert!(create.is_empty());
    }

    #[test]
    fn diff_no_change_is_empty() {
        use MemberRole::{Leader, Member};
        let current = [row(1, 10, Leader), row(2, 11, Member)];
        let (revoke, create) = diff_members(&current, &[(11, Member), (10, Leader)]);
        assert!(revoke.is_empty() && create.is_empty());
    }
}
