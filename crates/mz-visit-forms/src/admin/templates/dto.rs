//! DTO template admin (`dto/*.ts`) dengan validasi class-validator 0.14 + `ValidationPipe
//! ({ transform: true })`. Pesan & urutannya diverifikasi terhadap Nest asli: per properti
//! (urutan deklarasi), pesan dekorator dari bawah ke atas; `@IsOptional` melewati
//! `null`/`undefined`.

use axum::extract::{FromRequest, Request};
use mz_core::ApiError;
use mz_core::extract::validator_js_length;
use serde_json::{Map, Value};

use crate::consts::{LAYOUT_ASSET_CONTENT_TYPES, MAX_LAYOUT_ASSET_BYTES};

/// Body seperti `express.json` (strict): bukan JSON / kosong → `{}`; JSON rusak atau primitif →
/// 400 ([`mz_core::extract::parse_express_json`]). Array → `{}` (class-validator memeriksa
/// properti DTO pada array = semuanya absen).
pub struct JsonBody(pub Map<String, Value>);

impl<S: Send + Sync> FromRequest<S> for JsonBody {
    type Rejection = ApiError;

    async fn from_request(req: Request, state: &S) -> Result<Self, Self::Rejection> {
        let is_json = mz_core::extract::is_json_content_type(req.headers());
        let bytes = axum::body::Bytes::from_request(req, state)
            .await
            .map_err(mz_core::extract::bytes_rejection)?;
        match mz_core::extract::parse_express_json(is_json, &bytes)? {
            Value::Object(m) => Ok(Self(m)),
            _ => Ok(Self(Map::new())),
        }
    }
}

fn present(v: Option<&Value>) -> Option<&Value> {
    v.filter(|v| !v.is_null())
}

fn is_int(v: Option<&Value>) -> bool {
    v.and_then(Value::as_f64)
        .is_some_and(|n| n.is_finite() && n.fract() == 0.0)
}

fn num(v: Option<&Value>) -> Option<f64> {
    v.and_then(Value::as_f64)
}

/// `@IsString() @MinLength(min) @MaxLength(max)`.
fn string_len(e: &mut Vec<String>, field: &str, v: Option<&Value>, min: usize, max: usize) {
    let s = v.and_then(Value::as_str);
    if s.is_none_or(|s| validator_js_length(s) > max) {
        e.push(format!(
            "{field} must be shorter than or equal to {max} characters"
        ));
    }
    if min > 0 && s.is_none_or(|s| validator_js_length(s) < min) {
        e.push(format!(
            "{field} must be longer than or equal to {min} characters"
        ));
    }
    if s.is_none() {
        e.push(format!("{field} must be a string"));
    }
}

/// `@IsInt() @Min(min) @Max(max)` (pesan Max, Min, IsInt).
fn int_range(e: &mut Vec<String>, field: &str, v: Option<&Value>, min: f64, max: f64) {
    let n = num(v);
    if n.is_none_or(|n| n > max) {
        e.push(format!("{field} must not be greater than {max}"));
    }
    if n.is_none_or(|n| n < min) {
        e.push(format!("{field} must not be less than {min}"));
    }
    if !is_int(v) {
        e.push(format!("{field} must be an integer number"));
    }
}

fn scope_of(e: &mut Vec<String>, v: Option<&Value>) -> Option<String> {
    let v = present(v)?;
    match v.as_str() {
        Some(s @ ("all" | "targeted")) => Some(s.to_owned()),
        _ => {
            e.push("scope must be one of the following values: all, targeted".into());
            None
        }
    }
}

fn bool_of(e: &mut Vec<String>, field: &str, v: Option<&Value>) -> Option<bool> {
    let v = present(v)?;
    match v.as_bool() {
        Some(b) => Some(b),
        None => {
            e.push(format!("{field} must be a boolean value"));
            None
        }
    }
}

/// `isObject` class-validator.
fn is_object(v: Option<&Value>) -> bool {
    matches!(v, Some(Value::Object(_)))
}

fn finish<T>(e: Vec<String>, value: T) -> Result<T, ApiError> {
    if e.is_empty() {
        Ok(value)
    } else {
        Err(ApiError::Validation(e))
    }
}

/// `CreateFormTemplateDto`.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateTemplateDto {
    pub name: String,
    pub description: Option<String>,
    pub sort_order: Option<i32>,
    pub scope: Option<String>,
    pub is_active: Option<bool>,
    /// `None` = absen / `null`.
    pub schema: Option<Value>,
}

pub fn parse_create(b: &Map<String, Value>) -> Result<CreateTemplateDto, ApiError> {
    let mut e = Vec::new();
    string_len(&mut e, "name", b.get("name"), 1, 120);
    let description = present(b.get("description"));
    if description.is_some() {
        string_len(&mut e, "description", description, 0, 500);
    }
    let sort_order = present(b.get("sortOrder"));
    if sort_order.is_some() {
        int_range(&mut e, "sortOrder", sort_order, 0.0, 9999.0);
    }
    let scope = scope_of(&mut e, b.get("scope"));
    let is_active = bool_of(&mut e, "isActive", b.get("isActive"));
    let schema = present(b.get("schema"));
    if schema.is_some() && !is_object(schema) {
        e.push("schema must be an object".into());
    }
    let dto = CreateTemplateDto {
        name: b
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        description: description.and_then(Value::as_str).map(str::to_owned),
        sort_order: num(sort_order).map(|n| n as i32),
        scope,
        is_active,
        schema: schema.cloned(),
    };
    finish(e, dto)
}

/// `UpdateFormTemplateDto` — field `None` = tidak dikirim (atau `null`, yang
/// `@IsOptional` loloskan; lihat [`UpdateTemplateDto::description_null`]).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct UpdateTemplateDto {
    pub name: Option<String>,
    pub description: Option<String>,
    /// `description: null` dikirim: Prisma menulis NULL (`dto.description !== undefined`).
    pub description_null: bool,
    pub sort_order: Option<i32>,
    pub scope: Option<String>,
    pub is_active: Option<bool>,
    /// Kunci yang dikirim bernilai `null` untuk kolom NOT NULL (Prisma menolak → 500).
    pub null_required: Vec<&'static str>,
}

pub fn parse_update(b: &Map<String, Value>) -> Result<UpdateTemplateDto, ApiError> {
    let mut e = Vec::new();
    let mut dto = UpdateTemplateDto::default();
    let name = present(b.get("name"));
    if name.is_some() {
        string_len(&mut e, "name", name, 1, 120);
    }
    dto.name = name.and_then(Value::as_str).map(str::to_owned);
    let description = present(b.get("description"));
    if description.is_some() {
        string_len(&mut e, "description", description, 0, 500);
    }
    dto.description = description.and_then(Value::as_str).map(str::to_owned);
    dto.description_null = matches!(b.get("description"), Some(Value::Null));
    let sort_order = present(b.get("sortOrder"));
    if sort_order.is_some() {
        int_range(&mut e, "sortOrder", sort_order, 0.0, 9999.0);
    }
    dto.sort_order = num(sort_order).map(|n| n as i32);
    dto.scope = scope_of(&mut e, b.get("scope"));
    dto.is_active = bool_of(&mut e, "isActive", b.get("isActive"));
    for (key, field) in [
        ("name", "name"),
        ("sortOrder", "sortOrder"),
        ("scope", "scope"),
        ("isActive", "isActive"),
    ] {
        if matches!(b.get(key), Some(Value::Null)) {
            dto.null_required.push(field);
        }
    }
    finish(e, dto)
}

/// `ListFormTemplatesQueryDto` (query string Express).
#[derive(Debug, Clone, PartialEq)]
pub struct ListTemplatesQuery {
    pub q: Option<String>,
    pub is_active: Option<bool>,
    pub page: i64,
    pub limit: i64,
}

/// `parseInt(value, 10)` untuk nilai `req.query` (array → `String(array)`).
fn parse_int_query(v: &Value) -> Option<i64> {
    let s = match v {
        Value::String(s) => s.clone(),
        Value::Array(a) => a
            .iter()
            .map(|x| x.as_str().unwrap_or_default().to_owned())
            .collect::<Vec<_>>()
            .join(","),
        _ => "[object Object]".to_owned(),
    };
    mz_core::jsfmt::parse_int(&s)
}

pub fn parse_list_query(raw: &str) -> Result<ListTemplatesQuery, ApiError> {
    let q: Map<String, Value> = mz_core::extract::query_from_str(raw)?;
    let mut e = Vec::new();
    // q: @IsOptional @IsString @MaxLength(120).
    let q_val = present(q.get("q"));
    if q_val.is_some() {
        let s = q_val.and_then(Value::as_str);
        if s.is_none_or(|s| validator_js_length(s) > 120) {
            e.push("q must be shorter than or equal to 120 characters".to_owned());
        }
        if s.is_none() {
            e.push("q must be a string".to_owned());
        }
    }
    // isActive: Transform 'true'/'false' → boolean, lalu @IsOptional @IsBoolean.
    let is_active = match q.get("isActive") {
        None => None,
        Some(Value::String(s)) if s == "true" => Some(true),
        Some(Value::String(s)) if s == "false" => Some(false),
        Some(_) => {
            e.push("isActive must be a boolean value".to_owned());
            None
        }
    };
    // page/limit: parseInt, NaN gagal semua; urutan Max, Min, IsInt.
    let page = q.get("page").map_or(Some(1), parse_int_query);
    if page.is_none_or(|p| p < 1) {
        e.push("page must not be less than 1".to_owned());
    }
    if page.is_none() {
        e.push("page must be an integer number".to_owned());
    }
    let limit = q.get("limit").map_or(Some(20), parse_int_query);
    if limit.is_none_or(|l| l > 100) {
        e.push("limit must not be greater than 100".to_owned());
    }
    if limit.is_none_or(|l| l < 1) {
        e.push("limit must not be less than 1".to_owned());
    }
    if limit.is_none() {
        e.push("limit must be an integer number".to_owned());
    }
    let dto = ListTemplatesQuery {
        q: q_val.and_then(Value::as_str).map(str::to_owned),
        is_active,
        page: page.unwrap_or(1),
        limit: limit.unwrap_or(20),
    };
    finish(e, dto)
}

/// `ReplaceFormAssigneesDto` (`@IsArray @ArrayMaxSize(1000) @IsUUID('4', { each })`).
pub fn parse_replace_assignees(b: &Map<String, Value>) -> Result<Vec<String>, ApiError> {
    let mut e = Vec::new();
    let v = b.get("userUuids");
    let uuids_ok = match v {
        Some(Value::Array(a)) => a
            .iter()
            .all(|x| x.as_str().is_some_and(mz_core::uuid_rules::is_uuid_v4)),
        Some(Value::String(s)) => mz_core::uuid_rules::is_uuid_v4(s),
        _ => false,
    };
    if !uuids_ok {
        e.push("each value in userUuids must be a UUID".to_owned());
    }
    let arr = v.and_then(Value::as_array);
    if arr.is_none_or(|a| a.len() > 1000) {
        e.push("userUuids must contain no more than 1000 elements".to_owned());
    }
    if arr.is_none() {
        e.push("userUuids must be an array".to_owned());
    }
    let out = arr
        .map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    finish(e, out)
}

/// `UpdateFormVersionSchemaDto`.
#[derive(Debug, Clone, PartialEq)]
pub struct UpdateVersionSchemaDto {
    pub schema: Value,
    pub expected_template_updated_at: i64,
}

/// `@Type(() => Number)` class-transformer: string → `Number(s)`, boolean → 0/1, number apa
/// adanya; `null`/array/objek tidak menjadi angka yang lolos.
fn type_number(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => Some(mz_core::jsfmt::number(s)),
        Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    }
}

pub fn parse_update_version_schema(
    b: &Map<String, Value>,
) -> Result<UpdateVersionSchemaDto, ApiError> {
    let mut e = Vec::new();
    let schema = b.get("schema");
    if !is_object(schema) {
        e.push("schema must be an object".to_owned());
    }
    let n = type_number(b.get("expectedTemplateUpdatedAt"));
    if n.is_none_or(|n| n.is_nan() || n < 0.0) {
        e.push("expectedTemplateUpdatedAt must not be less than 0".to_owned());
    }
    let int = n.filter(|n| n.is_finite() && n.fract() == 0.0);
    if int.is_none() {
        e.push("expectedTemplateUpdatedAt must be an integer number".to_owned());
    }
    let dto = UpdateVersionSchemaDto {
        schema: schema.cloned().unwrap_or(Value::Null),
        expected_template_updated_at: int.map_or(0, |n| n as i64),
    };
    finish(e, dto)
}

/// `ValidateFormSchemaDto` (`@IsObject() schema`).
pub fn parse_schema_body(b: &Map<String, Value>) -> Result<Value, ApiError> {
    match b.get("schema") {
        Some(v @ Value::Object(_)) => Ok(v.clone()),
        _ => Err(ApiError::Validation(vec![
            "schema must be an object".to_owned(),
        ])),
    }
}

/// `CreateFormTemplateAssetUploadDto`.
#[derive(Debug, Clone, PartialEq)]
pub struct AssetUploadDto {
    pub content_type: String,
    pub size_bytes: i64,
}

pub fn parse_asset_upload(b: &Map<String, Value>) -> Result<AssetUploadDto, ApiError> {
    let mut e = Vec::new();
    let ct = b
        .get("contentType")
        .and_then(Value::as_str)
        .filter(|c| LAYOUT_ASSET_CONTENT_TYPES.contains(c));
    if ct.is_none() {
        e.push(format!(
            "contentType must be one of the following values: {}",
            LAYOUT_ASSET_CONTENT_TYPES.join(", ")
        ));
    }
    let size = b.get("sizeBytes");
    #[allow(clippy::cast_precision_loss)]
    int_range(
        &mut e,
        "sizeBytes",
        size,
        1.0,
        MAX_LAYOUT_ASSET_BYTES as f64,
    );
    let dto = AssetUploadDto {
        content_type: ct.unwrap_or_default().to_owned(),
        size_bytes: num(size).map_or(0, |n| n as i64),
    };
    finish(e, dto)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    fn errs<T: std::fmt::Debug>(r: Result<T, ApiError>) -> Vec<String> {
        match r {
            Err(ApiError::Validation(m)) => m,
            other => panic!("bukan galat validasi: {other:?}"),
        }
    }

    // Semua harapan di bawah adalah keluaran `ValidationPipe({ transform: true })` asli.
    #[test]
    fn create_messages() {
        assert_eq!(
            errs(parse_create(&obj(json!({})))),
            [
                "name must be shorter than or equal to 120 characters",
                "name must be longer than or equal to 1 characters",
                "name must be a string"
            ]
        );
        assert_eq!(
            errs(parse_create(&obj(json!({
                "name": 5, "description": 7, "sortOrder": "x", "scope": "x",
                "isActive": "true", "schema": []
            })))),
            [
                "name must be shorter than or equal to 120 characters",
                "name must be longer than or equal to 1 characters",
                "name must be a string",
                "description must be shorter than or equal to 500 characters",
                "description must be a string",
                "sortOrder must not be greater than 9999",
                "sortOrder must not be less than 0",
                "sortOrder must be an integer number",
                "scope must be one of the following values: all, targeted",
                "isActive must be a boolean value",
                "schema must be an object"
            ]
        );
        assert_eq!(
            errs(parse_create(&obj(json!({
                "name": "", "sortOrder": 1.5, "schema": null, "description": null
            })))),
            [
                "name must be longer than or equal to 1 characters",
                "sortOrder must be an integer number"
            ]
        );
        let ok = parse_create(&obj(
            json!({"name": "A", "scope": "targeted", "schema": {"x": 1}}),
        ))
        .unwrap();
        assert_eq!(ok.scope.as_deref(), Some("targeted"));
        assert_eq!(ok.schema, Some(json!({"x": 1})));
    }

    #[test]
    fn update_messages() {
        assert_eq!(
            errs(parse_update(&obj(
                json!({"sortOrder": 10000, "name": "a".repeat(121)})
            ))),
            [
                "name must be shorter than or equal to 120 characters",
                "sortOrder must not be greater than 9999"
            ]
        );
        let ok = parse_update(&obj(json!({"description": null, "isActive": false}))).unwrap();
        assert!(ok.description_null);
        assert_eq!(ok.is_active, Some(false));
    }

    #[test]
    fn list_query_messages() {
        assert_eq!(
            errs(parse_list_query("page=abc&limit=500&isActive=yes&q=a&q=b")),
            [
                "q must be shorter than or equal to 120 characters",
                "q must be a string",
                "isActive must be a boolean value",
                "page must not be less than 1",
                "page must be an integer number",
                "limit must not be greater than 100"
            ]
        );
        assert_eq!(
            errs(parse_list_query("page=0&limit=0&isActive=")),
            [
                "isActive must be a boolean value",
                "page must not be less than 1",
                "limit must not be less than 1"
            ]
        );
        let ok = parse_list_query("page=2x&isActive=false&q=x").unwrap();
        assert_eq!(
            ok,
            ListTemplatesQuery {
                q: Some("x".into()),
                is_active: Some(false),
                page: 2,
                limit: 20
            }
        );
    }

    #[test]
    fn assignee_messages() {
        let all = [
            "each value in userUuids must be a UUID",
            "userUuids must contain no more than 1000 elements",
            "userUuids must be an array",
        ];
        assert_eq!(errs(parse_replace_assignees(&obj(json!({})))), all);
        assert_eq!(
            errs(parse_replace_assignees(&obj(json!({"userUuids": "abc"})))),
            all
        );
        assert_eq!(
            errs(parse_replace_assignees(&obj(json!({
                "userUuids": ["4d1a2b3c-5e6f-4a7b-8c9d-0e1f2a3b4c5d", "x", 5]
            })))),
            ["each value in userUuids must be a UUID"]
        );
        assert!(
            parse_replace_assignees(&obj(json!({"userUuids": []})))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn version_schema_messages() {
        let both = [
            "expectedTemplateUpdatedAt must not be less than 0",
            "expectedTemplateUpdatedAt must be an integer number",
        ];
        let mut first = vec!["schema must be an object".to_owned()];
        first.extend(both.iter().map(|s| (*s).to_owned()));
        assert_eq!(errs(parse_update_version_schema(&obj(json!({})))), first);
        for (v, expect) in [
            (json!("123"), Some(123)),
            (json!(true), Some(1)),
            (json!(""), Some(0)),
            (json!(null), None),
            (json!(-1.5), None),
            (json!([]), None),
        ] {
            let r = parse_update_version_schema(&obj(
                json!({"schema": {}, "expectedTemplateUpdatedAt": v}),
            ));
            match expect {
                Some(n) => assert_eq!(r.unwrap().expected_template_updated_at, n),
                None => assert_eq!(errs(r), both),
            }
        }
    }

    #[test]
    fn asset_messages() {
        assert_eq!(
            errs(parse_asset_upload(&obj(json!({})))),
            [
                "contentType must be one of the following values: image/png, image/jpeg",
                "sizeBytes must not be greater than 1048576",
                "sizeBytes must not be less than 1",
                "sizeBytes must be an integer number"
            ]
        );
        assert_eq!(
            errs(parse_asset_upload(&obj(
                json!({"contentType": "image/gif", "sizeBytes": 2000000})
            ))),
            [
                "contentType must be one of the following values: image/png, image/jpeg",
                "sizeBytes must not be greater than 1048576"
            ]
        );
        assert_eq!(
            parse_asset_upload(&obj(json!({"contentType": "image/png", "sizeBytes": 10}))).unwrap(),
            AssetUploadDto {
                content_type: "image/png".into(),
                size_bytes: 10
            }
        );
    }
}
