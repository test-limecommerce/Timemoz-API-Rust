//! Extractor pengganti `ValidationPipe({ transform: true })` + body-parser express.
//!
//! * Body non-JSON / kosong → `{}` (express.json hanya mem-parse content-type JSON).
//! * Body berupa array JSON → `{}` (ValidationPipe memeriksa properti DTO pada array).
//! * JSON rusak → 400 `{"message":"<teks>","error":"Bad Request","statusCode":400}`.
//! * Gagal tipe/validasi → 400 `{"message":[...],"error":"Bad Request","statusCode":400}`.
//! * Field tak dikenal diabaikan (tidak ada `whitelist`).

use axum::extract::{FromRequest, FromRequestParts, Request};
use axum::http::request::Parts;
use serde::de::DeserializeOwned;

use crate::ApiError;

/// Penolakan `Bytes::from_request` → galat ala express. Body melebihi limit (`DefaultBodyLimit`,
/// setara `express.json({ limit })` di `main.ts`) → 413 `request entity too large` tanpa `error`,
/// seperti body-parser Node; penolakan lain apa adanya.
pub fn bytes_rejection(e: axum::extract::rejection::BytesRejection) -> ApiError {
    let status = e.status();
    if status == http::StatusCode::PAYLOAD_TOO_LARGE {
        payload_too_large()
    } else {
        ApiError::message(status, e.body_text())
    }
}

/// 413 body-parser express lewat `BaseExceptionFilter` Nest: `{ statusCode, message }` tanpa `error`.
pub fn payload_too_large() -> ApiError {
    ApiError::Plain {
        status: http::StatusCode::PAYLOAD_TOO_LARGE,
        message: "request entity too large".to_owned(),
    }
}

/// `Content-Type` yang di-parse `express.json` (`application/json` atau `*+json`).
pub fn is_json_content_type(headers: &http::HeaderMap) -> bool {
    headers
        .get(http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| {
            let ct = ct.to_ascii_lowercase();
            ct.starts_with("application/json") || ct.contains("+json")
        })
}

/// Body seperti `express.json` (strict): bukan JSON / kosong → `{}`; JSON rusak atau primitif →
/// 400 `BadJson`. Array dipertahankan (pemanggil yang memakai DTO memperlakukannya sebagai `{}`).
pub fn parse_express_json(is_json: bool, bytes: &[u8]) -> Result<serde_json::Value, ApiError> {
    if !is_json || bytes.is_empty() {
        return Ok(serde_json::Value::Object(serde_json::Map::new()));
    }
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|e| ApiError::BadJson(serde_message(&e)))?;
    if !value.is_object() && !value.is_array() {
        return Err(ApiError::BadJson(format!(
            "Unexpected token in JSON: {value}"
        )));
    }
    Ok(value)
}

/// Body mentah ala `express.json` (strict) untuk validator manual (`ADMIN_VALIDATION_PIPE` dll.):
/// lihat [`parse_express_json`]. Array dipertahankan (pipe whitelist melaporkan indeksnya).
/// Dulu disalin di mz-admin, mz-admin-jobs, mz-admin-misc, mz-timeconfig, mz-tasks-sync.
pub struct RawBody(pub serde_json::Value);

impl<S: Send + Sync> FromRequest<S> for RawBody {
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let is_json = is_json_content_type(req.headers());
        let bytes = axum::body::Bytes::from_request(req, state)
            .await
            .map_err(bytes_rejection)?;
        parse_express_json(is_json, &bytes).map(Self)
    }
}

/// Query mentah diparse seperti `req.query` Express ([`crate::qs::parse`]) untuk validator manual.
/// Dulu disalin di mz-admin, mz-admin-jobs, mz-overtime (di sana diekspor ulang sebagai `RawQuery`).
pub struct ParsedQuery(pub Vec<(String, crate::qs::QVal)>);

impl<S: Send + Sync> FromRequestParts<S> for ParsedQuery {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(crate::qs::parse(
            parts.uri.query().unwrap_or_default(),
        )))
    }
}

/// Pesan bawaan `@Length(min, max)` class-validator. Memakai `value.length` (unit UTF-16 / jumlah
/// elemen array): nilai falsy atau `length < min` → "longer", `length > max` → "shorter", selain
/// itu keduanya.
pub fn length_message(
    field: &str,
    v: Option<&serde_json::Value>,
    min: usize,
    max: usize,
) -> String {
    use serde_json::Value;
    let falsy = match v {
        None | Some(Value::Null | Value::Bool(false)) => true,
        Some(Value::String(s)) => s.is_empty(),
        Some(Value::Number(n)) => n.as_f64() == Some(0.0),
        _ => false,
    };
    let len = match v {
        Some(Value::String(s)) => Some(s.encode_utf16().count()),
        Some(Value::Array(a)) => Some(a.len()),
        _ => None,
    };
    if falsy || len.is_some_and(|l| l < min) {
        format!("{field} must be longer than or equal to {min} characters")
    } else if len.is_some_and(|l| l > max) {
        format!("{field} must be shorter than or equal to {max} characters")
    } else {
        format!(
            "{field} must be longer than or equal to {min} and shorter than or equal to {max} characters"
        )
    }
}

/// `@Length(min, max)`: `None` bila string dengan panjang validator.js `min..=max`, selain itu
/// [`length_message`].
pub fn length_error(
    field: &str,
    v: Option<&serde_json::Value>,
    min: usize,
    max: usize,
) -> Option<String> {
    if let Some(serde_json::Value::String(s)) = v
        && (min..=max).contains(&validator_js_length(s))
    {
        return None;
    }
    Some(length_message(field, v, min, max))
}

/// Validasi gaya class-validator. Kembalikan daftar pesan; kosong = valid.
pub trait Validate {
    fn validate(&self) -> Vec<String> {
        Vec::new()
    }
}

/// Pengumpul pesan dengan bunyi class-validator.
#[derive(Debug, Default)]
pub struct Errors(pub Vec<String>);

impl Errors {
    pub fn new() -> Self {
        Self(Vec::new())
    }
    pub fn push(&mut self, msg: impl Into<String>) {
        self.0.push(msg.into());
    }
    /// `@IsNotEmpty()`.
    pub fn not_empty(&mut self, field: &str, v: Option<&str>) {
        if v.is_none_or(str::is_empty) {
            self.push(format!("{field} should not be empty"));
        }
    }
    /// `@IsEmail()`.
    pub fn email(&mut self, field: &str, v: Option<&str>) {
        if let Some(v) = v
            && !is_email(v)
        {
            self.push(format!("{field} must be an email"));
        }
    }
    /// `@MinLength(n)`.
    pub fn min_len(&mut self, field: &str, v: Option<&str>, n: usize) {
        if let Some(v) = v
            && v.chars().count() < n
        {
            self.push(format!(
                "{field} must be longer than or equal to {n} characters"
            ));
        }
    }
    /// `@MaxLength(n)`.
    pub fn max_len(&mut self, field: &str, v: Option<&str>, n: usize) {
        if let Some(v) = v
            && v.chars().count() > n
        {
            self.push(format!(
                "{field} must be shorter than or equal to {n} characters"
            ));
        }
    }
    /// `@IsUUID()` (validator.js `all`; BUKAN `Uuid::parse_str`, yang juga menerima bentuk
    /// tanpa tanda hubung dan versi apa pun).
    pub fn uuid(&mut self, field: &str, v: Option<&str>) {
        if let Some(v) = v
            && !crate::uuid_rules::is_uuid(v)
        {
            self.push(format!("{field} must be a UUID"));
        }
    }
    pub fn into_vec(self) -> Vec<String> {
        self.0
    }
}

/// Perkiraan `isEmail` validator.js (cukup untuk kontrak, bukan RFC penuh).
pub fn is_email(v: &str) -> bool {
    let Some((local, domain)) = v.rsplit_once('@') else {
        return false;
    };
    // Batas panjang validator.js: `str.length > 254`, `isByteLength(user, {max: 64})`,
    // `isByteLength(domain, {max: 254})`, `isFQDN` label ≤ 63.
    v.encode_utf16().count() <= 254
        && local.len() <= 64
        && domain.len() <= 254
        && domain.split('.').all(|l| l.encode_utf16().count() <= 63)
        && !local.is_empty()
        && !v.chars().any(char::is_whitespace)
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && domain.rsplit('.').next().is_some_and(|tld| tld.len() >= 2)
}

fn serde_message(err: &serde_json::Error) -> String {
    // Hilangkan "at line X column Y" supaya pesannya mirip class-validator.
    let s = err.to_string();
    match s.find(" at line ") {
        Some(i) => s.get(..i).unwrap_or(&s).to_owned(),
        None => s,
    }
}

/// Body JSON tervalidasi.
#[derive(Debug, Clone)]
pub struct ValidatedJson<T>(pub T);

impl<S, T> FromRequest<S> for ValidatedJson<T>
where
    S: Send + Sync,
    T: DeserializeOwned + Validate,
{
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let is_json = is_json_content_type(req.headers());
        let bytes = axum::body::Bytes::from_request(req, state)
            .await
            .map_err(bytes_rejection)?;
        // express.json `strict: true`.
        let mut value = parse_express_json(is_json, &bytes)?;
        if value.is_array() {
            // Nest `ValidationPipe`: array → `plainToClass` menghasilkan array, lalu
            // `entity.constructor = metatype` sehingga class-validator memeriksa properti
            // DTO pada array itu (semuanya `undefined`) = sama dengan body `{}`. Tanpa ini
            // serde mengisi field struct dari array berdasarkan urutan (`["x"]` → field 1).
            value = serde_json::Value::Object(serde_json::Map::new());
        }
        let parsed: T = serde_json::from_value(value)
            .map_err(|e| ApiError::Validation(vec![serde_message(&e)]))?;
        let errors = parsed.validate();
        if !errors.is_empty() {
            return Err(ApiError::Validation(errors));
        }
        Ok(Self(parsed))
    }
}

/// Query string tervalidasi. Kunci & nilai diparse seperti `req.query` Express
/// ([`crate::qs`]: kunci berulang/`k[]`/`k[n]` → array, `k[x]` → objek, `__proto__` dibuang,
/// `parameterLimit` 1000), lalu diberikan ke serde sebagai objek JSON: nilai tunggal = string,
/// array/objek tetap array/objek (field `Option<String>` menolaknya → 400). Gunakan
/// [`de::opt_num`] untuk angka opsional (`@Transform(parseInt)`).
#[derive(Debug, Clone)]
pub struct ValidatedQuery<T>(pub T);

impl<S, T> FromRequestParts<S> for ValidatedQuery<T>
where
    S: Send + Sync,
    T: DeserializeOwned + Validate,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let parsed: T = query_from_str(parts.uri.query().unwrap_or_default())?;
        let errors = parsed.validate();
        if !errors.is_empty() {
            return Err(ApiError::Validation(errors));
        }
        Ok(Self(parsed))
    }
}

/// Query mentah → `req.query` (objek JSON) → `T`.
pub fn query_from_str<T: DeserializeOwned>(raw: &str) -> Result<T, ApiError> {
    let map: serde_json::Map<String, serde_json::Value> = crate::qs::parse(raw)
        .into_iter()
        .map(|(k, v)| {
            let v = match v {
                crate::qs::QVal::One(s) => serde_json::Value::String(s),
                crate::qs::QVal::Many(items) => serde_json::Value::Array(
                    items.into_iter().map(serde_json::Value::String).collect(),
                ),
                crate::qs::QVal::Obj => serde_json::Value::Object(serde_json::Map::new()),
            };
            (k, v)
        })
        .collect();
    serde_json::from_value(serde_json::Value::Object(map))
        .map_err(|e| ApiError::Validation(vec![format!("Failed to deserialize query string: {e}")]))
}

/// `@IsInt() @Min(min) @Max(max)` atas hasil `@Type(() => Number)` (urutan pesan Max, Min,
/// IsInt = dekorator dari bawah). `num = None` = bukan number (tidak dikirim sebagai string
/// tunggal). Mengembalikan bilangan bulat bila ketiganya lolos.
pub fn check_int_range(
    errors: &mut Vec<String>,
    field: &str,
    num: Option<f64>,
    min: i64,
    max: i64,
) -> Option<i64> {
    let before = errors.len();
    #[allow(clippy::cast_precision_loss)]
    let (minf, maxf) = (min as f64, max as f64);
    if num.is_none_or(|n| n.is_nan() || n > maxf) {
        errors.push(format!("{field} must not be greater than {max}"));
    }
    if num.is_none_or(|n| n.is_nan() || n < minf) {
        errors.push(format!("{field} must not be less than {min}"));
    }
    let int = num.filter(|n| n.is_finite() && n.fract() == 0.0);
    if int.is_none() {
        errors.push(format!("{field} must be an integer number"));
    }
    #[allow(clippy::cast_possible_truncation)]
    int.filter(|_| errors.len() == before).map(|n| n as i64)
}

/// Panjang `isLength` validator.js 13 (dipakai `@MaxLength`/`@Length` class-validator): unit
/// UTF-16 − pasangan surrogate − (karakter + variation selector U+FE0E/U+FE0F). Emoji = 1.
/// BUKAN untuk `.length` di service Node (itu unit UTF-16 murni).
pub fn validator_js_length(s: &str) -> usize {
    let units: Vec<u16> = s.encode_utf16().collect();
    let is_vs = |u: u16| u == 0xFE0F || u == 0xFE0E;
    // `str.match(/xy/g)`: kecocokan tak tumpang tindih, dipindai per unit UTF-16.
    let count_pairs = |pred: &dyn Fn(u16, u16) -> bool| {
        let mut n = 0;
        let mut i = 0;
        while i + 1 < units.len() {
            match (units.get(i), units.get(i + 1)) {
                (Some(&a), Some(&b)) if pred(a, b) => {
                    n += 1;
                    i += 2;
                }
                _ => i += 1,
            }
        }
        n
    };
    let presentation = count_pairs(&|a, b| !is_vs(a) && is_vs(b));
    let surrogates =
        count_pairs(&|a, b| (0xD800..=0xDBFF).contains(&a) && (0xDC00..=0xDFFF).contains(&b));
    units.len().saturating_sub(presentation + surrogates)
}

/// Path param apa adanya (Nest tidak memvalidasi `:uuid` tanpa pipe).
#[derive(Debug, Clone)]
pub struct PathParams<T>(pub T);

impl<S, T> FromRequestParts<S> for PathParams<T>
where
    S: Send + Sync,
    T: DeserializeOwned + Send,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &S) -> Result<Self, Self::Rejection> {
        axum::extract::Path::<T>::from_request_parts(parts, state)
            .await
            .map(|axum::extract::Path(v)| Self(v))
            .map_err(|e| ApiError::bad_request(e.body_text()))
    }
}

/// Helper deserialisasi untuk DTO.
pub mod de {
    use serde::{Deserialize, Deserializer};

    /// Angka dari query/body yang bisa berupa string (`@Transform(parseInt)`).
    /// String tak valid → `None` (parseInt → NaN, lalu `@IsNumber` gagal di Node;
    /// handler yang peduli bisa memvalidasi ulang).
    pub fn opt_num<'de, D, N>(d: D) -> Result<Option<N>, D::Error>
    where
        D: Deserializer<'de>,
        N: std::str::FromStr + TryFrom<i64>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            N(i64),
            F(f64),
            S(String),
        }
        Ok(match Option::<Raw>::deserialize(d)? {
            None => None,
            Some(Raw::N(n)) => N::try_from(n).ok(),
            #[allow(clippy::cast_possible_truncation)]
            Some(Raw::F(f)) => N::try_from(f.trunc() as i64).ok(),
            Some(Raw::S(s)) => {
                // parseInt: ambil awalan digit.
                let t = s.trim();
                let end = t
                    .char_indices()
                    .find(|(i, c)| !(c.is_ascii_digit() || (*i == 0 && (*c == '-' || *c == '+'))))
                    .map_or(t.len(), |(i, _)| i);
                t.get(..end).and_then(|p| p.parse::<N>().ok())
            }
        })
    }

    /// Boolean dari query (`"true"`/`"false"`).
    pub fn opt_bool<'de, D: Deserializer<'de>>(d: D) -> Result<Option<bool>, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            B(bool),
            S(String),
        }
        Ok(match Option::<Raw>::deserialize(d)? {
            None => None,
            Some(Raw::B(b)) => Some(b),
            Some(Raw::S(s)) => match s.as_str() {
                "true" | "1" => Some(true),
                "false" | "0" => Some(false),
                _ => None,
            },
        })
    }
}

#[cfg(test)]
#[allow(clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn email_check() {
        assert!(is_email("a@b.co"));
        assert!(!is_email("a@b"));
        assert!(!is_email("a b@c.com"));
    }

    #[derive(serde::Deserialize)]
    struct Q {
        #[serde(default, deserialize_with = "de::opt_num")]
        n: Option<i64>,
    }

    #[derive(Debug, serde::Deserialize)]
    struct Reason {
        reason: Option<String>,
    }

    impl Validate for Reason {
        fn validate(&self) -> Vec<String> {
            let mut e = Errors::new();
            e.not_empty("reason", self.reason.as_deref());
            e.into_vec()
        }
    }

    async fn extract(body: &'static str) -> Result<Reason, ApiError> {
        let req = Request::builder()
            .header(http::header::CONTENT_TYPE, "application/json")
            .body(axum::body::Body::from(body))
            .map_err(|_| ApiError::bad_request("req"))?;
        ValidatedJson::<Reason>::from_request(req, &())
            .await
            .map(|v| v.0)
    }

    #[tokio::test]
    async fn array_body_is_validated_like_empty_object() {
        // Node: `["x"]` → `reason should not be empty`; serde tanpa perlakuan khusus
        // akan mengisi `reason = "x"` dan meloloskannya.
        match extract(r#"["x"]"#).await {
            Err(ApiError::Validation(m)) => {
                assert_eq!(m, vec!["reason should not be empty".to_owned()]);
            }
            other => panic!("harus 400 validasi, dapat {other:?}"),
        }
        assert!(matches!(extract("[]").await, Err(ApiError::Validation(_))));
        assert!(matches!(extract("\"x\"").await, Err(ApiError::BadJson(_))));
        let ok = extract(r#"{"reason":"x"}"#)
            .await
            .ok()
            .and_then(|r| r.reason);
        assert_eq!(ok.as_deref(), Some("x"));
    }

    #[test]
    fn parse_int_semantics() {
        let q: Q =
            serde_json::from_value(serde_json::json!({ "n": "12abc" })).unwrap_or(Q { n: None });
        assert_eq!(q.n, Some(12));
    }

    #[derive(Debug, Default, serde::Deserialize, PartialEq)]
    #[serde(default, rename_all = "camelCase")]
    struct PageQ {
        page_number: Option<String>,
        search: Option<String>,
        #[serde(deserialize_with = "de::opt_num")]
        limit: Option<i64>,
    }

    #[test]
    fn query_uses_express_qs() {
        let q: PageQ =
            query_from_str("pageNumber=2&search=a+b%20c&limit=10x&other=1").unwrap_or_default();
        assert_eq!(
            q,
            PageQ {
                page_number: Some("2".into()),
                search: Some("a b c".into()),
                limit: Some(10)
            }
        );
        assert_eq!(query_from_str::<PageQ>("").ok(), Some(PageQ::default()));
        // `search[]=x`, kunci berulang, dan `search[k]=x` bukan string di Express → ditolak.
        for raw in [
            "search[]=x",
            "search=a&search=b",
            "search[k]=x",
            "search[0]=x",
        ] {
            assert!(
                matches!(query_from_str::<PageQ>(raw), Err(ApiError::Validation(_))),
                "{raw}"
            );
        }
        // `__proto__` dibuang seperti qs.
        assert_eq!(
            query_from_str::<PageQ>("__proto__=1").ok(),
            Some(PageQ::default())
        );
    }
}
