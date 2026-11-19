//! Port `form-file-url.ts`: path proxy file jawaban form (private_file_access.md F2).

use mz_core::AppState;
use mz_files::encode::encode_uri_component;
use mz_files::{FileUrlSecretMissingError, public_api_url, sign_file_path};

/// `adminFormFilePath` — ber-JWT.
pub fn admin_form_file_path(submission_uuid: &str, file_uuid: &str) -> String {
    format!(
        "/v1/admin/store-visit-forms/submissions/{}/files/{}",
        encode_uri_component(submission_uuid),
        encode_uri_component(file_uuid)
    )
}

/// `mobileFormFilePath` — tanpa JWT, hanya sah bersama `exp` + `sig`.
pub fn mobile_form_file_path(submission_uuid: &str, file_uuid: &str) -> String {
    format!(
        "/v1/mobile/store-visit-forms/submissions/{}/files/{}",
        encode_uri_component(submission_uuid),
        encode_uri_component(file_uuid)
    )
}

/// `signedMobileFormFileUrl` — URL absolut bertanda tangan (`FILE_URL_SECRET` wajib).
pub fn signed_mobile_form_file_url(
    state: &AppState,
    submission_uuid: &str,
    file_uuid: &str,
) -> Result<String, FileUrlSecretMissingError> {
    let signed = sign_file_path(state, &mobile_form_file_path(submission_uuid, file_uuid))?;
    Ok(public_api_url(state, &signed))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths() {
        assert_eq!(
            admin_form_file_path("a", "b"),
            "/v1/admin/store-visit-forms/submissions/a/files/b"
        );
        assert_eq!(
            mobile_form_file_path("a b", "c"),
            "/v1/mobile/store-visit-forms/submissions/a%20b/files/c"
        );
    }
}
