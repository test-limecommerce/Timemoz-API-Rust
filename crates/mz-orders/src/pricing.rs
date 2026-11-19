//! Port `src/orders/pricing/**` — matematika baris (§5.2), total header (§5.3), batas diskon
//! (§5.4), jatuh tempo (§5.7), resolusi harga (§5.1), carry diskon tersensor (N1), dan
//! matematika delivery (specs/003 R3). Murni, tanpa DB.
//!
//! Uang = string desimal (§0.4). Aritmetika meniru `Prisma.Decimal` (decimal.js, presisi 20
//! digit signifikan, `ROUND_HALF_UP`): setiap operasi dibulatkan ke 20 digit signifikan, lalu
//! `round2`/`toDecimalPlaces(4)` memakai pembulatan menjauhi nol. `toString()` decimal.js
//! membuang nol buntut → [`dstr`].

use std::collections::{HashMap, HashSet};
use std::str::FromStr;

use chrono::{DateTime, Days, NaiveDate, Utc};
use rust_decimal::{Decimal, RoundingStrategy};

use mz_products::util::{compare_decimal_strings, is_price_list_active_today};

// ───────────────────────── decimal ala decimal.js ─────────────────────────

/// `new Prisma.Decimal(s)`; string tak valid → 0 (input sudah lolos regex DTO / berasal dari DB).
pub fn dec(s: &str) -> Decimal {
    Decimal::from_str(s.trim())
        .or_else(|_| Decimal::from_scientific(s.trim()))
        .unwrap_or_default()
}

/// `decimal.toString()` decimal.js (tanpa nol buntut, `-0` → `0`).
pub fn dstr(d: Decimal) -> String {
    let n = d.normalize();
    if n.is_zero() {
        "0".to_owned()
    } else {
        n.to_string()
    }
}

/// Pembulatan hasil operasi ke 20 digit signifikan (`Decimal.precision = 20`, `ROUND_HALF_UP`).
fn sig20(d: Decimal) -> Decimal {
    if d.is_zero() {
        return d;
    }
    let a = d.abs();
    let int_digits = a
        .trunc()
        .normalize()
        .to_string()
        .trim_start_matches('0')
        .len();
    let dp: i64 = if int_digits > 0 {
        20 - i64::try_from(int_digits).unwrap_or(20)
    } else {
        // 0.000ddd → nol di depan digit signifikan pertama.
        let frac = a.normalize().to_string();
        let digits = frac.split_once('.').map_or("", |(_, f)| f);
        let lead = digits.len() - digits.trim_start_matches('0').len();
        i64::try_from(lead).unwrap_or(0) + 20
    };
    let dp = u32::try_from(dp.clamp(0, 28)).unwrap_or(28);
    if d.scale() <= dp {
        return d;
    }
    d.round_dp_with_strategy(dp, RoundingStrategy::MidpointAwayFromZero)
}

pub fn add(a: Decimal, b: Decimal) -> Decimal {
    sig20(a.checked_add(b).unwrap_or(Decimal::MAX))
}

pub fn sub(a: Decimal, b: Decimal) -> Decimal {
    sig20(a.checked_sub(b).unwrap_or(Decimal::MIN))
}

pub fn mul(a: Decimal, b: Decimal) -> Decimal {
    sig20(a.checked_mul(b).unwrap_or(Decimal::MAX))
}

pub fn div(a: Decimal, b: Decimal) -> Decimal {
    sig20(a.checked_div(b).unwrap_or_default())
}

/// `round2(value)` — `toDecimalPlaces(2, ROUND_HALF_UP)`.
pub fn round2(d: Decimal) -> Decimal {
    d.round_dp_with_strategy(2, RoundingStrategy::MidpointAwayFromZero)
}

fn round4(d: Decimal) -> Decimal {
    d.round_dp_with_strategy(4, RoundingStrategy::MidpointAwayFromZero)
}

const HUNDRED: Decimal = Decimal::ONE_HUNDRED;

// ───────────────────────── order-line-math.ts ─────────────────────────

/// `DiscountExceedsLineError`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiscountExceedsLine;

/// `ComputeLineInput`.
#[derive(Debug, Clone, Default)]
pub struct LineInput {
    pub qty: String,
    pub unit_price: String,
    pub conversion_rate: String,
    pub discount_percent: String,
    pub discount_amount: String,
    pub tax_rate: String,
    pub tax_is_inclusive: bool,
}

/// `ComputeLineResult` (semua string, nol buntut dibuang).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct LineResult {
    pub qty_in_base_uom: String,
    pub gross_amount: String,
    pub discount_value: String,
    pub net_amount: String,
    pub tax_amount: String,
    pub line_total: String,
}

fn or0(s: &str) -> &str {
    if s.is_empty() { "0" } else { s }
}

/// Node: src/orders/pricing/order-line-math.ts:48 `computeLine`.
pub fn compute_line(input: &LineInput) -> Result<LineResult, DiscountExceedsLine> {
    let qty = dec(&input.qty);
    let unit_price = dec(&input.unit_price);
    let conversion_rate = dec(&input.conversion_rate);
    let discount_percent = dec(or0(&input.discount_percent));
    let discount_amount = dec(or0(&input.discount_amount));
    let tax_rate = dec(or0(&input.tax_rate));

    let qty_in_base = round4(mul(qty, conversion_rate));
    let gross = round2(mul(qty, unit_price));
    let discount_value = add(
        round2(div(mul(gross, discount_percent), HUNDRED)),
        discount_amount,
    );
    if discount_value > gross {
        return Err(DiscountExceedsLine);
    }
    let net = sub(gross, discount_value);
    let (tax, total) = if input.tax_is_inclusive {
        let base = round2(div(net, add(div(tax_rate, HUNDRED), Decimal::ONE)));
        (sub(net, base), net)
    } else {
        let tax = round2(div(mul(net, tax_rate), HUNDRED));
        (tax, add(net, tax))
    };
    Ok(LineResult {
        qty_in_base_uom: dstr(qty_in_base),
        gross_amount: dstr(gross),
        discount_value: dstr(discount_value),
        net_amount: dstr(net),
        tax_amount: dstr(tax),
        line_total: dstr(total),
    })
}

// ───────────────────────── order-totals.ts ─────────────────────────

/// `OrderTotals`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Totals {
    pub subtotal: String,
    pub discount_total: String,
    pub tax_total: String,
    pub grand_total: String,
    pub item_count: i32,
}

/// Node: src/orders/pricing/order-totals.ts:19 `computeTotals`.
pub fn compute_totals(lines: &[LineResult]) -> Totals {
    let (mut s, mut d, mut t, mut g) = (Decimal::ZERO, Decimal::ZERO, Decimal::ZERO, Decimal::ZERO);
    for l in lines {
        s = add(s, dec(&l.gross_amount));
        d = add(d, dec(&l.discount_value));
        t = add(t, dec(&l.tax_amount));
        g = add(g, dec(&l.line_total));
    }
    Totals {
        subtotal: dstr(s),
        discount_total: dstr(d),
        tax_total: dstr(t),
        grand_total: dstr(g),
        item_count: i32::try_from(lines.len()).unwrap_or(i32::MAX),
    }
}

/// `DiscountLimitLine`.
#[derive(Debug, Clone)]
pub struct DiscountLimitLine {
    pub gross_amount: String,
    pub discount_value: String,
    pub is_free_good: bool,
}

/// `DiscountLimitResult`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscountLimitResult {
    pub ok: bool,
    pub percent_used: String,
}

/// Node: src/orders/pricing/order-totals.ts:62 `checkDiscountLimit`.
pub fn check_discount_limit(
    lines: &[DiscountLimitLine],
    max_discount_percent: Option<&str>,
) -> DiscountLimitResult {
    let (mut den, mut disc) = (Decimal::ZERO, Decimal::ZERO);
    for l in lines.iter().filter(|l| !l.is_free_good) {
        den = add(den, dec(&l.gross_amount));
        disc = add(disc, dec(&l.discount_value));
    }
    if den.is_zero() {
        return DiscountLimitResult {
            ok: true,
            percent_used: "0".into(),
        };
    }
    let used = mul(div(disc, den), HUNDRED);
    let ok = max_discount_percent.is_none_or(|m| used <= dec(m));
    DiscountLimitResult {
        ok,
        percent_used: dstr(round2(used)),
    }
}

/// Node: src/orders/pricing/order-totals.ts:89 `computeEffectiveDiscountPercent`.
pub fn compute_effective_discount_percent(subtotal: &str, discount_total: &str) -> String {
    check_discount_limit(
        &[DiscountLimitLine {
            gross_amount: subtotal.to_owned(),
            discount_value: discount_total.to_owned(),
            is_free_good: false,
        }],
        None,
    )
    .percent_used
}

// ───────────────────────── due-date.ts ─────────────────────────

/// Node: src/orders/pricing/due-date.ts:18 `computeDueDate` — tanggal kalender + `dueDays`.
/// Zona hanya menambatkan aritmetika (hasil tidak bergantung zona untuk tanggal polos).
pub fn compute_due_date(order_date: NaiveDate, due_days: i32, _time_zone: &str) -> String {
    let d = if due_days >= 0 {
        order_date.checked_add_days(Days::new(u64::try_from(due_days).unwrap_or(0)))
    } else {
        order_date.checked_sub_days(Days::new(u64::from(due_days.unsigned_abs())))
    };
    d.unwrap_or(order_date).format("%Y-%m-%d").to_string()
}

// ───────────────────────── carry-censored-discounts.ts ─────────────────────────

/// `CENSORABLE_DISCOUNT_COLUMNS`.
pub const CENSORABLE_DISCOUNT_COLUMNS: [&str; 2] = ["discountPercent", "discountAmount"];

/// `StoredItemDiscount`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredItemDiscount {
    pub line_no: i32,
    pub product_id: i32,
    pub uom_id: i32,
    pub is_free_good: bool,
    pub discount_percent: String,
    pub discount_amount: String,
}

/// `IncomingItemKey`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IncomingItemKey {
    pub product_id: i32,
    pub uom_id: i32,
    pub is_free_good: bool,
}

/// `CarriedDiscount` — kunci hanya untuk kolom tersensor.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CarriedDiscount {
    pub discount_percent: Option<String>,
    pub discount_amount: Option<String>,
}

/// Node: src/orders/pricing/carry-censored-discounts.ts:47 `carryCensoredDiscounts`.
pub fn carry_censored_discounts(
    incoming: &[IncomingItemKey],
    stored: &[StoredItemDiscount],
    censored: &HashSet<&str>,
    allow_discount: bool,
) -> Vec<CarriedDiscount> {
    if censored.is_empty() {
        return incoming
            .iter()
            .map(|_| CarriedDiscount::default())
            .collect();
    }
    let mut by_line: Vec<(usize, &StoredItemDiscount)> = stored.iter().enumerate().collect();
    by_line.sort_by_key(|(i, s)| (s.line_no, *i));
    let mut claimed: HashSet<usize> = HashSet::new();
    let mut matched: HashMap<usize, &StoredItemDiscount> = HashMap::new();
    for (index, row) in incoming.iter().enumerate() {
        if !(allow_discount && !row.is_free_good) {
            continue;
        }
        let hit = by_line.iter().find(|(i, s)| {
            !claimed.contains(i)
                && !s.is_free_good
                && s.product_id == row.product_id
                && s.uom_id == row.uom_id
        });
        if let Some((i, s)) = hit {
            claimed.insert(*i);
            matched.insert(index, s);
        }
    }
    (0..incoming.len())
        .map(|index| {
            let hit = matched.get(&index);
            let pick = |col: &str, v: fn(&StoredItemDiscount) -> &String| {
                censored
                    .contains(col)
                    .then(|| hit.map_or_else(|| "0".to_owned(), |h| v(h).clone()))
            };
            CarriedDiscount {
                discount_percent: pick("discountPercent", |s| &s.discount_percent),
                discount_amount: pick("discountAmount", |s| &s.discount_amount),
            }
        })
        .collect()
}

// ───────────────────────── resolve-price.ts ─────────────────────────

/// `PriceListSnapshot` (+ `currencyCode` untuk `OrderPriceList`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriceListSnap {
    pub id: i32,
    pub is_active: bool,
    pub start_date: Option<NaiveDate>,
    pub end_date: Option<NaiveDate>,
    pub currency_code: String,
}

/// `PriceListItemCandidate`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriceItemCandidate {
    pub id: i32,
    pub price_list_id: i32,
    pub min_qty: String,
    pub max_qty: Option<String>,
    pub price: String,
}

/// `PriceResolution` / `UomPriceResolution` (`found: true`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriceFound {
    pub price_list_id: i32,
    pub price_list_item_id: i32,
    pub unit_price: String,
    pub derived_from_base: bool,
}

fn active(pl: &PriceListSnap, tz: &str, now: DateTime<Utc>) -> bool {
    is_price_list_active_today(pl.is_active, pl.start_date, pl.end_date, tz, now)
}

/// Node: src/orders/pricing/resolve-price.ts:56 `resolveEffectivePriceList`.
pub fn resolve_effective_price_list<'a>(
    customer: Option<&'a PriceListSnap>,
    default: Option<&'a PriceListSnap>,
    tz: &str,
    now: DateTime<Utc>,
) -> Option<&'a PriceListSnap> {
    if let Some(c) = customer
        && active(c, tz, now)
    {
        return Some(c);
    }
    default.filter(|d| active(d, tz, now))
}

/// Node: src/orders/pricing/resolve-price.ts:71 `resolvePrice`.
pub fn resolve_price(
    customer: Option<&PriceListSnap>,
    default: Option<&PriceListSnap>,
    items: &[PriceItemCandidate],
    qty: &str,
    tz: &str,
    now: DateTime<Utc>,
) -> Option<PriceFound> {
    let resolved = resolve_effective_price_list(customer, default, tz, now)?;
    let tier = items.iter().find(|i| {
        i.price_list_id == resolved.id
            && compare_decimal_strings(qty, &i.min_qty).is_ge()
            && i.max_qty
                .as_deref()
                .is_none_or(|m| compare_decimal_strings(qty, m).is_le())
    })?;
    Some(PriceFound {
        price_list_id: resolved.id,
        price_list_item_id: tier.id,
        unit_price: tier.price.clone(),
        derived_from_base: false,
    })
}

/// Node: src/orders/pricing/resolve-price.ts:119 `resolveUomPrice` (§5.1 langkah 4b).
#[allow(clippy::too_many_arguments)]
pub fn resolve_uom_price(
    customer: Option<&PriceListSnap>,
    default: Option<&PriceListSnap>,
    uom_items: &[PriceItemCandidate],
    base_items: Option<&[PriceItemCandidate]>,
    qty: &str,
    conversion_rate: &str,
    tz: &str,
    now: DateTime<Utc>,
) -> Option<PriceFound> {
    let resolved = resolve_effective_price_list(customer, default, tz, now)?;
    let listed = uom_items.iter().any(|i| i.price_list_id == resolved.id);
    let Some(base_items) = base_items.filter(|_| !listed) else {
        return resolve_price(customer, default, uom_items, qty, tz, now);
    };
    let rate = dec(conversion_rate);
    let qty_in_base = round4(mul(dec(qty), rate));
    let base = resolve_price(customer, default, base_items, &dstr(qty_in_base), tz, now)?;
    Some(PriceFound {
        unit_price: dstr(round2(mul(dec(&base.unit_price), rate))),
        derived_from_base: true,
        ..base
    })
}

// ───────────────────────── order-delivery-math.ts ─────────────────────────

/// `DeliverableLine`: snapshot + hasil tersimpan satu `orderItem` (string).
#[derive(Debug, Clone, Default)]
pub struct DeliverableLine {
    pub qty: String,
    pub unit_price: String,
    pub conversion_rate: String,
    pub discount_percent: String,
    pub discount_amount: String,
    pub tax_rate: String,
    pub tax_is_inclusive: bool,
    pub stored: LineResult,
}

/// Node: src/orders/pricing/order-delivery-math.ts:27 `computeDeliveredLine`.
pub fn compute_delivered_line(line: &DeliverableLine, delivered_qty: &str) -> LineResult {
    let qty = dec(&line.qty);
    let delivered = dec(delivered_qty);
    if delivered == qty {
        return line.stored.clone();
    }
    let base = |discount_amount: String| LineInput {
        qty: dstr(delivered),
        unit_price: line.unit_price.clone(),
        conversion_rate: line.conversion_rate.clone(),
        discount_percent: line.discount_percent.clone(),
        discount_amount,
        tax_rate: line.tax_rate.clone(),
        tax_is_inclusive: line.tax_is_inclusive,
    };
    let scaled = round2(div(mul(dec(&line.discount_amount), delivered), qty));
    match compute_line(&base(dstr(scaled))) {
        Ok(r) => r,
        Err(DiscountExceedsLine) => {
            let gross = round2(mul(delivered, dec(&line.unit_price)));
            let percent_part = round2(div(mul(gross, dec(&line.discount_percent)), HUNDRED));
            let clamped = sub(gross, percent_part).max(Decimal::ZERO);
            compute_line(&base(dstr(clamped))).unwrap_or_default()
        }
    }
}

/// Node: src/orders/pricing/order-delivery-math.ts:71 `computeDeliveredTotals`.
pub fn compute_delivered_totals(lines: &[LineResult]) -> Totals {
    compute_totals(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(
        qty: &str,
        price: &str,
        rate: &str,
        pct: &str,
        amt: &str,
        tax: &str,
        incl: bool,
    ) -> LineInput {
        LineInput {
            qty: qty.into(),
            unit_price: price.into(),
            conversion_rate: rate.into(),
            discount_percent: pct.into(),
            discount_amount: amt.into(),
            tax_rate: tax.into(),
            tax_is_inclusive: incl,
        }
    }

    // ── order-line-math.spec.ts ──
    #[test]
    fn round2_half_up() {
        assert_eq!(dstr(round2(dec("10.005"))), "10.01");
        assert_eq!(dstr(round2(dec("0.125"))), "0.13");
    }

    #[test]
    fn tax_exclusive() {
        let r = compute_line(&line("10", "1000", "1", "", "", "10", false)).unwrap();
        assert_eq!(
            (
                r.gross_amount.as_str(),
                r.discount_value.as_str(),
                r.net_amount.as_str()
            ),
            ("10000", "0", "10000")
        );
        assert_eq!(
            (r.tax_amount.as_str(), r.line_total.as_str()),
            ("1000", "11000")
        );
    }

    #[test]
    fn tax_inclusive_backed_out() {
        let r = compute_line(&line("1", "11000", "1", "", "", "10", true)).unwrap();
        assert_eq!(r.gross_amount, "11000");
        assert_eq!(r.net_amount, "11000");
        assert_eq!(r.tax_amount, "1000");
        assert_eq!(r.line_total, "11000");
    }

    #[test]
    fn free_goods_zero() {
        let r = compute_line(&line("5", "0", "1", "", "", "10", false)).unwrap();
        assert_eq!(
            r,
            LineResult {
                qty_in_base_uom: "5".into(),
                ..zero_line()
            }
        );
    }

    fn zero_line() -> LineResult {
        LineResult {
            qty_in_base_uom: "0".into(),
            gross_amount: "0".into(),
            discount_value: "0".into(),
            net_amount: "0".into(),
            tax_amount: "0".into(),
            line_total: "0".into(),
        }
    }

    #[test]
    fn percent_then_nominal() {
        let r = compute_line(&line("10", "1000", "1", "10", "500", "", false)).unwrap();
        assert_eq!(r.gross_amount, "10000");
        assert_eq!(r.discount_value, "1500");
        assert_eq!(r.net_amount, "8500");
    }

    #[test]
    fn discount_over_gross_rejected_equal_allowed() {
        assert_eq!(
            compute_line(&line("1", "100", "1", "", "150", "", false)),
            Err(DiscountExceedsLine)
        );
        let r = compute_line(&line("1", "100", "1", "", "100", "", false)).unwrap();
        assert_eq!((r.net_amount.as_str(), r.line_total.as_str()), ("0", "0"));
    }

    #[test]
    fn qty_in_base_rounded_4dp() {
        let r = compute_line(&line("3", "100", "0.33335", "", "", "", false)).unwrap();
        assert_eq!(r.qty_in_base_uom, "1.0001");
    }

    // ── order-totals.spec.ts ──
    #[test]
    fn totals_sum_rounded_lines() {
        let a = compute_line(&line("1", "10.005", "1", "", "", "", false)).unwrap();
        let t = compute_totals(&[a.clone(), a.clone()]);
        assert_eq!(a.gross_amount, "10.01");
        assert_eq!(t.subtotal, "20.02");
        assert_eq!(t.grand_total, "20.02");
        assert_eq!(t.item_count, 2);
        let e = compute_totals(&[]);
        assert_eq!(
            e,
            Totals {
                subtotal: "0".into(),
                discount_total: "0".into(),
                tax_total: "0".into(),
                grand_total: "0".into(),
                item_count: 0
            }
        );
    }

    fn dl(g: &str, d: &str, free: bool) -> DiscountLimitLine {
        DiscountLimitLine {
            gross_amount: g.into(),
            discount_value: d.into(),
            is_free_good: free,
        }
    }

    #[test]
    fn discount_limit() {
        assert!(check_discount_limit(&[dl("1000", "900", false)], None).ok);
        let r = check_discount_limit(
            &[dl("1000", "100", false), dl("5000", "5000", true)],
            Some("20"),
        );
        assert_eq!((r.percent_used.as_str(), r.ok), ("10", true));
        let r = check_discount_limit(&[dl("1000", "200", false)], Some("20"));
        assert_eq!((r.percent_used.as_str(), r.ok), ("20", true));
        assert!(!check_discount_limit(&[dl("1000", "201", false)], Some("20")).ok);
        let r = check_discount_limit(&[dl("1000", "1000", true)], Some("0"));
        assert_eq!((r.percent_used.as_str(), r.ok), ("0", true));
        assert_eq!(compute_effective_discount_percent("1000", "100"), "10");
        assert_eq!(compute_effective_discount_percent("0", "0"), "0");
        assert_eq!(compute_effective_discount_percent("30000", "1000"), "3.33");
    }

    // ── due-date.spec.ts ──
    #[test]
    fn due_date() {
        let d = |s: &str| NaiveDate::parse_from_str(s, "%Y-%m-%d").unwrap();
        let j = "Asia/Jakarta";
        assert_eq!(compute_due_date(d("2026-03-10"), 7, j), "2026-03-17");
        assert_eq!(compute_due_date(d("2026-01-25"), 10, j), "2026-02-04");
        assert_eq!(compute_due_date(d("2026-12-28"), 10, j), "2027-01-07");
        assert_eq!(compute_due_date(d("2026-06-15"), 0, j), "2026-06-15");
        assert_eq!(
            compute_due_date(d("2026-01-31"), 1, "America/New_York"),
            "2026-02-01"
        );
    }

    // ── carry-censored-discounts.spec.ts ──
    fn stored(
        line_no: i32,
        product_id: i32,
        pct: &str,
        amt: &str,
        free: bool,
    ) -> StoredItemDiscount {
        StoredItemDiscount {
            line_no,
            product_id,
            uom_id: 10,
            is_free_good: free,
            discount_percent: pct.into(),
            discount_amount: amt.into(),
        }
    }
    fn row(product_id: i32, uom_id: i32, free: bool) -> IncomingItemKey {
        IncomingItemKey {
            product_id,
            uom_id,
            is_free_good: free,
        }
    }
    fn both() -> HashSet<&'static str> {
        CENSORABLE_DISCOUNT_COLUMNS.into_iter().collect()
    }
    fn cd(p: Option<&str>, a: Option<&str>) -> CarriedDiscount {
        CarriedDiscount {
            discount_percent: p.map(Into::into),
            discount_amount: a.map(Into::into),
        }
    }

    #[test]
    fn carry_rules() {
        let s1 = stored(1, 1, "10", "500", false);
        assert_eq!(
            carry_censored_discounts(
                &[row(1, 10, false)],
                std::slice::from_ref(&s1),
                &HashSet::new(),
                true
            ),
            vec![CarriedDiscount::default()]
        );
        assert_eq!(
            carry_censored_discounts(
                &[row(1, 10, false)],
                std::slice::from_ref(&s1),
                &both(),
                true
            ),
            vec![cd(Some("10"), Some("500"))]
        );
        let only: HashSet<&str> = ["discountPercent"].into_iter().collect();
        assert_eq!(
            carry_censored_discounts(&[row(1, 10, false)], std::slice::from_ref(&s1), &only, true),
            vec![cd(Some("10"), None)]
        );
        let before = [
            stored(1, 1, "5", "0", false),
            stored(2, 2, "20", "0", false),
        ];
        assert_eq!(
            carry_censored_discounts(&[row(2, 10, false)], &before, &both(), true),
            vec![cd(Some("20"), Some("0"))]
        );
        let before = [
            stored(1, 9, "1", "500", false),
            stored(2, 1, "5", "500", false),
            stored(3, 1, "15", "500", false),
        ];
        let r = carry_censored_discounts(
            &[row(1, 10, false), row(1, 10, false)],
            &before,
            &both(),
            true,
        );
        assert_eq!(r[0].discount_percent.as_deref(), Some("5"));
        assert_eq!(r[1].discount_percent.as_deref(), Some("15"));
        assert_eq!(
            carry_censored_discounts(
                &[row(1, 11, false)],
                std::slice::from_ref(&s1),
                &both(),
                true
            ),
            vec![cd(Some("0"), Some("0"))]
        );
        assert_eq!(
            carry_censored_discounts(&[row(1, 10, false)], &[], &both(), true),
            vec![cd(Some("0"), Some("0"))]
        );
        assert_eq!(
            carry_censored_discounts(
                &[row(1, 10, true)],
                std::slice::from_ref(&s1),
                &both(),
                true
            ),
            vec![cd(Some("0"), Some("0"))]
        );
        let before = [
            stored(1, 1, "0", "500", true),
            stored(2, 1, "12", "500", false),
        ];
        assert_eq!(
            carry_censored_discounts(&[row(1, 10, false)], &before, &both(), true)[0]
                .discount_percent
                .as_deref(),
            Some("12")
        );
        assert_eq!(
            carry_censored_discounts(&[row(1, 10, false)], &[s1], &both(), false),
            vec![cd(Some("0"), Some("0"))]
        );
    }

    // ── resolve-price.spec.ts ──
    fn pl(id: i32, is_active: bool, end: Option<&str>) -> PriceListSnap {
        PriceListSnap {
            id,
            is_active,
            start_date: None,
            end_date: end.map(|e| NaiveDate::parse_from_str(e, "%Y-%m-%d").unwrap()),
            currency_code: "IDR".into(),
        }
    }
    fn it(id: i32, pl: i32, min: &str, max: Option<&str>, price: &str) -> PriceItemCandidate {
        PriceItemCandidate {
            id,
            price_list_id: pl,
            min_qty: min.into(),
            max_qty: max.map(Into::into),
            price: price.into(),
        }
    }
    fn today() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-03-10T02:00:00Z")
            .unwrap()
            .with_timezone(&Utc)
    }
    const J: &str = "Asia/Jakarta";

    #[test]
    fn resolve_price_rules() {
        let active = pl(1, true, None);
        let inactive = pl(1, false, None);
        let expired = pl(1, true, Some("2026-01-01"));
        let def = pl(2, true, None);
        let items = vec![
            it(100, 1, "1", Some("9"), "1000"),
            it(101, 1, "10", None, "900"),
            it(200, 2, "1", None, "1100"),
        ];
        let id = |r: Option<PriceFound>| {
            r.map(|f| (f.price_list_id, f.price_list_item_id, f.unit_price))
        };
        assert_eq!(
            id(resolve_price(
                Some(&active),
                Some(&def),
                &items,
                "5",
                J,
                today()
            )),
            Some((1, 100, "1000".into()))
        );
        assert_eq!(
            id(resolve_price(
                Some(&active),
                Some(&def),
                &items,
                "10",
                J,
                today()
            )),
            Some((1, 101, "900".into()))
        );
        assert_eq!(
            id(resolve_price(
                Some(&active),
                Some(&def),
                &items,
                "9",
                J,
                today()
            )),
            Some((1, 100, "1000".into()))
        );
        assert_eq!(
            id(resolve_price(
                Some(&active),
                None,
                &items,
                "9.5",
                J,
                today()
            )),
            None
        );
        assert_eq!(
            id(resolve_price(None, Some(&def), &items, "5", J, today())),
            Some((2, 200, "1100".into()))
        );
        assert_eq!(
            id(resolve_price(
                Some(&inactive),
                Some(&def),
                &items,
                "5",
                J,
                today()
            )),
            Some((2, 200, "1100".into()))
        );
        assert_eq!(
            id(resolve_price(
                Some(&expired),
                Some(&def),
                &items,
                "5",
                J,
                today()
            )),
            Some((2, 200, "1100".into()))
        );
        assert_eq!(id(resolve_price(None, None, &items, "5", J, today())), None);
        assert_eq!(
            id(resolve_price(
                Some(&active),
                Some(&def),
                &items,
                "0.5",
                J,
                today()
            )),
            None
        );
        assert_eq!(
            resolve_effective_price_list(Some(&expired), None, J, today()),
            None
        );
        assert_eq!(
            resolve_effective_price_list(Some(&inactive), Some(&def), J, today()).map(|p| p.id),
            Some(2)
        );
    }

    #[test]
    fn resolve_uom_price_fallback() {
        let c = pl(1, true, None);
        let d = pl(2, true, None);
        let pcs = vec![
            it(10, 1, "1", Some("47"), "1000"),
            it(11, 1, "48", None, "900"),
        ];
        let r =
            resolve_uom_price(Some(&c), Some(&d), &[], Some(&pcs), "1", "24", J, today()).unwrap();
        assert_eq!(
            (
                r.price_list_item_id,
                r.unit_price.as_str(),
                r.derived_from_base
            ),
            (10, "24000", true)
        );
        let r =
            resolve_uom_price(Some(&c), Some(&d), &[], Some(&pcs), "2", "24", J, today()).unwrap();
        assert_eq!((r.price_list_item_id, r.unit_price.as_str()), (11, "21600"));
        let boxes = vec![it(20, 1, "1", None, "22000")];
        let r = resolve_uom_price(
            Some(&c),
            Some(&d),
            &boxes,
            Some(&pcs),
            "1",
            "24",
            J,
            today(),
        )
        .unwrap();
        assert_eq!((r.price_list_item_id, r.derived_from_base), (20, false));
        let boxes = vec![it(20, 1, "10", None, "22000")];
        assert!(
            resolve_uom_price(
                Some(&c),
                Some(&d),
                &boxes,
                Some(&pcs),
                "1",
                "24",
                J,
                today()
            )
            .is_none()
        );
        let other = vec![it(30, 2, "1", None, "1")];
        let r = resolve_uom_price(
            Some(&c),
            Some(&d),
            &other,
            Some(&pcs),
            "1",
            "24",
            J,
            today(),
        )
        .unwrap();
        assert_eq!((r.price_list_item_id, r.unit_price.as_str()), (10, "24000"));
        let pcs_def = vec![it(40, 2, "1", None, "1000")];
        assert!(
            resolve_uom_price(
                Some(&c),
                Some(&d),
                &[],
                Some(&pcs_def),
                "1",
                "24",
                J,
                today()
            )
            .is_none()
        );
        let kg = vec![it(50, 1, "0", None, "12345")];
        let r =
            resolve_uom_price(Some(&c), None, &[], Some(&kg), "500", "0.001", J, today()).unwrap();
        assert_eq!(r.unit_price, "12.35");
        let r = resolve_uom_price(Some(&c), Some(&d), &pcs, None, "5", "1", J, today()).unwrap();
        assert_eq!(
            (
                r.price_list_item_id,
                r.unit_price.as_str(),
                r.derived_from_base
            ),
            (10, "1000", false)
        );
    }

    // ── order-delivery-math.spec.ts ──
    fn stored_line(
        qty: &str,
        price: &str,
        rate: &str,
        pct: &str,
        amt: &str,
        tax: &str,
        incl: bool,
    ) -> DeliverableLine {
        let input = line(qty, price, rate, pct, amt, tax, incl);
        let stored = compute_line(&input).unwrap();
        DeliverableLine {
            qty: input.qty,
            unit_price: input.unit_price,
            conversion_rate: input.conversion_rate,
            discount_percent: or0(&input.discount_percent).into(),
            discount_amount: or0(&input.discount_amount).into(),
            tax_rate: or0(&input.tax_rate).into(),
            tax_is_inclusive: incl,
            stored,
        }
    }

    #[test]
    fn delivered_full_copies_verbatim() {
        let mut l = stored_line("3", "0.35", "1", "50", "0.52", "11", false);
        l.stored.gross_amount = "1.05".into();
        l.stored.line_total = "0.00".into();
        let r = compute_delivered_line(&l, "3.0000");
        assert_eq!(r.gross_amount, "1.05");
        assert_eq!(r.line_total, "0.00");
    }

    #[test]
    fn delivered_short_lines() {
        let l = stored_line("5", "10000", "1", "10", "", "11", false);
        let r = compute_delivered_line(&l, "3");
        assert_eq!(
            [
                r.gross_amount,
                r.discount_value,
                r.net_amount,
                r.tax_amount,
                r.line_total
            ],
            ["30000", "3000", "27000", "2970", "29970"].map(String::from)
        );
        let l = stored_line("5", "10000", "1", "", "5000", "", false);
        let r = compute_delivered_line(&l, "3");
        assert_eq!(
            (r.discount_value.as_str(), r.net_amount.as_str()),
            ("3000", "27000")
        );
        let l = stored_line("2", "0.01", "1", "50", "0.01", "", false);
        assert_eq!(l.stored.net_amount, "0");
        let r = compute_delivered_line(&l, "1");
        assert_eq!(
            (
                r.gross_amount.as_str(),
                r.discount_value.as_str(),
                r.net_amount.as_str(),
                r.line_total.as_str()
            ),
            ("0.01", "0.01", "0", "0")
        );
        let l = stored_line("4", "11100", "1", "", "", "11", true);
        let r = compute_delivered_line(&l, "2");
        assert_eq!(
            r,
            compute_line(&line("2", "11100", "1", "", "", "11", true)).unwrap()
        );
        assert_eq!(
            (r.line_total.as_str(), r.tax_amount.as_str()),
            ("22200", "2200")
        );
        let l = stored_line("6", "0", "1", "", "", "", false);
        let r = compute_delivered_line(&l, "2");
        assert_eq!(
            (r.gross_amount.as_str(), r.qty_in_base_uom.as_str()),
            ("0", "2")
        );
        let l = stored_line("5", "10000", "1", "10", "500", "11", false);
        assert_eq!(compute_delivered_line(&l, "0"), zero_line());
        let l = stored_line("2", "24000", "24", "", "", "", false);
        assert_eq!(compute_delivered_line(&l, "1.5").qty_in_base_uom, "36");
        let l = stored_line("1", "1000", "0.333333", "", "", "", false);
        assert_eq!(compute_delivered_line(&l, "0.5").qty_in_base_uom, "0.1667");
    }

    #[test]
    fn delivered_totals() {
        let lines = [
            stored_line("3", "0.35", "1", "50", "0.52", "11", false),
            stored_line("7", "12345.67", "1", "3", "", "11", true),
        ];
        let original = compute_totals(&lines.iter().map(|l| l.stored.clone()).collect::<Vec<_>>());
        let fin = compute_delivered_totals(
            &lines
                .iter()
                .map(|l| compute_delivered_line(l, &l.qty))
                .collect::<Vec<_>>(),
        );
        assert_eq!(fin, original);
        let a = stored_line("5", "10000", "1", "", "", "11", false);
        let b = stored_line("2", "25000", "1", "", "", "11", false);
        let f = compute_delivered_totals(&[
            compute_delivered_line(&a, "3"),
            compute_delivered_line(&b, "2"),
        ]);
        assert_eq!(
            (
                f.subtotal.as_str(),
                f.tax_total.as_str(),
                f.grand_total.as_str()
            ),
            ("80000", "8800", "88800")
        );
    }

    #[test]
    fn sig20_rounding() {
        // 1/3 → 20 digit signifikan.
        assert_eq!(dstr(div(Decimal::ONE, dec("3"))), "0.33333333333333333333");
        assert_eq!(dstr(div(dec("10"), dec("3"))), "3.3333333333333333333");
    }
}
