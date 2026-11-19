//! Port `src/visit-masters/default-visit-masters.ts` — data bawaan visit type & frequency.

use mz_core::ApiResult;
use sqlx::PgConnection;

pub struct DefaultVisitType {
    pub code: &'static str,
    pub name: &'static str,
    pub sort_order: i32,
    pub requires_order: bool,
    pub requires_photo: bool,
    pub requires_gps: bool,
    pub is_default: bool,
}

const fn visit_type(
    code: &'static str,
    name: &'static str,
    sort_order: i32,
    requires_order: bool,
    requires_photo: bool,
    is_default: bool,
) -> DefaultVisitType {
    DefaultVisitType {
        code,
        name,
        sort_order,
        requires_order,
        requires_photo,
        requires_gps: true,
        is_default,
    }
}

pub const DEFAULT_VISIT_TYPES: &[DefaultVisitType] = &[
    visit_type("REGULAR_VISIT", "Kunjungan Rutin", 10, false, false, true),
    visit_type("TAKING_ORDER", "Ambil Order", 20, true, false, false),
    visit_type("COLLECTION", "Penagihan", 30, false, false, false),
    visit_type("MERCHANDISING", "Merchandising", 40, false, true, false),
    visit_type("SURVEY", "Survei", 50, false, false, false),
    visit_type("COMPLAINT", "Penanganan Keluhan", 60, false, false, false),
    visit_type("CANVASSING", "Canvassing", 70, false, false, false),
];

/// Tipe yang lahir dengan `isDefault: true` — HARUS ada di daftar di atas.
pub const DEFAULT_VISIT_TYPE_CODE: &str = "REGULAR_VISIT";

pub struct DefaultVisitFrequency {
    pub code: &'static str,
    pub name: &'static str,
    pub sort_order: i32,
    pub interval_days: i32,
}

pub const DEFAULT_VISIT_FREQUENCIES: &[DefaultVisitFrequency] = &[
    DefaultVisitFrequency {
        code: "DAILY",
        name: "Harian",
        sort_order: 10,
        interval_days: 1,
    },
    DefaultVisitFrequency {
        code: "WEEKLY",
        name: "Mingguan",
        sort_order: 20,
        interval_days: 7,
    },
    DefaultVisitFrequency {
        code: "BIWEEKLY",
        name: "Dua Mingguan",
        sort_order: 30,
        interval_days: 14,
    },
    DefaultVisitFrequency {
        code: "MONTHLY",
        name: "Bulanan",
        sort_order: 40,
        interval_days: 30,
    },
];

/// `seedDefaultVisitTypes` -> jumlah baris tertulis (`skipDuplicates` = `ON CONFLICT DO NOTHING`).
pub async fn seed_default_visit_types(
    conn: &mut PgConnection,
    company_id: i32,
    created_by_id: i32,
) -> ApiResult<u64> {
    let t = DEFAULT_VISIT_TYPES;
    let codes: Vec<&str> = t.iter().map(|x| x.code).collect();
    let names: Vec<&str> = t.iter().map(|x| x.name).collect();
    let sort: Vec<i32> = t.iter().map(|x| x.sort_order).collect();
    let order: Vec<bool> = t.iter().map(|x| x.requires_order).collect();
    let photo: Vec<bool> = t.iter().map(|x| x.requires_photo).collect();
    let gps: Vec<bool> = t.iter().map(|x| x.requires_gps).collect();
    let def: Vec<bool> = t.iter().map(|x| x.is_default).collect();
    let res = sqlx::query(
        r#"INSERT INTO "visitType"
             ("companyId", code, name, "sortOrder", "createdById", "requiresOrder", "requiresPhoto", "requiresGps", "isDefault")
           SELECT $1, x.code, x.name, x.sort_order, $2, x.req_order, x.req_photo, x.req_gps, x.is_default
           FROM UNNEST($3::text[], $4::text[], $5::int[], $6::bool[], $7::bool[], $8::bool[], $9::bool[])
                AS x(code, name, sort_order, req_order, req_photo, req_gps, is_default)
           ON CONFLICT DO NOTHING"#,
    )
    .bind(company_id)
    .bind(created_by_id)
    .bind(&codes)
    .bind(&names)
    .bind(&sort)
    .bind(&order)
    .bind(&photo)
    .bind(&gps)
    .bind(&def)
    .execute(&mut *conn)
    .await?;
    Ok(res.rows_affected())
}

/// `seedDefaultVisitFrequencies` -> jumlah baris tertulis.
pub async fn seed_default_visit_frequencies(
    conn: &mut PgConnection,
    company_id: i32,
    created_by_id: i32,
) -> ApiResult<u64> {
    let f = DEFAULT_VISIT_FREQUENCIES;
    let codes: Vec<&str> = f.iter().map(|x| x.code).collect();
    let names: Vec<&str> = f.iter().map(|x| x.name).collect();
    let sort: Vec<i32> = f.iter().map(|x| x.sort_order).collect();
    let days: Vec<i32> = f.iter().map(|x| x.interval_days).collect();
    let res = sqlx::query(
        r#"INSERT INTO "visitFrequency" ("companyId", code, name, "sortOrder", "intervalDays", "createdById")
           SELECT $1, x.code, x.name, x.sort_order, x.interval_days, $2
           FROM UNNEST($3::text[], $4::text[], $5::int[], $6::int[]) AS x(code, name, sort_order, interval_days)
           ON CONFLICT DO NOTHING"#,
    )
    .bind(company_id)
    .bind(created_by_id)
    .bind(&codes)
    .bind(&names)
    .bind(&sort)
    .bind(&days)
    .execute(&mut *conn)
    .await?;
    Ok(res.rows_affected())
}

/// `seedDefaultVisitMasters` -> `(types, frequencies)`. Entry point registrasi: menyemai keduanya.
pub async fn seed_default_visit_masters(
    conn: &mut PgConnection,
    company_id: i32,
    created_by_id: i32,
) -> ApiResult<(u64, u64)> {
    let types = seed_default_visit_types(conn, company_id, created_by_id).await?;
    let frequencies = seed_default_visit_frequencies(conn, company_id, created_by_id).await?;
    Ok((types, frequencies))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exactly_one_default_and_it_exists() {
        let defaults: Vec<_> = DEFAULT_VISIT_TYPES
            .iter()
            .filter(|t| t.is_default)
            .collect();
        assert_eq!(defaults.len(), 1);
        assert_eq!(
            defaults.first().map(|t| t.code),
            Some(DEFAULT_VISIT_TYPE_CODE)
        );
        assert_eq!(DEFAULT_VISIT_TYPES.len(), 7);
        assert_eq!(DEFAULT_VISIT_FREQUENCIES.len(), 4);
    }
}
