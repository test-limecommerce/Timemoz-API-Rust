//! Port `form-dynamic-options.service.ts`: cek eksistensi (submit) + enrichment tampilan
//! (baca) untuk field ber-`options.source` `product`/`order`. Sengaja TANPA filter
//! `isActive`/`deletedAt`/`status` (FR-005): murni eksistensi + kecocokan company.

use std::collections::{HashMap, HashSet};

use axum::http::StatusCode;
use mz_core::{ApiError, ApiResult};
use serde_json::{Map, Value, json};
use sqlx::{PgConnection, PgPool};

use crate::answers_validator::{AnswerValidationMode, ValidateAnswersOptions, validate_answers};
use crate::errors::coded_details;
use crate::types::{AnswerProductOrderReference, FormDocument, OptionsSource};

/// Id per sumber, urutan rujukan dipertahankan.
pub fn split_ids(refs: &[AnswerProductOrderReference]) -> (Vec<String>, Vec<String>) {
    let pick = |s: OptionsSource| {
        refs.iter()
            .filter(|r| r.source == s)
            .flat_map(|r| r.ids.iter().cloned())
            .collect::<Vec<_>>()
    };
    (pick(OptionsSource::Product), pick(OptionsSource::Order))
}

/// Prisma memvalidasi literal `@db.Uuid` sebelum query; id yang bukan uuid membuat
/// `findMany` melempar (500). Ditiru: id tak terbaca → galat internal.
fn parse_uuids(ids: &[String]) -> ApiResult<Vec<uuid::Uuid>> {
    ids.iter()
        .map(|id| {
            uuid::Uuid::parse_str(id).map_err(|e| {
                ApiError::internal(anyhow::anyhow!("Inconsistent column data: {e} ({id})"))
            })
        })
        .collect()
}

/// `SELECT uuid[, label]` dari `product`/`order` milik company. Kunci = uuid huruf kecil
/// (bentuk yang dikembalikan Prisma).
async fn fetch_labels<'e, E>(
    exec: E,
    table: &str,
    label_col: Option<&str>,
    company_id: i32,
    ids: &[String],
) -> ApiResult<HashMap<String, Option<String>>>
where
    E: sqlx::PgExecutor<'e>,
{
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let uuids = parse_uuids(ids)?;
    let label = label_col.map_or("NULL::text".to_owned(), |c| format!("\"{c}\"::text"));
    let sql = format!(
        r#"SELECT uuid::text AS uuid, {label} AS label FROM "{table}"
            WHERE uuid = ANY($1::uuid[]) AND "companyId" = $2"#
    );
    let rows: Vec<(String, Option<String>)> = sqlx::query_as(&sql)
        .bind(&uuids)
        .bind(company_id)
        .fetch_all(exec)
        .await?;
    Ok(rows.into_iter().collect())
}

/// Galat per rujukan yang tidak ditemukan (`assertExistsOnSubmit`, bagian murni).
pub fn missing_reference_errors(
    refs: &[AnswerProductOrderReference],
    found_products: &HashSet<String>,
    found_orders: &HashSet<String>,
) -> Vec<Value> {
    let mut errors = Vec::new();
    for r in refs {
        let found = if r.source == OptionsSource::Product {
            found_products
        } else {
            found_orders
        };
        for id in &r.ids {
            if !found.contains(id) {
                errors.push(json!({
                    "key": r.field_path,
                    "message": format!("does not reference a {} in this company", r.source.as_str()),
                }));
            }
        }
    }
    errors
}

/// `422 FORM_OPTION_NOT_FOUND`.
pub fn option_not_found_error(errors: Vec<Value>) -> ApiError {
    coded_details(
        StatusCode::UNPROCESSABLE_ENTITY,
        "FORM_OPTION_NOT_FOUND",
        "Answers did not pass validation",
        json!({ "errors": errors }),
    )
}

/// `assertExistsOnSubmit(tx, companyId, refs)` (`form-dynamic-options.service.ts:26`).
/// Dijalankan di dalam transaksi submit/koreksi.
pub async fn assert_exists_on_submit(
    tx: &mut PgConnection,
    company_id: i32,
    refs: &[AnswerProductOrderReference],
) -> ApiResult<()> {
    let (product_ids, order_ids) = split_ids(refs);
    if product_ids.is_empty() && order_ids.is_empty() {
        return Ok(());
    }
    let products = fetch_labels(&mut *tx, "product", None, company_id, &product_ids).await?;
    let orders = fetch_labels(&mut *tx, "order", None, company_id, &order_ids).await?;
    let found_products: HashSet<String> = products.into_keys().collect();
    let found_orders: HashSet<String> = orders.into_keys().collect();
    let errors = missing_reference_errors(refs, &found_products, &found_orders);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(option_not_found_error(errors))
    }
}

/// `isSingleChoiceRef` — bentuk nilai di `answers[fieldPath]` menentukan single vs multi.
pub fn is_single_choice_ref(answers: Option<&Value>, field_path: &str) -> bool {
    let mut cursor = answers;
    for segment in field_path.split('.') {
        match cursor {
            Some(Value::Array(a)) => {
                // `cursor.at(Number(segment))`.
                let n = mz_core::jsfmt::number(segment);
                cursor = if n.is_finite() && n.fract() == 0.0 {
                    let i = n as i64;
                    let idx = if i < 0 { a.len() as i64 + i } else { i };
                    usize::try_from(idx).ok().and_then(|i| a.get(i))
                } else if n.is_nan() {
                    a.first()
                } else {
                    None
                };
            }
            Some(Value::Object(o)) => cursor = o.get(segment),
            _ => return true,
        }
    }
    !matches!(cursor, Some(Value::Array(_)))
}

fn is_index(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit())
}

fn set_slot<'a>(container: &'a mut Value, segment: &str, value: Value) -> Option<&'a mut Value> {
    match container {
        Value::Array(a) => {
            let i: usize = segment.parse().ok()?;
            if a.len() <= i {
                a.resize(i + 1, Value::Null);
            }
            let slot = a.get_mut(i)?;
            *slot = value;
            Some(slot)
        }
        Value::Object(o) => {
            o.insert(segment.to_owned(), value);
            o.get_mut(segment)
        }
        _ => None,
    }
}

fn child_mut<'a>(container: &'a mut Value, segment: &str) -> Option<&'a mut Value> {
    match container {
        Value::Array(a) => a.get_mut(segment.parse::<usize>().ok()?),
        Value::Object(o) => o.get_mut(segment),
        _ => None,
    }
}

/// `setDisplayValueAtFieldPath` — membuat wadah di sepanjang jalur.
///
/// Catatan: lubang array JS (`[ , x]`) diserialisasi `null`; di sini diisi `null` langsung.
pub fn set_display_value_at_field_path(
    root: &mut Map<String, Value>,
    field_path: &str,
    value: Value,
) {
    let segments: Vec<&str> = field_path.split('.').collect();
    let mut holder = Value::Object(std::mem::take(root));
    set_path(&mut holder, &segments, value);
    if let Value::Object(o) = holder {
        *root = o;
    }
}

fn set_path(cursor: &mut Value, segments: &[&str], value: Value) {
    let Some((segment, rest)) = segments.split_first() else {
        return;
    };
    if rest.is_empty() {
        set_slot(cursor, segment, value);
        return;
    }
    let next_is_index = rest.first().is_some_and(|s| is_index(s));
    let is_obj = cursor.is_object();
    let exists = matches!(child_mut(cursor, segment), Some(v) if is_obj || !v.is_null());
    let next = if exists {
        child_mut(cursor, segment)
    } else {
        let fresh = if next_is_index {
            Value::Array(Vec::new())
        } else {
            Value::Object(Map::new())
        };
        set_slot(cursor, segment, fresh)
    };
    if let Some(next) = next {
        set_path(next, rest, value);
    }
}

/// Bentuk respons enrichment murni (bagian `enrichDisplayValues` sesudah query).
pub fn build_answers_display(
    answers: Option<&Value>,
    refs: &[AnswerProductOrderReference],
    product_labels: &HashMap<String, Option<String>>,
    order_labels: &HashMap<String, Option<String>>,
) -> Map<String, Value> {
    let mut display = Map::new();
    for r in refs {
        let labels = if r.source == OptionsSource::Product {
            product_labels
        } else {
            order_labels
        };
        let value: Vec<Value> = r
            .ids
            .iter()
            .map(|id| json!({ "id": id, "label": labels.get(id).cloned().flatten() }))
            .collect();
        let v = if is_single_choice_ref(answers, &r.field_path) {
            value.into_iter().next().unwrap_or(Value::Null)
        } else {
            Value::Array(value)
        };
        set_display_value_at_field_path(&mut display, &r.field_path, v);
    }
    display
}

/// `enrichDisplayValues(schema, answers, companyId)` (`form-dynamic-options.service.ts:81`).
/// `None` = tidak ada field product/order (Node: `undefined`, key `answersDisplay` absen).
pub async fn enrich_display_values(
    db: &PgPool,
    schema: &FormDocument,
    answers: Option<&Value>,
    company_id: i32,
) -> ApiResult<Option<Map<String, Value>>> {
    let refs = validate_answers(
        schema,
        answers,
        &ValidateAnswersOptions {
            mode: AnswerValidationMode::Draft,
            time_zone: "UTC",
        },
    )
    .product_order_refs;
    if refs.is_empty() {
        return Ok(None);
    }
    let (product_ids, order_ids) = split_ids(&refs);
    let products = fetch_labels(db, "product", Some("name"), company_id, &product_ids).await?;
    let orders = fetch_labels(db, "order", Some("orderNumber"), company_id, &order_ids).await?;
    Ok(Some(build_answers_display(
        answers, &refs, &products, &orders,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(source: OptionsSource, path: &str, ids: &[&str]) -> AnswerProductOrderReference {
        AnswerProductOrderReference {
            source,
            field_path: path.into(),
            ids: ids.iter().map(|s| (*s).to_owned()).collect(),
        }
    }

    #[test]
    fn missing_errors_messages() {
        let refs = [
            r(OptionsSource::Product, "unit_list.0.produk", &["p-other"]),
            r(OptionsSource::Order, "unit_list.0.order", &["o-other"]),
            r(OptionsSource::Product, "ok", &["p-1"]),
        ];
        let found_p: HashSet<String> = ["p-1".to_owned()].into();
        let errors = missing_reference_errors(&refs, &found_p, &HashSet::new());
        assert_eq!(
            errors,
            vec![
                json!({"key": "unit_list.0.produk", "message": "does not reference a product in this company"}),
                json!({"key": "unit_list.0.order", "message": "does not reference a order in this company"}),
            ]
        );
        let body = match option_not_found_error(errors) {
            ApiError::Body { status, body } => {
                assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
                body
            }
            _ => panic!(),
        };
        assert_eq!(body["code"], "FORM_OPTION_NOT_FOUND");
        assert_eq!(body["message"], "Answers did not pass validation");
        assert_eq!(body["details"]["errors"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn split_and_empty() {
        let (p, o) = split_ids(&[]);
        assert!(p.is_empty() && o.is_empty());
        let (p, o) = split_ids(&[
            r(OptionsSource::Product, "a", &["p1", "p2"]),
            r(OptionsSource::Order, "b", &["o1"]),
        ]);
        assert_eq!((p, o), (vec!["p1".into(), "p2".into()], vec!["o1".into()]));
    }

    #[test]
    fn display_single_vs_multi_and_nested() {
        let answers = json!({"produk": "p-1", "order": ["o-1"], "unit_list": [{"x": "p-2"}, {"x": "p-ghost"}]});
        let refs = [
            r(OptionsSource::Product, "produk", &["p-1"]),
            r(OptionsSource::Order, "order", &["o-1"]),
            r(OptionsSource::Product, "unit_list.1.x", &["p-ghost"]),
        ];
        let products: HashMap<String, Option<String>> =
            [("p-1".to_owned(), Some("Produk Nonaktif".to_owned()))].into();
        let orders: HashMap<String, Option<String>> =
            [("o-1".to_owned(), Some("DRAFT-a1b2c3d4".to_owned()))].into();
        let d = build_answers_display(Some(&answers), &refs, &products, &orders);
        assert_eq!(
            Value::Object(d),
            json!({
                "produk": {"id": "p-1", "label": "Produk Nonaktif"},
                "order": [{"id": "o-1", "label": "DRAFT-a1b2c3d4"}],
                "unit_list": [null, {"x": {"id": "p-ghost", "label": null}}]
            })
        );
    }

    #[test]
    fn single_choice_detection() {
        let a = json!({"r": [{"m": ["x"]}, {"s": "y"}]});
        assert!(!is_single_choice_ref(Some(&a), "r.0.m"));
        assert!(is_single_choice_ref(Some(&a), "r.1.s"));
        assert!(is_single_choice_ref(Some(&json!("x")), "a.b"));
    }

    #[test]
    fn non_uuid_ids_fail_like_prisma() {
        assert!(parse_uuids(&["p-1".into()]).is_err());
        assert!(parse_uuids(&["3ab70000-1111-4222-8333-444455556666".into()]).is_ok());
    }
}
