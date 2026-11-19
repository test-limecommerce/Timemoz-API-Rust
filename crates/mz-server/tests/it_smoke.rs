//! Smoke test harness integrasi: DB nyata + gerbang terbuka untuk fixture company.
//! Jalankan: `MZ_TEST_DATABASE_URL=postgres://… cargo test -p mz-server --test it_smoke`

use mz_testkit::TestApp;

#[tokio::test]
async fn fixture_users_pass_gates() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    let fx = app.company().await;
    for user in [&fx.super_admin, &fx.admin, &fx.manager, &fx.staff] {
        app.get("/auth/me")
            .token(&user.token)
            .send()
            .await
            .assert_ok();
    }
    for uri in [
        "/trackings",
        "/projects",
        "/screenshots",
        "/timesheets",
        "/project-types",
    ] {
        app.get(uri)
            .token(&fx.super_admin.token)
            .send()
            .await
            .assert_no_sql_error();
    }
}

#[tokio::test]
async fn no_token_is_401() {
    let Some(app) = TestApp::new(mz_server::app).await else {
        return;
    };
    app.get("/projects").send().await.assert_status(401);
}
