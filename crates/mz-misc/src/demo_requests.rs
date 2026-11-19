//! Port `src/demo-requests/*`: form "request demo" di landing page.
//!
//! Node: `demo-requests.controller.ts` (`@PermissionExempt('no-user')`, `@HttpCode(200)`,
//! `ValidationPipe({ whitelist, transform })`), `demo-requests.service.ts`,
//! `dto/submit-demo-request.dto.ts`. Route PUBLIK tanpa JWT. Tidak menulis database,
//! hanya memicu email konfirmasi lewat Mailtrap; hasil kirim ditunggu dan dilaporkan.
//!
//! Limiter 3/menit per IP ada di `src/rate-limit/auth-rate-limits.ts` (Node) dan di
//! `mz_core::rate_limit::rules::DEMO_REQUESTS`.

use axum::Json;
use axum::Router;
use axum::extract::State;
use axum::routing::post;
use mz_core::AppState;
use mz_core::extract::{Validate, ValidatedJson, is_email, validator_js_length};
use mz_core::gate::{RouteSpec, guard};
use mz_core::mail::{Mail, send};
use mz_core::permissions::Exempt;
use mz_core::rate_limit::{RateRule, rules};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};

const TEMPLATE: &str = "demo-request";
const SUBJECT: &str = "Demo Anda terjadwal, ini yang akan kami tunjukkan";
const TEXT: &str = "Stop kunjungan fiktif mulai minggu ini.";
const CATEGORY: &str = "Demo Request";
const WHATSAPP_URL: &str = "https://wa.me/628218051703?text=Halo%2C%20saya%20sudah%20request%20demo%20Timemoz%2C%20mau%20diskusi%20lebih%20lanjut";

/// `@Throttle` yang tidak pernah jalan di Node, diganti limiter express di `main.ts`.
const DEMO_RATES: &[RateRule] = &[rules::DEMO_REQUESTS];

/// Publik: tanpa guard, `@PermissionExempt('no-user')`.
const PUBLIC: RouteSpec = RouteSpec::public()
    .exempt(Exempt::NoUser)
    .rate_limits(DEMO_RATES);

pub fn router(state: &AppState) -> Router<AppState> {
    Router::new().route("/demo-requests", guard(state, PUBLIC, post(submit)))
}

/// `.trim()` yang aman untuk nilai non-string (`@Transform(trimmed)`).
fn trimmed<'de, D: Deserializer<'de>>(d: D) -> Result<Value, D::Error> {
    Ok(match Value::deserialize(d)? {
        Value::String(s) => Value::String(s.trim().to_owned()),
        other => other,
    })
}

/// Node: `SubmitDemoRequestDto`. Field disimpan sebagai `Value` supaya tipe yang salah
/// (angka, objek, null) menghasilkan pesan class-validator yang sama dengan Node.
/// `null` dan field yang tidak dikirim sama-sama `Value::Null`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubmitDemoRequestDto {
    #[serde(default, deserialize_with = "trimmed")]
    name: Value,
    #[serde(default, deserialize_with = "trimmed")]
    email: Value,
    // Diterima dan divalidasi, tidak dipakai isi email mana pun (seperti Node).
    #[serde(default, deserialize_with = "trimmed")]
    phone: Value,
    #[serde(default, deserialize_with = "trimmed")]
    company_name: Value,
    #[serde(default, deserialize_with = "trimmed")]
    message: Value,
}

impl SubmitDemoRequestDto {
    fn name(&self) -> &str {
        self.name.as_str().unwrap_or_default()
    }
    fn email(&self) -> &str {
        self.email.as_str().unwrap_or_default()
    }
}

/// `@MaxLength(n)`: hanya string yang bisa lolos (class-validator menolak selain string).
fn max_ok(v: &Value, max: usize) -> bool {
    v.as_str().is_some_and(|s| validator_js_length(s) <= max)
}

/// `@IsOptional()` + `@MaxLength(n)` + `@IsString()` (urutan pesan seperti Node).
fn optional_string(errors: &mut Vec<String>, field: &str, v: &Value, max: usize) {
    if v.is_null() {
        return;
    }
    if !max_ok(v, max) {
        errors.push(format!(
            "{field} must be shorter than or equal to {max} characters"
        ));
    }
    if !v.is_string() {
        errors.push(format!("{field} must be a string"));
    }
}

impl Validate for SubmitDemoRequestDto {
    fn validate(&self) -> Vec<String> {
        let mut e = Vec::new();
        // name: @MaxLength(120), @IsNotEmpty(), @IsString().
        if !max_ok(&self.name, 120) {
            e.push("name must be shorter than or equal to 120 characters".to_owned());
        }
        if self.name.is_null() || self.name.as_str() == Some("") {
            e.push("name should not be empty".to_owned());
        }
        if !self.name.is_string() {
            e.push("name must be a string".to_owned());
        }
        // email: @MaxLength(254), @IsNotEmpty(), @IsEmail().
        if !max_ok(&self.email, 254) {
            e.push("email must be shorter than or equal to 254 characters".to_owned());
        }
        if self.email.is_null() || self.email.as_str() == Some("") {
            e.push("email should not be empty".to_owned());
        }
        if !self.email.as_str().is_some_and(is_email) {
            e.push("email must be an email".to_owned());
        }
        optional_string(&mut e, "phone", &self.phone, 30);
        optional_string(&mut e, "companyName", &self.company_name, 120);
        optional_string(&mut e, "message", &self.message, 1000);
        e
    }
}

/// Node: `firstNameOf` — kata pertama, atau "Tim" bila kosong.
pub fn first_name_of(name: &str) -> &str {
    name.split_whitespace().next().unwrap_or("Tim")
}

#[derive(Debug, Serialize)]
pub struct DemoData {
    pub email: String,
}

/// Node: `{ success, message, data }`; `data` = `null` bila email gagal dikirim.
#[derive(Debug, Serialize)]
pub struct SubmitResponse {
    pub success: bool,
    pub message: &'static str,
    pub data: Option<DemoData>,
}

/// Node: `demo-requests.service.ts:submit`. Dibalas HTTP 200 (`@HttpCode(200)`).
async fn submit(
    State(state): State<AppState>,
    ValidatedJson(dto): ValidatedJson<SubmitDemoRequestDto>,
) -> Json<SubmitResponse> {
    let email = dto.email().to_lowercase();
    let name = dto.name();

    let sent = send(
        &state,
        Mail {
            to_email: &email,
            to_name: name,
            subject: SUBJECT,
            text: Some(TEXT),
            template: TEMPLATE,
            context: json!({
                "firstName": first_name_of(name),
                "whatsappUrl": WHATSAPP_URL,
            }),
            category: CATEGORY,
            label: "send demo request email",
        },
    )
    .await;

    // Node mencatat alamat di sini; tidak disalin ke log di port ini (data pribadi).
    if !sent.success {
        tracing::error!("demo request email failed");
        return Json(SubmitResponse {
            success: false,
            message: "demo request submitted but email failed to send",
            data: None,
        });
    }
    Json(SubmitResponse {
        success: true,
        message: "demo request submitted",
        data: Some(DemoData { email }),
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn dto(v: Value) -> SubmitDemoRequestDto {
        serde_json::from_value(v).unwrap_or_else(|e| panic!("dto: {e}"))
    }

    #[test]
    fn first_name_of_takes_first_word_or_tim() {
        assert_eq!(first_name_of("  Yudha   Pratama "), "Yudha");
        assert_eq!(first_name_of("   "), "Tim");
        assert_eq!(first_name_of(""), "Tim");
    }

    #[test]
    fn empty_body_gives_node_messages_in_order() {
        assert_eq!(
            dto(json!({})).validate(),
            vec![
                "name must be shorter than or equal to 120 characters",
                "name should not be empty",
                "name must be a string",
                "email must be shorter than or equal to 254 characters",
                "email should not be empty",
                "email must be an email",
            ]
        );
    }

    #[test]
    fn non_string_and_blank_values() {
        assert_eq!(
            dto(json!({ "name": 5, "email": "x" })).validate(),
            vec![
                "name must be shorter than or equal to 120 characters",
                "name must be a string",
                "email must be an email",
            ]
        );
        assert_eq!(
            dto(json!({ "name": "   ", "email": "a@b.co" })).validate(),
            vec!["name should not be empty"]
        );
    }

    #[test]
    fn optional_fields_skip_null_and_check_length() {
        assert!(
            dto(json!({ "name": "B", "email": "a@b.co", "phone": null }))
                .validate()
                .is_empty()
        );
        assert_eq!(
            dto(json!({ "name": "B", "email": "a@b.co", "phone": 5 })).validate(),
            vec![
                "phone must be shorter than or equal to 30 characters",
                "phone must be a string",
            ]
        );
        let long = "x".repeat(31);
        assert_eq!(
            dto(json!({ "name": "B", "email": "a@b.co", "phone": long })).validate(),
            vec!["phone must be shorter than or equal to 30 characters"]
        );
    }

    #[test]
    fn values_are_trimmed_before_validation() {
        let d = dto(json!({ "name": "  Budi  ", "email": "  Budi@X.co  " }));
        assert!(d.validate().is_empty());
        assert_eq!(d.name(), "Budi");
        assert_eq!(d.email(), "Budi@X.co");
    }
}
