//! Baris `store` + relasi (`group`, `customer`), serializer admin/mobile, dan resolver
//! `groupId`/`customerId` yang dipakai jalur admin & mobile (`stores.service.ts`,
//! `mobile-stores.service.ts`). `pub` — dipakai juga modul lain (orders, offline sync).

use axum::http::StatusCode;
use mz_core::jsfmt::num;
use mz_core::time::JsDate;
use mz_core::{ApiError, ApiResult};
use serde_json::{Value, json};
use sqlx::PgExecutor;

/// Satu baris `store` + ringkasan relasi `group`/`customer` (`STORE_INCLUDE`).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct StoreRow {
    pub id: i32,
    pub uuid: String,
    #[sqlx(rename = "companyId")]
    pub company_id: i32,
    pub code: String,
    pub name: String,
    #[sqlx(rename = "ownerName")]
    pub owner_name: String,
    pub phone: Option<String>,
    pub address: String,
    pub lat: f64,
    pub lng: f64,
    #[sqlx(rename = "radiusMeters")]
    pub radius_meters: Option<i32>,
    #[sqlx(rename = "qrToken")]
    pub qr_token: Option<String>,
    #[sqlx(rename = "isActive")]
    pub is_active: bool,
    #[sqlx(rename = "groupId")]
    pub group_id: Option<i32>,
    #[sqlx(rename = "customerId")]
    pub customer_id: Option<i32>,
    #[sqlx(rename = "totalVisits")]
    pub total_visits: i32,
    #[sqlx(rename = "lastVisitAt")]
    pub last_visit_at: Option<JsDate>,
    #[sqlx(rename = "lastVisitStatus")]
    pub last_visit_status: Option<String>,
    #[sqlx(rename = "createdAt")]
    pub created_at: JsDate,
    #[sqlx(rename = "updatedAt")]
    pub updated_at: Option<JsDate>,
    pub group_uuid: Option<String>,
    pub group_code: Option<String>,
    pub group_name: Option<String>,
    pub customer_uuid: Option<String>,
    pub customer_code: Option<String>,
    pub customer_name: Option<String>,
}

/// `SELECT ... FROM store s LEFT JOIN "storeGroup" g LEFT JOIN customer c` (tanpa `WHERE`).
/// Relasi `customer` Prisma tidak menyaring `deletedAt` — LEFT JOIN polos juga tidak.
pub const STORE_SELECT: &str = r#"SELECT s.id, s.uuid::text AS uuid, s."companyId", s.code, s.name, s."ownerName",
       s.phone, s.address, s.lat, s.lng, s."radiusMeters", s."qrToken", s."isActive", s."groupId",
       s."customerId", s."totalVisits", s."lastVisitAt", s."lastVisitStatus"::text AS "lastVisitStatus",
       s."createdAt", s."updatedAt",
       g.uuid::text AS group_uuid, g.code AS group_code, g.name AS group_name,
       c.uuid::text AS customer_uuid, c.code AS customer_code, c.name AS customer_name
  FROM store s
  LEFT JOIN "storeGroup" g ON g.id = s."groupId"
  LEFT JOIN customer c ON c.id = s."customerId""#;

fn ms(d: Option<JsDate>) -> Value {
    d.map_or(Value::Null, |d| Value::from(d.timestamp_millis()))
}

impl StoreRow {
    /// `toAdminStore` (qrToken/companyId/deletedAt tidak pernah keluar).
    pub fn to_admin(&self) -> Value {
        let group = match (&self.group_uuid, &self.group_code, &self.group_name) {
            (Some(id), Some(code), Some(name)) => json!({ "id": id, "code": code, "name": name }),
            _ => Value::Null,
        };
        let customer = match (
            &self.customer_uuid,
            &self.customer_code,
            &self.customer_name,
        ) {
            (Some(id), Some(code), Some(name)) => json!({ "id": id, "code": code, "name": name }),
            _ => Value::Null,
        };
        json!({
            "id": self.uuid,
            "code": self.code,
            "name": self.name,
            "ownerName": self.owner_name,
            "phone": self.phone,
            "address": self.address,
            "lat": num(self.lat),
            "lng": num(self.lng),
            "radiusMeters": self.radius_meters,
            "isActive": self.is_active,
            "group": group,
            "customer": customer,
            "totalVisits": self.total_visits,
            "lastVisitAt": ms(self.last_visit_at),
            "lastVisitStatus": self.last_visit_status,
            "createdAt": self.created_at.timestamp_millis(),
            "updatedAt": ms(self.updated_at),
        })
    }

    /// `toAdminStoreDetail`: `toAdminStore` + `qrToken` (hanya detail/create/update admin).
    pub fn to_admin_detail(&self) -> Value {
        let mut v = self.to_admin();
        if let Some(o) = v.as_object_mut() {
            o.insert("qrToken".into(), json!(self.qr_token));
        }
        v
    }

    /// `toMobileStore(store, distanceMeters)` (§4: region & customer hanya `{ id, name }`).
    pub fn to_mobile(&self, distance_meters: Option<i64>) -> Value {
        let group = match (&self.group_uuid, &self.group_name) {
            (Some(id), Some(name)) => json!({ "id": id, "name": name }),
            _ => Value::Null,
        };
        let customer = match (&self.customer_uuid, &self.customer_name) {
            (Some(id), Some(name)) => json!({ "id": id, "name": name }),
            _ => Value::Null,
        };
        json!({
            "id": self.uuid,
            "code": self.code,
            "name": self.name,
            "ownerName": self.owner_name,
            "phone": self.phone,
            "address": self.address,
            "lat": num(self.lat),
            "lng": num(self.lng),
            "distanceMeters": distance_meters,
            "group": group,
            "customer": customer,
            "lastVisitAt": ms(self.last_visit_at),
        })
    }
}

/// Baca satu toko lewat [`STORE_SELECT`] + `WHERE s.id = $1`.
pub async fn store_by_id<'e>(exec: impl PgExecutor<'e>, id: i32) -> ApiResult<StoreRow> {
    Ok(sqlx::query_as(&format!("{STORE_SELECT} WHERE s.id = $1"))
        .bind(id)
        .fetch_one(exec)
        .await?)
}

/// 404 `STORE_NOT_FOUND`.
pub fn store_not_found() -> ApiError {
    ApiError::coded(StatusCode::NOT_FOUND, "STORE_NOT_FOUND", "Store not found")
}

/// 404 `STORE_GROUP_NOT_FOUND`.
pub fn group_not_found() -> ApiError {
    ApiError::coded(
        StatusCode::NOT_FOUND,
        "STORE_GROUP_NOT_FOUND",
        "Store group not found",
    )
}

/// 422 `STORE_GROUP_INACTIVE`.
pub fn group_inactive() -> ApiError {
    ApiError::coded(
        StatusCode::UNPROCESSABLE_ENTITY,
        "STORE_GROUP_INACTIVE",
        "Cannot assign a store to an archived store group",
    )
}

/// Kolom `@db.Uuid` dengan string non-UUID: Prisma melempar P2023 → 500 di Node.
pub fn prisma_uuid(raw: &str) -> ApiResult<uuid::Uuid> {
    mz_core::prisma::parse_uuid(raw).map_err(|m| ApiError::internal(anyhow::anyhow!("{m}")))
}

/// `storeGroup.findFirst({ uuid, companyId }, select { id, isActive })`.
pub async fn find_group<'e>(
    exec: impl PgExecutor<'e>,
    company_id: i32,
    group_uuid: &str,
) -> ApiResult<Option<(i32, bool)>> {
    Ok(sqlx::query_as(
        r#"SELECT id, "isActive" FROM "storeGroup" WHERE uuid = $1 AND "companyId" = $2 LIMIT 1"#,
    )
    .bind(prisma_uuid(group_uuid)?)
    .bind(company_id)
    .fetch_optional(exec)
    .await?)
}

/// `resolveActiveGroupId` (§4.2): 404 `STORE_GROUP_NOT_FOUND`, 422 `STORE_GROUP_INACTIVE`.
pub async fn resolve_active_group_id<'e>(
    exec: impl PgExecutor<'e>,
    company_id: i32,
    group_uuid: &str,
) -> ApiResult<i32> {
    match find_group(exec, company_id, group_uuid).await? {
        None => Err(group_not_found()),
        Some((_, false)) => Err(group_inactive()),
        Some((id, true)) => Ok(id),
    }
}

/// `resolveActiveCustomerId` (docs/customers.md §4.4): customer hidup company ini; 404
/// `CUSTOMER_NOT_FOUND`, 422 `CUSTOMER_ARCHIVED`.
pub async fn resolve_active_customer_id<'e>(
    exec: impl PgExecutor<'e>,
    company_id: i32,
    customer_uuid: &str,
) -> ApiResult<i32> {
    let row: Option<(i32, bool)> = sqlx::query_as(
        r#"SELECT id, "isActive" FROM customer
            WHERE uuid = $1 AND "companyId" = $2 AND "deletedAt" IS NULL LIMIT 1"#,
    )
    .bind(prisma_uuid(customer_uuid)?)
    .bind(company_id)
    .fetch_optional(exec)
    .await?;
    match row {
        None => Err(ApiError::coded(
            StatusCode::NOT_FOUND,
            "CUSTOMER_NOT_FOUND",
            "Customer not found",
        )),
        Some((_, false)) => Err(ApiError::coded(
            StatusCode::UNPROCESSABLE_ENTITY,
            "CUSTOMER_ARCHIVED",
            "Cannot assign a store to an archived customer",
        )),
        Some((id, true)) => Ok(id),
    }
}

/// `randomBytes(32).toString('hex')` (§7.3): 64 karakter hex, tidak diturunkan dari `code`.
pub fn generate_qr_token() -> String {
    use rand::RngCore;
    let mut buf = [0u8; 32];
    rand::rng().fill_bytes(&mut buf);
    buf.iter().map(|b| format!("{b:02x}")).collect()
}

/// `isQrTokenConflict`: unique violation pada constraint ber-nama `qrToken`.
pub fn is_qr_token_conflict(e: &sqlx::Error) -> bool {
    match e {
        sqlx::Error::Database(db) => db
            .constraint()
            .is_some_and(|c| c.to_lowercase().contains("qrtoken")),
        _ => false,
    }
}

/// `censoredColumns(req, 'stores')` / interceptor `@TableFields('stores', { path: 'data' })`.
pub const TABLE_KEY: &str = "stores";

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub fn row() -> StoreRow {
        StoreRow {
            id: 1,
            uuid: "s-uuid".into(),
            company_id: 9,
            code: "MJ-001".into(),
            name: "Toko".into(),
            owner_name: "Pak".into(),
            phone: None,
            address: "Jl. Satu Dua".into(),
            lat: -7.5,
            lng: 110.0,
            radius_meters: None,
            qr_token: Some("q".repeat(64)),
            is_active: true,
            group_id: Some(3),
            customer_id: None,
            total_visits: 0,
            last_visit_at: None,
            last_visit_status: None,
            created_at: JsDate::from_millis(1000).unwrap(),
            updated_at: None,
            group_uuid: Some("g".into()),
            group_code: Some("JOG".into()),
            group_name: Some("Jogja".into()),
            customer_uuid: None,
            customer_code: None,
            customer_name: None,
        }
    }

    #[test]
    fn admin_serializer_hides_internal_columns() {
        let v = row().to_admin();
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            vec![
                "id",
                "code",
                "name",
                "ownerName",
                "phone",
                "address",
                "lat",
                "lng",
                "radiusMeters",
                "isActive",
                "group",
                "customer",
                "totalVisits",
                "lastVisitAt",
                "lastVisitStatus",
                "createdAt",
                "updatedAt"
            ]
        );
        assert_eq!(
            v["group"],
            json!({ "id": "g", "code": "JOG", "name": "Jogja" })
        );
        assert_eq!(v["customer"], Value::Null);
        assert_eq!(v["lng"], json!(110));
        let d = row().to_admin_detail();
        assert_eq!(d["qrToken"], json!("q".repeat(64)));
        assert!(v.get("qrToken").is_none() && v.get("companyId").is_none());
    }

    #[test]
    fn mobile_serializer_exposes_only_mobile_fields() {
        let mut r = row();
        r.customer_uuid = Some("c".into());
        r.customer_code = Some("C1".into());
        r.customer_name = Some("PT".into());
        let v = r.to_mobile(Some(12));
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            vec![
                "id",
                "code",
                "name",
                "ownerName",
                "phone",
                "address",
                "lat",
                "lng",
                "distanceMeters",
                "group",
                "customer",
                "lastVisitAt"
            ]
        );
        assert_eq!(v["group"], json!({ "id": "g", "name": "Jogja" }));
        assert_eq!(v["customer"], json!({ "id": "c", "name": "PT" }));
    }

    #[test]
    fn qr_tokens_are_random_hex() {
        let a = generate_qr_token();
        let b = generate_qr_token();
        assert_eq!(a.len(), 64);
        assert!(
            a.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        assert_ne!(a, b);
    }
}
