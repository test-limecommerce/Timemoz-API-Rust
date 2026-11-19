# mz-files

Port `src/files/*` + `src/oss/oss.factory.ts` (Mudah-API). Klien GCS JSON API + signed URL V4
(RSA-SHA256, kompatibel byte-per-byte dengan `@google-cloud/storage`; test memakai URL keluaran Node asli).
Semua fungsi mengambil `&AppState`; env sama dengan Node: `GCP_BUCKET_NAME`, `GCP_KEY_PATH`
(tanpa itu: ADC metadata server + IAM `signBlob`), `FILE_URL_SECRET`, `HOST_BE`.
Galat GCS = `GcsError { NotFound, Other }`, otomatis jadi `ApiError` 500 lewat `?` (di Node galat GCS mentah = 500).
Semua item di-re-export di root crate (`mz_files::xxx`).

## Operasi object (`service`, `FilesService`)
- `upload_buffer(state, key, data: impl Into<reqwest::Body> /*Vec<u8>, Bytes*/, mime) -> Result<(), GcsError>`
- `download_buffer(state, key) -> Result<Vec<u8>, GcsError>` (404 = `GcsError::NotFound`)
- `delete_file(state, key) -> Result<DeleteResult{success,message}, GcsError>` (JSON = `{success:true,message:"file deleted successfully"}`)
- `get_object_info(state, key) -> Result<Option<ObjectInfo{key,content_type,size:u64,generation,etag}>, GcsError>` (`None` = 404)
- `get_object_metadata(state, key) -> Result<ObjectMetadata{exists,size_bytes:Option<u64>}, GcsError>`
- `upload_local_file(state, &Path, key, bucket, content_type)` / `download_to_file(state, key, &Path, bucket)` — di-stream disk.
- `object_key_of(state, value) -> Result<String, InvalidObjectKeyError>` (= `FilesService.toObjectKey`, bucket dari env).

## Signed URL
- `get_signed_file_url(state, key, expires_in_secs, content_type) -> Result<SignedFileUrl{success,message,url}, GcsError>` — PUT V4 (JSON Node: `{success,message:"success get presigned url",url}`).
- `get_signed_object_url(state, key, &SignedObjectOpts{content_type: Option<Option<&str>>, filename}, now_ms) -> Result<SignedReadUrl{url, expires_at}, GcsError>` — baca, jendela 10 mnt, berlaku 15-25 mnt. JSON `{url,expiresAt}`. `opts.content_type = None` ⇒ pakai metadata object (setara `undefined`); `Some(..)` ⇒ disaring `safe_content_type` + `response-content-type/disposition` ikut ditandatangani. Pakai `SignedObjectOpts::default()` + `chrono::Utc::now().timestamp_millis()` untuk panggilan polos.
- `get_signed_read_url(state, key, expires_in_secs, bucket: Option<&str>) -> Result<String, GcsError>`.
- `signed_read_window(now_ms) -> (accessible_at, expires)`, `SIGNED_URL_RESPONSE_CACHE_CONTROL` (`"private, max-age=300"`, pasang di respons `…/url`).

## Stream ke klien (`FilesService.streamObject`)
`stream_object(state, key, req_headers: Option<&HeaderMap>, StreamObjectOptions{cache_control, filename, force_attachment, info}) -> ApiResult<axum::response::Response>`
— 404 `{"message":"File not found",...}` bila object tidak ada; ETag + 304 (`If-None-Match`), `Range` 206/416, `If-Range`;
`Content-Type` disaring whitelist, `Content-Disposition`, `X-Content-Type-Options: nosniff`, `Content-Security-Policy: sandbox`,
`Cache-Control` (default `private, max-age=300`), `Accept-Ranges`. Body = `reqwest bytes_stream` → `Body::from_stream` (tidak dimuat ke memori).
Handler cukup: `Ok(mz_files::stream_object(&state, &key, Some(&headers), Default::default()).await?)` (extract `HeaderMap`).

## Pure helper
- `file_response`: `safe_content_type`, `content_disposition(DispositionType, Option<&str>)`, `etag_for`, `if_none_match_hits`, `parse_range(Option<&str>, size) -> RangeParse`, `DEFAULT_PRIVATE_CACHE_CONTROL`.
- `to_object_key(value, bucket: Option<&str>) -> Result<String, InvalidObjectKeyError>` (pesan `Invalid object key: <alasan>`).
- `sign_file_path(state, path) -> Result<String, FileUrlSecretMissingError>` (`<path>?exp=..&sig=..`, HMAC-SHA256 base64url, TTL 24 j), `verify_file_path_signature(state, path, exp: Option<&str>, sig: Option<&str>) -> bool`; varian `*_with(secret, ..., now_ms)` untuk test; `SIGNED_FILE_CACHE_CONTROL`, `SIGNED_FILE_PATH_TTL_SECONDS`, `set_unauthenticated_file_headers(&mut HeaderMap)`.
- `public_api_url(state, "/path") -> String` (`HOST_BE` + path).
- `load_file_requester(&PgPool, user_id) -> sqlx::Result<FileRequester{user_id, company_id: Option<i32>, role_code: Option<String>}>`.
- `signer::{prepare_v4, SignParams, RsaSigner}` (tingkat rendah), `gcs::{object_url, sign_url, default_bucket}`, `encode::{encode_uri_component, fixed_encode_uri_component, decode_uri_component}`.

## Catatan
- `AppState.http` punya timeout total 30 dtk; transfer byte memakai `.timeout(15 menit)` per request. Usulan core: klien HTTP khusus transfer.
- Deviasi: `plan/deviations/mz-files.md`.
