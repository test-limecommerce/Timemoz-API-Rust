//! Pembaca body & DTO (`ValidationPipe({ transform: true })` + class-validator/transformer).
//!
//! `POST /mobile/clock/in|out` memakai `FileInterceptor('photo')` (multer): body boleh
//! `multipart/form-data` (field teks → `req.body`, berkas `photo` → `@UploadedFile()`), atau
//! JSON / urlencoded biasa (multer melewatkan request non-multipart, body-parser express yang
//! mengisinya). [`ClockForm`] meniru ketiganya.

use axum::extract::{FromRequest, Multipart, Request};
use mz_core::ApiError;
use serde_json::{Map, Value};

/// Batas `express.json` / `express.urlencoded` bawaan `main.ts` (100kb).
pub const JSON_LIMIT: usize = 100 * 1024;

/// Berkas `photo` yang diunggah (`Express.Multer.File`).
#[derive(Debug, Clone)]
pub struct Photo {
    pub bytes: Vec<u8>,
    pub mime: String,
}

/// `req.body` + `@UploadedFile()`.
#[derive(Debug, Default)]
pub struct ClockForm {
    pub fields: Map<String, Value>,
    pub photo: Option<Photo>,
}

fn content_type(req: &Request) -> String {
    req.headers()
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_ascii_lowercase()
}

fn too_large() -> ApiError {
    mz_core::extract::payload_too_large()
}

/// Objek `req.query`-like dari string urlencoded (`qs` extended).
pub fn qs_object(raw: &str) -> Map<String, Value> {
    mz_core::qs::parse(raw)
        .into_iter()
        .map(|(k, v)| {
            let v = match v {
                mz_core::qs::QVal::One(s) => Value::String(s),
                mz_core::qs::QVal::Many(items) => {
                    Value::Array(items.into_iter().map(Value::String).collect())
                }
                mz_core::qs::QVal::Obj => Value::Object(Map::new()),
            };
            (k, v)
        })
        .collect()
}

/// Body JSON/urlencoded ala express (non-JSON → `{}`; array → `{}` seperti `ValidationPipe`).
pub async fn read_plain_body(req: Request) -> Result<Map<String, Value>, ApiError> {
    let ct = content_type(&req);
    let is_json = ct.starts_with("application/json") || ct.contains("+json");
    let is_form = ct.starts_with("application/x-www-form-urlencoded");
    let bytes = axum::body::Bytes::from_request(req, &())
        .await
        .map_err(mz_core::extract::bytes_rejection)?;
    if (is_json || is_form) && bytes.len() > JSON_LIMIT {
        return Err(too_large());
    }
    if is_form {
        return Ok(qs_object(&String::from_utf8_lossy(&bytes)));
    }
    if !is_json || bytes.is_empty() {
        return Ok(Map::new());
    }
    let value: Value = serde_json::from_slice(&bytes).map_err(|e| {
        let s = e.to_string();
        ApiError::BadJson(s.split(" at line ").next().unwrap_or(&s).to_owned())
    })?;
    match value {
        Value::Object(m) => Ok(m),
        Value::Array(_) => Ok(Map::new()),
        other => Err(ApiError::BadJson(format!(
            "Unexpected token in JSON: {other}"
        ))),
    }
}

impl<S: Send + Sync> FromRequest<S> for ClockForm {
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        if !content_type(&req).starts_with("multipart/form-data") {
            return Ok(Self {
                fields: read_plain_body(req).await?,
                photo: None,
            });
        }
        let mut mp = Multipart::from_request(req, state)
            .await
            .map_err(|e| ApiError::message(e.status(), e.body_text()))?;
        let mut form = Self::default();
        let mp_err = |e: axum::extract::multipart::MultipartError| {
            ApiError::message(e.status(), e.body_text())
        };
        while let Some(field) = mp.next_field().await.map_err(mp_err)? {
            let name = field.name().unwrap_or_default().to_owned();
            if field.file_name().is_some() {
                // `upload.single('photo')`: berkas lain atau berkas kedua → `LIMIT_UNEXPECTED_FILE`.
                if name != "photo" || form.photo.is_some() {
                    return Err(ApiError::bad_request("Unexpected field"));
                }
                let mime = field
                    .content_type()
                    .filter(|m| !m.is_empty())
                    .unwrap_or("application/octet-stream")
                    .to_owned();
                let bytes = field.bytes().await.map_err(mp_err)?.to_vec();
                form.photo = Some(Photo { bytes, mime });
            } else {
                let text = field.text().await.map_err(mp_err)?;
                // `append-field`: nama kembar → array.
                match form.fields.get_mut(&name) {
                    Some(Value::Array(items)) => items.push(Value::String(text)),
                    Some(existing) => {
                        let prev = existing.take();
                        *existing = Value::Array(vec![prev, Value::String(text)]);
                    }
                    None => {
                        form.fields.insert(name, Value::String(text));
                    }
                }
            }
        }
        Ok(form)
    }
}

/// `@IsString() @IsOptional()`: `undefined`/`null` → `None`.
pub fn opt_string(map: &Map<String, Value>, key: &str, errors: &mut Vec<String>) -> Option<String> {
    match map.get(key) {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => {
            errors.push(format!("{key} must be a string"));
            None
        }
    }
}

/// `@Type(() => Number)` (class-transformer `Number(value)`): array/objek tetap bukan angka.
pub fn transform_number(v: &Value) -> Option<f64> {
    match v {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => Some(mz_core::jsfmt::number(s)),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

fn is_number_msg(key: &str) -> String {
    format!("{key} must be a number conforming to the specified constraints")
}

/// `@Type(() => Number) @IsNumber()` (+ `@IsOptional()` bila `optional`).
pub fn number_field(
    map: &Map<String, Value>,
    key: &str,
    optional: bool,
    errors: &mut Vec<String>,
) -> Option<f64> {
    let v = map.get(key);
    if optional && matches!(v, None | Some(Value::Null)) {
        return None;
    }
    match v.and_then(transform_number) {
        Some(n) if n.is_finite() => Some(n),
        _ => {
            errors.push(is_number_msg(key));
            None
        }
    }
}

/// `ClockInDto`.
#[derive(Debug, Clone)]
pub struct ClockInDto {
    pub gps: Option<String>,
    pub note: Option<String>,
    pub face_data: Option<String>,
    pub client_timestamp: f64,
}

impl ClockInDto {
    pub fn parse(map: &Map<String, Value>) -> Result<Self, ApiError> {
        let mut e = Vec::new();
        let gps = opt_string(map, "gps", &mut e);
        let note = opt_string(map, "note", &mut e);
        let face_data = opt_string(map, "faceData", &mut e);
        let ts = number_field(map, "clientTimestamp", false, &mut e);
        match ts {
            Some(client_timestamp) if e.is_empty() => Ok(Self {
                gps,
                note,
                face_data,
                client_timestamp,
            }),
            _ => Err(ApiError::Validation(e)),
        }
    }
}

/// `ClockOutDto`.
#[derive(Debug, Clone)]
pub struct ClockOutDto {
    pub gps: Option<String>,
    pub client_timestamp: Option<f64>,
}

impl ClockOutDto {
    pub fn parse(map: &Map<String, Value>) -> Result<Self, ApiError> {
        let mut e = Vec::new();
        let gps = opt_string(map, "gps", &mut e);
        let _note = opt_string(map, "note", &mut e);
        let client_timestamp = number_field(map, "clientTimestamp", true, &mut e);
        if !e.is_empty() {
            return Err(ApiError::Validation(e));
        }
        Ok(Self {
            gps,
            client_timestamp,
        })
    }
}

/// `BreakStartDto`.
#[derive(Debug, Clone)]
pub struct BreakStartDto {
    pub kind: String,
    pub custom_label: Option<String>,
}

pub const BREAK_TYPES: [&str; 4] = ["lunch", "prayer", "coffee", "other"];

impl BreakStartDto {
    pub fn parse(map: &Map<String, Value>) -> Result<Self, ApiError> {
        let mut e = Vec::new();
        let kind = match map.get("type") {
            Some(Value::String(s)) if BREAK_TYPES.contains(&s.as_str()) => Some(s.clone()),
            _ => {
                // `@IsEnum([...])` dengan array: daftar nilai di pesan kosong (kunci numerik dibuang).
                e.push("type must be one of the following values: ".to_owned());
                None
            }
        };
        let custom_label = opt_string(map, "customLabel", &mut e);
        match kind {
            Some(kind) if e.is_empty() => Ok(Self { kind, custom_label }),
            _ => Err(ApiError::Validation(e)),
        }
    }
}

/// `AutoCloseDto`.
pub fn parse_auto_close(map: &Map<String, Value>) -> Result<Option<f64>, ApiError> {
    let mut e = Vec::new();
    let v = number_field(map, "thresholdHours", true, &mut e);
    if e.is_empty() {
        Ok(v)
    } else {
        Err(ApiError::Validation(e))
    }
}

/// Nilai enum Postgres `MobileShiftGpsStatus` (urutan = skema).
pub const GPS_STATUS_VALUES: [&str; 4] = [
    "ok",
    "gps_disabled",
    "permission_denied",
    "permission_denied_forever",
];

/// Rentang epoch ms yang masuk akal: 2020-01-01 s.d. 2100-01-01.
const MIN_EPOCH_MS: f64 = 1_577_836_800_000.0;
const MAX_EPOCH_MS: f64 = 4_102_444_800_000.0;

/// `GpsStatusDto` (`POST /mobile/clock/gps-status`).
#[derive(Debug, Clone, PartialEq)]
pub struct GpsStatusDto {
    pub status: String,
    pub timestamp: i64,
    /// `(lat, lng, locationAt)` — DTO memaksa ketiganya diisi bersama.
    pub location: Option<(f64, f64, i64)>,
}

/// `typeof v === 'number'` (body JSON; tanpa `@Type`, string tidak dikonversi).
fn js_number(v: Option<&Value>) -> Option<f64> {
    v.and_then(Value::as_f64)
}

/// `@IsInt() @Min(min) @Max(max)` — pesan urut dekorator dari bawah.
fn epoch_ms(map: &Map<String, Value>, key: &str, e: &mut Vec<String>) -> Option<i64> {
    let n = js_number(map.get(key));
    let before = e.len();
    if n.is_none_or(|n| n > MAX_EPOCH_MS) {
        e.push(format!("{key} must not be greater than {MAX_EPOCH_MS}"));
    }
    if n.is_none_or(|n| n < MIN_EPOCH_MS) {
        e.push(format!("{key} must not be less than {MIN_EPOCH_MS}"));
    }
    if n.is_none_or(|n| n.fract() != 0.0) {
        e.push(format!("{key} must be an integer number"));
    }
    #[allow(clippy::cast_possible_truncation)]
    n.filter(|_| e.len() == before).map(|n| n as i64)
}

/// `@IsNumber({ allowNaN: false, allowInfinity: false }) @Min(-lim) @Max(lim)`.
fn coord(map: &Map<String, Value>, key: &str, lim: f64, e: &mut Vec<String>) -> Option<f64> {
    let n = js_number(map.get(key));
    let before = e.len();
    if n.is_none_or(|n| n > lim) {
        e.push(format!("{key} must not be greater than {lim}"));
    }
    if n.is_none_or(|n| n < -lim) {
        e.push(format!("{key} must not be less than -{lim}"));
    }
    if n.is_none() {
        e.push(is_number_msg(key));
    }
    n.filter(|_| e.len() == before)
}

impl GpsStatusDto {
    pub fn parse(map: &Map<String, Value>) -> Result<Self, ApiError> {
        let mut e = Vec::new();
        let status = match map.get("status") {
            Some(Value::String(s)) if GPS_STATUS_VALUES.contains(&s.as_str()) => Some(s.clone()),
            _ => {
                e.push(format!(
                    "status must be one of the following values: {}",
                    GPS_STATUS_VALUES.join(", ")
                ));
                None
            }
        };
        let timestamp = epoch_ms(map, "timestamp", &mut e);
        // `@ValidateIf(hasLocation)`: lat/lng/locationAt divalidasi bila lat ATAU lng tidak null.
        let present = |k: &str| !matches!(map.get(k), None | Some(Value::Null));
        let location = if present("lat") || present("lng") {
            let lat = coord(map, "lat", 90.0, &mut e);
            let lng = coord(map, "lng", 180.0, &mut e);
            let at = epoch_ms(map, "locationAt", &mut e);
            lat.zip(lng).zip(at).map(|((a, b), c)| (a, b, c))
        } else {
            None
        };
        match (status, timestamp) {
            (Some(status), Some(timestamp)) if e.is_empty() => Ok(Self {
                status,
                timestamp,
                location,
            }),
            _ => Err(ApiError::Validation(e)),
        }
    }
}

/// `SaveFaceDto` (`@IsString() @IsNotEmpty()`).
pub fn parse_save_face(map: &Map<String, Value>) -> Result<String, ApiError> {
    let v = map.get("faces");
    let mut e = Vec::new();
    if matches!(v, None | Some(Value::Null)) || v == Some(&Value::String(String::new())) {
        e.push("faces should not be empty".to_owned());
    }
    match v {
        Some(Value::String(s)) if e.is_empty() => Ok(s.clone()),
        Some(Value::String(_)) => Err(ApiError::Validation(e)),
        _ => {
            e.push("faces must be a string".to_owned());
            Err(ApiError::Validation(e))
        }
    }
}

/// Query `GET .../attendance*` (`AdminAttendanceQueryDto`, `MobileAttendanceQueryDto`,
/// `AdminAttendanceExportQueryDto`).
#[derive(Debug, Clone)]
pub struct AttendanceQuery {
    pub start_date: String,
    pub end_date: String,
    pub user_uuid: Option<String>,
    pub geofence_uuid: Option<String>,
    /// `gpsOff=true`: hanya shift dengan `gpsDisabledCount > 0` (admin & export).
    pub gps_off: bool,
    pub page: i64,
    pub limit: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryKind {
    /// `AdminAttendanceQueryDto` (userUuid + page/limit).
    Admin,
    /// `MobileAttendanceQueryDto` (tanpa userUuid).
    Mobile,
    /// `AdminAttendanceExportQueryDto` (tanpa page/limit).
    Export,
}

/// `@IsDateString() @IsNotEmpty()` — pesan IsNotEmpty dulu (dekorator dari bawah).
fn date_field(map: &Map<String, Value>, key: &str, e: &mut Vec<String>) -> String {
    let v = map.get(key);
    if matches!(v, None | Some(Value::Null)) || v == Some(&Value::String(String::new())) {
        e.push(format!("{key} should not be empty"));
    }
    match v {
        Some(Value::String(s)) if mz_core::iso8601::is_iso8601(s) => s.clone(),
        _ => {
            e.push(format!("{key} must be a valid ISO 8601 date string"));
            String::new()
        }
    }
}

/// `@Transform('true'→true, 'false'→false) @IsBoolean() @IsOptional()`.
fn bool_field(map: &Map<String, Value>, key: &str, e: &mut Vec<String>) -> bool {
    match map.get(key) {
        None | Some(Value::Null) => false,
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) if s == "true" => true,
        Some(Value::String(s)) if s == "false" => false,
        Some(_) => {
            e.push(format!("{key} must be a boolean value"));
            false
        }
    }
}

fn uuid_field(map: &Map<String, Value>, key: &str, e: &mut Vec<String>) -> Option<String> {
    match map.get(key) {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if mz_core::uuid_rules::is_uuid(s) => Some(s.clone()),
        Some(_) => {
            e.push(format!("{key} must be a UUID"));
            None
        }
    }
}

/// `@Transform(v => v !== undefined ? parseInt(v, 10) : dflt)` + `@IsInt() @Min(1) [@Max(max)]`.
fn int_field(
    map: &Map<String, Value>,
    key: &str,
    dflt: i64,
    max: Option<i64>,
    e: &mut Vec<String>,
) -> i64 {
    let parsed: Option<f64> = match map.get(key) {
        None => Some(dflt as f64),
        Some(v) => js_parse_int(v),
    };
    // `@IsOptional()`: `null` dilewati (parseInt(null) = NaN tetapi `null` sudah ditransform → NaN
    // bukan null). Nilai yang dikirim lewat query tidak pernah `null`.
    let before = e.len();
    if let Some(max) = max
        && parsed.is_none_or(|n| n > max as f64)
    {
        e.push(format!("{key} must not be greater than {max}"));
    }
    if parsed.is_none_or(|n| n < 1.0) {
        e.push(format!("{key} must not be less than 1"));
    }
    if parsed.is_none() {
        e.push(format!("{key} must be an integer number"));
    }
    if e.len() != before {
        return dflt;
    }
    #[allow(clippy::cast_possible_truncation)]
    parsed.map_or(dflt, |n| n as i64)
}

/// `parseInt(String(v), 10)`; NaN → `None`.
fn js_parse_int(v: &Value) -> Option<f64> {
    let s = match v {
        Value::String(s) => s.clone(),
        Value::Array(items) => items
            .iter()
            .map(|i| match i {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => "null".to_owned(),
        Value::Object(_) => "[object Object]".to_owned(),
    };
    #[allow(clippy::cast_precision_loss)]
    mz_core::jsfmt::parse_int(mz_core::jsfmt::trim(&s)).map(|n| n as f64)
}

impl AttendanceQuery {
    pub fn parse(raw: Option<&str>, kind: QueryKind) -> Result<Self, ApiError> {
        let map = qs_object(raw.unwrap_or_default());
        let mut e = Vec::new();
        let start_date = date_field(&map, "startDate", &mut e);
        let end_date = date_field(&map, "endDate", &mut e);
        let user_uuid = if kind == QueryKind::Admin || kind == QueryKind::Export {
            uuid_field(&map, "userUuid", &mut e)
        } else {
            None
        };
        let geofence_uuid = uuid_field(&map, "geofenceUuid", &mut e);
        // `gpsOff` dideklarasikan sesudah `geofenceUuid` (sebelum page/limit) di DTO admin &
        // export; DTO mobile tidak punya.
        let gps_off = if kind == QueryKind::Mobile {
            false
        } else {
            bool_field(&map, "gpsOff", &mut e)
        };
        let (page, limit) = if kind == QueryKind::Export {
            (1, 20)
        } else {
            let page = int_field(&map, "page", 1, None, &mut e);
            let limit = int_field(&map, "limit", 20, Some(100), &mut e);
            (page, limit)
        };
        if !e.is_empty() {
            return Err(ApiError::Validation(e));
        }
        Ok(Self {
            start_date,
            end_date,
            user_uuid,
            geofence_uuid,
            gps_off,
            page,
            limit,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().cloned().unwrap_or_default()
    }

    fn msgs(r: Result<impl std::fmt::Debug, ApiError>) -> Vec<String> {
        match r {
            Err(ApiError::Validation(m)) => m,
            other => vec![format!("{other:?}")],
        }
    }

    // Pesan dibandingkan dengan keluaran ValidationPipe Node asli (dist/ + class-validator).
    #[test]
    fn clock_in_messages() {
        assert_eq!(
            msgs(ClockInDto::parse(&obj(json!({})))),
            vec!["clientTimestamp must be a number conforming to the specified constraints"]
        );
        assert_eq!(
            msgs(ClockInDto::parse(&obj(
                json!({"gps":1,"note":2,"faceData":3,"clientTimestamp":null})
            ))),
            vec![
                "gps must be a string",
                "note must be a string",
                "faceData must be a string",
                "clientTimestamp must be a number conforming to the specified constraints"
            ]
        );
        let ok = |v: Value| ClockInDto::parse(&obj(v)).ok().map(|d| d.client_timestamp);
        assert_eq!(ok(json!({"clientTimestamp": ""})), Some(0.0));
        assert_eq!(ok(json!({"clientTimestamp": true})), Some(1.0));
        assert_eq!(ok(json!({"clientTimestamp": " 12 "})), Some(12.0));
        assert_eq!(ok(json!({"clientTimestamp": "0x10"})), Some(16.0));
        assert_eq!(ok(json!({"clientTimestamp": [5]})), None);
        assert_eq!(ok(json!({"clientTimestamp": "1e400"})), None);
        assert_eq!(ok(json!({"clientTimestamp": "12abc"})), None);
    }

    #[test]
    fn misc_dto_messages() {
        assert_eq!(
            msgs(parse_save_face(&obj(json!({})))),
            vec!["faces should not be empty", "faces must be a string"]
        );
        assert_eq!(
            msgs(parse_save_face(&obj(json!({"faces": 5})))),
            vec!["faces must be a string"]
        );
        assert_eq!(
            msgs(BreakStartDto::parse(&obj(json!({"type": "bogus"})))),
            vec!["type must be one of the following values: "]
        );
        assert_eq!(
            msgs(BreakStartDto::parse(&obj(
                json!({"type": "other", "customLabel": 4})
            ))),
            vec!["customLabel must be a string"]
        );
        assert_eq!(
            msgs(parse_auto_close(&obj(json!({"thresholdHours": "abc"})))),
            vec!["thresholdHours must be a number conforming to the specified constraints"]
        );
        assert_eq!(
            parse_auto_close(&obj(json!({"thresholdHours": null}))).ok(),
            Some(None)
        );
        assert_eq!(
            parse_auto_close(&obj(json!({"thresholdHours": "-2.5"}))).ok(),
            Some(Some(-2.5))
        );
    }

    #[test]
    fn gps_status_messages() {
        assert_eq!(
            msgs(GpsStatusDto::parse(&obj(json!({})))),
            vec![
                "status must be one of the following values: ok, gps_disabled, permission_denied, permission_denied_forever",
                "timestamp must not be greater than 4102444800000",
                "timestamp must not be less than 1577836800000",
                "timestamp must be an integer number",
            ]
        );
        assert_eq!(
            msgs(GpsStatusDto::parse(&obj(
                json!({"status":"ok","timestamp":1732000000000.5_f64,"lat":95})
            ))),
            vec![
                "timestamp must be an integer number",
                "lat must not be greater than 90",
                "lng must not be greater than 180",
                "lng must not be less than -180",
                "lng must be a number conforming to the specified constraints",
                "locationAt must not be greater than 4102444800000",
                "locationAt must not be less than 1577836800000",
                "locationAt must be an integer number",
            ]
        );
        assert_eq!(
            GpsStatusDto::parse(&obj(
                json!({"status":"gps_disabled","timestamp":1732000000000_i64,"lat":null,"lng":null})
            ))
            .ok(),
            Some(GpsStatusDto {
                status: "gps_disabled".into(),
                timestamp: 1_732_000_000_000,
                location: None,
            })
        );
        assert_eq!(
            GpsStatusDto::parse(&obj(json!({
                "status":"permission_denied","timestamp":1732000000000_i64,
                "lat":-6.2,"lng":106.8,"locationAt":1731999400000_i64
            })))
            .ok()
            .and_then(|d| d.location),
            Some((-6.2, 106.8, 1_731_999_400_000))
        );
        // String tidak dikonversi (tanpa `@Type`).
        assert!(
            GpsStatusDto::parse(&obj(json!({"status":"ok","timestamp":"1732000000000"}))).is_err()
        );
    }

    #[test]
    fn gps_off_query() {
        let parse = |raw: &str, k| AttendanceQuery::parse(Some(raw), k);
        let base = "startDate=2026-01-01&endDate=2026-01-01";
        assert_eq!(
            parse(&format!("{base}&gpsOff=true"), QueryKind::Admin)
                .ok()
                .map(|q| q.gps_off),
            Some(true)
        );
        assert_eq!(
            parse(&format!("{base}&gpsOff=false"), QueryKind::Export)
                .ok()
                .map(|q| q.gps_off),
            Some(false)
        );
        assert_eq!(
            msgs(parse(
                &format!("{base}&geofenceUuid=x&gpsOff=1&page=0"),
                QueryKind::Admin
            )),
            vec![
                "geofenceUuid must be a UUID",
                "gpsOff must be a boolean value",
                "page must not be less than 1"
            ]
        );
        // DTO mobile tidak punya `gpsOff`.
        assert!(parse(&format!("{base}&gpsOff=1"), QueryKind::Mobile).is_ok());
    }

    #[test]
    fn query_messages() {
        let q = |raw: &str, k| msgs(AttendanceQuery::parse(Some(raw), k));
        assert_eq!(
            q("", QueryKind::Admin),
            vec![
                "startDate should not be empty",
                "startDate must be a valid ISO 8601 date string",
                "endDate should not be empty",
                "endDate must be a valid ISO 8601 date string"
            ]
        );
        assert_eq!(
            q(
                "startDate=x&endDate=2026-01-01&userUuid=bad&geofenceUuid=bad&page=abc&limit=500",
                QueryKind::Admin
            ),
            vec![
                "startDate must be a valid ISO 8601 date string",
                "userUuid must be a UUID",
                "geofenceUuid must be a UUID",
                "page must not be less than 1",
                "page must be an integer number",
                "limit must not be greater than 100"
            ]
        );
        assert_eq!(
            q(
                "startDate=2026-01-01&endDate=2026-01-01&page=0&limit=0",
                QueryKind::Admin
            ),
            vec![
                "page must not be less than 1",
                "limit must not be less than 1"
            ]
        );
        let ok = AttendanceQuery::parse(
            Some("startDate=2026-01-01&endDate=2026-01-01&page=2.5x&limit=10"),
            QueryKind::Mobile,
        )
        .ok()
        .map(|q| (q.page, q.limit));
        assert_eq!(ok, Some((2, 10)));
        let arr = AttendanceQuery::parse(
            Some("startDate=2026-01-01&endDate=2026-01-01&page=1&page=2"),
            QueryKind::Admin,
        )
        .ok()
        .map(|q| q.page);
        assert_eq!(arr, Some(1));
        assert_eq!(
            q(
                "startDate[]=2026-01-01&endDate=2026-01-01",
                QueryKind::Export
            ),
            vec!["startDate must be a valid ISO 8601 date string"]
        );
        assert!(
            AttendanceQuery::parse(
                Some("startDate=2026-01-01&endDate=2026-01-01&page=x"),
                QueryKind::Export
            )
            .is_ok()
        );
    }
}
