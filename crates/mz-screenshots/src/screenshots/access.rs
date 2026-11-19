//! Port `src/screenshots/screenshot-access.ts` + `screenshot-image-path.ts`:
//! aturan akses & key screenshot bersama.

use mz_core::enums::sees_whole_company;
use mz_files::{FileRequester, to_object_key};

/// Pemilik screenshot + company default aktifnya.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenshotOwner {
    pub user_id: i32,
    pub user_uuid: String,
    pub company_id: i32,
    pub company_uuid: String,
}

/// Pemilik screenshot, atau role `seesWholeCompany` di company yang SAMA dengan pemilik.
pub fn can_access_screenshot(
    owner_user_id: i32,
    owner_company_id: i32,
    requester: &FileRequester,
) -> bool {
    if owner_user_id == requester.user_id {
        return true;
    }
    sees_whole_company(requester.role_code.as_deref())
        && requester.company_id == Some(owner_company_id)
}

/// Key screenshot selalu di bawah `companies/<uuid pemilik>/` (atau `compressed/`-nya).
pub fn is_owner_company_key(key: &str, company_uuid: &str) -> bool {
    let prefix = format!("companies/{company_uuid}/");
    key.starts_with(&prefix) || key.starts_with(&format!("compressed/{prefix}"))
}

/// Key yang diterbitkan `POST /screenshots` untuk upload desktop.
pub fn screenshot_upload_key(
    company_uuid: &str,
    project_uuid: &str,
    user_uuid: &str,
    screenshot_uuid: &str,
) -> String {
    format!(
        "companies/{company_uuid}/projects/{project_uuid}/users/{user_uuid}/screenshots/{screenshot_uuid}.jpg"
    )
}

/// `key.replace(/-blurred-\d+(\.[^./]+)$/, '$1')`.
pub fn strip_blur_suffix(key: &str) -> String {
    let Some(dot) = key.rfind('.') else {
        return key.to_owned();
    };
    let (Some(head), Some(ext)) = (key.get(..dot), key.get(dot..)) else {
        return key.to_owned();
    };
    if ext.len() < 2 || ext.contains('/') {
        return key.to_owned();
    }
    let Some(marker) = head.rfind("-blurred-") else {
        return key.to_owned();
    };
    let digits = head.get(marker + "-blurred-".len()..).unwrap_or("");
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return key.to_owned();
    }
    format!("{}{ext}", head.get(..marker).unwrap_or(""))
}

/// `toObjectKey` yang mengembalikan `None` alih-alih galat untuk nilai tak sah.
pub fn object_key_or_null(bucket: Option<&str>, value: Option<&str>) -> Option<String> {
    let value = value.filter(|v| !v.is_empty())?;
    to_object_key(value, bucket).ok()
}

/// `encodeURIComponent`.
fn enc(s: &str) -> String {
    mz_files::encode::encode_uri_component(s)
}

/// `screenshotImagePath(uuid, variant)`.
pub fn screenshot_image_path(screenshot_uuid: &str, variant: &str) -> String {
    format!(
        "/screenshots/{}/image?variant={variant}",
        enc(screenshot_uuid)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(user: i32, company: Option<i32>, role: Option<&str>) -> FileRequester {
        FileRequester {
            user_id: user,
            company_id: company,
            role_code: role.map(str::to_owned),
        }
    }

    #[test]
    fn access_rules() {
        assert!(can_access_screenshot(1, 10, &req(1, None, None)));
        assert!(can_access_screenshot(
            1,
            10,
            &req(2, Some(10), Some("MANAGER"))
        ));
        assert!(!can_access_screenshot(
            1,
            10,
            &req(2, Some(11), Some("ADMIN"))
        ));
        assert!(!can_access_screenshot(
            1,
            10,
            &req(2, Some(10), Some("STAFF"))
        ));
        assert!(!can_access_screenshot(1, 10, &req(2, Some(10), None)));
    }

    #[test]
    fn owner_prefix() {
        assert!(is_owner_company_key("companies/c1/a.jpg", "c1"));
        assert!(is_owner_company_key("compressed/companies/c1/a.jpg", "c1"));
        assert!(!is_owner_company_key("companies/c2/a.jpg", "c1"));
        assert!(!is_owner_company_key("companies/c10/a.jpg", "c1"));
    }

    #[test]
    fn upload_key_shape() {
        assert_eq!(
            screenshot_upload_key("c", "p", "u", "s"),
            "companies/c/projects/p/users/u/screenshots/s.jpg"
        );
    }

    #[test]
    fn blur_suffix() {
        assert_eq!(
            strip_blur_suffix("a/b/x-blurred-1700000000000.jpg"),
            "a/b/x.jpg"
        );
        assert_eq!(strip_blur_suffix("a/b/x.jpg"), "a/b/x.jpg");
        assert_eq!(strip_blur_suffix("a/x-blurred-.jpg"), "a/x-blurred-.jpg");
        assert_eq!(
            strip_blur_suffix("a/x-blurred-12/y.jpg"),
            "a/x-blurred-12/y.jpg"
        );
        assert_eq!(strip_blur_suffix("x-blurred-9"), "x-blurred-9");
    }

    #[test]
    fn key_or_null() {
        assert_eq!(
            object_key_or_null(Some("b"), Some("a/b.jpg")).as_deref(),
            Some("a/b.jpg")
        );
        assert_eq!(
            object_key_or_null(Some("b"), Some("https://evil.com/b/a.jpg")),
            None
        );
        assert_eq!(object_key_or_null(Some("b"), None), None);
        assert_eq!(object_key_or_null(Some("b"), Some("")), None);
    }

    #[test]
    fn image_path() {
        assert_eq!(
            screenshot_image_path("abc", "display"),
            "/screenshots/abc/image?variant=display"
        );
    }
}
