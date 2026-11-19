//! Bagian bersama: query paginasi `PaginatorDto`, amplop respons,
//! helper invalidasi cache.

use mz_audit::{Columns, Lookup, Model, Row, read_rows};
use mz_core::extract::Validate;
use mz_core::{ApiError, ApiResult, AppState};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{Postgres, QueryBuilder};

const NUMBER_MSG: &str = "must be a number conforming to the specified constraints";

/// Galat Prisma yang di Node muncul sebagai 500 `Internal server error` (validasi argumen,
/// P2025 pada `update`, TypeError, dst).
pub fn node_500(why: &str) -> ApiError {
    ApiError::internal(anyhow::anyhow!("{why}"))
}

/// `parseInt(s, 10)`: awalan angka (spasi di depan dibuang, tanda `+`/`-`), selain itu NaN (`None`).
pub fn js_parse_int(s: &str) -> Option<i64> {
    let t = s.trim_start_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
    let (neg, rest) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let n: i64 = digits.parse().ok()?;
    Some(if neg { -n } else { n })
}

/// `{ success, message, data }` dengan `data` berupa objek generik.
#[derive(Serialize)]
pub struct Envelope<T: Serialize> {
    pub success: bool,
    pub message: String,
    pub data: T,
}

pub fn envelope<T: Serialize>(message: impl Into<String>, data: T) -> Envelope<T> {
    Envelope {
        success: true,
        message: message.into(),
        data,
    }
}

/// `{ success: false, message, data: null }`.
#[derive(Serialize)]
pub struct Failure {
    pub success: bool,
    pub message: String,
    pub data: Option<()>,
}

pub fn failure(message: impl Into<String>) -> Failure {
    Failure {
        success: false,
        message: message.into(),
        data: None,
    }
}

/// `PaginatorDto` (query string). Nilai mentah ditahan sebagai JSON supaya array/objek
/// (`?pageNumber=1&pageNumber=2`) gagal validasi seperti di Node, bukan di deserialisasi.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PageQuery {
    page_number: Option<Value>,
    items_per_page: Option<Value>,
    order_by: Option<Value>,
    order_column: Option<Value>,
    search: Option<Value>,
}

/// `@Transform(parseInt)` lalu `@IsNumber()`: `Ok(None)` = tidak dikirim, `Err` = gagal.
fn number_field(v: Option<&Value>) -> Result<Option<i64>, ()> {
    match v {
        None => Ok(None),
        Some(Value::String(s)) => js_parse_int(s).map(Some).ok_or(()),
        Some(_) => Err(()),
    }
}

fn string_field(v: Option<&Value>) -> Result<Option<String>, ()> {
    match v {
        None => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        Some(_) => Err(()),
    }
}

impl Validate for PageQuery {
    fn validate(&self) -> Vec<String> {
        let mut out = Vec::new();
        if number_field(self.page_number.as_ref()).is_err() {
            out.push(format!("pageNumber {NUMBER_MSG}"));
        }
        if number_field(self.items_per_page.as_ref()).is_err() {
            out.push(format!("itemsPerPage {NUMBER_MSG}"));
        }
        if string_field(self.order_by.as_ref()).is_err() {
            out.push("orderBy must be a string".to_owned());
        }
        if string_field(self.order_column.as_ref()).is_err() {
            out.push("orderColumn must be a string".to_owned());
        }
        if string_field(self.search.as_ref()).is_err() {
            out.push("search must be a string".to_owned());
        }
        out
    }
}

/// Argumen hasil validasi.
#[derive(Debug, Clone, Default)]
pub struct PageArgs {
    pub page_number: Option<i64>,
    pub items_per_page: Option<i64>,
    pub order_by: Option<String>,
    pub order_column: Option<String>,
    pub search: Option<String>,
}

impl PageQuery {
    pub fn args(&self) -> PageArgs {
        PageArgs {
            page_number: number_field(self.page_number.as_ref()).ok().flatten(),
            items_per_page: number_field(self.items_per_page.as_ref()).ok().flatten(),
            order_by: string_field(self.order_by.as_ref()).ok().flatten(),
            order_column: string_field(self.order_column.as_ref()).ok().flatten(),
            search: string_field(self.search.as_ref()).ok().flatten(),
        }
    }
}

/// `data` daftar: `{ total, itemsPerPage, pageNumber, result }`.
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PageData<T: Serialize> {
    pub total: i64,
    pub items_per_page: i64,
    pub page_number: i64,
    pub result: Vec<T>,
}

pub struct Page {
    pub total: i64,
    pub items_per_page: i64,
    pub page_number: i64,
    pub rows: Vec<Row>,
}

/// Port bagian bersama `findAll` (companies & access-levels): `count`, bawaan semua-kosong
/// (`page 1`, `itemsPerPage = total`, `createdAt desc`), lalu `findMany` ber-`orderBy`/`take`/`skip`.
/// `push_where` menulis isi klausa `WHERE` (dipakai dua kali: hitung & ambil id).
///
/// Kombinasi yang di Node menjadi galat Prisma (NaN pada `take`/`skip`, `orderBy` bukan
/// asc/desc, kolom urut tidak dikenal) menjadi 500, seperti filter Prisma-nya.
pub async fn paginate<F>(
    state: &AppState,
    model: Model,
    push_where: F,
    args: &PageArgs,
) -> ApiResult<Page>
where
    F: for<'a> Fn(&mut QueryBuilder<'a, Postgres>),
{
    let table = model.as_str();
    let mut count_qb =
        QueryBuilder::<Postgres>::new(format!(r#"SELECT count(*) FROM "{table}" WHERE "#));
    push_where(&mut count_qb);
    let total: i64 = count_qb.build_query_scalar().fetch_one(&state.db).await?;

    // `!query.pageNumber && !query.itemsPerPage && !query.orderBy && !query.orderColumn`.
    let falsy_num = |n: Option<i64>| n.is_none_or(|n| n == 0);
    let falsy_str = |s: &Option<String>| s.as_deref().is_none_or(str::is_empty);
    let all_unset = falsy_num(args.page_number)
        && falsy_num(args.items_per_page)
        && falsy_str(&args.order_by)
        && falsy_str(&args.order_column);

    let (page_number, items_per_page, order_by, order_column) = if all_unset {
        (1, total, Some("desc".to_owned()), "createdAt".to_owned())
    } else {
        let page = args
            .page_number
            .ok_or_else(|| node_500("skip: NaN (pageNumber)"))?;
        let items = args
            .items_per_page
            .ok_or_else(|| node_500("take: NaN (itemsPerPage)"))?;
        let col = args
            .order_column
            .clone()
            .ok_or_else(|| node_500("orderBy: unknown field undefined"))?;
        (page, items, args.order_by.clone(), col)
    };

    let direction = match order_by.as_deref() {
        None => None,
        Some("asc") => Some("ASC"),
        Some("desc") => Some("DESC"),
        Some(_) => return Err(node_500("orderBy: invalid sort order")),
    };

    // Kolom urut harus kolom skalar model (daftar kolom = kunci baris contoh).
    let sample = read_rows(
        &state.db,
        model,
        Columns::All,
        &Lookup::filter(|qb| {
            qb.push("true");
        }),
        None,
        Some(1),
    )
    .await?;
    let known = sample
        .first()
        .is_none_or(|row| row.keys().any(|k| k == order_column));
    if !known {
        return Err(node_500("orderBy: unknown field"));
    }

    let skip = page_number
        .checked_sub(1)
        .and_then(|p| p.checked_mul(items_per_page))
        .ok_or_else(|| node_500("skip: out of range"))?;

    let mut qb = QueryBuilder::<Postgres>::new(format!(r#"SELECT id FROM "{table}" WHERE "#));
    push_where(&mut qb);
    qb.push(format!(
        r#" ORDER BY "{}""#,
        order_column.replace('"', "\"\"")
    ));
    match direction {
        Some(d) => qb.push(format!(" {d}")),
        None => qb.push(""),
    };
    qb.push(", id LIMIT ").push_bind(items_per_page);
    qb.push(" OFFSET ").push_bind(skip);
    let ids: Vec<i32> = qb.build_query_scalar().fetch_all(&state.db).await?;

    let mut rows = read_rows(
        &state.db,
        model,
        Columns::All,
        &Lookup::Ids(ids.clone()),
        None,
        None,
    )
    .await?;
    rows.sort_by_key(|r| {
        ids.iter()
            .position(|id| r.int("id") == Some(i64::from(*id)))
            .unwrap_or(usize::MAX)
    });

    Ok(Page {
        total,
        items_per_page,
        page_number,
        rows,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_int_follows_js() {
        assert_eq!(js_parse_int("12abc"), Some(12));
        assert_eq!(js_parse_int(" -3"), Some(-3));
        assert_eq!(js_parse_int("abc"), None);
        assert_eq!(js_parse_int(""), None);
        assert_eq!(js_parse_int("+7"), Some(7));
    }

    #[test]
    fn page_query_validation_messages() {
        let q: PageQuery = serde_json::from_value(serde_json::json!({
            "pageNumber": "x", "itemsPerPage": ["1", "2"], "orderBy": ["a"]
        }))
        .unwrap_or_default();
        assert_eq!(
            q.validate(),
            vec![
                format!("pageNumber {NUMBER_MSG}"),
                format!("itemsPerPage {NUMBER_MSG}"),
                "orderBy must be a string".to_owned()
            ]
        );
    }
}
