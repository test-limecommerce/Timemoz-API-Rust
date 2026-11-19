//! Port `products/product-imports.service.ts` + 4 route import di `admin-products.controller.ts`
//! + `import-file-exception.filter.ts` + `product-import-template.generator.ts`.

use std::time::Duration;

use axum::Json;
use axum::extract::{FromRequest, Multipart, Path, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use mz_core::company::require_company_id;
use mz_core::extract::{ValidatedQuery, query_from_str};
use mz_core::queue::{self, JobOpts};
use mz_core::time::now_naive;
use mz_core::{ApiError, ApiResult, AppState, RequestCtx};
use rust_xlsxwriter::{Color, Format, Workbook};
use serde_json::{Value, json};
use sqlx::types::Json as SqlJson;

use super::columns::{MAX_IMPORT_FILE_BYTES, PRODUCT_IMPORT_QUEUE, build_import_headers};
use crate::common::{coded, internal, ms, ms_opt, not_found, offset, page_meta, row_uuid};
use crate::dto::{Raw, page_limit_max, query_bool};

pub const XLSX_CONTENT_TYPE: &str =
    "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet";

/// Batas body route import (file 10 MB + ruang untuk pembungkus multipart).
pub const IMPORT_BODY_LIMIT: usize = MAX_IMPORT_FILE_BYTES + 1024 * 1024;

fn file_too_large() -> ApiError {
    coded(
        StatusCode::PAYLOAD_TOO_LARGE,
        "IMPORT_FILE_TOO_LARGE",
        "File exceeds the 10 MB limit",
    )
}

fn invalid_file(message: &str) -> ApiError {
    coded(
        StatusCode::UNPROCESSABLE_ENTITY,
        "IMPORT_INVALID_FILE",
        message,
    )
}

/// Berkas yang diterima multer (`originalname`, `buffer`).
pub struct UploadedFile {
    pub original_name: String,
    pub buffer: Vec<u8>,
}

/// `FileInterceptor('file', { limits: { fileSize: 10 MB } })` + `ImportFileExceptionFilter`.
async fn read_upload(state: &AppState, req: Request) -> ApiResult<Option<UploadedFile>> {
    let is_multipart = req
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.to_ascii_lowercase().starts_with("multipart/form-data"));
    if !is_multipart {
        return Ok(None);
    }
    let mut mp = Multipart::from_request(req, state)
        .await
        .map_err(|e| internal(&e.body_text()))?;
    let mut file: Option<UploadedFile> = None;
    loop {
        let field = match mp.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) if e.status() == StatusCode::PAYLOAD_TOO_LARGE => return Err(file_too_large()),
            Err(e) => return Err(internal(&e.body_text())),
        };
        let Some(file_name) = field.file_name().map(str::to_owned) else {
            continue; // field teks biasa — diabaikan multer
        };
        if field.name() != Some("file") || file.is_some() {
            // `LIMIT_UNEXPECTED_FILE`.
            return Err(invalid_file("Unexpected field"));
        }
        let mut field = field;
        let mut buffer = Vec::new();
        loop {
            match field.chunk().await {
                Ok(Some(c)) => {
                    if buffer.len() + c.len() > MAX_IMPORT_FILE_BYTES {
                        return Err(file_too_large());
                    }
                    buffer.extend_from_slice(&c);
                }
                Ok(None) => break,
                Err(e) if e.status() == StatusCode::PAYLOAD_TOO_LARGE => {
                    return Err(file_too_large());
                }
                Err(e) => return Err(internal(&e.body_text())),
            }
        }
        file = Some(UploadedFile {
            original_name: file_name,
            buffer,
        });
    }
    Ok(file)
}

/// Node: src/products/product-imports.service.ts:91 `upload`.
pub async fn upload(
    state: &AppState,
    user_id: i32,
    file: Option<UploadedFile>,
    dry_run: bool,
) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let Some(file) = file.filter(|f| !f.buffer.is_empty()) else {
        return Err(invalid_file("No file uploaded"));
    };
    if !file.original_name.to_lowercase().ends_with(".xlsx") {
        return Err(invalid_file("File must have a .xlsx extension"));
    }
    let object_key = format!("product-imports/{company_id}/{}.xlsx", uuid::Uuid::new_v4());
    mz_files::upload_buffer(state, &object_key, file.buffer, XLSX_CONTENT_TYPE).await?;
    let (id, uuid, status): (i32, String, String) = sqlx::query_as(
        r#"INSERT INTO "productImportJob" ("companyId", "createdById", "fileName", "objectKey", "dryRun", status)
           VALUES ($1, $2, $3, $4, $5, 'queued'::"ProductImportStatus")
           RETURNING id, uuid::text, status::text"#,
    )
    .bind(company_id)
    .bind(user_id)
    .bind(&file.original_name)
    .bind(&object_key)
    .bind(dry_run)
    .fetch_one(&state.db)
    .await?;
    let enqueued = queue::enqueue(
        state,
        PRODUCT_IMPORT_QUEUE,
        Some(format!("product-import:{uuid}")),
        json!({ "jobId": id }),
        JobOpts::new(1, Duration::ZERO),
    )
    .await;
    if let Err(e) = enqueued {
        let message = e.to_string();
        tracing::error!("Failed to enqueue import job {uuid}: {message}");
        let now = now_naive();
        sqlx::query(
            r#"UPDATE "productImportJob" SET status = 'failed'::"ProductImportStatus", errors = $2,
                 "finishedAt" = $3, "updatedAt" = $3 WHERE id = $1"#,
        )
        .bind(id)
        .bind(SqlJson(json!([{
            "row": 0, "column": null, "code": "IMPORT_ENQUEUE_FAILED", "message": message
        }])))
        .bind(now)
        .execute(&state.db)
        .await?;
        return Err(ApiError::internal(e));
    }
    Ok(json!({ "id": uuid, "status": status }))
}

#[derive(sqlx::FromRow)]
struct JobEntityRow {
    uuid: String,
    #[sqlx(rename = "fileName")]
    file_name: String,
    #[sqlx(rename = "dryRun")]
    dry_run: bool,
    status: String,
    #[sqlx(rename = "totalRows")]
    total_rows: i32,
    processed: i32,
    succeeded: i32,
    failed: i32,
    errors: Value,
    warnings: Value,
    #[sqlx(rename = "startedAt")]
    started_at: Option<chrono::NaiveDateTime>,
    #[sqlx(rename = "finishedAt")]
    finished_at: Option<chrono::NaiveDateTime>,
    #[sqlx(rename = "createdAt")]
    created_at: chrono::NaiveDateTime,
}

const JOB_COLS: &str = r#"uuid::text AS uuid, "fileName", "dryRun", status::text AS status, "totalRows",
    processed, succeeded, failed, errors, warnings, "startedAt", "finishedAt", "createdAt""#;

fn to_entity(r: &JobEntityRow) -> Value {
    json!({
        "id": r.uuid,
        "fileName": r.file_name,
        "dryRun": r.dry_run,
        "status": r.status,
        "totalRows": r.total_rows,
        "processed": r.processed,
        "succeeded": r.succeeded,
        "failed": r.failed,
        "errors": r.errors,
        "warnings": r.warnings,
        "startedAt": ms_opt(r.started_at),
        "finishedAt": ms_opt(r.finished_at),
        "createdAt": ms(r.created_at),
    })
}

/// Node: src/products/product-imports.service.ts:141 `list`.
pub async fn list(state: &AppState, user_id: i32, page: i64, limit: i64) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let rows: Vec<JobEntityRow> = sqlx::query_as(&format!(
        r#"SELECT {JOB_COLS} FROM "productImportJob" WHERE "companyId" = $1
           ORDER BY "createdAt" DESC, id DESC LIMIT $2 OFFSET $3"#
    ))
    .bind(company_id)
    .bind(limit)
    .bind(offset(page, limit))
    .fetch_all(&state.db)
    .await?;
    let total: i64 =
        sqlx::query_scalar(r#"SELECT COUNT(*) FROM "productImportJob" WHERE "companyId" = $1"#)
            .bind(company_id)
            .fetch_one(&state.db)
            .await?;
    let data: Vec<Value> = rows.iter().map(to_entity).collect();
    Ok(json!({ "data": data, "meta": page_meta(total, page, limit) }))
}

/// Node: src/products/product-imports.service.ts:158 `detail`.
pub async fn detail(state: &AppState, user_id: i32, raw: &str) -> ApiResult<Value> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let row: Option<JobEntityRow> = sqlx::query_as(&format!(
        r#"SELECT {JOB_COLS} FROM "productImportJob" WHERE uuid = $1 AND "companyId" = $2 LIMIT 1"#
    ))
    .bind(row_uuid(raw)?)
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?;
    let row = row.ok_or_else(|| not_found("PRODUCT_IMPORT_NOT_FOUND", "Import job not found"))?;
    Ok(to_entity(&row))
}

/// `buildProductImportTemplateXlsx(activeCustomFieldKeys)`: hanya baris header.
pub fn build_template_xlsx(active_keys: &[String]) -> Result<Vec<u8>, rust_xlsxwriter::XlsxError> {
    let headers = build_import_headers(active_keys);
    let mut wb = Workbook::new();
    let ws = wb.add_worksheet();
    ws.set_name("Products")?;
    let fmt = Format::new()
        .set_bold()
        .set_font_color(Color::White)
        .set_background_color(Color::RGB(0x1a56db));
    for (i, h) in headers.iter().enumerate() {
        let col = u16::try_from(i).unwrap_or(u16::MAX);
        ws.write_string_with_format(0, col, h, &fmt)?;
    }
    wb.save_to_buffer()
}

/// Node: src/products/product-imports.service.ts:166 `template`.
pub async fn template(state: &AppState, user_id: i32) -> ApiResult<Vec<u8>> {
    let company_id = require_company_id(&state.db, user_id).await?;
    let keys: Vec<String> = sqlx::query_scalar(
        r#"SELECT key FROM "productCustomField"
           WHERE "companyId" = $1 AND entity = 'product'::"CustomFieldEntity" AND "isActive"
           ORDER BY "sortOrder" ASC, id ASC LIMIT 1000"#,
    )
    .bind(company_id)
    .fetch_all(&state.db)
    .await?;
    build_template_xlsx(&keys).map_err(ApiError::internal)
}

fn xlsx_response(buffer: Vec<u8>, filename: &str) -> Response {
    let mut res = buffer.into_response();
    let h = res.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(XLSX_CONTENT_TYPE),
    );
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\"")) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    res
}

// ───────────────────────── handler ─────────────────────────

/// Node: src/products/admin-products.controller.ts:264 `importProducts` (202).
pub async fn h_import(
    State(state): State<AppState>,
    ctx: RequestCtx,
    req: Request,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let raw_query = req.uri().query().unwrap_or_default().to_owned();
    // Interceptor multer (413/422) berjalan sebelum ValidationPipe query.
    let file = read_upload(&state, req).await?;
    let q: Raw = query_from_str(&raw_query)?;
    let mut errs = Vec::new();
    let dry_run = query_bool(&q.0, &mut errs, "dryRun");
    if !errs.is_empty() {
        return Err(ApiError::Validation(errs));
    }
    let data = upload(
        &state,
        ctx.require_user_id()?,
        file,
        dry_run.unwrap_or(false),
    )
    .await?;
    Ok((StatusCode::ACCEPTED, Json(json!({ "data": data }))))
}

/// Node: src/products/admin-products.controller.ts:295 `listImports`.
pub async fn h_list(
    State(state): State<AppState>,
    ctx: RequestCtx,
    ValidatedQuery(q): ValidatedQuery<Raw>,
) -> ApiResult<Json<Value>> {
    let mut errs = Vec::new();
    let (page, limit) = page_limit_max(&q.0, &mut errs, 100);
    if !errs.is_empty() {
        return Err(ApiError::Validation(errs));
    }
    Ok(Json(
        list(&state, ctx.require_user_id()?, page, limit).await?,
    ))
}

/// Node: src/products/admin-products.controller.ts:308 `importDetail`.
pub async fn h_detail(
    State(state): State<AppState>,
    ctx: RequestCtx,
    Path(id): Path<String>,
) -> ApiResult<Json<Value>> {
    let data = detail(&state, ctx.require_user_id()?, &id).await?;
    Ok(Json(json!({ "data": data })))
}

/// Node: src/products/admin-products.controller.ts:319 `importTemplate`.
pub async fn h_template(State(state): State<AppState>, ctx: RequestCtx) -> ApiResult<Response> {
    let buffer = template(&state, ctx.require_user_id()?).await?;
    Ok(xlsx_response(buffer, "product-import-template.xlsx"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use calamine::{Data, Reader, Xlsx};

    // product-import-template.generator.spec.ts
    #[test]
    fn template_hanya_header() {
        let buf = build_template_xlsx(&["shelf_life_days".into()]).unwrap();
        let mut wb: Xlsx<_> = Xlsx::new(std::io::Cursor::new(buf)).unwrap();
        assert_eq!(wb.sheet_names(), vec!["Products".to_owned()]);
        let range = wb.worksheet_range_at(0).unwrap().unwrap();
        assert_eq!(range.height(), 1);
        let row: Vec<String> = range
            .rows()
            .next()
            .unwrap()
            .iter()
            .map(|c| match c {
                Data::String(s) => s.clone(),
                _ => String::new(),
            })
            .collect();
        assert_eq!(row, build_import_headers(&["shelf_life_days".into()]));
        assert!(row.contains(&"cf:shelf_life_days".to_owned()));
    }
}
