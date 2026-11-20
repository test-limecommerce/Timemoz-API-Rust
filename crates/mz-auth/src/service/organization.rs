//! `AuthService.createOrganization` (`auth.service.ts:1760`) +
//! `QuickstartService.initForCompany`.

use mz_core::feature_modules::{flags_to_modules, invalidate_company};
use mz_core::{ApiResult, AppState};
use serde_json::{Value, json};
use sqlx::PgConnection;

use crate::analytics::{OrganizationCreated, spawn_organization_created};
use crate::dto::CreateOrganizationDto;
use crate::service::register::error_message;

/// `QuickstartService.initForCompany(tx, companyId)`.
pub async fn init_quickstart(conn: &mut PgConnection, company_id: i32) -> ApiResult<()> {
    sqlx::query(
        r#"INSERT INTO "companyQuickstart" ("companyId", "inviteTeamAt", "reminderEligible", "updatedAt")
           VALUES (
             $1,
             CASE WHEN (SELECT COUNT(*) FROM "userCompany" WHERE "companyId" = $1) > 1 THEN NOW() END,
             true,
             NOW()
           )
           ON CONFLICT ("companyId") DO NOTHING"#,
    )
    .bind(company_id)
    .execute(conn)
    .await?;
    Ok(())
}

#[derive(sqlx::FromRow)]
struct Updated {
    id: i32,
    uuid: String,
    name: String,
    #[sqlx(rename = "websiteURL")]
    website_url: Option<String>,
    register_total_employees: Option<String>,
    #[sqlx(rename = "hasCreateOrgnanization")]
    has_create_organization: bool,
    #[sqlx(rename = "moduleTimeTrackingEnabled")]
    module_time_tracking_enabled: bool,
    #[sqlx(rename = "moduleSalesTrackerEnabled")]
    module_sales_tracker_enabled: bool,
}

/// Nilai kolom opsional: `None` = tidak dikirim (kolom tidak disentuh),
/// `Some(None)` = `null` eksplisit.
fn column(v: &Option<Value>) -> Option<Option<String>> {
    match v {
        None => None,
        Some(Value::Null) => Some(None),
        Some(other) => Some(other.as_str().map(str::to_owned)),
    }
}

/// `{ success:false, message }` untuk semua galat (HTTP 200), sama dengan blok `catch` Node.
pub async fn create_organization(
    state: &AppState,
    audit: &mz_audit::AuditHandle,
    user_id: i32,
    dto: &CreateOrganizationDto,
) -> Value {
    match create_organization_inner(state, audit, user_id, dto).await {
        Ok(v) => v,
        Err(err) => {
            let message = error_message(&err);
            json!({
                "success": false,
                "message": if message.is_empty() { "Failed to create organization".to_owned() } else { message },
            })
        }
    }
}

async fn create_organization_inner(
    state: &AppState,
    audit: &mz_audit::AuditHandle,
    user_id: i32,
    dto: &CreateOrganizationDto,
) -> ApiResult<Value> {
    let Some(company_id) = mz_core::company::find_company_id(&state.db, user_id).await? else {
        return Ok(json!({ "success": false, "message": "Company not found" }));
    };
    let modules = dto.modules();
    let flags = modules
        .as_ref()
        .map(|m| (m.contains(&"TIME_TRACKING"), m.contains(&"SALES_TRACKER")));
    let website = column(&dto.website_url);
    let employees = column(&dto.register_total_employees);
    let time_zone = column(&dto.time_zone);

    let mut tx = state.db.begin().await?;
    // `FOR UPDATE`: request kedua menunggu commit yang pertama lalu membaca `true`.
    let locked: Option<bool> = sqlx::query_scalar(
        r#"SELECT "hasCreateOrgnanization" FROM company WHERE id = $1 FOR UPDATE"#,
    )
    .bind(company_id)
    .fetch_optional(&mut *tx)
    .await?;
    let is_first = locked == Some(false);

    // Kolom yang tidak dikirim (`undefined` di Prisma) tidak disentuh.
    let mut qb = sqlx::QueryBuilder::<sqlx::Postgres>::new("UPDATE company SET name = ");
    qb.push_bind(crate::dto::text(&dto.name).to_owned());
    qb.push(r#", "hasCreateOrgnanization" = true"#);
    if let Some(v) = website {
        qb.push(r#", "websiteURL" = "#).push_bind(v);
    }
    if let Some(v) = employees {
        qb.push(", register_total_employees = ").push_bind(v);
    }
    // Zona waktu hanya saat pembuatan pertama.
    if is_first && let Some(v) = time_zone {
        qb.push(r#", "timeZone" = "#).push_bind(v);
    }
    // Sama dengan `timeZone`: hanya pembuatan pertama. Endpoint ini tanpa role guard,
    // jadi tanpa ini STAFF/GUEST mana pun bisa mematikan modul seluruh company dengan
    // memanggil ulang onboarding; setelahnya modul diubah lewat endpoint fitur company.
    if is_first && let Some((tt, st)) = flags {
        qb.push(r#", "moduleTimeTrackingEnabled" = "#).push_bind(tt);
        qb.push(r#", "moduleSalesTrackerEnabled" = "#).push_bind(st);
    }
    qb.push(" WHERE id = ").push_bind(company_id);
    qb.push(
        r#" RETURNING id, uuid::text AS "uuid", name, "websiteURL", register_total_employees,
            "hasCreateOrgnanization", "moduleTimeTrackingEnabled", "moduleSalesTrackerEnabled""#,
    );
    // Audit `company.update` ber-`select`: pra-baca semua kolom, `after` = kolom `select` saja
    // (kolom lain yang tidak null ikut terbaca berubah, persis seperti Node).
    let pre = audit
        .pre_read(mz_audit::Model::Company, mz_audit::Lookup::Id(company_id))
        .await;
    let updated = qb
        .build_query_as::<Updated>()
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| {
            mz_core::ApiError::internal(anyhow::anyhow!("Record to update not found."))
        })?;
    audit
        .record_update(
            mz_audit::Model::Company,
            pre,
            &mz_audit::Row::new()
                .with("id", updated.id)
                .with("uuid", updated.uuid.as_str())
                .with("name", updated.name.as_str())
                .with("websiteURL", updated.website_url.clone())
                .with(
                    "register_total_employees",
                    updated.register_total_employees.clone(),
                )
                .with("hasCreateOrgnanization", updated.has_create_organization)
                .with(
                    "moduleTimeTrackingEnabled",
                    updated.module_time_tracking_enabled,
                )
                .with(
                    "moduleSalesTrackerEnabled",
                    updated.module_sales_tracker_enabled,
                ),
        )
        .await;

    if is_first
        && modules
            .as_ref()
            .is_some_and(|m| m.contains(&"SALES_TRACKER"))
    {
        init_quickstart(&mut tx, company_id).await?;
    }
    tx.commit().await?;

    if is_first && modules.is_some() {
        invalidate_company(state, company_id);
    }
    let module_list: Vec<&'static str> = flags_to_modules(
        updated.module_time_tracking_enabled,
        updated.module_sales_tracker_enabled,
    )
    .iter()
    .map(|m| m.as_str())
    .collect();

    if is_first {
        spawn_organization_created(
            state,
            user_id,
            OrganizationCreated {
                uuid: updated.uuid.clone(),
                name: updated.name.clone(),
                website_url: updated.website_url.clone(),
                register_total_employees: updated.register_total_employees.clone(),
                modules: module_list.clone(),
            },
        );
    }

    Ok(json!({
        "success": true,
        "message": "Organization created successfully",
        "data": {
            "id": updated.id,
            "uuid": updated.uuid,
            "name": updated.name,
            "websiteURL": updated.website_url,
            "register_total_employees": updated.register_total_employees,
            "hasCreateOrgnanization": updated.has_create_organization,
            "modules": module_list,
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn optional_columns() {
        assert_eq!(column(&None), None);
        assert_eq!(column(&Some(Value::Null)), Some(None));
        assert_eq!(column(&Some(json!("x"))), Some(Some("x".into())));
    }
}
