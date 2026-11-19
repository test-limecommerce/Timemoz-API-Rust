//! Helper geofence & negative zone bersama (rumah tetap; menggantikan `mz-attendance/src/geo.rs`).
//!
//! Port `MobileGeofencesService.{getActiveGeofencesForCompany, isGeofenceApplicableTo}`
//! (`src/mobile-geofences/mobile-geofences.service.ts:17,366`) dan
//! `NegativeZonesService.{getActiveNegativeZonesForCompany, findContainingNegativeZone}`
//! (`src/negative-zones/negative-zones.service.ts:100,111`). Nama & tanda tangan fungsi dibuat sama
//! dengan `mz-attendance::geo` supaya pemanggil cukup `pub use mz_sales_geo::geo::*`.
//! (`NegativeZone` di sini punya tambahan `id`.)

use sqlx::{PgExecutor, PgPool};

/// `geolib.getDistance(from, to)` (v3.3, akurasi 1 m): rumus kosinus bola, jari-jari 6 378 137 m,
/// argumen `acos` dijepit ke [-1, 1], dibulatkan `Math.round`.
pub fn get_distance(from_lat: f64, from_lng: f64, to_lat: f64, to_lng: f64) -> i64 {
    let to_rad = |v: f64| v * std::f64::consts::PI / 180.0;
    let from_lat_r = to_rad(from_lat);
    let to_lat_r = to_rad(to_lat);
    let delta_lon = to_rad(from_lng - to_lng);
    let cos =
        to_lat_r.sin() * from_lat_r.sin() + to_lat_r.cos() * from_lat_r.cos() * delta_lon.cos();
    let cos = cos.clamp(-1.0, 1.0);
    let distance = cos.acos() * 6_378_137.0;
    js_round(distance)
}

/// `Math.round` JS (setengah dibulatkan ke atas).
#[allow(clippy::cast_possible_truncation)]
pub fn js_round(x: f64) -> i64 {
    (x + 0.5).floor() as i64
}

/// Baris `geofence` (kolom yang dipakai clock-in/out & kunjungan).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct Geofence {
    pub id: i32,
    pub uuid: String,
    pub name: String,
    pub lat: f64,
    pub lng: f64,
    #[sqlx(rename = "radiusMeters")]
    pub radius_meters: i32,
    pub enforced: bool,
    #[sqlx(rename = "applicableTo")]
    pub applicable_to: String,
    #[sqlx(rename = "applicableIds")]
    pub applicable_ids: Vec<i32>,
}

impl Geofence {
    /// `isGeofenceApplicableTo`.
    pub fn applies_to(&self, user_id: i32) -> bool {
        is_applicable_to(&self.applicable_to, &self.applicable_ids, user_id)
    }
}

/// `isGeofenceApplicableTo(gf, userId)`.
pub fn is_applicable_to(applicable_to: &str, applicable_ids: &[i32], user_id: i32) -> bool {
    applicable_to == "all" || (applicable_to == "user_ids" && applicable_ids.contains(&user_id))
}

/// `getActiveGeofencesForCompany(companyId, userId)`. Node tanpa `orderBy`; di sini urut `id`
/// supaya urutan seri jarak deterministik (urutan heap Postgres = urutan sisip).
pub async fn active_geofences_for<'e>(
    db: impl PgExecutor<'e>,
    company_id: i32,
    user_id: i32,
) -> sqlx::Result<Vec<Geofence>> {
    let rows: Vec<Geofence> = sqlx::query_as(
        r#"SELECT id, uuid::text AS uuid, name, lat, lng, "radiusMeters", enforced,
                  "applicableTo"::text AS "applicableTo", COALESCE("applicableIds", '{}') AS "applicableIds"
             FROM geofence
            WHERE "companyId" = $1 AND "isActive" = true
            ORDER BY id"#,
    )
    .bind(company_id)
    .fetch_all(db)
    .await?;
    Ok(rows.into_iter().filter(|g| g.applies_to(user_id)).collect())
}

/// Geofence + jaraknya ke titik, urut jarak naik (stabil, seperti `Array.sort` V8).
pub fn by_distance(geofences: &[Geofence], lat: f64, lng: f64) -> Vec<(&Geofence, i64)> {
    let mut v: Vec<(&Geofence, i64)> = geofences
        .iter()
        .map(|g| (g, get_distance(lat, lng, g.lat, g.lng)))
        .collect();
    v.sort_by_key(|(_, d)| *d);
    v
}

/// Aturan pemilihan clock-in & `POST /mobile/geofences/event` tanpa `geofenceId`: geofence
/// TERDEKAT yang memuat titik, kalau tidak ada, geofence terdekat. `None` bila daftar kosong.
pub fn pick_geofence(geofences: &[Geofence], lat: f64, lng: f64) -> Option<(&Geofence, i64)> {
    let sorted = by_distance(geofences, lat, lng);
    sorted
        .iter()
        .find(|(g, d)| *d <= i64::from(g.radius_meters))
        .or_else(|| sorted.first())
        .copied()
}

/// Baris `negativeZone` (kolom yang dipakai clock-in/out & kunjungan).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct NegativeZone {
    pub id: i32,
    pub uuid: String,
    pub name: String,
    pub lat: f64,
    pub lng: f64,
    #[sqlx(rename = "radiusMeters")]
    pub radius_meters: i32,
}

/// `NegativeZoneHit` Node (`{ zone, distanceMeters }`).
#[derive(Debug, Clone)]
pub struct NegativeZoneHit {
    pub zone: NegativeZone,
    pub distance_meters: i64,
}

/// `getActiveNegativeZonesForCompany` (`orderBy: { id: 'asc' }`).
pub async fn active_negative_zones<'e>(
    db: impl PgExecutor<'e>,
    company_id: i32,
) -> sqlx::Result<Vec<NegativeZone>> {
    sqlx::query_as(
        r#"SELECT id, uuid::text AS uuid, name, lat, lng, "radiusMeters"
             FROM "negativeZone"
            WHERE "companyId" = $1 AND "isActive" = true
            ORDER BY id ASC"#,
    )
    .bind(company_id)
    .fetch_all(db)
    .await
}

/// `findContainingNegativeZone` (bagian murni): zona TERDEKAT yang memuat titik, atau `None`.
pub fn containing_zone(zones: &[NegativeZone], lat: f64, lng: f64) -> Option<(&NegativeZone, i64)> {
    let mut v: Vec<(&NegativeZone, i64)> = zones
        .iter()
        .map(|z| (z, get_distance(lat, lng, z.lat, z.lng)))
        .collect();
    v.sort_by_key(|(_, d)| *d);
    v.into_iter()
        .find(|(z, d)| *d <= i64::from(z.radius_meters))
}

/// `findContainingNegativeZone(companyId, { lat, lng })` lengkap dengan query.
pub async fn find_containing_negative_zone(
    db: &PgPool,
    company_id: i32,
    lat: f64,
    lng: f64,
) -> sqlx::Result<Option<NegativeZoneHit>> {
    let zones = active_negative_zones(db, company_id).await?;
    Ok(
        containing_zone(&zones, lat, lng).map(|(z, d)| NegativeZoneHit {
            zone: z.clone(),
            distance_meters: d,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gf(id: i32, lat: f64, lng: f64, radius: i32) -> Geofence {
        Geofence {
            id,
            uuid: format!("u{id}"),
            name: format!("g{id}"),
            lat,
            lng,
            radius_meters: radius,
            enforced: false,
            applicable_to: "all".into(),
            applicable_ids: vec![],
        }
    }

    #[test]
    fn distance_matches_geolib() {
        // Nilai dari geolib 3.3.14 (node): getDistance({lat:-6.2088,lng:106.8456},{lat:-6.2,lng:106.85}).
        assert_eq!(get_distance(-6.2088, 106.8456, -6.2, 106.85), 1094);
        assert_eq!(get_distance(-6.2088, 106.8456, -6.2088, 106.8456), 0);
        assert_eq!(get_distance(0.0, 0.0, 0.0, 1.0), 111_319);
    }

    #[test]
    fn js_round_half_up() {
        assert_eq!(js_round(2.5), 3);
        assert_eq!(js_round(2.49), 2);
        assert_eq!(js_round(-2.5), -2);
    }

    #[test]
    fn applicability() {
        assert!(is_applicable_to("all", &[], 1));
        assert!(is_applicable_to("user_ids", &[1, 2], 2));
        assert!(!is_applicable_to("user_ids", &[1, 2], 3));
        assert!(!is_applicable_to("other", &[1], 1));
    }

    #[test]
    fn pick_prefers_nearest_containing_then_nearest() {
        // Titik tepat di g2; g1 sedikit lebih jauh tetapi juga memuat titik.
        let gs = vec![gf(1, 0.0, 0.0005, 1000), gf(2, 0.0, 0.0, 50)];
        let (g, d) = pick_geofence(&gs, 0.0, 0.0).unwrap();
        assert_eq!((g.id, d), (2, 0));
        // Di luar semuanya: yang terdekat.
        let gs = vec![gf(1, 0.0, 0.02, 50), gf(2, 0.0, 0.01, 50)];
        let (g, _) = pick_geofence(&gs, 0.0, 0.0).unwrap();
        assert_eq!(g.id, 2);
        assert!(pick_geofence(&[], 0.0, 0.0).is_none());
    }

    #[test]
    fn containing_zone_nearest_wins() {
        let z = |id, lng, r| NegativeZone {
            id,
            uuid: format!("z{id}"),
            name: String::new(),
            lat: 0.0,
            lng,
            radius_meters: r,
        };
        let zones = vec![z(1, 0.0004, 500), z(2, 0.0001, 500), z(3, 1.0, 10)];
        let (hit, d) = containing_zone(&zones, 0.0, 0.0).unwrap();
        assert_eq!(hit.id, 2);
        assert_eq!(d, 11);
        assert!(containing_zone(&zones, 5.0, 5.0).is_none());
    }
}
