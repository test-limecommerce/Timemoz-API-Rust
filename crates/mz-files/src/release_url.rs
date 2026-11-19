//! URL unduhan app release (`releaseDownloadUrl` + `PLATFORM_REVERSE_MAP`). Rumah bersama
//! `mz-timeconfig` dan `mz-settings` (dulu salinan di `mz-settings`).

use mz_core::AppState;

use crate::encode::encode_uri_component;
use crate::public_api_url::public_api_url;

/// `PLATFORM_REVERSE_MAP`.
pub fn platform_key(platform: &str) -> Option<&'static str> {
    match platform {
        "WINDOWS" => Some("win"),
        "MAC" => Some("mac"),
        "LINUX" => Some("linux"),
        _ => None,
    }
}

/// Path unduhan (`releaseDownloadUrl` tanpa `publicApiUrl`); `None` bila platform tak dikenal.
pub fn release_download_path(platform: &str, version: &str, file_name: &str) -> Option<String> {
    let key = platform_key(platform)?;
    Some(format!(
        "/app-releases/download/{key}/{}/{}",
        encode_uri_component(version),
        encode_uri_component(file_name)
    ))
}

/// `releaseDownloadUrl(release)`; `None` bila platform tidak dikenal.
pub fn release_download_url(
    state: &AppState,
    platform: &str,
    version: &str,
    file_name: &str,
) -> Option<String> {
    release_download_path(platform, version, file_name).map(|p| public_api_url(state, &p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn platform_key_maps_known_platforms() {
        assert_eq!(platform_key("MAC"), Some("mac"));
        assert_eq!(platform_key("BSD"), None);
    }

    #[test]
    fn download_path_encodes_segments() {
        assert_eq!(
            release_download_path("WINDOWS", "1.0.0", "Setup 1.0.0.exe").as_deref(),
            Some("/app-releases/download/win/1.0.0/Setup%201.0.0.exe")
        );
        assert_eq!(release_download_path("BSD", "1", "a"), None);
    }
}
