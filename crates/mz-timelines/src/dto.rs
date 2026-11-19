//! DTO + validasi gaya class-validator (`src/tracking-timelines/dto/*.ts`).
//!
//! Field disimpan sebagai `Option<Value>` supaya "tidak dikirim" (`None`) dan
//! `null` tetap dibedakan seperti di JS. Urutan pesan per properti mengikuti
//! urutan dekorator dari BAWAH ke atas (urutan registrasi metadata TS).

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use mz_core::ApiError;
use mz_core::extract::Validate;
use mz_core::iso8601::is_iso8601;
use mz_core::uuid_rules::{is_uuid, is_uuid_shape};
use serde::{Deserialize, Deserializer};
use serde_json::Value;

use crate::range::ms_of;

fn present<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(d).map(Some)
}

/// `value === '' || value === null || value === undefined`.
fn is_empty(v: Option<&Value>) -> bool {
    match v {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) => s.is_empty(),
        _ => false,
    }
}

fn is_nullish(v: Option<&Value>) -> bool {
    matches!(v, None | Some(Value::Null))
}

/// Validator per properti; `p` = path lengkap (`segments.0.startAt`).
struct Check<'a> {
    errs: &'a mut Vec<String>,
}

impl Check<'_> {
    fn not_empty(&mut self, p: &str, v: Option<&Value>) {
        if is_empty(v) {
            self.errs.push(format!("{p} should not be empty"));
        }
    }
    fn date_string(&mut self, p: &str, v: Option<&Value>) {
        if !matches!(v, Some(Value::String(s)) if is_iso8601(s)) {
            self.errs
                .push(format!("{p} must be a valid ISO 8601 date string"));
        }
    }
    fn string(&mut self, p: &str, v: Option<&Value>) {
        if !matches!(v, Some(Value::String(_))) {
            self.errs.push(format!("{p} must be a string"));
        }
    }
    fn boolean(&mut self, p: &str, v: Option<&Value>) {
        if !matches!(v, Some(Value::Bool(_))) {
            self.errs.push(format!("{p} must be a boolean value"));
        }
    }
    fn number(&mut self, p: &str, v: Option<&Value>) {
        if !matches!(v, Some(Value::Number(_))) {
            self.errs.push(format!(
                "{p} must be a number conforming to the specified constraints"
            ));
        }
    }
    fn uuid(&mut self, p: &str, v: Option<&Value>) {
        if !matches!(v, Some(Value::String(s)) if is_uuid(s)) {
            self.errs.push(format!("{p} must be a UUID"));
        }
    }

    /// `@IsDateString() @IsNotEmpty()`.
    fn required_date(&mut self, p: &str, v: Option<&Value>) {
        self.not_empty(p, v);
        self.date_string(p, v);
    }
    /// `@IsUUID() @IsNotEmpty()`.
    fn required_uuid(&mut self, p: &str, v: Option<&Value>) {
        self.not_empty(p, v);
        self.uuid(p, v);
    }
    /// `@IsString() @IsOptional()`.
    fn optional_string(&mut self, p: &str, v: Option<&Value>) {
        if !is_nullish(v) {
            self.string(p, v);
        }
    }
    /// `@IsBoolean() @IsOptional()`.
    fn optional_bool(&mut self, p: &str, v: Option<&Value>) {
        if !is_nullish(v) {
            self.boolean(p, v);
        }
    }
    /// `@IsDateString() @IsOptional()`.
    fn optional_date(&mut self, p: &str, v: Option<&Value>) {
        if !is_nullish(v) {
            self.date_string(p, v);
        }
    }
}

/// Bentuk `ParseUUIDPipe` → `Uuid`.
pub fn uuid_of(raw: &str) -> Option<uuid::Uuid> {
    is_uuid_shape(raw)
        .then(|| uuid::Uuid::parse_str(raw).ok())
        .flatten()
}

/// `ParseUUIDPipe` sebagai extractor path (`{uuid}` satu-satunya param), jalan
/// SEBELUM extractor body — sama dengan urutan pipe parameter Nest.
pub struct UuidParam(pub uuid::Uuid, pub String);

impl<S: Send + Sync> FromRequestParts<S> for UuidParam {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        let axum::extract::Path(raw) =
            axum::extract::Path::<String>::from_request_parts(parts, state)
                .await
                .map_err(|e| ApiError::bad_request(e.body_text()))?;
        uuid_of(&raw)
            .map(|u| Self(u, raw))
            .ok_or_else(|| ApiError::bad_request("Validation failed (uuid is expected)"))
    }
}

/// Nilai string bila ada & bertipe string.
pub fn str_of(v: Option<&Value>) -> Option<&str> {
    match v {
        Some(Value::String(s)) => Some(s),
        _ => None,
    }
}

/// JS truthy string (`if (dto.x)`).
pub fn truthy_str(v: Option<&Value>) -> Option<&str> {
    str_of(v).filter(|s| !s.is_empty())
}

pub fn bool_of(v: Option<&Value>) -> Option<bool> {
    match v {
        Some(Value::Bool(b)) => Some(*b),
        _ => None,
    }
}

/// `new Date(string)` → epoch ms; `None` = Invalid Date.
pub fn date_ms(v: Option<&Value>) -> Option<i64> {
    str_of(v).and_then(parse_v8_date).map(ms_of)
}

/// `new Date(s)` V8 lebih longgar dari ISO-8601 ketat: ia juga menerima offset
/// `+HHMM` tanpa titik dua dan bentuk dasar `YYYYMMDDTHHMMSSZ`. Node menghitung cek
/// tengah malam dengan tanggal itu, lalu Prisma yang menolaknya saat `create`
/// (`is_prisma_datetime`). Tanpa kelonggaran ini Rust jatuh ke `CROSS_MIDNIGHT`.
fn parse_v8_date(s: &str) -> Option<chrono::NaiveDateTime> {
    if let Some(d) = mz_core::time::parse_js_date(s) {
        return Some(d);
    }
    let t = s.trim();
    // `...+0700` / `...-0530` → `...+07:00`.
    if t.len() > 5 {
        let (head, tail) = t.split_at(t.len() - 5);
        let tb = tail.as_bytes();
        if matches!(tb.first(), Some(b'+' | b'-'))
            && tb
                .get(1..)
                .is_some_and(|d| d.iter().all(u8::is_ascii_digit))
        {
            let fixed = format!(
                "{head}{}:{}",
                tail.get(..3).unwrap_or_default(),
                tail.get(3..).unwrap_or_default()
            );
            if let Some(d) = mz_core::time::parse_js_date(&fixed) {
                return Some(d);
            }
        }
    }
    // Bentuk dasar `YYYYMMDDTHHMMSSZ`.
    chrono::NaiveDateTime::parse_from_str(t.strip_suffix('Z')?, "%Y%m%dT%H%M%S").ok()
}

/// Bentuk ISO-8601 DateTime penuh yang diterima Prisma
/// (`YYYY-MM-DDTHH:MM:SS[.f+](Z|+HH:MM)`).
pub fn is_prisma_datetime(s: &str) -> bool {
    let b = s.as_bytes();
    let digits =
        |r: std::ops::Range<usize>| b.get(r).is_some_and(|x| x.iter().all(u8::is_ascii_digit));
    let at = |i: usize, c: u8| b.get(i) == Some(&c);
    if !(digits(0..4)
        && at(4, b'-')
        && digits(5..7)
        && at(7, b'-')
        && digits(8..10)
        && at(10, b'T')
        && digits(11..13)
        && at(13, b':')
        && digits(14..16)
        && at(16, b':')
        && digits(17..19))
    {
        return false;
    }
    let mut i = 19;
    if at(i, b'.') {
        let start = i + 1;
        i = start;
        while b.get(i).is_some_and(u8::is_ascii_digit) {
            i += 1;
        }
        if i == start {
            return false;
        }
    }
    match b.get(i) {
        Some(b'Z') => i + 1 == b.len(),
        Some(b'+' | b'-') => {
            digits(i + 1..i + 3) && at(i + 3, b':') && digits(i + 4..i + 6) && i + 6 == b.len()
        }
        _ => false,
    }
}

/// Galat Prisma untuk `startAt`/`endAt` string yang bukan ISO-8601 DateTime penuh.
pub fn prisma_datetime_error(field: &str, v: Option<&Value>) -> Option<String> {
    let s = str_of(v)?;
    (!is_prisma_datetime(s)).then(|| {
        format!("Invalid value for argument `{field}`: premature end of input. Expected ISO-8601 DateTime.")
    })
}

// ---------------------------------------------------------------------------

/// `CreateTrackingTimelineDto` (juga dasar `UpdateTrackingTimelineDto`).
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TimelineDto {
    #[serde(default, deserialize_with = "present")]
    pub note: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub from: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub start_at: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub end_at: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub start_at_utc: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub end_at_utc: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub timezone: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub offset_minutes: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub is_offline: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub tracking_uuid: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub project_uuid: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub user_uuid: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub billable: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub clickup_task_uuid: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub jira_task_uuid: Option<Value>,
}

impl TimelineDto {
    /// `@Transform(({ value, obj }) => value ?? obj.startAt)`.
    fn start_utc_transformed(&self) -> Option<&Value> {
        if is_nullish(self.start_at_utc.as_ref()) {
            self.start_at.as_ref()
        } else {
            self.start_at_utc.as_ref()
        }
    }
    fn end_utc_transformed(&self) -> Option<&Value> {
        if is_nullish(self.end_at_utc.as_ref()) {
            self.end_at.as_ref()
        } else {
            self.end_at_utc.as_ref()
        }
    }

    /// Validasi; `partial` = `PartialType` (semua properti `@IsOptional`).
    pub fn validate_with(&self, partial: bool) -> Vec<String> {
        let mut errs = Vec::new();
        let mut c = Check { errs: &mut errs };
        // `PartialType` menambahkan `@IsOptional` ke setiap properti.
        let skip = |v: Option<&Value>| partial && is_nullish(v);

        c.optional_string("note", self.note.as_ref());
        c.optional_string("from", self.from.as_ref());
        if !skip(self.start_at.as_ref()) {
            c.required_date("startAt", self.start_at.as_ref());
        }
        if !skip(self.end_at.as_ref()) {
            c.required_date("endAt", self.end_at.as_ref());
        }
        c.optional_date("startAtUtc", self.start_utc_transformed());
        c.optional_date("endAtUtc", self.end_utc_transformed());
        if !skip(self.timezone.as_ref()) {
            c.string("timezone", self.timezone.as_ref());
        }
        if !skip(self.offset_minutes.as_ref()) {
            c.number("offsetMinutes", self.offset_minutes.as_ref());
        }
        if !skip(self.is_offline.as_ref()) {
            c.not_empty("isOffline", self.is_offline.as_ref());
            c.boolean("isOffline", self.is_offline.as_ref());
        }
        if !skip(self.tracking_uuid.as_ref()) {
            c.required_uuid("trackingUuid", self.tracking_uuid.as_ref());
        }
        if !skip(self.project_uuid.as_ref()) {
            c.required_uuid("projectUuid", self.project_uuid.as_ref());
        }
        c.optional_string("userUuid", self.user_uuid.as_ref());
        c.optional_bool("billable", self.billable.as_ref());
        c.optional_string("clickupTaskUuid", self.clickup_task_uuid.as_ref());
        c.optional_string("jiraTaskUuid", self.jira_task_uuid.as_ref());
        errs
    }

    pub fn source_tag(&self) -> Option<&str> {
        str_of(self.from.as_ref())
    }
    pub fn is_dashboard(&self) -> bool {
        self.source_tag() == Some("dashboard")
    }
}

/// Body `POST /tracking-timelines` & `create-offline`.
#[derive(Debug, Default, Deserialize)]
#[serde(transparent)]
pub struct CreateTimelineDto(pub TimelineDto);

impl Validate for CreateTimelineDto {
    fn validate(&self) -> Vec<String> {
        self.0.validate_with(false)
    }
}

/// `UpdateTrackingTimelineDto = PartialType(CreateTrackingTimelineDto)`.
#[derive(Debug, Default, Deserialize)]
#[serde(transparent)]
pub struct UpdateTimelineDto(pub TimelineDto);

impl Validate for UpdateTimelineDto {
    fn validate(&self) -> Vec<String> {
        self.0.validate_with(true)
    }
}

// ---------------------------------------------------------------------------

/// `CreateManualTimeDto`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateManualTimeDto {
    #[serde(default, deserialize_with = "present")]
    pub start_at: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub end_at: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub start_at_utc: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub end_at_utc: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub timezone: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub offset_minutes: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub project_uuid: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub tracking_uuid: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub note: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub reason: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub attachments: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub billable: Option<Value>,
}

/// Lampiran tervalidasi.
#[derive(Debug, Clone)]
pub struct Attachment {
    pub file_name: String,
    pub mime_type: String,
    pub file_size: f64,
}

/// `@ValidateNested({ each: true })` pada nilai non-array/objek.
fn nested_shape(errs: &mut Vec<String>, path: &str, v: Option<&Value>) -> bool {
    match v {
        Some(Value::Array(_) | Value::Object(_)) => true,
        Some(_) => {
            errs.push(format!(
                "nested property {path} must be either object or array"
            ));
            false
        }
        None => false,
    }
}

impl Validate for CreateManualTimeDto {
    fn validate(&self) -> Vec<String> {
        let mut errs = Vec::new();
        {
            let mut c = Check { errs: &mut errs };
            c.required_date("startAt", self.start_at.as_ref());
            c.required_date("endAt", self.end_at.as_ref());
            c.optional_date("startAtUtc", self.start_at_utc.as_ref());
            c.optional_date("endAtUtc", self.end_at_utc.as_ref());
            c.optional_string("timezone", self.timezone.as_ref());
            if !is_nullish(self.offset_minutes.as_ref()) {
                c.number("offsetMinutes", self.offset_minutes.as_ref());
            }
            c.required_uuid("projectUuid", self.project_uuid.as_ref());
            c.required_uuid("trackingUuid", self.tracking_uuid.as_ref());
            c.optional_string("note", self.note.as_ref());
            c.not_empty("reason", self.reason.as_ref());
            c.string("reason", self.reason.as_ref());
        }
        if !is_nullish(self.attachments.as_ref()) {
            let v = self.attachments.as_ref();
            let mut own = Vec::new();
            if !matches!(v, Some(Value::Array(_))) {
                own.push("attachments must be an array".to_owned());
            }
            let mut children = Vec::new();
            if nested_shape(&mut own, "attachments", v)
                && let Some(Value::Array(items)) = v
            {
                for (i, item) in items.iter().enumerate() {
                    let path = format!("attachments.{i}");
                    match item {
                        Value::Object(o) => {
                            let mut c = Check { errs: &mut children };
                            let f = |k: &str| o.get(k);
                            c.not_empty(&format!("{path}.fileName"), f("fileName"));
                            c.string(&format!("{path}.fileName"), f("fileName"));
                            c.not_empty(&format!("{path}.mimeType"), f("mimeType"));
                            c.string(&format!("{path}.mimeType"), f("mimeType"));
                            c.number(&format!("{path}.fileSize"), f("fileSize"));
                        }
                        _ => children.push(
                            "attachments.nested property attachments must be either object or array"
                                .to_owned(),
                        ),
                    }
                }
            }
            // Nest membuang constraint milik properti induk bila ada error anak.
            if children.is_empty() {
                errs.extend(own);
            } else {
                errs.extend(children);
            }
        }
        Check { errs: &mut errs }.optional_bool("billable", self.billable.as_ref());
        errs
    }
}

impl CreateManualTimeDto {
    pub fn attachments(&self) -> Option<Vec<Attachment>> {
        let Some(Value::Array(items)) = self.attachments.as_ref() else {
            return None;
        };
        Some(
            items
                .iter()
                .map(|i| Attachment {
                    file_name: i
                        .get("fileName")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    mime_type: i
                        .get("mimeType")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned(),
                    file_size: i.get("fileSize").and_then(Value::as_f64).unwrap_or(0.0),
                })
                .collect(),
        )
    }
}

// ---------------------------------------------------------------------------

/// `SplitTrackingTimelineDto`.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SplitEntryDto {
    #[serde(default, deserialize_with = "present")]
    pub carve_start_at: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub carve_end_at: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub carved_billable: Option<Value>,
}

impl Validate for SplitEntryDto {
    fn validate(&self) -> Vec<String> {
        let mut errs = Vec::new();
        let mut c = Check { errs: &mut errs };
        c.required_date("carveStartAt", self.carve_start_at.as_ref());
        c.required_date("carveEndAt", self.carve_end_at.as_ref());
        c.optional_bool("carvedBillable", self.carved_billable.as_ref());
        errs
    }
}

/// `SplitTimeDto`.
#[derive(Debug, Default, Deserialize)]
pub struct SplitTimeDto {
    #[serde(default, deserialize_with = "present")]
    pub segments: Option<Value>,
}

/// Segmen tervalidasi (`SplitTimeSegmentDto`).
#[derive(Debug, Clone)]
pub struct Segment {
    pub start_ms: i64,
    pub end_ms: i64,
    pub project_uuid: String,
    /// `None` = tidak dikirim; `Some(None)` = `null`.
    pub billable: Option<Option<bool>>,
    /// `seg.note ?? original.note` → `None` untuk null/undefined.
    pub note: Option<String>,
}

impl Validate for SplitTimeDto {
    fn validate(&self) -> Vec<String> {
        let v = self.segments.as_ref();
        let mut own = Vec::new();
        // Urutan registrasi: @ArrayMinSize lalu @IsArray (custom), lalu nested.
        let len_ok = matches!(v, Some(Value::Array(a)) if a.len() >= 2);
        if !len_ok {
            own.push("Minimal 2 segmen diperlukan untuk melakukan split".to_owned());
        }
        if !matches!(v, Some(Value::Array(_))) {
            own.push("segments must be an array".to_owned());
        }
        let mut children = Vec::new();
        if v.is_some()
            && nested_shape(&mut own, "segments", v)
            && let Some(Value::Array(items)) = v
        {
            for (i, item) in items.iter().enumerate() {
                let p = format!("segments.{i}");
                match item {
                    Value::Object(o) => {
                        let mut c = Check {
                            errs: &mut children,
                        };
                        c.required_date(&format!("{p}.startAt"), o.get("startAt"));
                        c.required_date(&format!("{p}.endAt"), o.get("endAt"));
                        c.required_uuid(&format!("{p}.projectUuid"), o.get("projectUuid"));
                        c.optional_bool(&format!("{p}.billable"), o.get("billable"));
                        c.optional_string(&format!("{p}.note"), o.get("note"));
                    }
                    _ => children.push(
                        "segments.nested property segments must be either object or array"
                            .to_owned(),
                    ),
                }
            }
        }
        if children.is_empty() { own } else { children }
    }
}

impl SplitTimeDto {
    pub fn segments(&self) -> Vec<Segment> {
        let Some(Value::Array(items)) = self.segments.as_ref() else {
            return Vec::new();
        };
        items
            .iter()
            .map(|o| Segment {
                start_ms: date_ms(o.get("startAt")).unwrap_or_default(),
                end_ms: date_ms(o.get("endAt")).unwrap_or_default(),
                project_uuid: str_of(o.get("projectUuid")).unwrap_or_default().to_owned(),
                billable: o.get("billable").map(|v| bool_of(Some(v))),
                note: str_of(o.get("note")).map(str::to_owned),
            })
            .collect()
    }
}

// ---------------------------------------------------------------------------

/// `UpdateTrackingNoteDto`.
#[derive(Debug, Default, Deserialize)]
pub struct UpdateNoteDto {
    #[serde(default, deserialize_with = "present")]
    pub note: Option<Value>,
}

impl Validate for UpdateNoteDto {
    fn validate(&self) -> Vec<String> {
        let mut errs = Vec::new();
        let v = self.note.as_ref();
        // @MaxLength(255) (bawah) → @IsNotEmpty → @IsString (atas).
        if !matches!(v, Some(Value::String(s)) if s.chars().map(char::len_utf16).sum::<usize>() <= 255)
        {
            errs.push("note must be shorter than or equal to 255 characters".to_owned());
        }
        let mut c = Check { errs: &mut errs };
        c.not_empty("note", v);
        c.string("note", v);
        errs
    }
}

/// `BulkUpdateBillableDto`.
#[derive(Debug, Default, Deserialize)]
pub struct BulkBillableDto {
    #[serde(default, deserialize_with = "present")]
    pub uuids: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    pub billable: Option<Value>,
}

impl Validate for BulkBillableDto {
    fn validate(&self) -> Vec<String> {
        let mut errs = Vec::new();
        let v = self.uuids.as_ref();
        // @IsUUID(each) (bawah) → @IsNotEmpty → @IsArray (atas).
        let each_ok = match v {
            Some(Value::Array(items)) => items
                .iter()
                .all(|i| matches!(i, Value::String(s) if is_uuid(s))),
            Some(Value::String(s)) => is_uuid(s),
            _ => false,
        };
        if !each_ok {
            errs.push("each value in uuids must be a UUID".to_owned());
        }
        let mut c = Check { errs: &mut errs };
        c.not_empty("uuids", v);
        if !matches!(v, Some(Value::Array(_))) {
            c.errs.push("uuids must be an array".to_owned());
        }
        c.not_empty("billable", self.billable.as_ref());
        c.boolean("billable", self.billable.as_ref());
        errs
    }
}

impl BulkBillableDto {
    pub fn uuids(&self) -> Vec<String> {
        match self.uuids.as_ref() {
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(|i| i.as_str().map(str::to_owned))
                .collect(),
            _ => Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse<T: for<'de> Deserialize<'de> + Default>(v: Value) -> T {
        serde_json::from_value(v).unwrap_or_default()
    }

    fn desktop() -> Value {
        json!({
            "endAt": "2026-10-26T04:17:07.430Z",
            "endAtUtc": null,
            "isOffline": false,
            "note": "-",
            "offsetMinutes": -420,
            "projectUuid": "2a84965c-2416-4948-ab22-2567c6b80450",
            "startAt": "2026-10-26T04:17:00.424Z",
            "startAtUtc": "2026-10-26T04:17:00.424Z",
            "timezone": "Asia/Jakarta",
            "total_seconds": 7,
            "trackingUuid": "5953cb5e-5b35-4fcd-a43b-805463f4181b"
        })
    }

    #[test]
    fn desktop_payload_with_null_end_utc_passes() {
        let d: CreateTimelineDto = parse(desktop());
        assert!(d.validate().is_empty());
        let mut bad = desktop();
        if let Some(o) = bad.as_object_mut() {
            o.insert("endAtUtc".into(), json!("kemarin"));
        }
        let d: CreateTimelineDto = parse(bad);
        assert_eq!(
            d.validate(),
            vec!["endAtUtc must be a valid ISO 8601 date string"]
        );
    }

    #[test]
    fn create_requires_core_fields() {
        let d: CreateTimelineDto = parse(json!({}));
        assert_eq!(
            d.validate(),
            vec![
                "startAt should not be empty",
                "startAt must be a valid ISO 8601 date string",
                "endAt should not be empty",
                "endAt must be a valid ISO 8601 date string",
                "timezone must be a string",
                "offsetMinutes must be a number conforming to the specified constraints",
                "isOffline should not be empty",
                "isOffline must be a boolean value",
                "trackingUuid should not be empty",
                "trackingUuid must be a UUID",
                "projectUuid should not be empty",
                "projectUuid must be a UUID",
            ]
        );
        // Partial: kosong lolos.
        let u: UpdateTimelineDto = parse(json!({}));
        assert!(u.validate().is_empty());
    }

    #[test]
    fn manual_time_nested_attachments() {
        let d: CreateManualTimeDto = parse(json!({
            "startAt": "2026-11-16T01:00:00Z", "endAt": "2026-11-16T02:00:00Z",
            "projectUuid": "2a84965c-2416-4948-ab22-2567c6b80450",
            "trackingUuid": "5953cb5e-5b35-4fcd-a43b-805463f4181b",
            "reason": "lupa", "attachments": [{"fileName": "", "mimeType": "image/png"}, 3]
        }));
        assert_eq!(
            d.validate(),
            vec![
                "attachments.0.fileName should not be empty",
                "attachments.0.fileSize must be a number conforming to the specified constraints",
                "attachments.nested property attachments must be either object or array",
            ]
        );
        let d: CreateManualTimeDto = parse(json!({"attachments": "x"}));
        let v = d.validate();
        assert!(v.contains(&"attachments must be an array".to_owned()));
        assert!(v.contains(&"reason should not be empty".to_owned()));
    }

    #[test]
    fn split_time_segments_rules() {
        let d: SplitTimeDto = parse(json!({}));
        assert_eq!(
            d.validate(),
            vec![
                "Minimal 2 segmen diperlukan untuk melakukan split",
                "segments must be an array"
            ]
        );
        let d: SplitTimeDto = parse(json!({"segments": [{"startAt": "x"}, {}]}));
        let v = d.validate();
        assert_eq!(
            v.first().map(String::as_str),
            Some("segments.0.startAt must be a valid ISO 8601 date string")
        );
        assert!(v.contains(&"segments.1.projectUuid must be a UUID".to_owned()));
    }

    #[test]
    fn note_and_bulk() {
        let d: UpdateNoteDto = parse(json!({}));
        assert_eq!(
            d.validate(),
            vec![
                "note must be shorter than or equal to 255 characters",
                "note should not be empty",
                "note must be a string"
            ]
        );
        let d: BulkBillableDto = parse(json!({"uuids": [], "billable": true}));
        assert!(d.validate().is_empty());
        let d: BulkBillableDto = parse(json!({"uuids": ["nope"]}));
        assert_eq!(
            d.validate(),
            vec![
                "each value in uuids must be a UUID",
                "billable should not be empty",
                "billable must be a boolean value"
            ]
        );
    }

    #[test]
    fn uuid_shape() {
        assert!(is_uuid_shape("9c9dd5b0-645e-499d-9c3e-d6cfc2f4ac55"));
        assert!(!is_uuid_shape("9c9dd5b0645e499d9c3ed6cfc2f4ac55"));
        assert!(!is_uuid_shape("{9c9dd5b0-645e-499d-9c3e-d6cfc2f4ac55}"));
        // DTO `@IsUUID()` lebih ketat dari `ParseUUIDPipe`.
        assert!(is_uuid_shape("12345678-1234-1234-1234-123456789012"));
        assert!(!is_uuid("12345678-1234-1234-1234-123456789012"));
    }
}

#[cfg(test)]
mod datetime_tests {
    use super::*;

    #[test]
    fn v8_lenient_dates_parse_but_fail_prisma_validation() {
        for s in ["2031-05-02T12:30:00+0700", "20310502T053000Z"] {
            assert!(
                parse_v8_date(s).is_some(),
                "{s} harus terbaca seperti new Date()"
            );
            assert!(!is_prisma_datetime(s), "{s} harus ditolak validator Prisma");
        }
        assert_eq!(
            parse_v8_date("2031-05-02T12:30:00+0700").map(|d| d.to_string()),
            Some("2031-05-02 05:30:00".to_owned())
        );
    }

    #[test]
    fn prisma_datetime_shapes() {
        for ok in [
            "2031-05-01T05:00:00Z",
            "2031-05-01T05:00:00.424Z",
            "2031-05-01T05:00:00.1234567Z",
            "2031-05-01T05:00:00+07:00",
        ] {
            assert!(is_prisma_datetime(ok), "{ok}");
        }
        for bad in [
            "2031-05-01",
            "2031-05-01T05:00:00",
            "2031-05-01T05:10",
            "2031-05-01 05:20:00",
            "2031-05-01T05:00:00+0700",
            "20310502T053000Z",
            "2031-05-01T05:00:00.Z",
        ] {
            assert!(!is_prisma_datetime(bad), "{bad}");
        }
    }
}
