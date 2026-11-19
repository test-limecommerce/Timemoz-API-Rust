//! Port `src/store-visits/geo.util.ts`. Jarak dihitung di aplikasi (`geolib` →
//! [`mz_sales_geo::geo::get_distance`]); SQL hanya mempersempit kandidat dengan kotak
//! `[companyId, lat, lng]` yang sama untuk §6.1, §6.7, §6.10.

pub use mz_sales_geo::geo::get_distance;

/// 1° lintang ≈ 111,32 km; 1° bujur menyusut dengan cos(lat).
pub const KM_PER_DEGREE_LAT: f64 = 111.32;

/// Plafon baris yang boleh dikembalikan kotak prefilter (sisanya diurutkan di aplikasi).
pub const MAX_PREFILTER_ROWS: i64 = 300;

/// `DEFAULT_RADIUS_KM`.
pub const DEFAULT_RADIUS_KM: f64 = 10.0;

/// §5.2.6 `effectiveRadius`: radius toko menang, default company mengisi kekosongan.
pub fn effective_radius(
    store_radius_meters: Option<i32>,
    company_default_radius_meters: i32,
) -> i32 {
    store_radius_meters.unwrap_or(company_default_radius_meters)
}

/// Rentang tertutup `[gte, lte]`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Range {
    pub gte: f64,
    pub lte: f64,
}

/// Hasil [`bounding_box`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BoundingBox {
    pub lat: Range,
    pub lng: Range,
}

/// `boundingBox(lat, lng, radiusKm)`: kotak di sekitar titik; lingkaran harus diterapkan ulang
/// sesudahnya. cos(lat) dilantai 1e-6, delta bujur dijepit 180.
pub fn bounding_box(lat: f64, lng: f64, radius_km: f64) -> BoundingBox {
    let d_lat = radius_km / KM_PER_DEGREE_LAT;
    let cos_lat = (lat * std::f64::consts::PI / 180.0).cos().abs().max(1e-6);
    let d_lng = (radius_km / (KM_PER_DEGREE_LAT * cos_lat)).min(180.0);
    BoundingBox {
        lat: Range {
            gte: lat - d_lat,
            lte: lat + d_lat,
        },
        lng: Range {
            gte: lng - d_lng,
            lte: lng + d_lng,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn radius_fallback() {
        assert_eq!(effective_radius(Some(50), 150), 50);
        assert_eq!(effective_radius(None, 150), 150);
    }

    #[test]
    fn box_equator_and_pole() {
        let b = bounding_box(0.0, 0.0, 111.32);
        assert!((b.lat.gte + 1.0).abs() < 1e-9 && (b.lat.lte - 1.0).abs() < 1e-9);
        assert!((b.lng.lte - 1.0).abs() < 1e-9);
        let p = bounding_box(90.0, 10.0, 10.0);
        assert!((p.lng.lte - 190.0).abs() < 1e-9);
        assert!((p.lng.gte + 170.0).abs() < 1e-9);
    }
}
