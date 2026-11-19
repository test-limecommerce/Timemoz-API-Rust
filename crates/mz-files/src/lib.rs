//! Port `src/files/*` + `src/oss/oss.factory.ts` dari Mudah-API: akses object storage
//! GCS (upload/download/delete/stream), signed URL V4, `object-key`, signed file path
//! (HMAC), `public-api-url`, `file-requester`, `file-response`.
//!
//! Daftar API publik: `README.md`. Deviasi: `plan/deviations/mz-files.md`.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::indexing_slicing,
        clippy::panic
    )
)]

pub mod encode;
pub mod file_requester;
pub mod file_response;
pub mod gcs;
pub mod object_key;
pub mod public_api_url;
pub mod release_url;
pub mod service;
pub mod signed_file_path;
pub mod signed_read_url;
pub mod signer;

pub use file_requester::{FileRequester, load_file_requester};
pub use gcs::GcsError;
pub use object_key::{InvalidObjectKeyError, to_object_key};
pub use public_api_url::public_api_url;
pub use service::{
    DeleteResult, ObjectInfo, ObjectMetadata, SignedFileUrl, SignedObjectOpts, StreamObjectOptions,
    delete_file, download_buffer, download_to_file, get_object_info, get_object_metadata,
    get_signed_file_url, get_signed_object_url, get_signed_read_url, object_key_of, stream_object,
    upload_buffer, upload_local_file,
};
pub use signed_file_path::{
    FileUrlSecretMissingError, SIGNED_FILE_CACHE_CONTROL, SIGNED_FILE_PATH_TTL_SECONDS,
    set_unauthenticated_file_headers, sign_file_path, verify_file_path_signature,
};
pub use signed_read_url::{SIGNED_URL_RESPONSE_CACHE_CONTROL, SignedReadUrl, signed_read_window};
