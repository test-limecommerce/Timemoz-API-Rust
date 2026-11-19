//! Kasus murni `admin-store-visit-forms.service.spec.ts` bagian submission.

use super::*;
use serde_json::json;

fn q(v: Value) -> Map<String, Value> {
    v.as_object().cloned().unwrap()
}

#[test]
fn filename_slugs() {
    assert_eq!(
        submission_pdf_filename("Laporan Kunjungan", Some("V-001"), "uuid"),
        "laporan-kunjungan-v-001.pdf"
    );
    let f = submission_pdf_filename(
        "Laporan / Kunjungan \"Harian\"\nBaru",
        Some("V-002"),
        "uuid",
    );
    assert!(
        f.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.')
    );
    assert!(!f.contains('/'));
    assert_eq!(
        submission_pdf_filename("Laporan", None, "ab12cd34-ef56-0000-0000-000000000000"),
        "laporan-ab12cd34.pdf"
    );
    assert_eq!(
        submission_pdf_filename("!!!", Some(""), "12345678-x"),
        "form-12345678.pdf"
    );
    let long = "a".repeat(59) + " b";
    assert_eq!(
        submission_pdf_filename(&long, Some("V"), "u"),
        format!("{}-v.pdf", "a".repeat(59))
    );
}

#[test]
fn list_query_defaults_and_errors() {
    let ok = parse_list_query(&q(json!({}))).unwrap();
    assert_eq!(ok.status, StatusFilter::Submitted);
    assert_eq!((ok.page, ok.limit), (1, 20));
    let ok = parse_list_query(&q(json!({"status": "all", "page": "3", "limit": "10"}))).unwrap();
    assert_eq!((ok.status, ok.page, ok.limit), (StatusFilter::All, 3, 10));
    let err = parse_list_query(&q(json!({
        "dateFrom": "2026-8-1", "templateUuid": "x", "status": "nope", "page": "0", "limit": "abc"
    })))
    .unwrap_err();
    assert_eq!(
        err,
        vec![
            "dateFrom must be YYYY-MM-DD",
            "templateUuid must be a UUID",
            "status must be one of the following values: submitted, draft, all",
            "page must not be less than 1",
            "limit must not be greater than 100",
            "limit must not be less than 1",
            "limit must be an integer number",
        ]
    );
    let err = parse_list_query(&q(json!({"limit": "101"}))).unwrap_err();
    assert_eq!(err, vec!["limit must not be greater than 100"]);
}

#[test]
fn date_bounds_in_company_zone() {
    let gte = day_bound_ms("2026-08-01", "Asia/Jakarta", false).unwrap();
    let lte = day_bound_ms("2026-08-31", "Asia/Jakarta", true).unwrap();
    let iso = |ms| {
        DateTime::<Utc>::from_timestamp_millis(ms)
            .unwrap()
            .format("%Y-%m-%dT%H:%M:%S%.3fZ")
            .to_string()
    };
    assert_eq!(iso(gte), "2026-07-31T17:00:00.000Z");
    assert_eq!(iso(lte), "2026-08-31T16:59:59.999Z");
    assert!(day_bound_ms("2026-02-30", "UTC", false).is_none());
}

#[test]
fn correct_body_validation() {
    let body: CorrectBody = serde_json::from_value(json!({})).unwrap();
    assert_eq!(
        body.check().unwrap_err(),
        vec![
            "answers must be an object",
            "reason must be shorter than or equal to 500 characters",
            "reason must be longer than or equal to 1 characters",
            "reason must be a string",
            "expectedRevision must not be less than 0",
            "expectedRevision must be an integer number",
        ]
    );
    let body: CorrectBody =
        serde_json::from_value(json!({"answers": [], "reason": "   ", "expectedRevision": -1}))
            .unwrap();
    assert_eq!(
        body.check().unwrap_err(),
        vec![
            "answers must be an object",
            "reason must be longer than or equal to 1 characters",
            "expectedRevision must not be less than 0",
        ]
    );
    // `Number(null)` = 0, `Number("2")` = 2; reason di-trim.
    let body: CorrectBody =
        serde_json::from_value(json!({"answers": {}, "reason": " ok ", "expectedRevision": null}))
            .unwrap();
    assert_eq!(body.check().unwrap().expected_revision, 0);
    let body: CorrectBody = serde_json::from_value(
        json!({"answers": {"a": 1}, "reason": "ok", "expectedRevision": "2"}),
    )
    .unwrap();
    let dto = body.check().unwrap();
    assert_eq!((dto.reason.as_str(), dto.expected_revision), ("ok", 2));
    let body: CorrectBody = serde_json::from_value(
        json!({"answers": {}, "reason": "x".repeat(501), "expectedRevision": 1.5}),
    )
    .unwrap();
    assert_eq!(
        body.check().unwrap_err(),
        vec![
            "reason must be shorter than or equal to 500 characters",
            "expectedRevision must be an integer number",
        ]
    );
}

fn row() -> SubmissionRow {
    let t = |s: &str| chrono::DateTime::parse_from_rfc3339(s).unwrap().naive_utc();
    SubmissionRow {
        id: 1,
        uuid: "submission-uuid".into(),
        user_id: 12,
        visit_id: 5,
        store_id: 77,
        store_name: "Toko Makmur (saat kunjungan)".into(),
        visit_uuid: "visit-uuid".into(),
        visit_check_in_at: t("2026-08-11T01:00:00Z"),
        template_id: 9,
        template_version_id: 199,
        submitted_at: Some(t("2026-08-11T02:00:00Z")),
        revision_count: 0,
        last_edited_at: None,
        last_edited_by_id: None,
        created_at: t("2026-08-11T01:30:00Z"),
        answers: Value::Null,
    }
}

#[test]
fn serializes_submission_row() {
    let mut l = Lookups::default();
    l.users.insert(
        12,
        UserRef {
            uuid: "user-uuid".into(),
            name: "Budi".into(),
        },
    );
    l.stores.insert(77, "store-uuid".into());
    l.templates.insert(
        9,
        TemplateRef {
            uuid: "template-uuid".into(),
            name: "Laporan".into(),
        },
    );
    l.versions.insert(
        199,
        VersionRef {
            uuid: "version-uuid-1".into(),
            version: 1,
        },
    );
    let v = Value::Object(to_submission(&row(), &l));
    assert_eq!(
        v,
        json!({
            "id": "submission-uuid",
            "status": "submitted",
            "submittedAt": 1_786_413_600_000_i64,
            "createdAt": 1_786_411_800_000_i64,
            "template": {"id": "template-uuid", "name": "Laporan"},
            "version": {"id": "version-uuid-1", "version": 1},
            "user": {"id": "user-uuid", "name": "Budi"},
            "store": {"id": "store-uuid", "name": "Toko Makmur (saat kunjungan)"},
            "visit": {"id": "visit-uuid", "checkInAt": 1_786_410_000_000_i64},
            "revisionCount": 0,
            "lastEditedAt": null,
            "lastEditedBy": null,
        })
    );
    // Key urut seperti objek Node.
    let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
    assert_eq!(keys[0], "id");
    assert_eq!(keys[11], "lastEditedBy");

    // Sales/toko yang hilang tetap tampil dengan `null`; pengoreksi yang hilang = {null,null}.
    let mut r = row();
    r.last_edited_by_id = Some(7);
    r.submitted_at = None;
    let v = Value::Object(to_submission(&r, &Lookups::default()));
    assert_eq!(v["status"], "draft");
    assert_eq!(v["user"], json!({"id": null, "name": null}));
    assert_eq!(
        v["store"],
        json!({"id": null, "name": "Toko Makmur (saat kunjungan)"})
    );
    assert_eq!(v["template"], json!({"id": null, "name": null}));
    assert_eq!(v["lastEditedBy"], json!({"id": null, "name": null}));
}

const UUID_TTD: &str = "2ab70000-1111-4222-8333-444455556666";
const UUID_OTHER: &str = "3ab70000-1111-4222-8333-444455556666";

fn media_schema() -> FormDocument {
    FormDocument::from_value(&json!({
        "schemaVersion": 1,
        "fields": [
            {"key": "catatan", "type": "text", "required": true},
            {"key": "ttd", "type": "signature"},
            {"key": "gate", "type": "boolean"},
            {"key": "ttd_opsional", "type": "signature",
             "visibleIf": {"all": [{"field": "gate", "op": "eq", "value": true}]}}
        ]
    }))
}

fn code_of(e: &ApiError) -> Value {
    match e {
        ApiError::Body { body, .. } => body.clone(),
        _ => Value::Null,
    }
}

#[test]
fn media_is_immutable() {
    let s = media_schema();
    let before = json!({"catatan": "lama", "ttd": {"fileUuid": UUID_TTD}});
    // Menghapus tanda tangan.
    let e = validate_correction(&s, &before, &json!({"catatan": "baru"}), "UTC").unwrap_err();
    let b = code_of(&e);
    assert_eq!(b["code"], "FORM_MEDIA_IMMUTABLE");
    assert_eq!(
        b["details"],
        json!({"expected": [UUID_TTD], "received": []})
    );
    // Mengganti dengan file lain.
    let e = validate_correction(
        &s,
        &before,
        &json!({"catatan": "baru", "ttd": {"fileUuid": UUID_OTHER}}),
        "UTC",
    )
    .unwrap_err();
    assert_eq!(code_of(&e)["code"], "FORM_MEDIA_IMMUTABLE");
    // Menyembunyikan node media secara tidak langsung.
    let before2 = json!({"catatan": "lama", "ttd": {"fileUuid": UUID_TTD}, "gate": true,
                         "ttd_opsional": {"fileUuid": UUID_OTHER}});
    let e = validate_correction(
        &s,
        &before2,
        &json!({"catatan": "baru", "ttd": {"fileUuid": UUID_TTD}, "gate": false,
                "ttd_opsional": {"fileUuid": UUID_OTHER}}),
        "UTC",
    )
    .unwrap_err();
    assert_eq!(code_of(&e)["code"], "FORM_MEDIA_IMMUTABLE");
    // Koreksi yang tidak menyentuh media lolos.
    let ok = validate_correction(
        &s,
        &before,
        &json!({"catatan": "baru", "ttd": {"fileUuid": UUID_TTD}}),
        "UTC",
    )
    .unwrap();
    assert_eq!(ok["catatan"], "baru");
}

#[test]
fn correction_enforces_full_validation() {
    let s = media_schema();
    let e = validate_correction(&s, &json!({}), &json!({}), "UTC").unwrap_err();
    let b = code_of(&e);
    assert_eq!(b["code"], "FORM_VALIDATION_FAILED");
    assert_eq!(b["message"], "Answers did not pass validation");
    assert_eq!(b["details"]["errors"][0]["key"], "catatan");
}
