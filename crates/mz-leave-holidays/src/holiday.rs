//! Baris `companyHoliday` (semua kolom skalar, urutan = `schema.prisma`) + akses SQL.

use chrono::{NaiveDate, NaiveDateTime, NaiveTime};
use mz_core::time::JsDate;
use serde::{Serialize, Serializer};
use sqlx::PgConnection;

/// `companyHoliday` apa adanya (hasil Prisma tanpa `select`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, sqlx::FromRow)]
#[serde(rename_all = "camelCase")]
pub struct HolidayRow {
    pub id: i32,
    #[sqlx(rename = "companyId")]
    pub company_id: i32,
    pub name: String,
    #[serde(serialize_with = "ser_date")]
    pub date: NaiveDate,
    #[sqlx(rename = "type")]
    #[serde(rename = "type")]
    pub ty: String,
    #[sqlx(rename = "isActive")]
    pub is_active: bool,
    #[sqlx(rename = "deductsLeave")]
    pub deducts_leave: bool,
    #[sqlx(rename = "parentHolidayId")]
    pub parent_holiday_id: Option<i32>,
    #[sqlx(rename = "createdAt")]
    #[serde(serialize_with = "ser_ts")]
    pub created_at: NaiveDateTime,
    #[sqlx(rename = "updatedAt")]
    #[serde(serialize_with = "ser_opt_ts")]
    pub updated_at: Option<NaiveDateTime>,
}

/// `include: { jointLeaves: { orderBy: { date: 'asc' } } }` / `{ ...parent, jointLeaves }`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HolidayWithJoint {
    #[serde(flatten)]
    pub holiday: HolidayRow,
    pub joint_leaves: Vec<HolidayRow>,
}

/// Kolom `companyHoliday` (enum dibaca `::text`).
pub const COLS: &str = r#"id, "companyId", name, date, type::text AS "type", "isActive", "deductsLeave", "parentHolidayId", "createdAt", "updatedAt""#;

fn ser_date<S: Serializer>(d: &NaiveDate, s: S) -> Result<S::Ok, S::Error> {
    JsDate(d.and_time(NaiveTime::MIN)).serialize(s)
}

fn ser_ts<S: Serializer>(d: &NaiveDateTime, s: S) -> Result<S::Ok, S::Error> {
    JsDate(*d).serialize(s)
}

fn ser_opt_ts<S: Serializer>(d: &Option<NaiveDateTime>, s: S) -> Result<S::Ok, S::Error> {
    match d {
        Some(d) => JsDate(*d).serialize(s),
        None => s.serialize_none(),
    }
}

/// `toDateKey(date)`.
pub fn date_key(d: NaiveDate) -> String {
    d.format("%Y-%m-%d").to_string()
}

/// `sortByDate`: urut menaik menurut tanggal, stabil.
pub fn sort_by_date(mut rows: Vec<HolidayRow>) -> Vec<HolidayRow> {
    rows.sort_by_key(|r| r.date);
    rows
}

/// `findMany`/`findFirst` dengan `JOINT_LEAVES_INCLUDE`: cuti bersama tiap induk (urut tanggal).
pub async fn attach_joint_leaves(
    conn: &mut PgConnection,
    rows: Vec<HolidayRow>,
) -> sqlx::Result<Vec<HolidayWithJoint>> {
    let ids: Vec<i32> = rows.iter().map(|r| r.id).collect();
    let children: Vec<HolidayRow> = if ids.is_empty() {
        Vec::new()
    } else {
        sqlx::query_as(&format!(
            r#"SELECT {COLS} FROM "companyHoliday" WHERE "parentHolidayId" = ANY($1) ORDER BY date ASC, id ASC"#
        ))
        .bind(&ids)
        .fetch_all(&mut *conn)
        .await?
    };
    Ok(rows
        .into_iter()
        .map(|holiday| {
            let joint_leaves = children
                .iter()
                .filter(|c| c.parent_holiday_id == Some(holiday.id))
                .cloned()
                .collect();
            HolidayWithJoint {
                holiday,
                joint_leaves,
            }
        })
        .collect())
}

/// `findFirst({ where: { id, companyId }, include: JOINT_LEAVES_INCLUDE })`.
pub async fn find_with_joint(
    conn: &mut PgConnection,
    id: i32,
    company_id: i32,
) -> sqlx::Result<Option<HolidayWithJoint>> {
    let row: Option<HolidayRow> = sqlx::query_as(&format!(
        r#"SELECT {COLS} FROM "companyHoliday" WHERE id = $1 AND "companyId" = $2 LIMIT 1"#
    ))
    .bind(id)
    .bind(company_id)
    .fetch_optional(&mut *conn)
    .await?;
    match row {
        None => Ok(None),
        Some(row) => Ok(attach_joint_leaves(conn, vec![row]).await?.pop()),
    }
}

/// Nama cuti bersama: `Cuti Bersama ${name}`.slice(0, 200) (unit UTF-16, seperti JS).
pub fn joint_leave_name(holiday_name: &str) -> String {
    let units: Vec<u16> = format!("Cuti Bersama {holiday_name}")
        .encode_utf16()
        .take(200)
        .collect();
    String::from_utf16_lossy(&units)
}

/// Nilai `INSERT` satu baris `companyHoliday` (induk atau cuti bersama).
pub struct NewHoliday<'a> {
    pub company_id: i32,
    pub parent_holiday_id: Option<i32>,
    pub name: &'a str,
    pub date: NaiveDate,
    pub ty: &'a str,
    /// `None` = default kolom (`true`).
    pub is_active: Option<bool>,
    pub deducts_leave: bool,
}

/// `tx.companyHoliday.create({ data })` → baris hasil.
pub async fn insert_row(conn: &mut PgConnection, n: &NewHoliday<'_>) -> sqlx::Result<HolidayRow> {
    sqlx::query_as(&format!(
        r#"INSERT INTO "companyHoliday"
              ("companyId", "parentHolidayId", name, date, type, "isActive", "deductsLeave")
           VALUES ($1, $2, $3, $4, $5::"HolidayType", COALESCE($6, true), $7)
           RETURNING {COLS}"#
    ))
    .bind(n.company_id)
    .bind(n.parent_holiday_id)
    .bind(n.name)
    .bind(n.date)
    .bind(n.ty)
    .bind(n.is_active)
    .bind(n.deducts_leave)
    .fetch_one(&mut *conn)
    .await
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn nama_cuti_bersama_dipotong_200_unit_utf16() {
        assert_eq!(joint_leave_name("Idul Fitri"), "Cuti Bersama Idul Fitri");
        let long = "x".repeat(200);
        let out = joint_leave_name(&long);
        assert_eq!(out.encode_utf16().count(), 200);
        assert!(out.starts_with("Cuti Bersama xxx"));
    }

    #[test]
    fn serialisasi_urutan_kunci_dan_tanggal() {
        let d = NaiveDate::from_ymd_opt(2026, 8, 17).unwrap();
        let ts = d.and_hms_milli_opt(1, 2, 3, 4).unwrap();
        let row = HolidayRow {
            id: 1,
            company_id: 5,
            name: "Hari".into(),
            date: d,
            ty: "NATIONAL_HOLIDAY".into(),
            is_active: true,
            deducts_leave: false,
            parent_holiday_id: None,
            created_at: ts,
            updated_at: None,
        };
        let v = serde_json::to_value(HolidayWithJoint {
            holiday: row,
            joint_leaves: vec![],
        })
        .unwrap();
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "id",
                "companyId",
                "name",
                "date",
                "type",
                "isActive",
                "deductsLeave",
                "parentHolidayId",
                "createdAt",
                "updatedAt",
                "jointLeaves"
            ]
        );
        assert_eq!(v["date"], "2026-08-17T00:00:00.000Z");
        assert_eq!(v["createdAt"], "2026-08-17T01:02:03.004Z");
        assert!(v["updatedAt"].is_null());
    }
}
