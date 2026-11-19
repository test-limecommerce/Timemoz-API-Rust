//! Port `src/store-visits/store-visit-photo-file-url.ts` — path proxy foto kunjungan
//! (private_file_access.md F2). Satu sumber untuk endpoint file (memeriksa `sig` atas path
//! kanonis) dan respons mobile (membuat URL-nya).

use mz_core::AppState;
use mz_files::encode::encode_uri_component;
use mz_files::signed_file_path::{
    FileUrlSecretMissingError, SIGNED_FILE_PATH_TTL_SECONDS, sign_file_path_with,
};

/// Admin, ber-JWT.
pub fn admin_visit_photo_file_path(visit_uuid: &str, photo_uuid: &str) -> String {
    format!(
        "/v1/admin/store-visits/{}/photos/{}/file",
        encode_uri_component(visit_uuid),
        encode_uri_component(photo_uuid)
    )
}

/// Mobile, TANPA JWT — hanya sah bersama `exp` + `sig`.
pub fn mobile_visit_photo_file_path(visit_uuid: &str, photo_uuid: &str) -> String {
    format!(
        "/v1/mobile/store-visits/{}/photos/{}/file",
        encode_uri_component(visit_uuid),
        encode_uri_component(photo_uuid)
    )
}

/// `signedMobileVisitPhotoUrl(visitUuid, photoUuid, nowMs)`: URL absolut bertanda tangan
/// (`HOST_BE` + path + `?exp&sig`). Galat bila `FILE_URL_SECRET` tidak ada (→ 500).
pub fn signed_mobile_visit_photo_url(
    state: &AppState,
    visit_uuid: &str,
    photo_uuid: &str,
    now_ms: i64,
) -> Result<String, FileUrlSecretMissingError> {
    let signed = sign_file_path_with(
        state.config.var("FILE_URL_SECRET").as_deref(),
        &mobile_visit_photo_file_path(visit_uuid, photo_uuid),
        now_ms,
        SIGNED_FILE_PATH_TTL_SECONDS,
    )?;
    Ok(mz_files::public_api_url(state, &signed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_paths() {
        assert_eq!(
            admin_visit_photo_file_path("v-1", "p 2"),
            "/v1/admin/store-visits/v-1/photos/p%202/file"
        );
        assert_eq!(
            mobile_visit_photo_file_path("a/b", "c"),
            "/v1/mobile/store-visits/a%2Fb/photos/c/file"
        );
    }
}
