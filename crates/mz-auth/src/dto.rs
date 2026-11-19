//! DTO `src/auth/dto/*.ts` + validasi bergaya class-validator 0.14.
//!
//! Urutan pesan per properti = urutan dekorator dari BAWAH ke atas (TypeScript
//! menjalankan dekorator properti dari yang paling dekat ke nama properti, dan
//! class-validator menyimpan constraint sesuai urutan registrasi). Properti
//! diurutkan sesuai deklarasi kelas. Field disimpan sebagai `Option<Value>`
//! supaya "tidak dikirim" (`None`), `null`, dan tipe yang salah tetap dibedakan.

use mz_core::extract::Validate;
use serde::{Deserialize, Deserializer};
use serde_json::Value;

/// `Some(Null)` untuk `null`, `None` untuk field yang tidak dikirim.
fn present<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(d).map(Some)
}

/// `@Transform(({ value }) => typeof value === 'string' ? value.trim() : value)`.
fn trimmed<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
    Ok(Some(match Value::deserialize(d)? {
        Value::String(s) => Value::String(s.trim().to_owned()),
        other => other,
    }))
}

/// `LOGIN_PLATFORMS` (`dto/login.dto.ts`).
pub const LOGIN_PLATFORMS: [&str; 4] = ["desktop", "android", "ios", "web"];

/// `FeatureModule` (untuk `@IsEnum`).
const FEATURE_MODULES: [&str; 2] = ["TIME_TRACKING", "SALES_TRACKER"];

/// Pemeriksa constraint class-validator untuk satu nilai.
pub mod cv {
    use serde_json::Value;

    pub fn as_str(v: Option<&Value>) -> Option<&str> {
        v.and_then(Value::as_str)
    }
    /// `@IsOptional()`: `null`/`undefined` melewati semua validator lain.
    pub fn is_nullish(v: Option<&Value>) -> bool {
        matches!(v, None | Some(Value::Null))
    }
    /// `isNotEmpty`: `value !== '' && value !== null && value !== undefined`.
    pub fn not_empty(v: Option<&Value>) -> bool {
        !matches!(v, None | Some(Value::Null)) && as_str(v) != Some("")
    }
    pub fn is_string(v: Option<&Value>) -> bool {
        matches!(v, Some(Value::String(_)))
    }
    pub fn is_bool(v: Option<&Value>) -> bool {
        matches!(v, Some(Value::Bool(_)))
    }
    pub fn is_email(v: Option<&Value>) -> bool {
        as_str(v).is_some_and(mz_core::extract::is_email)
    }
    /// `validator.isLength`: titik kode, tanpa selektor variasi U+FE0E/U+FE0F.
    pub fn length(s: &str) -> usize {
        s.chars()
            .filter(|c| !matches!(c, '\u{FE0E}' | '\u{FE0F}'))
            .count()
    }
    /// `@MinLength(n)` — non-string selalu gagal.
    pub fn min_len(v: Option<&Value>, n: usize) -> bool {
        as_str(v).is_some_and(|s| length(s) >= n)
    }
    /// `@MaxLength(n)` — non-string selalu gagal.
    pub fn max_len(v: Option<&Value>, n: usize) -> bool {
        as_str(v).is_some_and(|s| length(s) <= n)
    }
    /// `@IsUUID()` (semua versi): `8-4-4-4-12` heksadesimal.
    pub fn is_uuid(v: Option<&Value>) -> bool {
        as_str(v).is_some_and(mz_core::uuid_rules::is_uuid)
    }
    pub fn is_in(v: Option<&Value>, list: &[&str]) -> bool {
        as_str(v).is_some_and(|s| list.contains(&s))
    }
    /// `@IsTimeZone()` — `Intl.DateTimeFormat` menerima nama IANA tanpa peka huruf.
    pub fn is_time_zone(v: Option<&Value>) -> bool {
        as_str(v).is_some_and(|s| {
            !s.is_empty()
                && chrono_tz::TZ_VARIANTS
                    .iter()
                    .any(|tz| tz.name().eq_ignore_ascii_case(s))
        })
    }
}

/// Pengumpul pesan.
#[derive(Default)]
struct Msgs(Vec<String>);

impl Msgs {
    fn check(&mut self, ok: bool, msg: impl FnOnce() -> String) {
        if !ok {
            self.0.push(msg());
        }
    }
    fn not_empty(&mut self, f: &str, v: Option<&Value>) {
        self.check(cv::not_empty(v), || format!("{f} should not be empty"));
    }
    fn string(&mut self, f: &str, v: Option<&Value>) {
        self.check(cv::is_string(v), || format!("{f} must be a string"));
    }
    fn email(&mut self, f: &str, v: Option<&Value>) {
        self.check(cv::is_email(v), || format!("{f} must be an email"));
    }
    fn min_len(&mut self, f: &str, v: Option<&Value>, n: usize) {
        self.check(cv::min_len(v, n), || {
            format!("{f} must be longer than or equal to {n} characters")
        });
    }
    fn max_len(&mut self, f: &str, v: Option<&Value>, n: usize) {
        self.check(cv::max_len(v, n), || {
            format!("{f} must be shorter than or equal to {n} characters")
        });
    }
    fn uuid(&mut self, f: &str, v: Option<&Value>) {
        self.check(cv::is_uuid(v), || format!("{f} must be a UUID"));
    }
    /// `@IsEmail() @IsNotEmpty()` (urutan pesan: kosong, lalu email).
    fn required_email(&mut self, f: &str, v: Option<&Value>) {
        self.not_empty(f, v);
        self.email(f, v);
    }
    /// `@IsString() @IsNotEmpty()`.
    fn required_string(&mut self, f: &str, v: Option<&Value>) {
        self.not_empty(f, v);
        self.string(f, v);
    }
    /// `@IsString() @IsNotEmpty() @MinLength(n)`.
    fn required_password(&mut self, f: &str, v: Option<&Value>, n: usize) {
        self.min_len(f, v, n);
        self.required_string(f, v);
    }
}

pub const REGISTER_VARCHAR_MAX_LENGTH: usize = 255;
pub const REGISTER_INVITE_CODE_MAX_LENGTH: usize = 12;

/// Nilai string yang sudah lolos validasi (`@IsString`/`@IsEmail`).
pub fn text(v: &Option<Value>) -> &str {
    cv::as_str(v.as_ref()).unwrap_or_default()
}

// ── login.dto.ts ──────────────────────────────────────────────────────────────

/// `LoginDto`.
#[derive(Debug, Default, Deserialize)]
pub struct LoginDto {
    #[serde(default, deserialize_with = "present")]
    pub username: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub password: Option<Value>,
    #[serde(default, deserialize_with = "present", rename = "macAddress")]
    pub mac_address: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub os: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub platform: Option<Value>,
}

impl Validate for LoginDto {
    fn validate(&self) -> Vec<String> {
        let mut m = Msgs::default();
        m.required_email("username", self.username.as_ref());
        // Tetap 6 untuk login (user lama ber-password 6-7 karakter).
        m.required_password("password", self.password.as_ref(), 6);
        for (f, v) in [("macAddress", &self.mac_address), ("os", &self.os)] {
            if !cv::is_nullish(v.as_ref()) {
                m.string(f, v.as_ref());
            }
        }
        if !cv::is_nullish(self.platform.as_ref()) {
            m.check(cv::is_in(self.platform.as_ref(), &LOGIN_PLATFORMS), || {
                "platform must be one of the following values: desktop, android, ios, web".into()
            });
            m.string("platform", self.platform.as_ref());
        }
        m.0
    }
}

/// `ConfirmRegisterDto`.
#[derive(Debug, Default, Deserialize)]
pub struct ConfirmRegisterDto {
    #[serde(default, deserialize_with = "present")]
    pub username: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub token: Option<Value>,
}

impl Validate for ConfirmRegisterDto {
    fn validate(&self) -> Vec<String> {
        let mut m = Msgs::default();
        m.required_email("username", self.username.as_ref());
        m.required_string("token", self.token.as_ref());
        m.0
    }
}

/// `ResendConfirmationDto` dan `SendForgotPasswordDto` (bentuknya sama).
#[derive(Debug, Default, Deserialize)]
pub struct UsernameDto {
    #[serde(default, deserialize_with = "present")]
    pub username: Option<Value>,
}

impl Validate for UsernameDto {
    fn validate(&self) -> Vec<String> {
        let mut m = Msgs::default();
        m.required_email("username", self.username.as_ref());
        m.0
    }
}

/// `Token` (body `POST /auth/confirm`).
#[derive(Debug, Default, Deserialize)]
pub struct TokenDto {
    #[serde(default, deserialize_with = "present")]
    pub username: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub password: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub token: Option<Value>,
}

impl Validate for TokenDto {
    fn validate(&self) -> Vec<String> {
        let mut m = Msgs::default();
        m.required_email("username", self.username.as_ref());
        m.required_password("password", self.password.as_ref(), 6);
        m.required_string("token", self.token.as_ref());
        m.0
    }
}

// ── forgotpassword.dto.ts / change-password.dto.ts ────────────────────────────

/// `ForgotPasswordDto`.
#[derive(Debug, Default, Deserialize)]
pub struct ForgotPasswordDto {
    #[serde(default, deserialize_with = "present")]
    pub username: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub password: Option<Value>,
    #[serde(default, deserialize_with = "present", rename = "forgotPasswordUuid")]
    pub forgot_password_uuid: Option<Value>,
}

impl Validate for ForgotPasswordDto {
    fn validate(&self) -> Vec<String> {
        let mut m = Msgs::default();
        m.required_email("username", self.username.as_ref());
        m.required_password("password", self.password.as_ref(), 8);
        m.not_empty("forgotPasswordUuid", self.forgot_password_uuid.as_ref());
        m.uuid("forgotPasswordUuid", self.forgot_password_uuid.as_ref());
        m.0
    }
}

/// `ChangePasswordDto`.
#[derive(Debug, Default, Deserialize)]
pub struct ChangePasswordDto {
    #[serde(default, deserialize_with = "present", rename = "oldPassword")]
    pub old_password: Option<Value>,
    #[serde(default, deserialize_with = "present", rename = "newPassword")]
    pub new_password: Option<Value>,
    #[serde(default, deserialize_with = "present", rename = "confirmNewPassword")]
    pub confirm_new_password: Option<Value>,
}

impl Validate for ChangePasswordDto {
    fn validate(&self) -> Vec<String> {
        let mut m = Msgs::default();
        m.required_string("oldPassword", self.old_password.as_ref());
        m.required_password("newPassword", self.new_password.as_ref(), 8);
        m.required_password("confirmNewPassword", self.confirm_new_password.as_ref(), 8);
        m.0
    }
}

// ── register.dto.ts ──────────────────────────────────────────────────────────

/// `RegisterDto`. `companyName`, `address`, `attribution` tidak divalidasi.
#[derive(Debug, Default, Deserialize)]
pub struct RegisterDto {
    #[serde(default, deserialize_with = "present")]
    pub username: Option<Value>,
    #[serde(default, deserialize_with = "trimmed")]
    pub name: Option<Value>,
    #[serde(default, deserialize_with = "present", rename = "companyName")]
    pub company_name: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub address: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub handphone: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub roles: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub total_employees: Option<Value>,
    #[serde(default, deserialize_with = "present", rename = "pricingUuid")]
    pub pricing_uuid: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub password: Option<Value>,
    #[serde(default, deserialize_with = "present", rename = "isTrial")]
    pub is_trial: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub attribution: Option<Value>,
}

impl RegisterDto {
    pub fn is_trial(&self) -> bool {
        matches!(self.is_trial, Some(Value::Bool(true)))
    }
}

fn email_max_255(m: &mut Msgs, v: Option<&Value>) {
    m.check(cv::max_len(v, REGISTER_VARCHAR_MAX_LENGTH), || {
        format!("Email maksimal {REGISTER_VARCHAR_MAX_LENGTH} karakter.")
    });
    m.required_email("username", v);
}

fn name_max_255(m: &mut Msgs, v: Option<&Value>) {
    m.check(cv::max_len(v, REGISTER_VARCHAR_MAX_LENGTH), || {
        format!("Nama maksimal {REGISTER_VARCHAR_MAX_LENGTH} karakter.")
    });
    m.required_string("name", v);
}

impl Validate for RegisterDto {
    fn validate(&self) -> Vec<String> {
        let mut m = Msgs::default();
        email_max_255(&mut m, self.username.as_ref());
        name_max_255(&mut m, self.name.as_ref());
        for (f, v) in [
            ("handphone", &self.handphone),
            ("roles", &self.roles),
            ("total_employees", &self.total_employees),
        ] {
            if !cv::is_nullish(v.as_ref()) {
                m.max_len(f, v.as_ref(), REGISTER_VARCHAR_MAX_LENGTH);
                m.string(f, v.as_ref());
            }
        }
        m.not_empty("pricingUuid", self.pricing_uuid.as_ref());
        m.uuid("pricingUuid", self.pricing_uuid.as_ref());
        m.required_password("password", self.password.as_ref(), 8);
        m.not_empty("isTrial", self.is_trial.as_ref());
        m.check(cv::is_bool(self.is_trial.as_ref()), || {
            "isTrial must be a boolean value".into()
        });
        m.0
    }
}

// ── register-invite.dto.ts ───────────────────────────────────────────────────

/// `RegisterInviteDto`.
#[derive(Debug, Default, Deserialize)]
pub struct RegisterInviteDto {
    #[serde(default, deserialize_with = "present")]
    pub username: Option<Value>,
    #[serde(default, deserialize_with = "trimmed")]
    pub name: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub password: Option<Value>,
    #[serde(default, deserialize_with = "present", rename = "inviteCode")]
    pub invite_code: Option<Value>,
}

impl Validate for RegisterInviteDto {
    fn validate(&self) -> Vec<String> {
        let mut m = Msgs::default();
        email_max_255(&mut m, self.username.as_ref());
        name_max_255(&mut m, self.name.as_ref());
        m.min_len("password", self.password.as_ref(), 8);
        m.string("password", self.password.as_ref());
        m.max_len(
            "inviteCode",
            self.invite_code.as_ref(),
            REGISTER_INVITE_CODE_MAX_LENGTH,
        );
        m.required_string("inviteCode", self.invite_code.as_ref());
        m.0
    }
}

// ── create-organization.dto.ts ───────────────────────────────────────────────

/// `CreateOrganizationDto`.
#[derive(Debug, Default, Deserialize)]
pub struct CreateOrganizationDto {
    #[serde(default, deserialize_with = "present")]
    pub name: Option<Value>,
    #[serde(default, deserialize_with = "present", rename = "websiteURL")]
    pub website_url: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub register_total_employees: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub modules: Option<Value>,
    #[serde(default, deserialize_with = "present", rename = "timeZone")]
    pub time_zone: Option<Value>,
}

impl CreateOrganizationDto {
    /// `dto.modules` (sudah lolos `@IsEnum(each)`); `None` = tidak dikirim / `null`.
    pub fn modules(&self) -> Option<Vec<&str>> {
        match &self.modules {
            Some(Value::Array(items)) => Some(items.iter().filter_map(Value::as_str).collect()),
            _ => None,
        }
    }
}

impl Validate for CreateOrganizationDto {
    fn validate(&self) -> Vec<String> {
        let mut m = Msgs::default();
        m.required_string("name", self.name.as_ref());
        for (f, v) in [
            ("websiteURL", &self.website_url),
            ("register_total_employees", &self.register_total_employees),
        ] {
            if !cv::is_nullish(v.as_ref()) {
                m.string(f, v.as_ref());
            }
        }
        if !cv::is_nullish(self.modules.as_ref()) {
            let v = self.modules.as_ref();
            let arr = v.and_then(Value::as_array);
            // `@IsEnum(FeatureModule, { each: true })`
            let enum_ok = match arr {
                Some(items) => items
                    .iter()
                    .all(|i| i.as_str().is_some_and(|s| FEATURE_MODULES.contains(&s))),
                None => v
                    .and_then(Value::as_str)
                    .is_some_and(|s| FEATURE_MODULES.contains(&s)),
            };
            m.check(enum_ok, || {
                "each value in modules must be one of the following values: TIME_TRACKING, SALES_TRACKER"
                    .into()
            });
            // `@ArrayUnique()`
            let unique = arr.is_some_and(|items| {
                items
                    .iter()
                    .enumerate()
                    .all(|(i, a)| items.iter().skip(i + 1).all(|b| a != b))
            });
            m.check(unique, || "All modules's elements must be unique".into());
            // `@ArrayNotEmpty()`
            m.check(arr.is_some_and(|a| !a.is_empty()), || {
                "modules should not be empty".into()
            });
            // `@IsArray()`
            m.check(arr.is_some(), || "modules must be an array".into());
        }
        if !cv::is_nullish(self.time_zone.as_ref()) {
            m.check(cv::is_time_zone(self.time_zone.as_ref()), || {
                "timeZone must be a valid IANA time-zone".into()
            });
            m.string("timeZone", self.time_zone.as_ref());
        }
        m.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn errors<T: Validate + serde::de::DeserializeOwned>(body: Value) -> Vec<String> {
        serde_json::from_value::<T>(body)
            .map(|d| d.validate())
            .unwrap_or_else(|e| vec![e.to_string()])
    }

    #[test]
    fn login_missing_fields_match_class_validator() {
        assert_eq!(
            errors::<LoginDto>(json!({})),
            vec![
                "username should not be empty",
                "username must be an email",
                "password must be longer than or equal to 6 characters",
                "password should not be empty",
                "password must be a string",
            ]
        );
        assert!(errors::<LoginDto>(json!({"username":"a@b.co","password":"123456"})).is_empty());
        assert_eq!(
            errors::<LoginDto>(
                json!({"username":"a@b.co","password":"123456","platform":"PC","os":5})
            ),
            vec![
                "os must be a string",
                "platform must be one of the following values: desktop, android, ios, web",
            ]
        );
    }

    #[test]
    fn register_landing_payload_is_valid() {
        let base = json!({
            "username": "agus@shieldtag.co", "name": "  Agus  ", "companyName": "", "address": "-",
            "handphone": "", "roles": "", "total_employees": "",
            "pricingUuid": "3f1c2b9e-8d7a-4c5b-9e1f-2a3b4c5d6e7f", "password": "Rahasia123!", "isTrial": true
        });
        assert!(errors::<RegisterDto>(base.clone()).is_empty());
        let dto: RegisterDto = serde_json::from_value(base).unwrap_or_default();
        assert_eq!(text(&dto.name), "Agus");
        let spaces = errors::<RegisterDto>(json!({
            "username": "agus@shieldtag.co", "name": "   ",
            "pricingUuid": "3f1c2b9e-8d7a-4c5b-9e1f-2a3b4c5d6e7f", "password": "Rahasia123!", "isTrial": false
        }));
        assert_eq!(spaces, vec!["name should not be empty"]);
        let long = errors::<RegisterDto>(json!({
            "username": "agus@shieldtag.co", "name": "a".repeat(256), "roles": "r".repeat(256),
            "pricingUuid": "nope", "password": "short", "isTrial": "yes"
        }));
        assert_eq!(
            long,
            vec![
                "Nama maksimal 255 karakter.",
                "roles must be shorter than or equal to 255 characters",
                "pricingUuid must be a UUID",
                "password must be longer than or equal to 8 characters",
                "isTrial must be a boolean value",
            ]
        );
    }

    #[test]
    fn register_invite_limits() {
        let ok = json!({"username":"agus@shieldtag.co","name":"Agus","password":"Rahasia123!","inviteCode":"ABCDEF123456"});
        assert!(errors::<RegisterInviteDto>(ok).is_empty());
        let bad = errors::<RegisterInviteDto>(
            json!({"username":"agus@shieldtag.co","name":"Agus","password":"Rahasia123!","inviteCode":"A".repeat(13)}),
        );
        assert_eq!(
            bad,
            vec!["inviteCode must be shorter than or equal to 12 characters"]
        );
        let missing = errors::<RegisterInviteDto>(json!({}));
        assert_eq!(
            missing.first().map(String::as_str),
            Some("Email maksimal 255 karakter.")
        );
    }

    #[test]
    fn create_organization_rules() {
        assert!(
            errors::<CreateOrganizationDto>(json!({"name":"Acme","timeZone":"Asia/Jakarta"}))
                .is_empty()
        );
        assert!(
            errors::<CreateOrganizationDto>(json!({"name":"Acme","timeZone":"asia/jakarta"}))
                .is_empty()
        );
        assert_eq!(
            errors::<CreateOrganizationDto>(json!({"name":"Acme","timeZone":"Mars/Olympus"})),
            vec!["timeZone must be a valid IANA time-zone"]
        );
        assert_eq!(
            errors::<CreateOrganizationDto>(json!({"name":"Acme","timeZone":7})),
            vec![
                "timeZone must be a valid IANA time-zone",
                "timeZone must be a string"
            ]
        );
        assert_eq!(
            errors::<CreateOrganizationDto>(json!({"name":"Acme","modules":[]})),
            vec!["modules should not be empty"]
        );
        assert_eq!(
            errors::<CreateOrganizationDto>(
                json!({"name":"Acme","modules":["TIME_TRACKING","TIME_TRACKING","X"]})
            ),
            vec![
                "each value in modules must be one of the following values: TIME_TRACKING, SALES_TRACKER",
                "All modules's elements must be unique",
            ]
        );
    }

    #[test]
    fn uuid_and_length_helpers() {
        assert!(cv::is_uuid(Some(&json!(
            "7fe52a4f-86fe-4e4d-84c7-be42d94a3868"
        ))));
        assert!(!cv::is_uuid(Some(&json!(
            "7fe52a4f86fe4e4d84c7be42d94a3868"
        ))));
        assert_eq!(cv::length("a\u{FE0F}b"), 2);
    }
}
