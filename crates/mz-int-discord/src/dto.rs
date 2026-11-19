//! DTO `src/integrations/discord/dto/*.ts` dengan validasi gaya class-validator 0.14.
//!
//! Field disimpan sebagai `Value` mentah (class-validator memeriksa nilai apa pun tipenya).
//! `None` = properti tidak ada (`undefined`), `Some(Value::Null)` = `null`.
//! Urutan pesan: properti sesuai deklarasi kelas; dalam satu properti dekorator dari
//! BAWAH ke atas.

use mz_core::extract::Validate;
pub use mz_core::uuid_rules::is_uuid;
use serde::{Deserialize, Deserializer};
use serde_json::Value;

use crate::util::{is_discord_cdn_url, is_snowflake, validator_len};

/// Bedakan `null` dari properti yang tidak ada.
fn present<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(d).map(Some)
}

/// `@IsOptional()`: lewati semua validator bila `null`/`undefined`.
fn optional(v: Option<&Value>) -> bool {
    matches!(v, None | Some(Value::Null))
}

fn as_str(v: Option<&Value>) -> Option<&str> {
    v.and_then(Value::as_str)
}

/// Validator satu nilai (`None` = `undefined`).
type Check = fn(Option<&Value>) -> bool;

fn is_string(v: Option<&Value>) -> bool {
    as_str(v).is_some()
}

/// Jalankan validator; dengan `each` dan nilai array, setiap elemen diperiksa
/// (satu pesan untuk properti itu).
fn run(
    errs: &mut Vec<String>,
    v: Option<&Value>,
    each: bool,
    ok: impl Fn(Option<&Value>) -> bool,
    msg: String,
) {
    let valid = match (each, v) {
        (true, Some(Value::Array(items))) => items.iter().all(|i| ok(Some(i))),
        _ => ok(v),
    };
    if !valid {
        errs.push(msg);
    }
}

fn prefix(each: bool) -> &'static str {
    if each { "each value in " } else { "" }
}

fn max_len(errs: &mut Vec<String>, field: &str, v: Option<&Value>, n: usize, each: bool) {
    run(
        errs,
        v,
        each,
        |v| as_str(v).is_some_and(|s| validator_len(s) <= n),
        format!(
            "{}{field} must be shorter than or equal to {n} characters",
            prefix(each)
        ),
    );
}

fn string(errs: &mut Vec<String>, field: &str, v: Option<&Value>, each: bool) {
    let ok: Check = is_string;
    run(
        errs,
        v,
        each,
        ok,
        format!("{}{field} must be a string", prefix(each)),
    );
}

fn array_len(v: Option<&Value>) -> Option<usize> {
    match v {
        Some(Value::Array(a)) => Some(a.len()),
        _ => None,
    }
}

fn array_max(errs: &mut Vec<String>, field: &str, v: Option<&Value>, n: usize) {
    if !array_len(v).is_some_and(|l| l <= n) {
        errs.push(format!("{field} must contain no more than {n} elements"));
    }
}

fn array_min(errs: &mut Vec<String>, field: &str, v: Option<&Value>, n: usize) {
    if !array_len(v).is_some_and(|l| l >= n) {
        errs.push(format!("{field} must contain at least {n} elements"));
    }
}

fn is_array(errs: &mut Vec<String>, field: &str, v: Option<&Value>) {
    if array_len(v).is_none() {
        errs.push(format!("{field} must be an array"));
    }
}

/// `@Matches(SNOWFLAKE_REGEX, { message })` di atas `@IsString()`.
fn snowflake_string(errs: &mut Vec<String>, field: &str, v: Option<&Value>) {
    if !as_str(v).is_some_and(is_snowflake) {
        errs.push(format!("{field} must be a valid Discord ID"));
    }
    string(errs, field, v, false);
}

// ───────────────────────── LinkChannelDto ─────────────────────────

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkChannelDto {
    #[serde(default, deserialize_with = "present")]
    pub project_uuid: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub discord_channel_id: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub discord_channel_name: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub notification_types: Option<Value>,
}

impl Validate for LinkChannelDto {
    fn validate(&self) -> Vec<String> {
        let mut e = Vec::new();
        let pu = self.project_uuid.as_ref();
        max_len(&mut e, "projectUuid", pu, 64, false);
        string(&mut e, "projectUuid", pu, false);
        snowflake_string(&mut e, "discordChannelId", self.discord_channel_id.as_ref());
        let name = self.discord_channel_name.as_ref();
        max_len(&mut e, "discordChannelName", name, 100, false);
        string(&mut e, "discordChannelName", name, false);
        let nt = self.notification_types.as_ref();
        if !optional(nt) {
            max_len(&mut e, "notificationTypes", nt, 64, true);
            string(&mut e, "notificationTypes", nt, true);
            array_max(&mut e, "notificationTypes", nt, 50);
            is_array(&mut e, "notificationTypes", nt);
        }
        e
    }
}

impl LinkChannelDto {
    pub fn project_uuid(&self) -> &str {
        as_str(self.project_uuid.as_ref()).unwrap_or_default()
    }
    pub fn discord_channel_id(&self) -> &str {
        as_str(self.discord_channel_id.as_ref()).unwrap_or_default()
    }
    pub fn discord_channel_name(&self) -> &str {
        as_str(self.discord_channel_name.as_ref()).unwrap_or_default()
    }
    /// `dto.notificationTypes` (array string tervalidasi) atau `None` bila `null`/tidak ada.
    pub fn notification_types(&self) -> Option<&Value> {
        self.notification_types.as_ref().filter(|v| !v.is_null())
    }
}

// ───────────────────────── LinkUserDto ─────────────────────────

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkUserDto {
    #[serde(default, deserialize_with = "present")]
    pub user_uuid: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub discord_user_id: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub discord_username: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub discord_display_name: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub discord_avatar_url: Option<Value>,
}

impl Validate for LinkUserDto {
    fn validate(&self) -> Vec<String> {
        let mut e = Vec::new();
        let uu = self.user_uuid.as_ref();
        max_len(&mut e, "userUuid", uu, 64, false);
        string(&mut e, "userUuid", uu, false);
        snowflake_string(&mut e, "discordUserId", self.discord_user_id.as_ref());
        for (field, v) in [
            ("discordUsername", self.discord_username.as_ref()),
            ("discordDisplayName", self.discord_display_name.as_ref()),
        ] {
            if !optional(v) {
                max_len(&mut e, field, v, 100, false);
                string(&mut e, field, v, false);
            }
        }
        let url = self.discord_avatar_url.as_ref();
        if !optional(url) {
            max_len(&mut e, "discordAvatarUrl", url, 512, false);
            if !as_str(url).is_some_and(is_discord_cdn_url) {
                e.push("discordAvatarUrl must be a URL address".to_owned());
            }
        }
        e
    }
}

impl LinkUserDto {
    pub fn user_uuid(&self) -> &str {
        as_str(self.user_uuid.as_ref()).unwrap_or_default()
    }
    pub fn discord_user_id(&self) -> &str {
        as_str(self.discord_user_id.as_ref()).unwrap_or_default()
    }
    /// `dto.x ?? null` untuk field opsional bertipe string.
    pub fn opt(v: Option<&Value>) -> Option<String> {
        as_str(v).map(str::to_owned)
    }
}

// ───────────────────────── SendNotificationDto ─────────────────────────

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SendNotificationDto {
    #[serde(default, deserialize_with = "present")]
    pub title: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub message: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub color: Option<Value>,
}

impl Validate for SendNotificationDto {
    fn validate(&self) -> Vec<String> {
        let mut e = Vec::new();
        let t = self.title.as_ref();
        max_len(&mut e, "title", t, 256, false);
        string(&mut e, "title", t, false);
        let m = self.message.as_ref();
        max_len(&mut e, "message", m, 4096, false);
        string(&mut e, "message", m, false);
        let c = self.color.as_ref();
        if !optional(c) {
            let n = c.and_then(Value::as_f64);
            if !n.is_some_and(|n| n <= f64::from(0x00ff_ffff)) {
                e.push("color must not be greater than 16777215".to_owned());
            }
            if !n.is_some_and(|n| n >= 0.0) {
                e.push("color must not be less than 0".to_owned());
            }
            if !n.is_some_and(|n| n.is_finite() && n.fract() == 0.0) {
                e.push("color must be an integer number".to_owned());
            }
        }
        e
    }
}

// ───────────────────────── SetDefaultTaskChannelDto ─────────────────────────

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetDefaultTaskChannelDto {
    #[serde(default, deserialize_with = "present")]
    pub channel_id: Option<Value>,
}

impl Validate for SetDefaultTaskChannelDto {
    fn validate(&self) -> Vec<String> {
        let mut e = Vec::new();
        let c = self.channel_id.as_ref();
        if !optional(c) {
            string(&mut e, "channelId", c, false);
        }
        e
    }
}

// ───────────────────────── PostTasksDto ─────────────────────────

#[derive(Debug, Deserialize)]
pub struct PostTasksDto {
    #[serde(default, deserialize_with = "present")]
    pub projects: Option<Value>,
}

/// Satu `ProjectTaskItemDto` tervalidasi.
#[derive(Debug, Clone)]
pub struct TaskItem {
    pub project_uuid: String,
    pub tasks: Vec<String>,
}

/// Pesan anak `ProjectTaskItemDto` (tanpa awalan path).
fn item_errors(item: &serde_json::Map<String, Value>) -> Vec<String> {
    let mut e = Vec::new();
    let pu = item.get("projectUuid");
    if matches!(pu, None | Some(Value::Null)) || as_str(pu) == Some("") {
        e.push("projectUuid should not be empty".to_owned());
    }
    if !as_str(pu).is_some_and(is_uuid) {
        e.push("projectUuid must be a UUID".to_owned());
    }
    let t = item.get("tasks");
    max_len(&mut e, "tasks", t, 200, true);
    string(&mut e, "tasks", t, true);
    array_max(&mut e, "tasks", t, 30);
    array_min(&mut e, "tasks", t, 1);
    is_array(&mut e, "tasks", t);
    e
}

/// `@ValidateNested({ each: true })` untuk satu nilai pada `path` (rekursif untuk array
/// bersarang, sama dengan `ValidationExecutor.nestedValidations`). Hasil = pesan
/// berawalan path seperti `ValidationPipe.flattenValidationErrors`.
fn nested(path: &str, v: &Value, out: &mut Vec<String>) {
    match v {
        Value::Array(items) => {
            for (i, sub) in items.iter().enumerate() {
                let p = format!("{path}.{i}");
                match sub {
                    Value::Object(_) | Value::Array(_) => nested(&p, sub, out),
                    _ => out.push(format!(
                        "{path}.each value in nested property projects must be either object or array"
                    )),
                }
            }
        }
        Value::Object(m) => {
            for msg in item_errors(m) {
                out.push(format!("{path}.{msg}"));
            }
        }
        _ => {}
    }
}

impl Validate for PostTasksDto {
    fn validate(&self) -> Vec<String> {
        let p = self.projects.as_ref();
        // Galat anak (nested) menggantikan constraint `projects` sendiri di hasil
        // ValidationPipe (`mapChildrenToValidationErrors`).
        let mut children = Vec::new();
        if let Some(v @ (Value::Array(_) | Value::Object(_))) = p {
            nested("projects", v, &mut children);
        }
        if !children.is_empty() {
            return children;
        }
        let mut e = Vec::new();
        array_max(&mut e, "projects", p, 10);
        array_min(&mut e, "projects", p, 1);
        is_array(&mut e, "projects", p);
        if let Some(v) = p
            && !matches!(v, Value::Array(_) | Value::Object(_))
        {
            e.push(
                "each value in nested property projects must be either object or array".to_owned(),
            );
        }
        e
    }
}

impl PostTasksDto {
    /// Item tervalidasi (dipanggil sesudah `validate()` kosong).
    pub fn items(&self) -> Vec<TaskItem> {
        let Some(Value::Array(items)) = &self.projects else {
            return Vec::new();
        };
        items
            .iter()
            .filter_map(|i| {
                Some(TaskItem {
                    project_uuid: i.get("projectUuid")?.as_str()?.to_owned(),
                    tasks: i
                        .get("tasks")?
                        .as_array()?
                        .iter()
                        .filter_map(|t| t.as_str().map(str::to_owned))
                        .collect(),
                })
            })
            .collect()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    fn v<T: for<'de> Deserialize<'de> + Validate>(body: Value) -> Vec<String> {
        serde_json::from_value::<T>(body).unwrap().validate()
    }

    #[test]
    fn link_channel_messages() {
        assert_eq!(
            v::<LinkChannelDto>(json!({})),
            vec![
                "projectUuid must be shorter than or equal to 64 characters",
                "projectUuid must be a string",
                "discordChannelId must be a valid Discord ID",
                "discordChannelId must be a string",
                "discordChannelName must be shorter than or equal to 100 characters",
                "discordChannelName must be a string",
            ]
        );
        assert_eq!(
            v::<LinkChannelDto>(json!({
                "projectUuid": "x", "discordChannelId": "12345678901234567",
                "discordChannelName": "n", "notificationTypes": [1, "a"]
            })),
            vec![
                "each value in notificationTypes must be shorter than or equal to 64 characters",
                "each value in notificationTypes must be a string",
            ]
        );
        assert_eq!(
            v::<LinkChannelDto>(json!({
                "projectUuid": "x", "discordChannelId": "12345678901234567",
                "discordChannelName": "n", "notificationTypes": "a"
            })),
            vec![
                "notificationTypes must contain no more than 50 elements",
                "notificationTypes must be an array",
            ]
        );
        assert!(
            v::<LinkChannelDto>(json!({
                "projectUuid": "x", "discordChannelId": "12345678901234567",
                "discordChannelName": "n", "notificationTypes": null
            }))
            .is_empty()
        );
    }

    #[test]
    fn link_user_messages() {
        assert_eq!(
            v::<LinkUserDto>(json!({
                "userUuid": "u", "discordUserId": "1",
                "discordUsername": 5, "discordAvatarUrl": "https://x.com/a.png"
            })),
            vec![
                "discordUserId must be a valid Discord ID",
                "discordUsername must be shorter than or equal to 100 characters",
                "discordUsername must be a string",
                "discordAvatarUrl must be a URL address",
            ]
        );
    }

    #[test]
    fn notification_messages() {
        assert_eq!(
            v::<SendNotificationDto>(json!({ "title": "t", "message": "m", "color": "x" })),
            vec![
                "color must not be greater than 16777215",
                "color must not be less than 0",
                "color must be an integer number",
            ]
        );
        assert_eq!(
            v::<SendNotificationDto>(json!({ "title": "t", "message": "m", "color": 1.5 })),
            vec!["color must be an integer number"]
        );
        assert!(
            v::<SendNotificationDto>(json!({ "title": "t", "message": "m", "color": null }))
                .is_empty()
        );
    }

    #[test]
    fn post_tasks_messages() {
        assert_eq!(
            v::<PostTasksDto>(json!({})),
            vec![
                "projects must contain no more than 10 elements",
                "projects must contain at least 1 elements",
                "projects must be an array",
            ]
        );
        assert_eq!(
            v::<PostTasksDto>(json!({ "projects": [] })),
            vec!["projects must contain at least 1 elements"]
        );
        assert_eq!(
            v::<PostTasksDto>(json!({ "projects": "x" })),
            vec![
                "projects must contain no more than 10 elements",
                "projects must contain at least 1 elements",
                "projects must be an array",
                "each value in nested property projects must be either object or array",
            ]
        );
        assert_eq!(
            v::<PostTasksDto>(json!({ "projects": [
                { "projectUuid": "a1b2c3d4-e5f6-7890-abcd-ef1234567890", "tasks": ["a"] },
                { "projectUuid": "", "tasks": [] },
                3
            ] })),
            vec![
                "projects.1.projectUuid should not be empty",
                "projects.1.projectUuid must be a UUID",
                "projects.1.tasks must contain at least 1 elements",
                "projects.each value in nested property projects must be either object or array",
            ]
        );
        assert_eq!(
            v::<PostTasksDto>(json!({ "projects": { "tasks": ["a"] } })),
            vec![
                "projects.projectUuid should not be empty",
                "projects.projectUuid must be a UUID",
            ]
        );
    }

    #[test]
    fn uuid_rule() {
        assert!(is_uuid("a1b2c3d4-e5f6-4890-abcd-ef1234567890"));
        assert!(!is_uuid("a1b2c3d4-e5f6-9890-abcd-ef1234567890"));
        assert!(!is_uuid("a1b2c3d4-e5f6-4890-cbcd-ef1234567890"));
        assert!(is_uuid("00000000-0000-0000-0000-000000000000"));
    }
}
