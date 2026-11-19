//! Port `src/products/product-image-url.ts` — URL proxy publik gambar produk.

use std::sync::LazyLock;

use mz_core::AppState;
use regex::Regex;

/// `products/<companyId>/<productUuid>/<fileId>.<ext>` — segmen terakhir.
static FILE_NAME_RE: LazyLock<Regex> = LazyLock::new(|| {
    crate::dto::re(
        r"/([0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12})\.(jpg|png|webp)$",
    )
});

/// Node: src/products/product-image-url.ts:22 `productImageFileId`.
pub fn product_image_file_id(object_key: Option<&str>) -> Option<String> {
    let key = object_key.filter(|k| !k.is_empty())?;
    FILE_NAME_RE
        .captures(key)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_owned())
}

/// Node: src/products/product-image-url.ts:31 `productImagePath` — bentuk yang disimpan ke
/// kolom `imageUrl`.
pub fn product_image_path(product_uuid: &str, object_key: Option<&str>) -> Option<String> {
    let file_id = product_image_file_id(object_key)?;
    Some(format!(
        "/products/{}/image/{file_id}",
        mz_files::encode::encode_uri_component(product_uuid)
    ))
}

/// Node: src/products/product-image-url.ts:41 `productImageUrl` (`HOST_BE` + path).
pub fn product_image_url(
    state: &AppState,
    product_uuid: &str,
    object_key: Option<&str>,
) -> Option<String> {
    product_image_path(product_uuid, object_key).map(|p| mz_files::public_api_url(state, &p))
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: &str = "9f1c2d3e-4a5b-6c7d-8e9f-0a1b2c3d4e5f";
    const F: &str = "7c2a1b30-9d44-4e21-8f77-1b2c3d4e5f60";

    // Node: src/products/product-image-url.spec.ts
    #[test]
    fn file_id_from_key() {
        let key = format!("products/10/{P}/{F}.jpg");
        assert_eq!(product_image_file_id(Some(&key)).as_deref(), Some(F));
        assert_eq!(product_image_file_id(None), None);
        assert_eq!(product_image_file_id(Some("")), None);
        assert_eq!(product_image_file_id(Some(&format!("products/10/{P}/{F}.gif"))), None);
        // UUID produk di tengah key tidak ikut cocok.
        assert_eq!(product_image_file_id(Some(&format!("products/10/{P}/x.png"))), None);
        assert_eq!(
            product_image_file_id(Some("https://storage.googleapis.com/b/products/1/a.jpg")),
            None
        );
    }

    #[test]
    fn path_from_key() {
        let key = format!("products/10/{P}/{F}.webp");
        assert_eq!(
            product_image_path(P, Some(&key)).as_deref(),
            Some(format!("/products/{P}/image/{F}").as_str())
        );
        assert_eq!(product_image_path(P, None), None);
    }
}
