//! `companies/dto/companies-dto.ts`: `UpdateCompanyDto` (body PATCH) dan `SignUrlDTO` (query).
//! Body dibaca sebagai objek JSON mentah dan divalidasi manual supaya urutan pesan class-validator
//! (dekorator dievaluasi dari bawah ke atas, properti menurut urutan deklarasi) sama persis.

use chrono::NaiveTime;
use mz_core::enums::FeatureModule;
use mz_core::extract::{Validate, check_int_range, validator_js_length};
use mz_core::iso8601::is_iso8601;
use mz_core::time::parse_js_date;
use mz_core::{ApiError, ApiResult, AppState};
use serde::Deserialize;
use serde_json::{Map, Value};

use crate::common::{js_parse_int, node_500};

const NUMBER_MSG: &str = "must be a number conforming to the specified constraints";
const IMAGE_BLUR_ROLES: &[&str] = &["SUPER_ADMIN", "ADMIN", "MANAGER"];
const MODULES: &[&str] = &["TIME_TRACKING", "SALES_TRACKER"];

/// `String.prototype.trim` (spasi Unicode + BOM).
fn js_trim(s: &str) -> &str {
    s.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}')
}

/// Body `PATCH /companies/:uuid` mentah.
#[derive(Debug, Clone, Deserialize)]
#[serde(transparent)]
pub struct UpdateBody(pub Map<String, Value>);

/// `@IsOptional()`: null/undefined melewati semua validator properti.
fn given<'a>(m: &'a Map<String, Value>, k: &str) -> Option<&'a Value> {
    m.get(k).filter(|v| !v.is_null())
}

fn opt_string(out: &mut Vec<String>, m: &Map<String, Value>, k: &str) {
    if given(m, k).is_some_and(|v| !v.is_string()) {
        out.push(format!("{k} must be a string"));
    }
}

fn opt_bool(out: &mut Vec<String>, m: &Map<String, Value>, k: &str) {
    if given(m, k).is_some_and(|v| !v.is_boolean()) {
        out.push(format!("{k} must be a boolean value"));
    }
}

fn opt_int(out: &mut Vec<String>, m: &Map<String, Value>, k: &str, min: i64, max: i64) {
    if let Some(v) = given(m, k) {
        check_int_range(out, k, v.as_f64().filter(|_| v.is_number()), min, max);
    }
}

/// Pesan `@IsEnum(E, { each: true })` + `@ArrayUnique` (+ `@ArrayNotEmpty`) + `@IsArray`.
fn enum_array(
    out: &mut Vec<String>,
    m: &Map<String, Value>,
    k: &str,
    allowed: &[&str],
    not_empty: bool,
) {
    let Some(v) = given(m, k) else {
        return;
    };
    let in_enum = |x: &Value| x.as_str().is_some_and(|s| allowed.contains(&s));
    let (all_enum, unique, non_empty, is_array) = match v {
        Value::Array(a) => {
            let mut seen = std::collections::HashSet::new();
            (
                a.iter().all(in_enum),
                a.iter().all(|x| seen.insert(x.to_string())),
                !a.is_empty(),
                true,
            )
        }
        other => (in_enum(other), false, false, false),
    };
    if !all_enum {
        out.push(format!(
            "each value in {k} must be one of the following values: {}",
            allowed.join(", ")
        ));
    }
    if !unique {
        out.push(format!("All {k}'s elements must be unique"));
    }
    if not_empty && !non_empty {
        out.push(format!("{k} should not be empty"));
    }
    if !is_array {
        out.push(format!("{k} must be an array"));
    }
}

/// `MAX_ORDER_DISCOUNT_PERCENT_PATTERN`: `^100\.0{1,2}$|^100$|^\d{1,2}\.\d{1,2}$|^\d{1,2}$`.
fn discount_pattern_ok(s: &str) -> bool {
    let digits = |p: &str, lo: usize, hi: usize| {
        (lo..=hi).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_digit())
    };
    match s.split_once('.') {
        None => s == "100" || digits(s, 1, 2),
        Some((int, frac)) => {
            (int == "100" && !frac.is_empty() && frac.len() <= 2 && frac.bytes().all(|b| b == b'0'))
                || (digits(int, 1, 2) && digits(frac, 1, 2))
        }
    }
}

/// `WorkPatternInputDto` (pesan diawali `workPattern.`).
fn validate_work_pattern_item(out: &mut Vec<String>, prefix: &str, v: &Value) {
    let Some(o) = v.as_object() else {
        out.push(format!(
            "nested property {prefix} must be either object or array"
        ));
        return;
    };
    let p = |k: &str| format!("{prefix}.{k}");
    // workDays: @IsIn([5, 6]).
    if !o
        .get("workDays")
        .and_then(Value::as_f64)
        .is_some_and(|n| n == 5.0 || n == 6.0)
    {
        out.push(format!(
            "{} must be one of the following values: 5, 6",
            p("workDays")
        ));
    }
    // offDays: IsArray, ArrayUnique, IsInt(each), Min(1, each), Max(7, each) — dari bawah.
    let key = p("offDays");
    let off = o.get("offDays");
    let items: Vec<Option<f64>> = match off {
        Some(Value::Array(a)) => a
            .iter()
            .map(|x| x.as_f64().filter(|_| x.is_number()))
            .collect(),
        other => vec![other.and_then(|x| x.as_f64().filter(|_| x.is_number()))],
    };
    if items.iter().any(|n| n.is_none_or(|n| n > 7.0)) {
        out.push(format!("each value in {key} must not be greater than 7"));
    }
    if items.iter().any(|n| n.is_none_or(|n| n < 1.0)) {
        out.push(format!("each value in {key} must not be less than 1"));
    }
    if items.iter().any(|n| n.is_none_or(|n| n.fract() != 0.0)) {
        out.push(format!("each value in {key} must be an integer number"));
    }
    let unique = match off {
        Some(Value::Array(a)) => {
            let mut seen = std::collections::HashSet::new();
            a.iter().all(|x| seen.insert(x.to_string()))
        }
        _ => false,
    };
    if !unique {
        out.push(format!("All {key}'s elements must be unique"));
    }
    if !matches!(off, Some(Value::Array(_))) {
        out.push(format!("{key} must be an array"));
    }
    // shortDay: @IsOptional @IsInt @Min(1) @Max(7).
    if let Some(sd) = given(o, "shortDay") {
        check_int_range(
            out,
            &p("shortDay"),
            sd.as_f64().filter(|_| sd.is_number()),
            1,
            7,
        );
    }
}

impl Validate for UpdateBody {
    fn validate(&self) -> Vec<String> {
        let m = &self.0;
        let mut out = Vec::new();

        // name: IsString, Transform(trim), IsNotEmpty.
        match m.get("name") {
            Some(Value::String(s)) => {
                if js_trim(s).is_empty() {
                    out.push("name should not be empty".into());
                }
            }
            Some(v) if !v.is_null() => out.push("name must be a string".into()),
            _ => {
                out.push("name should not be empty".into());
                out.push("name must be a string".into());
            }
        }
        opt_string(&mut out, m, "telp");
        // timeZone: IsString, IsNotEmpty.
        match m.get("timeZone") {
            Some(Value::String(s)) => {
                if s.is_empty() {
                    out.push("timeZone should not be empty".into());
                }
            }
            Some(v) if !v.is_null() => out.push("timeZone must be a string".into()),
            _ => {
                out.push("timeZone should not be empty".into());
                out.push("timeZone must be a string".into());
            }
        }
        opt_string(&mut out, m, "currency");
        opt_int(&mut out, m, "workweekStartDay", 0, 6);
        if given(m, "dailyReportSentAt").is_some_and(|v| !v.as_str().is_some_and(is_iso8601)) {
            out.push("dailyReportSentAt must be a valid ISO 8601 date string".into());
        }
        opt_bool(&mut out, m, "isAlllowDeleteScreenshot");
        // fixedHourNotification: @IsNumber() @IsOptional().
        if given(m, "fixedHourNotification").is_some_and(|v| !v.is_number()) {
            out.push(format!("fixedHourNotification {NUMBER_MSG}"));
        }
        opt_bool(&mut out, m, "isAgencyMode");
        // address: IsOptional, IsString, Transform(trim), MaxLength(500).
        if let Some(v) = given(m, "address") {
            let too_long = match v.as_str() {
                Some(s) => validator_js_length(js_trim(s)) > 500,
                None => true,
            };
            if too_long {
                out.push("address must be shorter than or equal to 500 characters".into());
            }
            if !v.is_string() {
                out.push("address must be a string".into());
            }
        }
        opt_bool(&mut out, m, "isTrial");
        opt_string(&mut out, m, "actualProfilePictureName");
        opt_string(&mut out, m, "profilePictureFullPath");
        opt_bool(&mut out, m, "imageBlurEnabled");
        enum_array(&mut out, m, "imageBlurRoles", IMAGE_BLUR_ROLES, false);
        opt_int(&mut out, m, "storeVisitDefaultRadiusMeters", 1, 5000);
        opt_int(&mut out, m, "storeVisitMaxAccuracyMeters", 1, 100_000);
        opt_bool(&mut out, m, "storeVisitBlockMockLocation");
        opt_bool(&mut out, m, "storeVisitRequirePhotos");
        opt_bool(&mut out, m, "storeVisitAllowOverride");
        opt_int(&mut out, m, "storeVisitMaxDurationHours", 1, 24);
        opt_bool(&mut out, m, "storeVisitEnforceGroupScope");
        opt_int(&mut out, m, "storeVisitMinDurationMinutes", 0, 1440);
        opt_bool(&mut out, m, "storeVisitCountsAsAttendance");
        enum_array(&mut out, m, "modules", MODULES, true);
        // maxOrderDiscountPercent: IsString, IsOptional, Matches.
        if let Some(v) = given(m, "maxOrderDiscountPercent") {
            if !v.as_str().is_some_and(discount_pattern_ok) {
                out.push(
                    "maxOrderDiscountPercent must be a decimal string between 0 and 100 with up to 2 decimal places"
                        .into(),
                );
            }
            if !v.is_string() {
                out.push("maxOrderDiscountPercent must be a string".into());
            }
        }
        // workPattern: IsOptional, ValidateNested, Type.
        match given(m, "workPattern") {
            Some(Value::Array(a)) => {
                for (i, item) in a.iter().enumerate() {
                    validate_work_pattern_item(&mut out, &format!("workPattern.{i}"), item);
                }
            }
            Some(v) => validate_work_pattern_item(&mut out, "workPattern", v),
            None => {}
        }
        // cancelScheduledWorkPattern: IsOptional, IsBoolean.
        opt_bool(&mut out, m, "cancelScheduledWorkPattern");
        out
    }
}

/// Nilai kolom untuk `UPDATE`.
#[derive(Debug, Clone, PartialEq)]
pub enum SetVal {
    Text(Option<String>),
    Int(i32),
    Bool(bool),
    Time(NaiveTime),
    TextList(Vec<String>),
    /// `Decimal(5,2)` dikirim sebagai teks lalu di-cast `numeric`.
    Decimal(Option<String>),
}

/// Hasil pemetaan body → yang ditulis service.
#[derive(Debug, Clone)]
pub struct UpdateInput {
    /// `(kolom, nilai)`, tanpa `updatedAt`.
    pub sets: Vec<(&'static str, SetVal)>,
    pub modules: Option<Vec<FeatureModule>>,
    pub work_pattern: Option<Value>,
    pub cancel_scheduled: Option<Value>,
}

/// `PrismaClientValidationError` (null pada kolom non-nullable, tipe tak cocok) → 500.
fn prisma_invalid(col: &str) -> ApiError {
    node_500(&format!("Invalid value for argument `{col}`"))
}

impl UpdateBody {
    /// Terjemahan DTO yang sudah lolos validasi ke kolom. Kunci di luar DTO DIABAIKAN
    /// (lihat deviasi: Node meneruskannya apa adanya ke Prisma).
    pub fn to_input(&self) -> ApiResult<UpdateInput> {
        let m = &self.0;
        let mut sets: Vec<(&'static str, SetVal)> = Vec::new();

        // Teks: `nullable` → null menulis NULL; selain itu null = galat Prisma.
        let text =
            |sets: &mut Vec<_>, key: &'static str, nullable: bool, trim: bool| -> ApiResult<()> {
                match m.get(key) {
                    None => {}
                    Some(Value::Null) if nullable => sets.push((key, SetVal::Text(None))),
                    Some(Value::String(s)) => {
                        let s = if trim {
                            js_trim(s).to_owned()
                        } else {
                            s.clone()
                        };
                        sets.push((key, SetVal::Text(Some(s))));
                    }
                    Some(_) => return Err(prisma_invalid(key)),
                }
                Ok(())
            };
        text(&mut sets, "name", false, true)?;
        text(&mut sets, "telp", true, false)?;
        text(&mut sets, "timeZone", false, false)?;
        text(&mut sets, "currency", false, false)?;
        text(&mut sets, "address", true, true)?;
        text(&mut sets, "actualProfilePictureName", true, false)?;
        text(&mut sets, "profilePictureFullPath", true, false)?;

        let int = |sets: &mut Vec<_>, key: &'static str| -> ApiResult<()> {
            match m.get(key) {
                None => {}
                Some(v) => {
                    let n = v
                        .as_f64()
                        .filter(|n| n.fract() == 0.0 && n.abs() <= f64::from(i32::MAX))
                        .ok_or_else(|| prisma_invalid(key))?;
                    sets.push((key, SetVal::Int(n as i32)));
                }
            }
            Ok(())
        };
        int(&mut sets, "workweekStartDay")?;
        int(&mut sets, "fixedHourNotification")?;
        int(&mut sets, "storeVisitDefaultRadiusMeters")?;
        int(&mut sets, "storeVisitMaxAccuracyMeters")?;
        int(&mut sets, "storeVisitMaxDurationHours")?;
        int(&mut sets, "storeVisitMinDurationMinutes")?;

        let boolean = |sets: &mut Vec<_>, key: &'static str| -> ApiResult<()> {
            match m.get(key) {
                None => {}
                Some(Value::Bool(b)) => sets.push((key, SetVal::Bool(*b))),
                Some(_) => return Err(prisma_invalid(key)),
            }
            Ok(())
        };
        boolean(&mut sets, "isAlllowDeleteScreenshot")?;
        boolean(&mut sets, "isAgencyMode")?;
        boolean(&mut sets, "isTrial")?;
        boolean(&mut sets, "imageBlurEnabled")?;
        boolean(&mut sets, "storeVisitBlockMockLocation")?;
        boolean(&mut sets, "storeVisitRequirePhotos")?;
        boolean(&mut sets, "storeVisitAllowOverride")?;
        boolean(&mut sets, "storeVisitEnforceGroupScope")?;
        boolean(&mut sets, "storeVisitCountsAsAttendance")?;

        // `new Date(dailyReportSentAt)` ditulis ke kolom `Time`: bagian jam UTC-nya.
        match m.get("dailyReportSentAt") {
            None => {}
            Some(Value::String(s)) => {
                let t = parse_js_date(s)
                    .map(|d| d.time())
                    .ok_or_else(|| prisma_invalid("dailyReportSentAt"))?;
                sets.push(("dailyReportSentAt", SetVal::Time(t)));
            }
            Some(_) => return Err(prisma_invalid("dailyReportSentAt")),
        }
        match m.get("imageBlurRoles") {
            None => {}
            Some(Value::Array(a)) => sets.push((
                "imageBlurRoles",
                SetVal::TextList(
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_owned))
                        .collect(),
                ),
            )),
            Some(_) => return Err(prisma_invalid("imageBlurRoles")),
        }
        match m.get("maxOrderDiscountPercent") {
            None => {}
            Some(Value::Null) => sets.push(("maxOrderDiscountPercent", SetVal::Decimal(None))),
            Some(Value::String(s)) => {
                sets.push(("maxOrderDiscountPercent", SetVal::Decimal(Some(s.clone()))));
            }
            Some(_) => return Err(prisma_invalid("maxOrderDiscountPercent")),
        }

        // `modules` bukan kolom: daftar → dua boolean (REPLACE). `null` → TypeError di Node (500).
        let modules = match m.get("modules") {
            None => None,
            Some(Value::Array(a)) => Some(
                a.iter()
                    .filter_map(|x| x.as_str().and_then(|s| s.parse::<FeatureModule>().ok()))
                    .collect::<Vec<_>>(),
            ),
            Some(_) => return Err(node_500("modules: TypeError")),
        };
        if let Some(mods) = &modules {
            sets.push((
                "moduleTimeTrackingEnabled",
                SetVal::Bool(mods.contains(&FeatureModule::TIME_TRACKING)),
            ));
            sets.push((
                "moduleSalesTrackerEnabled",
                SetVal::Bool(mods.contains(&FeatureModule::SALES_TRACKER)),
            ));
        }

        Ok(UpdateInput {
            sets,
            modules,
            work_pattern: m.get("workPattern").cloned(),
            cancel_scheduled: m.get("cancelScheduledWorkPattern").cloned(),
        })
    }
}

/// Query `GET /companies/:uuid/profile-picture/presigned-url` (`SignUrlDTO`).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SignUrlQuery {
    pub file_name: Option<Value>,
    pub file_extension: Option<Value>,
    pub file_size: Option<Value>,
    /// `parseInt(process.env.MAX_SIZE_FILE_UPLOAD)` (diisi handler; bukan dari query).
    #[serde(skip)]
    pub max_size: Option<(Option<i64>, String)>,
}

impl SignUrlQuery {
    pub fn with_env(mut self, state: &AppState) -> Self {
        let raw = state.config.var("MAX_SIZE_FILE_UPLOAD");
        let limit = raw.as_deref().and_then(js_parse_int);
        self.max_size = Some((limit, raw.unwrap_or_else(|| "undefined".to_owned())));
        self
    }

    /// `@Transform(parseInt)` atas `fileSize` (nilai bukan string/undefined → tidak diubah/NaN).
    fn size(&self) -> Option<i64> {
        match &self.file_size {
            Some(Value::String(s)) => js_parse_int(s),
            _ => None,
        }
    }

    pub fn extension(&self) -> &str {
        self.file_extension
            .as_ref()
            .and_then(Value::as_str)
            .unwrap_or_default()
    }

    pub fn file_name(&self) -> &str {
        self.file_name
            .as_ref()
            .and_then(Value::as_str)
            .unwrap_or_default()
    }
}

impl Validate for SignUrlQuery {
    fn validate(&self) -> Vec<String> {
        let mut out = Vec::new();
        // fileName: IsString, IsNotEmpty (dari bawah: NotEmpty dulu).
        match &self.file_name {
            Some(Value::String(s)) if !s.is_empty() => {}
            Some(Value::String(_)) => out.push("fileName should not be empty".into()),
            Some(_) => out.push("fileName must be a string".into()),
            None => {
                out.push("fileName should not be empty".into());
                out.push("fileName must be a string".into());
            }
        }
        // fileExtension: IsString, IsNotEmpty, IsIn → IsIn, NotEmpty, IsString.
        let ext = self.file_extension.as_ref();
        if !ext
            .and_then(Value::as_str)
            .is_some_and(|s| ["jpg", "jpeg", "png"].contains(&s))
        {
            out.push("Invalid file type. Only JPG, JPEG, and PNG files are allowed.".into());
        }
        match ext {
            Some(Value::String(s)) if !s.is_empty() => {}
            Some(Value::String(_)) | None => out.push("fileExtension should not be empty".into()),
            Some(_) => {}
        }
        if !ext.is_some_and(Value::is_string) {
            out.push("fileExtension must be a string".into());
        }
        // fileSize: IsNumber, IsNotEmpty, Transform(parseInt), Max → Max, NotEmpty, IsNumber.
        let (limit, raw) = self
            .max_size
            .clone()
            .unwrap_or((None, "undefined".to_owned()));
        let size = self.size();
        if !matches!((size, limit), (Some(s), Some(l)) if s <= l) {
            out.push(format!("File size must be less than {raw}."));
        }
        if size.is_none() {
            out.push(format!("fileSize {NUMBER_MSG}"));
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn body(v: Value) -> UpdateBody {
        UpdateBody(v.as_object().cloned().unwrap_or_default())
    }

    #[test]
    fn empty_body_requires_name_and_time_zone() {
        let errs = body(json!({})).validate();
        assert_eq!(
            errs,
            vec![
                "name should not be empty",
                "name must be a string",
                "timeZone should not be empty",
                "timeZone must be a string"
            ]
        );
    }

    #[test]
    fn valid_body_passes() {
        let b = body(json!({
            "name": " Acme ", "timeZone": "Asia/Jakarta", "workweekStartDay": 1,
            "modules": ["TIME_TRACKING"], "maxOrderDiscountPercent": "12.5",
            "imageBlurRoles": ["ADMIN"], "dailyReportSentAt": "2026-11-16T08:00:00.000Z",
            "workPattern": {"workDays": 5, "offDays": [6, 7]}
        }));
        assert!(b.validate().is_empty(), "{:?}", b.validate());
        let input = b.to_input().ok();
        let sets = input.map(|i| i.sets).unwrap_or_default();
        assert!(sets.contains(&("name", SetVal::Text(Some("Acme".into())))));
        assert!(sets.contains(&("moduleSalesTrackerEnabled", SetVal::Bool(false))));
    }

    #[test]
    fn range_and_array_messages_follow_decorator_order() {
        let errs = body(json!({
            "name": "x", "timeZone": "UTC", "workweekStartDay": 9,
            "modules": [], "imageBlurRoles": ["X", "X"], "maxOrderDiscountPercent": "101"
        }))
        .validate();
        assert_eq!(
            errs,
            vec![
                "workweekStartDay must not be greater than 6",
                "each value in imageBlurRoles must be one of the following values: SUPER_ADMIN, ADMIN, MANAGER",
                "All imageBlurRoles's elements must be unique",
                "modules should not be empty",
                "maxOrderDiscountPercent must be a decimal string between 0 and 100 with up to 2 decimal places",
            ]
        );
    }

    #[test]
    fn nested_work_pattern_is_prefixed() {
        let errs = body(json!({
            "name": "x", "timeZone": "UTC", "workPattern": {"workDays": 7}
        }))
        .validate();
        assert_eq!(
            errs.first().map(String::as_str),
            Some("workPattern.workDays must be one of the following values: 5, 6")
        );
        assert!(errs.contains(&"workPattern.offDays must be an array".to_owned()));
        let bad = body(json!({"name": "x", "timeZone": "UTC", "workPattern": 5})).validate();
        assert_eq!(
            bad,
            vec!["nested property workPattern must be either object or array"]
        );
    }

    #[test]
    fn discount_pattern() {
        for ok in ["0", "5", "99", "100", "100.0", "100.00", "12.5", "0.05"] {
            assert!(discount_pattern_ok(ok), "{ok}");
        }
        for bad in ["", "101", "100.1", "1.234", "-1", "abc", "1.", ".5", "1000"] {
            assert!(!discount_pattern_ok(bad), "{bad}");
        }
    }

    #[test]
    fn null_on_required_column_is_a_500() {
        let b = body(json!({"name": "x", "timeZone": "UTC", "currency": null}));
        assert!(b.validate().is_empty());
        assert!(b.to_input().is_err());
        let ok = body(json!({"name": "x", "timeZone": "UTC", "telp": null}));
        assert!(ok.to_input().is_ok());
    }

    #[test]
    fn sign_url_messages() {
        let q = SignUrlQuery {
            max_size: Some((Some(1000), "1000".into())),
            ..SignUrlQuery::default()
        };
        assert_eq!(
            q.validate(),
            vec![
                "fileName should not be empty",
                "fileName must be a string",
                "Invalid file type. Only JPG, JPEG, and PNG files are allowed.",
                "fileExtension should not be empty",
                "fileExtension must be a string",
                "File size must be less than 1000.",
                "fileSize must be a number conforming to the specified constraints",
            ]
        );
        let ok = SignUrlQuery {
            file_name: Some(json!("a.png")),
            file_extension: Some(json!("png")),
            file_size: Some(json!("900")),
            max_size: Some((Some(1000), "1000".into())),
        };
        assert!(ok.validate().is_empty());
    }
}
