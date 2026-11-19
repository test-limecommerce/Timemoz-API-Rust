//! Port `src/files/public-api-url.ts`.

use mz_core::AppState;

/// `publicApiUrl(path)`: `HOST_BE` (tanpa `/` di ujung) + path. Tanpa `HOST_BE`,
/// path relatif dikembalikan.
pub fn public_api_url(state: &AppState, path: &str) -> String {
    public_api_url_with(state.config.var("HOST_BE").as_deref(), path)
}

pub fn public_api_url_with(host_be: Option<&str>, path: &str) -> String {
    let base = host_be.unwrap_or("").trim().trim_end_matches('/');
    format!("{base}{path}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joins_base_and_path() {
        assert_eq!(
            public_api_url_with(Some("https://api.x.id/"), "/a/b"),
            "https://api.x.id/a/b"
        );
        assert_eq!(
            public_api_url_with(Some("  https://api.x.id// "), "/a"),
            "https://api.x.id/a"
        );
        assert_eq!(public_api_url_with(None, "/a"), "/a");
        assert_eq!(public_api_url_with(Some(""), "/a"), "/a");
    }
}
