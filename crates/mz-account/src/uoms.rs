//! Port `src/products/default-uoms.ts` — 63 UOM bawaan setiap company baru.

use mz_core::ApiResult;
use sqlx::PgConnection;

pub struct DefaultUom {
    pub code: &'static str,
    pub name: &'static str,
    /// Nilai enum Postgres `UomType`: unit | weight | volume | length.
    pub uom_type: &'static str,
    /// `Decimal(18,6)` sebagai teks (sama dengan Node).
    pub default_factor: &'static str,
}

pub const DEFAULT_UOMS: &[DefaultUom] = &[
    DefaultUom {
        code: "EA",
        name: "Each",
        uom_type: "unit",
        default_factor: "1",
    },
    DefaultUom {
        code: "PCS",
        name: "Pieces",
        uom_type: "unit",
        default_factor: "1",
    },
    DefaultUom {
        code: "UNIT",
        name: "Unit",
        uom_type: "unit",
        default_factor: "1",
    },
    DefaultUom {
        code: "DOZ",
        name: "Dozen",
        uom_type: "unit",
        default_factor: "12",
    },
    DefaultUom {
        code: "DZ",
        name: "Dozen",
        uom_type: "unit",
        default_factor: "12",
    },
    DefaultUom {
        code: "GS",
        name: "Gross",
        uom_type: "unit",
        default_factor: "144",
    },
    DefaultUom {
        code: "PAIR",
        name: "Pair",
        uom_type: "unit",
        default_factor: "2",
    },
    DefaultUom {
        code: "SET",
        name: "Set",
        uom_type: "unit",
        default_factor: "1",
    },
    DefaultUom {
        code: "PACK",
        name: "Pack",
        uom_type: "unit",
        default_factor: "1",
    },
    DefaultUom {
        code: "PK",
        name: "Pack",
        uom_type: "unit",
        default_factor: "1",
    },
    DefaultUom {
        code: "BOX",
        name: "Box",
        uom_type: "unit",
        default_factor: "1",
    },
    DefaultUom {
        code: "BX",
        name: "Box",
        uom_type: "unit",
        default_factor: "1",
    },
    DefaultUom {
        code: "CASE",
        name: "Case",
        uom_type: "unit",
        default_factor: "1",
    },
    DefaultUom {
        code: "CS",
        name: "Case",
        uom_type: "unit",
        default_factor: "1",
    },
    DefaultUom {
        code: "CTN",
        name: "Carton",
        uom_type: "unit",
        default_factor: "1",
    },
    DefaultUom {
        code: "BAG",
        name: "Bag",
        uom_type: "unit",
        default_factor: "1",
    },
    DefaultUom {
        code: "SAK",
        name: "Sack",
        uom_type: "unit",
        default_factor: "1",
    },
    DefaultUom {
        code: "KARUNG",
        name: "Sack/Karung",
        uom_type: "unit",
        default_factor: "1",
    },
    DefaultUom {
        code: "BOT",
        name: "Bottle",
        uom_type: "unit",
        default_factor: "1",
    },
    DefaultUom {
        code: "BTL",
        name: "Bottle",
        uom_type: "unit",
        default_factor: "1",
    },
    DefaultUom {
        code: "CAN",
        name: "Can",
        uom_type: "unit",
        default_factor: "1",
    },
    DefaultUom {
        code: "JAR",
        name: "Jar",
        uom_type: "unit",
        default_factor: "1",
    },
    DefaultUom {
        code: "ROLL",
        name: "Roll",
        uom_type: "unit",
        default_factor: "1",
    },
    DefaultUom {
        code: "RIM",
        name: "Rim",
        uom_type: "unit",
        default_factor: "1",
    },
    DefaultUom {
        code: "PAL",
        name: "Pallet",
        uom_type: "unit",
        default_factor: "1",
    },
    DefaultUom {
        code: "SHEET",
        name: "Sheet",
        uom_type: "unit",
        default_factor: "1",
    },
    DefaultUom {
        code: "LEMBAR",
        name: "Sheet/Lembar",
        uom_type: "unit",
        default_factor: "1",
    },
    DefaultUom {
        code: "DRUM",
        name: "Drum",
        uom_type: "unit",
        default_factor: "1",
    },
    DefaultUom {
        code: "TUBE",
        name: "Tube",
        uom_type: "unit",
        default_factor: "1",
    },
    DefaultUom {
        code: "SACHET",
        name: "Sachet",
        uom_type: "unit",
        default_factor: "1",
    },
    DefaultUom {
        code: "STRIP",
        name: "Strip",
        uom_type: "unit",
        default_factor: "1",
    },
    DefaultUom {
        code: "TRAY",
        name: "Tray",
        uom_type: "unit",
        default_factor: "1",
    },
    DefaultUom {
        code: "CRATE",
        name: "Crate",
        uom_type: "unit",
        default_factor: "1",
    },
    DefaultUom {
        code: "BUNDLE",
        name: "Bundle",
        uom_type: "unit",
        default_factor: "1",
    },
    DefaultUom {
        code: "DOZEN",
        name: "Dozen",
        uom_type: "unit",
        default_factor: "12",
    },
    DefaultUom {
        code: "MTR",
        name: "Meter",
        uom_type: "length",
        default_factor: "1",
    },
    DefaultUom {
        code: "M",
        name: "Meter",
        uom_type: "length",
        default_factor: "1",
    },
    DefaultUom {
        code: "CM",
        name: "Centimeter",
        uom_type: "length",
        default_factor: "0.01",
    },
    DefaultUom {
        code: "MM",
        name: "Millimeter",
        uom_type: "length",
        default_factor: "0.001",
    },
    DefaultUom {
        code: "KM",
        name: "Kilometer",
        uom_type: "length",
        default_factor: "1000",
    },
    DefaultUom {
        code: "IN",
        name: "Inch",
        uom_type: "length",
        default_factor: "0.0254",
    },
    DefaultUom {
        code: "FT",
        name: "Foot",
        uom_type: "length",
        default_factor: "0.3048",
    },
    DefaultUom {
        code: "YD",
        name: "Yard",
        uom_type: "length",
        default_factor: "0.9144",
    },
    DefaultUom {
        code: "KG",
        name: "Kilogram",
        uom_type: "weight",
        default_factor: "1",
    },
    DefaultUom {
        code: "G",
        name: "Gram",
        uom_type: "weight",
        default_factor: "0.001",
    },
    DefaultUom {
        code: "GR",
        name: "Gram",
        uom_type: "weight",
        default_factor: "0.001",
    },
    DefaultUom {
        code: "MG",
        name: "Milligram",
        uom_type: "weight",
        default_factor: "0.000001",
    },
    DefaultUom {
        code: "TON",
        name: "Metric Ton",
        uom_type: "weight",
        default_factor: "1000",
    },
    DefaultUom {
        code: "KWINTAL",
        name: "Quintal",
        uom_type: "weight",
        default_factor: "100",
    },
    DefaultUom {
        code: "ONS",
        name: "Ounce (Indonesia)",
        uom_type: "weight",
        default_factor: "0.1",
    },
    DefaultUom {
        code: "POUND",
        name: "Pound",
        uom_type: "weight",
        default_factor: "0.453592",
    },
    DefaultUom {
        code: "LB",
        name: "Pound",
        uom_type: "weight",
        default_factor: "0.453592",
    },
    DefaultUom {
        code: "OZ",
        name: "Ounce",
        uom_type: "weight",
        default_factor: "0.02835",
    },
    DefaultUom {
        code: "L",
        name: "Liter",
        uom_type: "volume",
        default_factor: "1",
    },
    DefaultUom {
        code: "LTR",
        name: "Liter",
        uom_type: "volume",
        default_factor: "1",
    },
    DefaultUom {
        code: "ML",
        name: "Milliliter",
        uom_type: "volume",
        default_factor: "0.001",
    },
    DefaultUom {
        code: "CC",
        name: "Cubic Centimeter",
        uom_type: "volume",
        default_factor: "0.001",
    },
    DefaultUom {
        code: "KL",
        name: "Kiloliter",
        uom_type: "volume",
        default_factor: "1000",
    },
    DefaultUom {
        code: "M3",
        name: "Cubic Meter",
        uom_type: "volume",
        default_factor: "1000",
    },
    DefaultUom {
        code: "CM3",
        name: "Cubic Centimeter",
        uom_type: "volume",
        default_factor: "0.001",
    },
    DefaultUom {
        code: "GAL",
        name: "Gallon (US)",
        uom_type: "volume",
        default_factor: "3.785412",
    },
    DefaultUom {
        code: "QT",
        name: "Quart (US)",
        uom_type: "volume",
        default_factor: "0.946353",
    },
    DefaultUom {
        code: "PT",
        name: "Pint (US)",
        uom_type: "volume",
        default_factor: "0.473176",
    },
];

/// `seedDefaultUoms(client, companyId, createdById)` -> `{ count }`. Pakai `&mut *tx` di
/// dalam transaksi registrasi. `skipDuplicates` = `ON CONFLICT DO NOTHING`
/// (unique `companyId, code`); mengembalikan jumlah baris yang tertulis.
/// `defaultOperator` selalu `multiply`.
pub async fn seed_default_uoms(
    conn: &mut PgConnection,
    company_id: i32,
    created_by_id: i32,
) -> ApiResult<u64> {
    let codes: Vec<&str> = DEFAULT_UOMS.iter().map(|u| u.code).collect();
    let names: Vec<&str> = DEFAULT_UOMS.iter().map(|u| u.name).collect();
    let types: Vec<&str> = DEFAULT_UOMS.iter().map(|u| u.uom_type).collect();
    let factors: Vec<&str> = DEFAULT_UOMS.iter().map(|u| u.default_factor).collect();
    let res = sqlx::query(
        r#"INSERT INTO uom ("companyId", code, name, "uomType", "defaultOperator", "defaultFactor", "createdById")
           SELECT $1, t.code, t.name, t.uom_type::"UomType", 'multiply'::"UomOperator", t.factor::numeric, $2
           FROM UNNEST($3::text[], $4::text[], $5::text[], $6::text[]) AS t(code, name, uom_type, factor)
           ON CONFLICT DO NOTHING"#,
    )
    .bind(company_id)
    .bind(created_by_id)
    .bind(&codes)
    .bind(&names)
    .bind(&types)
    .bind(&factors)
    .execute(&mut *conn)
    .await?;
    Ok(res.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn list_is_consistent() {
        assert_eq!(DEFAULT_UOMS.len(), 63);
        let codes: HashSet<_> = DEFAULT_UOMS.iter().map(|u| u.code).collect();
        assert_eq!(codes.len(), 63, "kode UOM harus unik");
        assert!(codes.contains("PCS"));
        assert!(
            DEFAULT_UOMS
                .iter()
                .all(|u| ["unit", "weight", "volume", "length"].contains(&u.uom_type))
        );
        assert!(
            DEFAULT_UOMS
                .iter()
                .all(|u| u.default_factor.parse::<f64>().is_ok_and(|f| f > 0.0))
        );
    }
}
