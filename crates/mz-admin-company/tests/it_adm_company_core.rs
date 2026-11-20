//! Uji integrasi admin-company (controller lama, features, lifecycle, ownership, plan-override,
//! provisioning) terhadap Postgres nyata. Email tidak terkirim (tanpa `MAILTRAP_TOKEN`).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

mod common;

use common::{build, company, company_audit_by_operator, member, op_audit, operator, uniq};
use mz_testkit::TestApp;
use serde_json::{Value, json};
use tokio::sync::Mutex;

static SERIAL: Mutex<()> = Mutex::const_new(());

const FREE_TIME_TIER: i32 = 6;
const PAID_TIER: i32 = 1;
const BAD: &str = "00000000-0000-4000-8000-000000000000";

#[tokio::test]
async fn auth_gates_and_roles() {
    let _g = SERIAL.lock().await;
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let user = app.company().await;
    let (_, support) = operator(&app, "SUPPORT").await;
    let (_, finance) = operator(&app, "FINANCE").await;
    let fx = company(&app, "ACTIVE", "PAYPAL", PAID_TIER).await;
    let u = &fx.uuid;

    let routes: Vec<(&str, String)> = vec![
        ("GET", "/admin-company".into()),
        ("GET", format!("/admin-company/{u}")),
        ("GET", format!("/admin-company/user/{u}")),
        ("PATCH", format!("/admin-company/set/trial/{u}")),
        ("GET", format!("/admin-company/{u}/features")),
        ("POST", format!("/admin-company/{u}/delete")),
        ("POST", "/admin/invite-codes/fill-missing".into()),
        ("GET", format!("/admin-company/{u}/subscription")),
        ("POST", format!("/admin-company/{u}/transfer-owner")),
        ("PUT", format!("/admin-company/{u}/plan-override")),
        ("POST", "/admin-company".into()),
        ("POST", format!("/admin-company/{u}/users")),
    ];
    for (method, path) in &routes {
        let m: http::Method = method.parse().unwrap();
        app.request(m.clone(), path).send().await.assert_status(401);
        let r = app
            .request(m, path)
            .token(&user.super_admin.token)
            .send()
            .await;
        r.assert_status(401);
        assert_eq!(
            r.json,
            json!({ "message": "Unauthorized", "statusCode": 401 })
        );
    }

    // Role salah → 403 sebelum validasi body/param.
    for (method, path, token) in [
        ("POST", format!("/admin-company/{u}/delete"), &support),
        (
            "POST",
            "/admin/invite-codes/fill-missing".to_owned(),
            &finance,
        ),
        ("PUT", format!("/admin-company/{u}/plan-override"), &support),
        ("POST", "/admin-company".to_owned(), &finance),
        (
            "POST",
            "/admin-company/not-a-uuid/users".to_owned(),
            &finance,
        ),
        (
            "POST",
            format!("/admin-company/{u}/report-exports/reset"),
            &finance,
        ),
        (
            "POST",
            format!("/admin-company/{u}/subscription/manual/activate"),
            &support,
        ),
    ] {
        let r = app
            .request(method.parse().unwrap(), &path)
            .token(token)
            .json(&json!({}))
            .send()
            .await;
        r.assert_status(403);
        assert_eq!(r.json["code"], "OPERATOR_ROLE_FORBIDDEN", "{path}");
    }

    // JSON rusak = 400 body-parser (sebelum guard).
    let r = app
        .post(&format!("/admin-company/{u}/delete"))
        .raw("application/json", "{bad")
        .send()
        .await;
    r.assert_status(400);
}

#[tokio::test]
async fn list_detail_and_user() {
    let _g = SERIAL.lock().await;
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let (_, owner) = operator(&app, "OWNER").await;
    let fx = company(&app, "ACTIVE", "PAYPAL", PAID_TIER).await;

    let r = app
        .get(&format!("/admin-company?search={}", &fx.name[7..]))
        .token(&owner)
        .send()
        .await;
    r.assert_ok().assert_no_sql_error();
    let data = r.data();
    assert_eq!(data["total"], 1);
    assert_eq!(data["pageNumber"], 1);
    assert_eq!(data["itemsPerPage"], 1);
    let item = &data["result"][0];
    assert_eq!(item["uuid"], fx.uuid);
    assert_eq!(item["totalEmployees"], 1);
    assert_eq!(item["pricingType"], "FULL");
    assert_eq!(item["subscription"]["type"], "PRO");
    assert_eq!(item["subscription"]["status"], "ACTIVE");
    assert!(item["subscription"]["expiredAt"].is_string());
    assert_eq!(item["planBoost"]["SALES"]["enabled"], false);
    let keys: Vec<&String> = item.as_object().unwrap().keys().collect();
    assert_eq!(
        keys,
        vec![
            "uuid",
            "name",
            "telp",
            "timeZone",
            "isAgencyMode",
            "isTrial",
            "status",
            "email",
            "ownerName",
            "totalEmployees",
            "createdAt",
            "pricingType",
            "subscription",
            "planBoost"
        ]
    );

    // Paging + kolom urut tak dikenal.
    let r = app
        .get("/admin-company?pageNumber=1&itemsPerPage=2&orderColumn=bogus&orderBy=asc")
        .token(&owner)
        .send()
        .await;
    r.assert_ok().assert_no_sql_error();
    assert_eq!(r.data()["result"].as_array().unwrap().len(), 2);
    // Validasi PaginatorDto.
    app.get("/admin-company?pageNumber=x")
        .token(&owner)
        .send()
        .await
        .assert_status(400);
    // pageNumber tanpa itemsPerPage → take NaN (500 di Node).
    app.get("/admin-company?pageNumber=1")
        .token(&owner)
        .send()
        .await
        .assert_status(500);

    let r = app
        .get(&format!("/admin-company/{}", fx.uuid))
        .token(&owner)
        .send()
        .await;
    r.assert_ok().assert_no_sql_error();
    let d = r.data();
    assert_eq!(d["id"], fx.id);
    assert_eq!(d["userCompany"][0]["user"]["uuid"], fx.owner_uuid);
    assert_eq!(
        d["subscriptions"][0]["pricingTier"]["basePrice"],
        json!(4.99)
    );
    assert!(
        d["dailyReportSentAt"]
            .as_str()
            .unwrap()
            .starts_with("1970-01-01T")
    );
    assert_eq!(d["planBoost"]["TIME"]["enabled"], false);

    let r = app
        .get(&format!("/admin-company/{BAD}"))
        .token(&owner)
        .send()
        .await;
    assert_eq!(
        r.json,
        json!({ "success": false, "message": format!("Data company by uuid {BAD} not found"), "data": null })
    );
    let r = app.get("/admin-company/xyz").token(&owner).send().await;
    assert_eq!(r.json["message"], "Company not found");

    let r = app
        .get(&format!("/admin-company/user/{}", fx.owner_uuid))
        .token(&owner)
        .send()
        .await;
    r.assert_ok().assert_no_sql_error();
    assert_eq!(r.data()["userCompany"][0]["company"]["name"], fx.name);
    let r = app
        .get("/admin-company/user/xyz")
        .token(&owner)
        .send()
        .await;
    assert_eq!(r.json["message"], "User not found");
    let r = app
        .get(&format!("/admin-company/user/{BAD}"))
        .token(&owner)
        .send()
        .await;
    assert_eq!(r.json["success"], true);
    assert_eq!(r.json["data"], Value::Null);
}

#[tokio::test]
async fn flags_trial_and_interceptor_audit() {
    let _g = SERIAL.lock().await;
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let (op, token) = operator(&app, "SUPPORT").await;
    let fx = company(&app, "TRIAL", "PAYPAL", FREE_TIME_TIER).await;
    let u = &fx.uuid;

    let r = app
        .patch(&format!("/admin-company/set/offline-mode/{u}"))
        .token(&token)
        .json(&json!({ "isSupportOfflineMode": true }))
        .send()
        .await;
    r.assert_ok().assert_no_sql_error();
    assert_eq!(
        r.data(),
        &json!({ "uuid": u, "isSupportOfflineMode": true })
    );
    let r = app
        .patch(&format!("/admin-company/set/activity-track/{u}"))
        .token(&token)
        .json(&json!({ "useActivityTrack": true }))
        .send()
        .await;
    r.assert_ok();
    let flags: (bool, bool) = sqlx::query_as(
        r#"SELECT "isSupportOfflineMode", "useActivityTrack" FROM company WHERE id = $1"#,
    )
    .bind(fx.id)
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!(flags, (true, true));

    // Galat body (pipe global) lebih dulu dari ParseUUIDPipe; body sah → galat uuid.
    let r = app
        .patch("/admin-company/set/offline-mode/xyz")
        .token(&token)
        .json(&json!({}))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(
        r.json["message"],
        json!(["isSupportOfflineMode must be a boolean value"])
    );
    let r = app
        .patch("/admin-company/set/offline-mode/xyz")
        .token(&token)
        .json(&json!({ "isSupportOfflineMode": true }))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(r.json["message"], "Validation failed (uuid is expected)");
    let r = app
        .patch(&format!("/admin-company/set/offline-mode/{u}"))
        .token(&token)
        .json(&json!({ "isSupportOfflineMode": "ya" }))
        .send()
        .await;
    assert_eq!(
        r.json["message"],
        json!(["isSupportOfflineMode must be a boolean value"])
    );
    // Company tidak ada → success:false (tanpa baris audit).
    let r = app
        .patch(&format!("/admin-company/set/offline-mode/{BAD}"))
        .token(&token)
        .json(&json!({ "isSupportOfflineMode": false }))
        .send()
        .await;
    assert_eq!(r.json["success"], false);

    let r = app
        .patch(&format!("/admin-company/set/trial/{u}"))
        .token(&token)
        .json(&json!({ "isTrial": false, "endAt": "2031-01-01T00:00:00.000Z" }))
        .send()
        .await;
    r.assert_ok();
    let end: String = app
        .scalar(&format!(
            r#"SELECT to_char("endDate", 'YYYY-MM-DD') FROM subscriptions WHERE "companyId" = {}"#,
            fx.id
        ))
        .await;
    assert_eq!(end, "2031-01-01");
    // Tanggal tanpa jam ditolak Prisma → catch.
    let r = app
        .patch(&format!("/admin-company/set/trial/{u}"))
        .token(&token)
        .json(&json!({ "isTrial": false, "endAt": "2031-01-01" }))
        .send()
        .await;
    assert_eq!(r.json["message"], "Cannot update company trial");

    let rows = op_audit(&app, op).await;
    let actions: Vec<&str> = rows.iter().map(|r| r[0].as_str().unwrap()).collect();
    assert_eq!(
        actions,
        vec![
            "company.offline_mode",
            "company.activity_track",
            "company.trial"
        ]
    );
    assert_eq!(rows[0][2], json!(u));
    assert_eq!(rows[0][3], json!(fx.id));
    assert_eq!(rows[0][5], json!({ "isSupportOfflineMode": true }));
    assert!(company_audit_by_operator(&app, fx.id).await > 0);
}

#[tokio::test]
async fn plan_boost_toggle() {
    let _g = SERIAL.lock().await;
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let (op, token) = operator(&app, "OWNER").await;
    let fx = company(&app, "TRIAL", "PAYPAL", FREE_TIME_TIER).await;
    app.exec(&format!(
        r#"UPDATE company SET "maxProject" = 3, "dataHistoryMonths" = 3, "reportExportsPerMonth" = 3,
             "screenshotRetentionDays" = 7, "screenshotResolution" = '640x360' WHERE id = {}"#,
        fx.id
    ))
    .await;
    let path = format!("/admin-company/set/plan-boost/{}", fx.uuid);

    let r = app
        .patch(&path)
        .token(&token)
        .json(&json!({ "product": "TIME", "enabled": true }))
        .send()
        .await;
    r.assert_ok().assert_no_sql_error();
    assert_eq!(r.json["message"], "Plan boost TIME enabled successfully");
    assert_eq!(r.data()["boostedValues"]["maxProject"], 100);
    let mp: i32 = app
        .scalar(&format!(
            r#"SELECT "maxProject" FROM company WHERE id = {}"#,
            fx.id
        ))
        .await;
    assert_eq!(mp, 100);

    let r = app
        .patch(&path)
        .token(&token)
        .json(&json!({ "product": "TIME", "enabled": true }))
        .send()
        .await;
    assert_eq!(r.json["message"], "Plan boost TIME already enabled");
    assert_eq!(r.data()["changed"], false);

    let r = app
        .patch(&path)
        .token(&token)
        .json(&json!({ "product": "TIME", "enabled": false }))
        .send()
        .await;
    r.assert_ok();
    assert_eq!(r.data()["restoredValues"]["maxProject"], 3);
    assert_eq!(r.data()["keptFields"], json!([]));
    let mp: i32 = app
        .scalar(&format!(
            r#"SELECT "maxProject" FROM company WHERE id = {}"#,
            fx.id
        ))
        .await;
    assert_eq!(mp, 3);

    let r = app
        .patch(&path)
        .token(&token)
        .json(&json!({ "product": "FULL", "enabled": 1 }))
        .send()
        .await;
    assert_eq!(
        r.json["message"],
        json!([
            "product must be one of the following values: SALES, TIME",
            "enabled must be a boolean value"
        ])
    );
    let r = app
        .patch(&format!("/admin-company/set/plan-boost/{BAD}"))
        .token(&token)
        .json(&json!({ "product": "SALES", "enabled": true }))
        .send()
        .await;
    assert_eq!(
        r.json,
        json!({ "success": false, "message": "Company not found", "data": null })
    );
    let n = op_audit(&app, op).await.len();
    assert_eq!(n, 3, "baris interceptor hanya untuk respons sukses");
}

#[tokio::test]
async fn features_and_report_exports() {
    let _g = SERIAL.lock().await;
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let (op, owner) = operator(&app, "OWNER").await;
    let (_, finance) = operator(&app, "FINANCE").await;
    let fx = company(&app, "ACTIVE", "PAYPAL", PAID_TIER).await;
    let path = format!("/admin-company/{}/features", fx.uuid);

    let r = app.get(&path).token(&finance).send().await;
    r.assert_ok().assert_no_sql_error();
    assert_eq!(r.data()["customRolesEnabled"]["effective"], false);
    assert_eq!(r.data()["planProductType"], "FULL");
    assert!(
        r.data()["divisions"]["explanation"]
            .as_str()
            .unwrap()
            .starts_with("Divisions: switch OFF")
    );
    app.get(&format!("/admin-company/{BAD}/features"))
        .token(&owner)
        .send()
        .await
        .assert_status(404);

    // FINANCE hanya boleh dontChargeInvite.
    let r = app
        .patch(&path)
        .token(&finance)
        .json(&json!({ "isAgencyMode": true, "reason": "ganti mode agensi" }))
        .send()
        .await;
    r.assert_status(403);
    assert_eq!(
        r.json,
        json!({ "code": "OPERATOR_ROLE_FORBIDDEN", "message": "Operator role FINANCE may not change isAgencyMode", "field": "isAgencyMode" })
    );
    let r = app
        .patch(&path)
        .token(&owner)
        .json(&json!({ "moduleTimeTrackingEnabled": false, "moduleSalesTrackerEnabled": false, "reason": "matikan semua" }))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(r.json["code"], "AT_LEAST_ONE_MODULE");
    let r = app
        .patch(&path)
        .token(&owner)
        .json(&json!({ "reason": "x" }))
        .send()
        .await;
    r.assert_status(400);

    let r = app
        .patch(&path)
        .token(&owner)
        .json(&json!({ "isAgencyMode": true, "divisionsEnabled": true, "reason": "  aktifkan agensi  " }))
        .send()
        .await;
    r.assert_ok().assert_no_sql_error();
    assert_eq!(r.data()["isAgencyMode"], true);
    // PLAN_PRODUCT_MODULE_MISMATCH dihapus: PATCH fitur tidak lagi membawa `warnings`.
    assert!(r.data().get("warnings").is_none());
    let r = app
        .post(&format!("/admin-company/{}/report-exports/reset", fx.uuid))
        .token(&owner)
        .json(&json!({ "reason": "reset kuota bulan ini" }))
        .send()
        .await;
    r.assert_status(201);
    assert_eq!(r.data()["reportExportsUsedThisMonth"], 0);
    // Tanpa perubahan: tidak ada baris audit baru.
    app.patch(&path)
        .token(&owner)
        .json(&json!({ "isAgencyMode": true, "reason": "tidak berubah" }))
        .send()
        .await
        .assert_ok();

    let rows = op_audit(&app, op).await;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0][0], "company.features");
    assert_eq!(
        rows[0][4],
        json!({ "isAgencyMode": false, "divisionsEnabled": false })
    );
    assert_eq!(rows[0][6], "aktifkan agensi");
    assert_eq!(rows[1][0], "company.report_exports_reset");
}

#[tokio::test]
async fn lifecycle_delete_restore_invite_codes() {
    let _g = SERIAL.lock().await;
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let (op, owner) = operator(&app, "OWNER").await;
    let (_, support) = operator(&app, "SUPPORT").await;
    let fx = company(&app, "ACTIVE", "MANUAL", PAID_TIER).await;
    let (staff, _) = member(&app, fx.id, "STAFF").await;
    let base = format!("/admin-company/{}", fx.uuid);

    let r = app
        .post(&format!("{base}/delete"))
        .token(&owner)
        .json(&json!({ "confirmName": "salah", "reason": "tutup akun" }))
        .send()
        .await;
    r.assert_status(400);
    assert_eq!(r.json["code"], "CONFIRM_NAME_MISMATCH");
    let r = app
        .post(&format!("{base}/delete"))
        .token(&owner)
        .json(&json!({ "confirmName": fx.name, "reason": "tutup akun" }))
        .send()
        .await;
    r.assert_status(201).assert_no_sql_error();
    assert_eq!(
        r.data(),
        &json!({ "uuid": fx.uuid, "status": 0, "revokedUsers": 2 })
    );
    let revoked: bool = app
        .scalar(&format!(
            r#"SELECT "tokensValidAfter" IS NOT NULL FROM users WHERE id = {staff}"#
        ))
        .await;
    assert!(revoked);
    let r = app
        .post(&format!("{base}/delete"))
        .token(&owner)
        .json(&json!({ "confirmName": fx.name, "reason": "tutup akun" }))
        .send()
        .await;
    assert_eq!(r.json["code"], "ALREADY_DELETED");
    // Company terhapus: regenerate = 404.
    app.post(&format!("{base}/invite-code/regenerate"))
        .token(&support)
        .json(&json!({ "reason": "putar kode" }))
        .send()
        .await
        .assert_status(404);

    let r = app
        .post(&format!("{base}/restore"))
        .token(&owner)
        .json(&json!({ "reason": "pulihkan akun" }))
        .send()
        .await;
    r.assert_status(201);
    assert_eq!(r.data(), &json!({ "uuid": fx.uuid, "status": 1 }));
    let r = app
        .post(&format!("{base}/restore"))
        .token(&owner)
        .json(&json!({ "reason": "pulihkan akun" }))
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(r.json["code"], "NOT_DELETED");

    let r = app
        .post(&format!("{base}/invite-code/regenerate"))
        .token(&support)
        .json(&json!({ "reason": "putar kode" }))
        .send()
        .await;
    r.assert_status(201);
    let code = r.data()["inviteCode"].as_str().unwrap().to_owned();
    assert_eq!(code.len(), 10);
    assert!(
        r.data()["inviteLink"]
            .as_str()
            .unwrap()
            .ends_with(&format!("/join/{code}"))
    );
    let r = app
        .post(&format!("{base}/invite-code/regenerate"))
        .token(&support)
        .json(&json!({ "reason": "putar kode", "extra": 1 }))
        .send()
        .await;
    assert_eq!(
        r.json["message"],
        json!(["property extra should not exist"])
    );

    app.exec(&format!(
        r#"UPDATE company SET "inviteCode" = NULL WHERE id = {}"#,
        fx.id
    ))
    .await;
    let r = app
        .post("/admin/invite-codes/fill-missing")
        .token(&owner)
        .json(&json!({ "reason": "isi kode kosong" }))
        .send()
        .await;
    r.assert_status(201).assert_no_sql_error();
    assert!(r.data()["filled"].as_i64().unwrap() >= 1);
    let filled: bool = app
        .scalar(&format!(
            r#"SELECT "inviteCode" IS NOT NULL FROM company WHERE id = {}"#,
            fx.id
        ))
        .await;
    assert!(filled);

    let actions: Vec<String> = op_audit(&app, op)
        .await
        .iter()
        .map(|r| r[0].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        actions,
        vec![
            "company.delete",
            "company.restore",
            "invite_codes.fill_missing"
        ]
    );
}

#[tokio::test]
async fn transfer_owner() {
    let _g = SERIAL.lock().await;
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let (op, owner) = operator(&app, "OWNER").await;
    let fx = company(&app, "ACTIVE", "PAYPAL", PAID_TIER).await;
    let (admin_id, admin_uuid) = member(&app, fx.id, "ADMIN").await;
    let path = format!("/admin-company/{}/transfer-owner", fx.uuid);

    let r = app
        .post(&path)
        .token(&owner)
        .json(&json!({ "newOwnerUserUuid": BAD, "reason": "pindah owner" }))
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(r.json["code"], "NOT_ACTIVE_MEMBER");
    let r = app
        .post(&path)
        .token(&owner)
        .json(&json!({ "newOwnerUserUuid": fx.owner_uuid, "reason": "pindah owner" }))
        .send()
        .await;
    assert_eq!(r.json["code"], "ALREADY_OWNER");

    let r = app
        .post(&path)
        .token(&owner)
        .json(&json!({ "newOwnerUserUuid": admin_uuid, "reason": "pindah owner" }))
        .send()
        .await;
    r.assert_status(200).assert_no_sql_error();
    assert_eq!(
        r.data(),
        &json!({
            "previousOwner": { "uuid": fx.owner_uuid, "role": "ADMIN" },
            "newOwner": { "uuid": admin_uuid, "role": "SUPER_ADMIN" },
            "warnings": [],
        })
    );
    let owner_id: i32 = app
        .scalar(&format!(
            r#"SELECT "ownerUserId" FROM company WHERE id = {}"#,
            fx.id
        ))
        .await;
    assert_eq!(owner_id, admin_id);
    let roles: Vec<(i32, String)> = sqlx::query_as(
        r#"SELECT ual."userId", al.code FROM "userAccessLevel" ual JOIN "accessLevel" al ON al.id = ual."accessLevelId"
           WHERE ual."companyId" = $1 AND ual.status = 1 ORDER BY ual."userId""#,
    )
    .bind(fx.id)
    .fetch_all(app.db())
    .await
    .unwrap();
    assert!(roles.contains(&(admin_id, "SUPER_ADMIN".into())));
    assert!(roles.contains(&(fx.owner_id, "ADMIN".into())));
    let rows = op_audit(&app, op).await;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0][4]["ownerSource"], "ownerUserId");
    assert_eq!(rows[0][4]["previousOwnerRole"], "SUPER_ADMIN");
    assert_eq!(rows[0][5]["previousOwnerRole"], "ADMIN");
}

#[tokio::test]
async fn plan_override_put_get_delete() {
    let _g = SERIAL.lock().await;
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let (op, finance) = operator(&app, "FINANCE").await;
    let (_, support) = operator(&app, "SUPPORT").await;
    let fx = company(&app, "ACTIVE", "PAYPAL", PAID_TIER).await;
    let path = format!("/admin-company/{}/plan-override", fx.uuid);

    let r = app.get(&path).token(&support).send().await;
    r.assert_ok().assert_no_sql_error();
    assert_eq!(r.data(), &json!({ "override": null, "applies": true }));

    let r = app
        .put(&path)
        .token(&finance)
        .json(&json!({ "maxProject": "UNLIMITED", "maxStores": 50, "form": true, "note": " kontrak ", "reason": "override kontrak" }))
        .send()
        .await;
    r.assert_ok().assert_no_sql_error();
    let ov = &r.data()["override"];
    assert_eq!(ov["maxProject"], "UNLIMITED");
    assert_eq!(ov["maxStores"], 50);
    assert_eq!(ov["note"], "kontrak");
    assert_eq!(ov["updatedBy"]["name"], "IT Op FINANCE");
    // TIME ditulis ke company: tier berbayar + override.
    let (mp, ret): (i32, i32) = sqlx::query_as(
        r#"SELECT "maxProject", "screenshotRetentionDays" FROM company WHERE id = $1"#,
    )
    .bind(fx.id)
    .fetch_one(app.db())
    .await
    .unwrap();
    assert_eq!((mp, ret), (0, 9999));

    let r = app
        .put(&path)
        .token(&finance)
        .json(&json!({ "maxProject": 0, "reason": "override kontrak" }))
        .send()
        .await;
    r.assert_status(400);
    app.put(&format!("/admin-company/{BAD}/plan-override"))
        .token(&finance)
        .json(&json!({ "reason": "override kontrak" }))
        .send()
        .await
        .assert_status(404);

    let r = app
        .delete(&path)
        .token(&finance)
        .json(&json!({ "reason": "kembali ke paket" }))
        .send()
        .await;
    r.assert_ok();
    assert_eq!(r.data(), &json!({ "override": null, "applies": true }));
    let rows = op_audit(&app, op).await;
    let actions: Vec<&str> = rows.iter().map(|r| r[0].as_str().unwrap()).collect();
    assert_eq!(actions, vec!["plan_override.set", "plan_override.reset"]);
    assert_eq!(rows[0][5]["override"]["maxProject"], 0);
    assert_eq!(rows[0][5]["timeLimits"]["maxProject"], 0);
}

#[tokio::test]
async fn provisioning_company_and_user() {
    let _g = SERIAL.lock().await;
    let Some(app) = TestApp::new(build).await else {
        return;
    };
    let (op, support) = operator(&app, "SUPPORT").await;
    let u = uniq();
    let email = format!("Owner-{u}@Example.com");
    let tier_uuid: String = app
        .scalar(&format!(
            r#"SELECT uuid::text FROM "pricingTiers" WHERE id = {FREE_TIME_TIER}"#
        ))
        .await;
    let body = json!({
        "companyName": format!(" Prov {u} "),
        "timeZone": "Asia/Jakarta",
        "modules": ["TIME_TRACKING", "SALES_TRACKER"],
        "pricingUuid": tier_uuid,
        "trialEndDate": "2031-01-01T00:00:00Z",
        "owner": { "name": "Budi", "email": email },
        "reason": "onboarding manual",
    });
    let r = app
        .post("/admin-company")
        .token(&support)
        .json(&body)
        .send()
        .await;
    r.assert_status(201).assert_no_sql_error();
    let d = r.data().clone();
    assert_eq!(d["company"]["name"], format!("Prov {u}"));
    assert_eq!(d["owner"]["email"], email.to_lowercase());
    assert_eq!(d["passwordGenerated"], true);
    assert_eq!(d["credentialsEmailSent"], false);
    assert_eq!(d["warnings"], json!(["CREDENTIALS_EMAIL_FAILED"]));
    let company_uuid = d["company"]["uuid"].as_str().unwrap().to_owned();
    let (cid, trial, sub_status, uc): (i32, bool, String, i64) = sqlx::query_as(
        r#"SELECT c.id, c."isTrial", (SELECT status::text FROM subscriptions WHERE "companyId" = c.id),
                  (SELECT COUNT(*) FROM "userCompany" WHERE "companyId" = c.id)
           FROM company c WHERE c.uuid = $1::uuid"#,
    )
    .bind(&company_uuid)
    .fetch_one(app.db())
    .await
    .unwrap();
    assert!(trial);
    assert_eq!(sub_status, "TRIAL");
    assert_eq!(uc, 1);
    let qs: i64 = app
        .scalar(&format!(
            r#"SELECT COUNT(*) FROM "companyQuickstart" WHERE "companyId" = {cid}"#
        ))
        .await;
    assert_eq!(qs, 1);

    // Email dipakai → 409; tanggal lampau → 400; tier tidak ada → 404.
    let r = app
        .post("/admin-company")
        .token(&support)
        .json(&body)
        .send()
        .await;
    r.assert_status(409);
    assert_eq!(r.json["code"], "EMAIL_IN_USE");
    let mut past = body.clone();
    past["trialEndDate"] = json!("2020-01-01");
    past["owner"]["email"] = json!(format!("x-{u}@example.com"));
    let r = app
        .post("/admin-company")
        .token(&support)
        .json(&past)
        .send()
        .await;
    assert_eq!(r.json["code"], "INVALID_DATE");
    let mut nf = past.clone();
    nf["trialEndDate"] = json!("2031-01-01");
    nf["pricingUuid"] = json!(BAD);
    let r = app
        .post("/admin-company")
        .token(&support)
        .json(&nf)
        .send()
        .await;
    r.assert_status(404);
    assert_eq!(r.json["code"], "PRICING_NOT_FOUND");

    let r = app
        .post(&format!("/admin-company/{company_uuid}/users"))
        .token(&support)
        .json(&json!({
            "name": "Sari", "email": format!("sari-{u}@example.com"), "password": "rahasia123",
            "role": "STAFF", "reason": "tambah karyawan"
        }))
        .send()
        .await;
    r.assert_status(201).assert_no_sql_error();
    assert_eq!(r.data()["passwordGenerated"], false);
    assert_eq!(r.data()["warnings"], json!([]));
    assert_eq!(r.data()["user"]["role"], "STAFF");
    let r = app
        .post(&format!("/admin-company/{BAD}/users"))
        .token(&support)
        .json(&json!({
            "name": "Sari", "email": format!("sari2-{u}@example.com"), "role": "STAFF", "reason": "tambah karyawan"
        }))
        .send()
        .await;
    r.assert_status(404);

    let rows = op_audit(&app, op).await;
    let actions: Vec<&str> = rows.iter().map(|r| r[0].as_str().unwrap()).collect();
    assert_eq!(actions, vec!["company.create", "company.user_create"]);
    assert_eq!(rows[0][5]["owner"]["passwordGenerated"], true);
    assert_eq!(rows[1][1], "user");
}
