//! Endpoint publik `SettingsService.getLatestVersion/getLatestVersionMobile/getProhibitedApps/
//! getPricingLists`. Semuanya `@HttpCode(200)`; galat DB menjadi `{ success:false, ... }`.

use axum::Json;
use axum::extract::State;
use mz_core::time::JsDate;
use mz_core::{ApiResult, AppState};
use mz_files::release_url::release_download_url;
use mz_int_common::envelope::Envelope;
use mz_int_common::jsonb;
use serde::{Serialize, Serializer};
use serde_json::value::RawValue;

use crate::dto::{MobileQuery, PricingQuery};
use mz_core::extract::ValidatedQuery;

/// Float Prisma → angka JSON gaya JS (`100`, bukan `100.0`).
fn js_f64<S: Serializer>(v: &f64, s: S) -> Result<S::Ok, S::Error> {
    if v.is_finite() && v.fract() == 0.0 && v.abs() < 9.0e15 {
        #[allow(clippy::cast_possible_truncation)]
        return s.serialize_i64(*v as i64);
    }
    s.serialize_f64(*v)
}

#[derive(Debug, sqlx::FromRow)]
struct LatestVersion {
    id: i32,
    version: String,
    mac_url: String,
    windows_url: String,
    linux_url: String,
}

#[derive(sqlx::FromRow)]
struct ReleaseRow {
    platform: String,
    version: String,
    file_name: String,
}

#[derive(Serialize)]
pub struct DownloadUrl {
    id: i32,
    version: String,
    mac_url: String,
    windows_url: String,
    linux_url: String,
}

#[derive(Serialize)]
pub struct Versions {
    mac: String,
    windows: String,
    linux: String,
}

#[derive(Serialize)]
pub struct LatestOk {
    success: bool,
    message: &'static str,
    data: String,
    versions: Versions,
    download_url: DownloadUrl,
}

#[derive(Serialize)]
#[serde(untagged)]
pub enum LatestOut {
    Ok(LatestOk),
    Fail(Envelope<()>),
}

async fn load_latest(state: &AppState) -> anyhow::Result<Option<LatestOk>> {
    let data: Option<LatestVersion> = sqlx::query_as(
        r#"SELECT id, version, mac_url, windows_url, linux_url FROM "latestVersion"
           ORDER BY id LIMIT 1"#,
    )
    .fetch_optional(&state.db)
    .await?;
    // `data.version` pada `null` = TypeError → catch → success:false.
    let Some(data) = data else { return Ok(None) };
    let releases: Vec<ReleaseRow> = sqlx::query_as(
        r#"SELECT platform::text AS platform, version, "fileName" AS file_name
             FROM "appRelease" WHERE channel = 'LATEST' AND "isActive"
            ORDER BY "releaseDate" DESC, id DESC"#,
    )
    .fetch_all(&state.db)
    .await?;
    // Versi & URL tiap platform dari release LATEST aktif terbaru platform itu.
    let latest_for = |platform: &str, fallback: &str| {
        let release = releases.iter().find(|r| r.platform == platform);
        (
            release.map_or_else(|| data.version.clone(), |r| r.version.clone()),
            release
                .and_then(|r| release_download_url(state, &r.platform, &r.version, &r.file_name))
                .filter(|u| !u.is_empty())
                .unwrap_or_else(|| fallback.to_owned()),
        )
    };
    let mac = latest_for("MAC", &data.mac_url);
    let windows = latest_for("WINDOWS", &data.windows_url);
    let linux = latest_for("LINUX", &data.linux_url);
    Ok(Some(LatestOk {
        success: true,
        message: "Success get latest version",
        data: data.version.clone(),
        versions: Versions {
            mac: mac.0,
            windows: windows.0,
            linux: linux.0,
        },
        download_url: DownloadUrl {
            id: data.id,
            version: data.version,
            mac_url: mac.1,
            windows_url: windows.1,
            linux_url: linux.1,
        },
    }))
}

/// Node: src/settings/settings.controller.ts:117 (`GET /settings/latest-version`, 200)
pub async fn latest_version(State(state): State<AppState>) -> Json<LatestOut> {
    match load_latest(&state).await {
        Ok(Some(ok)) => Json(LatestOut::Ok(ok)),
        Ok(None) => Json(LatestOut::Fail(Envelope::fail("Failed get latest version"))),
        Err(err) => {
            tracing::warn!(error = %err, "latest-version gagal");
            Json(LatestOut::Fail(Envelope::fail("Failed get latest version")))
        }
    }
}

#[derive(Serialize)]
pub struct MobileData {
    version: String,
    #[serde(rename = "type")]
    kind: String,
    force: bool,
}

/// Node: src/settings/settings.controller.ts:123 (`GET /settings/latest-version-mobile`, 200)
pub async fn latest_version_mobile(
    State(state): State<AppState>,
    ValidatedQuery(q): ValidatedQuery<MobileQuery>,
) -> Json<Envelope<MobileData>> {
    let row: sqlx::Result<Option<(String, String, bool)>> = sqlx::query_as(
        r#"SELECT version, type::text, "force" FROM "latestVersionMobile" WHERE type = $1::"LatestVersionMobileType""#,
    )
    .bind(q.kind())
    .fetch_optional(&state.db)
    .await;
    Json(match row {
        Ok(Some((version, kind, force))) => Envelope::ok(
            "Success get latest version mobile",
            MobileData {
                version,
                kind,
                force,
            },
        ),
        Ok(None) => Envelope::fail("Latest version not found"),
        Err(err) => {
            tracing::warn!(error = %err, "latest-version-mobile gagal");
            Envelope::fail("Failed get latest version mobile")
        }
    })
}

#[derive(sqlx::FromRow, Serialize)]
pub struct ProhibitedApp {
    id: i32,
    uuid: String,
    #[serde(rename = "appName")]
    app_name: String,
    platform: String,
    #[serde(rename = "createdAt")]
    created_at: JsDate,
    #[serde(rename = "updatedAt")]
    updated_at: Option<JsDate>,
}

/// Node: src/settings/settings.controller.ts:129 (`GET /settings/prohibited-apps`, 200)
pub async fn prohibited_apps(State(state): State<AppState>) -> Json<Envelope<Vec<ProhibitedApp>>> {
    let rows: sqlx::Result<Vec<ProhibitedApp>> = sqlx::query_as(
        r#"SELECT id, uuid::text AS uuid, "appName" AS app_name, platform,
                  "createdAt" AS created_at, "updatedAt" AS updated_at
             FROM "prohibitedApps" ORDER BY id ASC"#,
    )
    .fetch_all(&state.db)
    .await;
    Json(match rows {
        Ok(data) => Envelope::ok("Success get prohibited apps", data),
        Err(err) => {
            tracing::warn!(error = %err, "prohibited-apps gagal");
            Envelope::fail("Failed get prohibited apps")
        }
    })
}

#[derive(sqlx::FromRow)]
struct PricingRow {
    uuid: String,
    name: String,
    base_price: f64,
    extra_user_price: f64,
    min_users: i32,
    max_users: i32,
    product_type: String,
    list_features: Option<String>,
}

#[derive(Serialize)]
pub struct Pricing {
    uuid: String,
    name: String,
    #[serde(rename = "basePrice", serialize_with = "js_f64")]
    base_price: f64,
    #[serde(rename = "extraUserPrice", serialize_with = "js_f64")]
    extra_user_price: f64,
    #[serde(rename = "minUsers")]
    min_users: i32,
    #[serde(rename = "maxUsers")]
    max_users: i32,
    #[serde(rename = "productType")]
    product_type: String,
    #[serde(rename = "listFeatures")]
    list_features: Option<Box<RawValue>>,
}

/// Node: src/settings/settings.controller.ts:136 (`GET /settings/pricing-lists`, 200)
pub async fn pricing_lists(
    State(state): State<AppState>,
    ValidatedQuery(q): ValidatedQuery<PricingQuery>,
) -> ApiResult<Json<Envelope<Vec<Pricing>>>> {
    let rows: sqlx::Result<Vec<PricingRow>> = sqlx::query_as(
        r#"SELECT uuid::text AS uuid, name, "basePrice" AS base_price,
                  "extraUserPrice" AS extra_user_price, "minUsers" AS min_users,
                  "maxUsers" AS max_users, "productType"::text AS product_type,
                  "listFeatures"::text AS list_features
             FROM "pricingTiers"
            WHERE status = 1 AND ($1::text IS NULL OR "productType" = $1::"PricingProductType")
            ORDER BY "basePrice" DESC"#,
    )
    .bind(q.product())
    .fetch_all(&state.db)
    .await;
    Ok(Json(match rows {
        Ok(rows) => Envelope::ok(
            "success",
            rows.into_iter()
                .map(|r| Pricing {
                    uuid: r.uuid,
                    name: r.name,
                    base_price: r.base_price,
                    extra_user_price: r.extra_user_price,
                    min_users: r.min_users,
                    max_users: r.max_users,
                    product_type: r.product_type,
                    list_features: jsonb::raw_opt(r.list_features.as_deref()),
                })
                .collect(),
        ),
        Err(err) => {
            tracing::warn!(error = %err, "pricing-lists gagal");
            Envelope::fail("Failed to get pricing lists")
        }
    }))
}
