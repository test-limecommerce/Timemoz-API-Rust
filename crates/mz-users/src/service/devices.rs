//! `UsersService`: `revokeUserConnected` (:1347), `findConnectedDevice` (:2244),
//! `revokeConnectedDevice` (:2276).

use mz_core::jwt::blacklist_token;
use mz_core::time::JsDate;
use mz_core::{ApiResult, AppState};
use serde_json::{Value, json};

#[derive(sqlx::FromRow)]
struct DeviceRow {
    id: i32,
    status: i32,
    os: String,
    #[sqlx(rename = "macAddress")]
    mac_address: String,
    #[sqlx(rename = "connectedAt")]
    connected_at: JsDate,
    #[sqlx(rename = "lastConnectedAt")]
    last_connected_at: JsDate,
}

/// `DELETE /users/revoke/:macAddress` -> `usersService.revokeUserConnected`.
/// Satu transaksi: device diputus dan token yang dipakai logout ditolak bersama-sama.
pub async fn revoke_user_connected(
    state: &AppState,
    user_id: i32,
    mac_address: &str,
    sid: Option<&str>,
    raw_token: &str,
) -> ApiResult<Value> {
    let now = JsDate::now();
    let mut tx = state.db.begin().await?;
    // Dengan `sid`, logout tetap kena walau MAC device sudah berubah sejak login.
    // (`sid::text`: sid dari klaim JWT tidak boleh membuat cast uuid gagal.)
    sqlx::query(
        r#"UPDATE "userConnected" SET status = 0, "lastConnectedAt" = $2
           WHERE "userId" = $1 AND ("macAddress" = $3 OR ($4::text IS NOT NULL AND sid::text = $4::text))"#,
    )
    .bind(user_id)
    .bind(now)
    .bind(mac_address)
    .bind(sid)
    .execute(&mut *tx)
    .await?;
    blacklist_token(&mut tx, raw_token, user_id).await?;
    tx.commit().await?;

    // Sesudah commit: invalidasi lebih awal bisa ditimpa load yang berjalan sebelum blacklist terlihat.
    state.invalidate_token_revocation(user_id);

    Ok(json!({ "success": true, "message": "revoke user connected success" }))
}

/// `GET /users/connected/devices` -> `{ success, data }`; galat DB menjadi `200 { success: false }`.
pub async fn find_connected_device(state: &AppState, user_id: i32) -> ApiResult<Value> {
    let rows = sqlx::query_as::<_, DeviceRow>(
        r#"SELECT id, status, os, "macAddress", "connectedAt", "lastConnectedAt"
           FROM "userConnected" WHERE "userId" = $1 AND status = 1 ORDER BY id"#,
    )
    .bind(user_id)
    .fetch_all(&state.db)
    .await;
    Ok(match rows {
        Ok(rows) => json!({
            "success": true,
            "data": rows.iter().map(|r| json!({
                "id": r.id,
                "status": r.status,
                "os": r.os,
                "macAddress": r.mac_address,
                "connectedAt": r.connected_at,
                "lastConnectedAt": r.last_connected_at,
            })).collect::<Vec<_>>(),
        }),
        Err(err) => {
            tracing::error!(error = %err, "findConnectedDevice gagal");
            json!({ "success": false, "message": "Cannot get connected device", "error": {} })
        }
    })
}

/// `DELETE /users/connected/devices/revoke/:id` — parameter `id` diabaikan Node: SEMUA device
/// aktif user diputus (`_id` tak dipakai).
pub async fn revoke_connected_device(state: &AppState, user_id: i32) -> ApiResult<Value> {
    let done =
        sqlx::query(r#"UPDATE "userConnected" SET status = 0 WHERE "userId" = $1 AND status = 1"#)
            .bind(user_id)
            .execute(&state.db)
            .await;
    Ok(match done {
        Ok(_) => json!({ "success": true, "message": "Device has been revoke successfully" }),
        Err(err) => {
            tracing::error!(error = %err, "revokeConnectedDevice gagal");
            json!({ "success": false, "message": "Cannot revoke connected device", "error": {} })
        }
    })
}
