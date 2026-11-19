//! Port `src/analytics/signup-tracking.ts` — atribusi & property event Mixpanel `Sign Up`.
//!
//! Atribusi datang dari browser dan sepenuhnya dikendalikan pemanggil: semua nilai
//! dinormalisasi di sini dan yang tidak berbentuk seperti diharapkan dibuang.

use chrono::{DateTime, Utc};
use mz_core::enums::PricingProductType;
use mz_core::time::{parse_js_date, to_js_iso};
use serde::Serialize;
use serde_json::{Map, Value, json};

pub type SignupType = &'static str;
pub const SELF_SIGNUP: &str = "self_signup";
pub const ADMIN_INVITED: &str = "admin_invited";
pub const JOIN_LINK: &str = "join_link";

/// Click id iklan: yang disimpan hanya "ada/tidak", nilainya tidak pernah dikirim.
pub const AD_CLICK_ID_KEYS: [&str; 6] =
    ["gclid", "gbraid", "wbraid", "msclkid", "ttclid", "fbclid"];

const MAX_TEXT_LENGTH: usize = 255;
const MAX_FIRST_VISIT_AGE_MS: i64 = 400 * 24 * 60 * 60 * 1000;
const FIRST_VISIT_FUTURE_SKEW_MS: i64 = 5 * 60 * 1000;
const DAY_MS: i64 = 24 * 60 * 60 * 1000;

/// `SignupAttribution`. Field `None` tidak diserialisasi.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct SignupAttribution {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub utm_source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub utm_medium: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub utm_campaign: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub utm_content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub utm_term: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub referrer_domain: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub landing_page: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_visit_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
    pub click_ids: Vec<&'static str>,
}

/// `readText`: hanya string; buang karakter kontrol, trim, potong 255.
fn read_text(value: Option<&Value>) -> Option<String> {
    let Value::String(s) = value? else {
        return None;
    };
    // Potong dulu: string raksasa dari pemanggil tidak diproses utuh.
    let cleaned: String = s
        .chars()
        .take(MAX_TEXT_LENGTH * 2)
        .filter(|c| !(*c <= '\u{1f}' || *c == '\u{7f}'))
        .collect();
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.chars().take(MAX_TEXT_LENGTH).collect())
}

fn valid_label(label: &str) -> bool {
    (1..=63).contains(&label.len())
        && label
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !label.starts_with('-')
        && !label.ends_with('-')
}

/// `readDomain`: huruf kecil, tanpa `www.`, dicek per label.
fn read_domain(value: Option<&Value>) -> Option<String> {
    let text = read_text(value)?.to_lowercase();
    let text = text.strip_prefix("www.").unwrap_or(&text).to_owned();
    if text.is_empty() || text.len() > 253 {
        return None;
    }
    let labels: Vec<&str> = text.split('.').collect();
    if labels.len() < 2 {
        return None;
    }
    let tld = labels.last().copied().unwrap_or_default();
    let tld_ok = (2..=63).contains(&tld.len()) && tld.bytes().all(|b| b.is_ascii_lowercase());
    (tld_ok && labels.iter().all(|l| valid_label(l))).then_some(text)
}

/// `readPath`: path saja; query & hash dibuang.
fn read_path(value: Option<&Value>) -> Option<String> {
    let text = read_text(value)?;
    let path = text.split(['?', '#']).next().unwrap_or_default();
    let body = path.strip_prefix('/')?;
    let ok = body.len() <= 254
        && body.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~' | b'/' | b'%')
        });
    ok.then(|| path.to_owned())
}

/// `readPastIsoDate`.
fn read_past_iso_date(value: Option<&Value>, now: DateTime<Utc>) -> Option<String> {
    let text = read_text(value)?;
    let date = parse_js_date(&text)?.and_utc();
    let diff_ms = now.timestamp_millis() - date.timestamp_millis();
    // Jam browser boleh sedikit maju; lebih dari itu = sampah.
    if !(-FIRST_VISIT_FUTURE_SKEW_MS..=MAX_FIRST_VISIT_AGE_MS).contains(&diff_ms) {
        return None;
    }
    Some(to_js_iso(&date))
}

/// `readFlag`: `true` atau string non-kosong.
fn read_flag(value: Option<&Value>) -> bool {
    matches!(value, Some(Value::Bool(true))) || read_text(value).is_some()
}

/// `normalizeSignupAttribution(raw, now)`: objek mentah (body register / `state` Google OAuth)
/// -> bentuk yang aman dikirim ke Mixpanel & Discord. Bukan objek -> atribusi kosong.
pub fn normalize_signup_attribution(raw: &Value, now: DateTime<Utc>) -> SignupAttribution {
    let Value::Object(source) = raw else {
        return SignupAttribution::default();
    };

    let mut result = SignupAttribution {
        utm_source: read_text(source.get("utm_source")),
        // Medium dipakai untuk klasifikasi channel; `CPC` dan `cpc` itu sama.
        utm_medium: read_text(source.get("utm_medium")).map(|m| m.to_lowercase()),
        utm_campaign: read_text(source.get("utm_campaign")),
        utm_content: read_text(source.get("utm_content")),
        utm_term: read_text(source.get("utm_term")),
        referrer_domain: read_domain(source.get("referrer_domain")),
        landing_page: read_path(source.get("landing_page")),
        first_visit_at: read_past_iso_date(source.get("first_visit_at"), now),
        device_id: None,
        click_ids: Vec::new(),
    };

    result.device_id = read_text(source.get("device_id")).filter(|d| {
        (8..=100).contains(&d.len()) && d.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    });

    // Array `click_ids` dari state Google yang sudah dinormalisasi, atau key mentah dari body/query.
    let listed: &[Value] = match source.get("click_ids") {
        Some(Value::Array(a)) => a,
        _ => &[],
    };
    for key in AD_CLICK_ID_KEYS {
        if listed.iter().any(|v| v.as_str() == Some(key)) || read_flag(source.get(key)) {
            result.click_ids.push(key);
        }
    }
    result
}

/// `compactSignupAttribution`: tanpa key kosong; `None` bila tidak ada isi.
pub fn compact_signup_attribution(attribution: &SignupAttribution) -> Option<Map<String, Value>> {
    let Ok(Value::Object(map)) = serde_json::to_value(attribution) else {
        return None;
    };
    let compact: Map<String, Value> = map
        .into_iter()
        .filter(|(k, v)| {
            if k == "click_ids" {
                v.as_array().is_some_and(|a| !a.is_empty())
            } else {
                !v.is_null()
            }
        })
        .collect();
    (!compact.is_empty()).then_some(compact)
}

// ── Klasifikasi channel ──────────────────────────────────────────────────────

const PAID_MEDIUMS: &[&str] = &[
    "cpc",
    "ppc",
    "paid",
    "paidsearch",
    "paid_search",
    "paid-search",
    "sem",
    "paidsocial",
    "paid_social",
    "paid-social",
    "cpm",
    "cpv",
    "display",
    "banner",
    "ads",
    "ad",
    "retargeting",
    "remarketing",
];
const DISPLAY_MEDIUMS: &[&str] = &["cpm", "cpv", "display", "banner"];
const EMAIL_MEDIUMS: &[&str] = &["email", "e-mail", "newsletter"];
const SOCIAL_MEDIUMS: &[&str] = &[
    "social",
    "organic_social",
    "organic-social",
    "social-network",
    "social-media",
    "sm",
];
const SOCIAL_LABELS: &[&str] = &[
    "facebook",
    "fb",
    "instagram",
    "ig",
    "meta",
    "linkedin",
    "lnkd",
    "tiktok",
    "twitter",
    "youtube",
    "threads",
    "pinterest",
    "reddit",
];
const SEARCH_LABELS: &[&str] = &[
    "google",
    "bing",
    "yahoo",
    "duckduckgo",
    "yandex",
    "baidu",
    "ecosia",
];
const AI_DOMAINS: &[&str] = &[
    "chatgpt.com",
    "chat.openai.com",
    "perplexity.ai",
    "claude.ai",
    "gemini.google.com",
    "copilot.microsoft.com",
];

fn is_ai_domain(host: &str) -> bool {
    AI_DOMAINS
        .iter()
        .any(|d| host == *d || host.strip_suffix(d).is_some_and(|rest| rest.ends_with('.')))
}

fn is_social_host(host: &str) -> bool {
    host == "t.co" || host == "x.com" || host.split('.').any(|l| SOCIAL_LABELS.contains(&l))
}

fn is_search_host(host: &str) -> bool {
    host == "search.brave.com" || host.split('.').any(|l| SEARCH_LABELS.contains(&l))
}

fn is_social_source(source: Option<&str>) -> bool {
    let Some(source) = source else { return false };
    let lower = source.to_lowercase();
    lower == "x"
        || lower
            .split(|c: char| !(c.is_ascii_lowercase() || c.is_ascii_digit()))
            .any(|t| SOCIAL_LABELS.contains(&t))
}

/// `\bword\b` untuk kata ASCII pendek (batas = karakter non `[A-Za-z0-9_]`).
fn has_word(haystack: &str, word: &str) -> bool {
    haystack
        .split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .any(|t| t == word)
}

fn ad_platform_from_source(source: Option<&str>) -> Option<&'static str> {
    let lower = source.unwrap_or_default().to_lowercase();
    if lower.contains("facebook")
        || lower.contains("instagram")
        || has_word(&lower, "fb")
        || has_word(&lower, "ig")
        || lower.contains("meta")
    {
        return Some("Meta Ads");
    }
    if lower.contains("linkedin") {
        return Some("LinkedIn Ads");
    }
    if lower.contains("tiktok") {
        return Some("TikTok Ads");
    }
    if lower.contains("google") || lower.contains("youtube") {
        return Some("Google Ads");
    }
    if lower.contains("bing") || lower.contains("microsoft") {
        return Some("Microsoft Ads");
    }
    None
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AcquisitionSummary {
    pub acquisition_channel: &'static str,
    pub is_paid_ads: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ad_platform: Option<&'static str>,
}

fn organic(channel: &'static str) -> AcquisitionSummary {
    AcquisitionSummary {
        acquisition_channel: channel,
        is_paid_ads: false,
        ad_platform: None,
    }
}

fn paid(channel: &'static str, ad_platform: Option<&'static str>) -> AcquisitionSummary {
    AcquisitionSummary {
        acquisition_channel: channel,
        is_paid_ads: true,
        ad_platform,
    }
}

/// `classifyAcquisition(signupType, attribution)`: satu label channel untuk breakdown marketing.
/// `fbclid` sengaja TIDAK dianggap iklan.
pub fn classify_acquisition(
    signup_type: &str,
    attribution: &SignupAttribution,
) -> AcquisitionSummary {
    if signup_type != SELF_SIGNUP {
        return organic("Invite");
    }
    let click_ids = &attribution.click_ids;
    let source = attribution.utm_source.as_deref();
    let medium = attribution.utm_medium.as_deref();

    if click_ids
        .iter()
        .any(|id| matches!(*id, "gclid" | "gbraid" | "wbraid"))
    {
        return paid("Paid Search", Some("Google Ads"));
    }
    if click_ids.contains(&"msclkid") {
        return paid("Paid Search", Some("Microsoft Ads"));
    }
    if click_ids.contains(&"ttclid") {
        return paid("Paid Social", Some("TikTok Ads"));
    }

    if let Some(m) = medium.filter(|m| PAID_MEDIUMS.contains(m)) {
        let platform = ad_platform_from_source(source);
        if is_social_source(source) || m.contains("social") {
            return paid("Paid Social", platform);
        }
        if DISPLAY_MEDIUMS.contains(&m) {
            return paid("Paid Other", platform);
        }
        return paid("Paid Search", platform);
    }

    if medium.is_some_and(|m| EMAIL_MEDIUMS.contains(&m)) {
        return organic("Email");
    }
    if medium.is_some_and(|m| SOCIAL_MEDIUMS.contains(&m)) || is_social_source(source) {
        return organic("Organic Social");
    }
    // JS truthiness: string kosong = falsy (read_text sudah membuang string kosong).
    if source.is_some() || medium.is_some() || attribution.utm_campaign.is_some() {
        return organic("Other Campaign");
    }

    if let Some(host) = attribution.referrer_domain.as_deref() {
        if is_ai_domain(host) {
            return organic("AI Assistant");
        }
        if is_search_host(host) {
            return organic("Organic Search");
        }
        if is_social_host(host) {
            return organic("Organic Social");
        }
        return organic("Referral");
    }

    if click_ids.contains(&"fbclid") {
        return organic("Organic Social");
    }
    organic("Direct")
}

// ── Produk ───────────────────────────────────────────────────────────────────

/// `productLabel`: label untuk breakdown Mixpanel & embed Discord.
pub fn product_label(product_type: PricingProductType) -> &'static str {
    match product_type {
        PricingProductType::TIME => "Time Tracking",
        PricingProductType::SALES => "Sales Tracker",
        PricingProductType::FULL => "Time + Sales",
    }
}

/// `productTypeFromModules`: undangan tidak punya paket sendiri; produknya = modul yang hidup.
pub fn product_type_from_modules(
    time_tracking_enabled: bool,
    sales_tracker_enabled: bool,
) -> PricingProductType {
    match (time_tracking_enabled, sales_tracker_enabled) {
        (true, false) => PricingProductType::TIME,
        (false, true) => PricingProductType::SALES,
        _ => PricingProductType::FULL,
    }
}

// ── Property event ───────────────────────────────────────────────────────────

pub struct SignUpPlan {
    pub uuid: String,
    pub name: String,
    pub billing_interval: String,
    pub is_trial: bool,
}

pub struct SignUpEventInput {
    pub signup_type: SignupType,
    /// `email` | `google`.
    pub signup_method: &'static str,
    /// `web` / `desktop` / ...
    pub signup_platform: String,
    pub product_type: PricingProductType,
    pub role: String,
    pub company_uuid: String,
    pub is_workspace_owner: bool,
    pub plan: Option<SignUpPlan>,
    pub invited_by_uuid: Option<String>,
    pub attribution: Option<SignupAttribution>,
    pub occurred_at: DateTime<Utc>,
}

/// `buildSignUpProperties(input)`. Nilai `undefined` Node = key tidak ada di map.
pub fn build_sign_up_properties(input: &SignUpEventInput) -> Map<String, Value> {
    let empty = SignupAttribution::default();
    let attribution = input.attribution.as_ref().unwrap_or(&empty);
    let acquisition = classify_acquisition(input.signup_type, attribution);

    let days_to_signup = attribution
        .first_visit_at
        .as_deref()
        .and_then(parse_js_date)
        .map(|d| {
            (input.occurred_at.timestamp_millis() - d.and_utc().timestamp_millis())
                .div_euclid(DAY_MS)
                .max(0)
        });

    let mut props = Map::new();
    let mut put = |key: &str, value: Value| {
        if !value.is_null() {
            props.insert(key.to_owned(), value);
        }
    };
    put("signup_type", json!(input.signup_type));
    put("signup_method", json!(input.signup_method));
    put("signup_platform", json!(input.signup_platform));
    put("product", json!(product_label(input.product_type)));
    put("product_type", json!(input.product_type.as_str()));
    put("plan_id", json!(input.plan.as_ref().map(|p| &p.uuid)));
    put("plan_name", json!(input.plan.as_ref().map(|p| &p.name)));
    put(
        "billing_interval",
        json!(input.plan.as_ref().map(|p| &p.billing_interval)),
    );
    put("is_trial", json!(input.plan.as_ref().map(|p| p.is_trial)));
    put("role", json!(input.role));
    put("is_workspace_owner", json!(input.is_workspace_owner));
    put("company_id", json!(input.company_uuid));
    put("invited_by", json!(input.invited_by_uuid));
    put(
        "acquisition_channel",
        json!(acquisition.acquisition_channel),
    );
    put("is_paid_ads", json!(acquisition.is_paid_ads));
    put("ad_platform", json!(acquisition.ad_platform));
    put("utm_source", json!(attribution.utm_source));
    put("utm_medium", json!(attribution.utm_medium));
    put("utm_campaign", json!(attribution.utm_campaign));
    put("utm_content", json!(attribution.utm_content));
    put("utm_term", json!(attribution.utm_term));
    put("referrer_domain", json!(attribution.referrer_domain));
    put("landing_page", json!(attribution.landing_page));
    put("first_visit_at", json!(attribution.first_visit_at));
    put("days_to_signup", json!(days_to_signup));
    // Menyambungkan page view anonim di landing page ke user ini (Simplified ID Merge).
    put("$device_id", json!(attribution.device_id));
    props
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 11, 16, 8, 0, 0)
            .single()
            .unwrap_or_default()
    }

    fn norm(v: Value) -> SignupAttribution {
        normalize_signup_attribution(&v, now())
    }

    fn classify(v: Value) -> AcquisitionSummary {
        classify_acquisition(SELF_SIGNUP, &norm(v))
    }

    #[test]
    fn normalize_cleans_text_and_lowercases_medium() {
        let a =
            norm(json!({"utm_source":"  Google\u{0007} ","utm_medium":"CPC","utm_campaign":123}));
        assert_eq!(a.utm_source.as_deref(), Some("Google"));
        assert_eq!(a.utm_medium.as_deref(), Some("cpc"));
        assert_eq!(a.utm_campaign, None);
        assert_eq!(norm(json!("x")), SignupAttribution::default());
        assert_eq!(norm(json!([1])), SignupAttribution::default());
    }

    #[test]
    fn normalize_domain_path_device() {
        let a = norm(
            json!({"referrer_domain":"WWW.Example.com","landing_page":"/pricing/time?email=a@b.c#x","device_id":"abcd-1234-ef"}),
        );
        assert_eq!(a.referrer_domain.as_deref(), Some("example.com"));
        assert_eq!(a.landing_page.as_deref(), Some("/pricing/time"));
        assert_eq!(a.device_id.as_deref(), Some("abcd-1234-ef"));
        let bad = norm(
            json!({"referrer_domain":"localhost","landing_page":"pricing","device_id":"short"}),
        );
        assert_eq!(
            (bad.referrer_domain, bad.landing_page, bad.device_id),
            (None, None, None)
        );
        assert_eq!(
            norm(json!({"referrer_domain":"-a.com"})).referrer_domain,
            None
        );
        assert_eq!(
            norm(json!({"referrer_domain":"a.c0m"})).referrer_domain,
            None
        );
        assert_eq!(norm(json!({"landing_page":"/a b"})).landing_page, None);
    }

    #[test]
    fn normalize_first_visit_window() {
        assert_eq!(
            norm(json!({"first_visit_at":"2026-11-10T08:00:00Z"}))
                .first_visit_at
                .as_deref(),
            Some("2026-11-10T08:00:00.000Z")
        );
        // terlalu lama (>400 hari), masa depan (>5 menit), sampah
        assert_eq!(
            norm(json!({"first_visit_at":"2024-01-01T00:00:00Z"})).first_visit_at,
            None
        );
        assert_eq!(
            norm(json!({"first_visit_at":"2026-11-16T09:00:00Z"})).first_visit_at,
            None
        );
        assert_eq!(
            norm(json!({"first_visit_at":"2026-11-16T08:03:00Z"}))
                .first_visit_at
                .as_deref(),
            Some("2026-11-16T08:03:00.000Z")
        );
        assert_eq!(norm(json!({"first_visit_at":"nope"})).first_visit_at, None);
    }

    #[test]
    fn normalize_click_ids() {
        let a =
            norm(json!({"gclid":"abc","fbclid":true,"msclkid":"","click_ids":["ttclid","bogus"]}));
        assert_eq!(a.click_ids, vec!["gclid", "ttclid", "fbclid"]);
    }

    #[test]
    fn compact_drops_empty() {
        assert_eq!(
            compact_signup_attribution(&SignupAttribution::default()),
            None
        );
        let c = compact_signup_attribution(&norm(json!({"utm_source":"x","gclid":"1"})))
            .unwrap_or_default();
        assert_eq!(
            Value::Object(c),
            json!({"utm_source":"x","click_ids":["gclid"]})
        );
    }

    #[test]
    fn classify_paid_click_ids_and_mediums() {
        assert_eq!(
            classify(json!({"gclid":"1"})),
            paid("Paid Search", Some("Google Ads"))
        );
        assert_eq!(
            classify(json!({"msclkid":"1"})),
            paid("Paid Search", Some("Microsoft Ads"))
        );
        assert_eq!(
            classify(json!({"ttclid":"1"})),
            paid("Paid Social", Some("TikTok Ads"))
        );
        assert_eq!(
            classify(json!({"utm_medium":"cpc","utm_source":"facebook"})),
            paid("Paid Social", Some("Meta Ads"))
        );
        assert_eq!(
            classify(json!({"utm_medium":"display","utm_source":"foo"})),
            paid("Paid Other", None)
        );
        assert_eq!(
            classify(json!({"utm_medium":"CPC","utm_source":"google"})),
            paid("Paid Search", Some("Google Ads"))
        );
        assert_eq!(
            classify(json!({"utm_medium":"paid_social"})),
            paid("Paid Social", None)
        );
    }

    #[test]
    fn classify_organic_paths() {
        assert_eq!(
            classify(json!({"utm_medium":"newsletter"})),
            organic("Email")
        );
        assert_eq!(
            classify(json!({"utm_medium":"social"})),
            organic("Organic Social")
        );
        assert_eq!(
            classify(json!({"utm_source":"linkedin"})),
            organic("Organic Social")
        );
        assert_eq!(
            classify(json!({"utm_source":"x"})),
            organic("Organic Social")
        );
        assert_eq!(
            classify(json!({"utm_source":"partner"})),
            organic("Other Campaign")
        );
        assert_eq!(
            classify(json!({"utm_campaign":"launch"})),
            organic("Other Campaign")
        );
        assert_eq!(
            classify(json!({"referrer_domain":"chat.openai.com"})),
            organic("AI Assistant")
        );
        assert_eq!(
            classify(json!({"referrer_domain":"www.google.co.id"})),
            organic("Organic Search")
        );
        assert_eq!(
            classify(json!({"referrer_domain":"l.facebook.com"})),
            organic("Organic Social")
        );
        assert_eq!(
            classify(json!({"referrer_domain":"blog.example.com"})),
            organic("Referral")
        );
        // fbclid tanpa UTM/referrer: organik, BUKAN iklan
        assert_eq!(classify(json!({"fbclid":"1"})), organic("Organic Social"));
        assert_eq!(classify(json!({})), organic("Direct"));
    }

    #[test]
    fn invite_types_always_invite() {
        let a = norm(json!({"gclid":"1"}));
        assert_eq!(classify_acquisition(ADMIN_INVITED, &a), organic("Invite"));
        assert_eq!(classify_acquisition(JOIN_LINK, &a), organic("Invite"));
    }

    #[test]
    fn ai_domain_requires_dot_boundary() {
        assert!(is_ai_domain("gemini.google.com"));
        assert!(is_ai_domain("www.claude.ai"));
        assert!(!is_ai_domain("notclaude.ai"));
    }

    #[test]
    fn product_helpers() {
        assert_eq!(product_label(PricingProductType::FULL), "Time + Sales");
        assert_eq!(
            product_type_from_modules(true, false),
            PricingProductType::TIME
        );
        assert_eq!(
            product_type_from_modules(false, true),
            PricingProductType::SALES
        );
        assert_eq!(
            product_type_from_modules(true, true),
            PricingProductType::FULL
        );
        assert_eq!(
            product_type_from_modules(false, false),
            PricingProductType::FULL
        );
    }

    #[test]
    fn sign_up_properties() {
        let input = SignUpEventInput {
            signup_type: SELF_SIGNUP,
            signup_method: "email",
            signup_platform: "web".into(),
            product_type: PricingProductType::TIME,
            role: "SUPER_ADMIN".into(),
            company_uuid: "c-uuid".into(),
            is_workspace_owner: true,
            plan: Some(SignUpPlan {
                uuid: "p".into(),
                name: "Free".into(),
                billing_interval: "MONTH".into(),
                is_trial: false,
            }),
            invited_by_uuid: None,
            attribution: Some(norm(
                json!({"utm_source":"google","utm_medium":"cpc","first_visit_at":"2026-11-13T07:00:00Z","device_id":"dev-12345678"}),
            )),
            occurred_at: now(),
        };
        let p = Value::Object(build_sign_up_properties(&input));
        assert_eq!(p.get("days_to_signup"), Some(&json!(3)));
        assert_eq!(p.get("acquisition_channel"), Some(&json!("Paid Search")));
        assert_eq!(p.get("ad_platform"), Some(&json!("Google Ads")));
        assert_eq!(p.get("product"), Some(&json!("Time Tracking")));
        assert_eq!(p.get("$device_id"), Some(&json!("dev-12345678")));
        assert_eq!(p.get("invited_by"), None);
        assert_eq!(p.get("is_trial"), Some(&json!(false)));
    }
}
