//! Seed data bersama untuk uji integrasi mz-screenshots (idempotent; uuid deterministik).
#![allow(dead_code, clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use mz_testkit::{Company, TestApp, TestResponse};

pub const D1: &str = "2026-03-10";
pub const D2: &str = "2026-03-11";

pub struct Seed {
    pub project_id: i32,
    pub project_uuid: String,
    pub company_uuid: String,
}

/// Tiap user fixture dapat, untuk D1 dan D2: timeline "itseed A" (09:00-10:00, billable NULL)
/// dan "itseed B" (13:00-13:30, billable=false), masing-masing dengan app + url, serta satu
/// screenshot FINISHED pada timeline A. Idempotent & aman dipanggil paralel.
pub async fn seed(app: &TestApp, fx: &Company) -> Seed {
    let ids = [
        fx.super_admin.id,
        fx.admin.id,
        fx.manager.id,
        fx.staff.id,
        fx.staff2.id,
    ]
    .map(|i| i.to_string())
    .join(",");
    let (project_id, project_uuid): (i32, String) = sqlx::query_as(
        r#"SELECT p.id, p.uuid::text FROM project p
           JOIN "projectCompany" pc ON pc."projectId" = p.id AND pc."companyId" = $1 AND pc.status = 1
           ORDER BY p.id LIMIT 1"#,
    )
    .bind(fx.id)
    .fetch_one(app.db())
    .await
    .unwrap();
    app.exec(&format!(
        r#"
SELECT pg_advisory_xact_lock(7770001);
INSERT INTO tracking ("takenDate", "userId", status, "isAbsent")
  SELECT d::date, u, 1, false FROM unnest(ARRAY[{ids}]) u, unnest(ARRAY['{D1}','{D2}']) d
  ON CONFLICT ("userId", "takenDate") DO UPDATE SET status = 1;
INSERT INTO "trackingTimeline" (uuid, note, "startAt", "endAt", "startAtUtc", "endAtUtc", timezone,
                                "trackingId", "projectId", status, billable)
  -- UUID v4 deterministik (nibble versi `4`, varian `8`): DTO `@IsUUID()` menolak md5 mentah,
  -- sama seperti Node; data asli dibuat Prisma `uuid()` (v4).
  SELECT overlay(overlay(md5('ittl:' || tr.id || ':' || v.k) placing '4' from 13) placing '8' from 17)::uuid,
         'itseed ' || v.k,
         tr."takenDate" + v.s::time, tr."takenDate" + v.e::time,
         tr."takenDate" + v.s::time, tr."takenDate" + v.e::time,
         'UTC', tr.id, {project_id}, 1, v.b
  FROM tracking tr,
       (VALUES ('A', '09:00', '10:00', NULL::boolean), ('B', '13:00', '13:30', false)) v(k, s, e, b)
  WHERE tr."userId" = ANY(ARRAY[{ids}]) AND tr."takenDate" IN ('{D1}', '{D2}')
  ON CONFLICT (uuid) DO NOTHING;
INSERT INTO app (uuid, "trackingTimelineId", name, title, "startAt", "endAt", timezone)
  SELECT md5('itapp:' || tt.id)::uuid, tt.id, 'ItSeedApp', 'ItSeed window title',
         tt."startAt" + interval '1 minute', tt."endAt" - interval '1 minute', 'UTC'
  FROM "trackingTimeline" tt WHERE tt.note LIKE 'itseed %'
  ON CONFLICT (uuid) DO NOTHING;
INSERT INTO url (uuid, "trackingTimelineId", host, url, title, "startAt", "endAt", timezone)
  SELECT md5('iturl:' || tt.id)::uuid, tt.id, 'itseed.example.com', 'https://itseed.example.com/page',
         'ItSeed page', tt."startAt" + interval '1 minute', tt."endAt" - interval '1 minute', 'UTC'
  FROM "trackingTimeline" tt WHERE tt.note LIKE 'itseed %'
  ON CONFLICT (uuid) DO NOTHING;
INSERT INTO screenshot (uuid, "fileFullpath", "isBlur", "takenAt", "trackingTimelineId", "projectId", status)
  SELECT md5('itss:' || tt.id)::uuid,
         'companies/{cuuid}/projects/{project_uuid}/users/' || u.uuid || '/screenshots/' || md5('itss:' || tt.id) || '.jpg',
         false, tt."startAt" + interval '5 minutes', tt.id, {project_id}, 3
  FROM "trackingTimeline" tt JOIN tracking tr ON tr.id = tt."trackingId" JOIN users u ON u.id = tr."userId"
  WHERE tt.note = 'itseed A'
  ON CONFLICT (uuid) DO NOTHING;
"#,
        cuuid = fx.uuid,
    ))
    .await;
    Seed {
        project_id,
        project_uuid,
        company_uuid: fx.uuid.clone(),
    }
}

/// uuid screenshot seed FINISHED milik `user_id` pada D1.
pub async fn shot_of(app: &TestApp, user_id: i32) -> String {
    sqlx::query_scalar(
        r#"SELECT s.uuid::text FROM screenshot s
           JOIN "trackingTimeline" tt ON tt.id = s."trackingTimelineId"
           JOIN tracking tr ON tr.id = tt."trackingId"
           WHERE tr."userId" = $1 AND tt.note = 'itseed A' AND tr."takenDate" = $2::date
             AND s.status = 3 AND s.uuid = md5('itss:' || tt.id)::uuid"#,
    )
    .bind(user_id)
    .bind(D1)
    .fetch_one(app.db())
    .await
    .unwrap()
}

/// uuid timeline "itseed A" milik `user_id` pada D1.
pub async fn timeline_of(app: &TestApp, user_id: i32) -> String {
    sqlx::query_scalar(
        r#"SELECT tt.uuid::text FROM "trackingTimeline" tt
           JOIN tracking tr ON tr.id = tt."trackingId"
           WHERE tr."userId" = $1 AND tt.note = 'itseed A' AND tr."takenDate" = $2::date"#,
    )
    .bind(user_id)
    .bind(D1)
    .fetch_one(app.db())
    .await
    .unwrap()
}

/// Tidak 5xx, tanpa jejak SQL, dan bukan 2xx (jalur gagal GCS).
pub fn assert_gcs_failure(res: &TestResponse) {
    let text = res.text().to_lowercase();
    for n in mz_testkit::SQL_ERROR_NEEDLES {
        assert!(
            !text.contains(n),
            "jejak galat SQL `{n}`: {} {}",
            res.uri,
            text
        );
    }
    assert!(
        !res.status.is_success(),
        "{} harus gagal tanpa GCS: {}",
        res.uri,
        res.status
    );
}

/// Env GCS ada = jangan sentuh jalur GCS (bisa memanggil jaringan).
pub fn gcs_env_present() -> bool {
    std::env::var("GCP_BUCKET_NAME").is_ok_and(|v| !v.trim().is_empty())
}
