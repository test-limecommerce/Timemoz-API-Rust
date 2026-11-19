//! Port `src/store-visit-forms/form-schema.const.ts` + `enums/*.ts`.
//!
//! Pola regex JS dipetakan ke `regex` Rust dengan semantik JS: `\d` = ASCII (`[0-9]`),
//! `\s` = whitespace ECMAScript (Unicode White_Space + U+FEFF).

use std::sync::LazyLock;

use regex::Regex;

/// Versi *format* schema, bukan versi template (§2.1).
pub const FORM_SCHEMA_VERSION: i64 = 1;

pub const MAX_NODES_PER_TEMPLATE: usize = 200;
pub const MAX_OPTIONS_PER_FIELD: usize = 200;
pub const MAX_REPEATER_ROWS: usize = 200;
pub const MAX_KEY_LENGTH: usize = 40;
/// `JSON.stringify(answers).length` dalam byte (§2.8).
pub const MAX_ANSWERS_BYTES: usize = 256 * 1024;
pub const MAX_FILE_BYTES: i64 = 2 * 1024 * 1024;
pub const GRID_COLUMNS: i64 = 12;

pub const MAX_LABEL_LENGTH: usize = 200;
pub const MAX_DESCRIPTION_LENGTH: usize = 500;
pub const MAX_PLACEHOLDER_LENGTH: usize = 120;
pub const MAX_OPTION_LABEL_LENGTH: usize = 200;

fn re(p: &str) -> Regex {
    // Semua pola literal & teruji di unit test.
    Regex::new(p).unwrap_or_else(|e| unreachable!("{e}"))
}

/// `FORM_KEY_PATTERN` (= `FORM_OPTION_VALUE_PATTERN`).
pub static FORM_KEY_PATTERN: LazyLock<Regex> = LazyLock::new(|| re(r"^[a-z][a-z0-9_]{0,39}$"));

pub fn is_form_key(s: &str) -> bool {
    FORM_KEY_PATTERN.is_match(s)
}

/// `\s` ECMAScript.
const JS_WS: &str = r"\s\x{FEFF}";

static EMAIL_RE: LazyLock<Regex> =
    LazyLock::new(|| re(&format!(r"^[^{JS_WS}@]+@[^{JS_WS}@]+\.[^{JS_WS}@]{{2,}}$")));
static PHONE_ID_RE: LazyLock<Regex> = LazyLock::new(|| re(r"^(?:\+62|62|0)8[1-9][0-9]{6,11}$"));
static URL_RE: LazyLock<Regex> =
    LazyLock::new(|| re(&format!(r"^https?://[^{JS_WS}/$.?#][^{JS_WS}]*$")));
static NUMERIC_RE: LazyLock<Regex> = LazyLock::new(|| re(r"^[0-9]+$"));
static ALNUM_RE: LazyLock<Regex> = LazyLock::new(|| re(r"^[A-Za-z0-9]+$"));
static NIK_RE: LazyLock<Regex> = LazyLock::new(|| re(r"^[0-9]{16}$"));
static NPWP_RE: LazyLock<Regex> = LazyLock::new(|| re(r"^[0-9]{15,16}$"));

/// `FormFieldFormat`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormFieldFormat {
    Email,
    PhoneId,
    Url,
    Numeric,
    Alphanumeric,
    Nik,
    Npwp,
}

impl FormFieldFormat {
    pub const ALL: [FormFieldFormat; 7] = [
        Self::Email,
        Self::PhoneId,
        Self::Url,
        Self::Numeric,
        Self::Alphanumeric,
        Self::Nik,
        Self::Npwp,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Email => "email",
            Self::PhoneId => "phone_id",
            Self::Url => "url",
            Self::Numeric => "numeric",
            Self::Alphanumeric => "alphanumeric",
            Self::Nik => "nik",
            Self::Npwp => "npwp",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|f| f.as_str() == s)
    }

    /// `formatPattern(name).test(value)`.
    pub fn matches(self, value: &str) -> bool {
        let re: &Regex = match self {
            Self::Email => &EMAIL_RE,
            Self::PhoneId => &PHONE_ID_RE,
            Self::Url => &URL_RE,
            Self::Numeric => &NUMERIC_RE,
            Self::Alphanumeric => &ALNUM_RE,
            Self::Nik => &NIK_RE,
            Self::Npwp => &NPWP_RE,
        };
        re.is_match(value)
    }
}

/// `DATE_PATTERN` `^\d{4}-\d{2}-\d{2}$`.
pub fn is_date_shape(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b.iter().enumerate().all(|(i, c)| {
            if i == 4 || i == 7 {
                *c == b'-'
            } else {
                c.is_ascii_digit()
            }
        })
}

/// `TIME_PATTERN` `^([01]\d|2[0-3]):[0-5]\d$`.
pub fn is_time_shape(s: &str) -> bool {
    let b = s.as_bytes();
    let [h1, h2, colon, m1, m2] = b else {
        return false;
    };
    let hour_ok = match h1 {
        b'0' | b'1' => h2.is_ascii_digit(),
        b'2' => (b'0'..=b'3').contains(h2),
        _ => false,
    };
    hour_ok && *colon == b':' && (b'0'..=b'5').contains(m1) && m2.is_ascii_digit()
}

/// `RELATIVE_DATE_OFFSET_PATTERN` `^today([+-])(\d{1,4})d$` → (tanda negatif, hari).
pub fn parse_relative_offset(s: &str) -> Option<(bool, i64)> {
    let rest = s.strip_prefix("today")?;
    let (neg, rest) = if let Some(r) = rest.strip_prefix('-') {
        (true, r)
    } else {
        (false, rest.strip_prefix('+')?)
    };
    let digits = rest.strip_suffix('d')?;
    if digits.is_empty() || digits.len() > 4 || !digits.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some((neg, digits.parse().ok()?))
}

/// `UUID_PATTERN` (tak peka huruf).
pub fn is_uuid_pattern(s: &str) -> bool {
    mz_core::uuid_rules::is_uuid_shape(s)
}

// ── Katalog type (§2.4) ──────────────────────────────────────────────────────

/// `FormNodeType` (urutan = deklarasi enum Node).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FormNodeType {
    Text,
    Textarea,
    Email,
    Phone,
    Url,
    Number,
    Currency,
    Percent,
    Date,
    Time,
    Datetime,
    Select,
    Radio,
    Multiselect,
    Checkbox,
    Boolean,
    Rating,
    Signature,
    Photo,
    File,
    Location,
    Barcode,
    Heading,
    Paragraph,
    Divider,
    Section,
    Group,
    Repeater,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormNodeCategory {
    Leaf,
    Display,
    Container,
}

impl FormNodeCategory {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Leaf => "leaf",
            Self::Display => "display",
            Self::Container => "container",
        }
    }
}

/// `FormTypeSpec`.
#[derive(Debug, Clone, Copy)]
pub struct FormTypeSpec {
    pub category: FormNodeCategory,
    pub allowed_validation: &'static [&'static str],
    pub allowed_config: &'static [&'static str],
    pub requires_options: bool,
    pub ordered: bool,
    pub media: bool,
}

const TEXT_VALIDATION: &[&str] = &["minLength", "maxLength", "format"];
const NUMBER_VALIDATION: &[&str] = &["min", "max", "decimals"];
const DATE_VALIDATION: &[&str] = &["minDate", "maxDate"];
const MULTI_VALIDATION: &[&str] = &["minItems", "maxItems"];
const FILE_VALIDATION: &[&str] = &["maxSizeBytes", "accept"];
const NO_VALIDATION: &[&str] = &[];
const NO_CONFIG: &[&str] = &[];

const fn spec(
    category: FormNodeCategory,
    allowed_validation: &'static [&'static str],
    allowed_config: &'static [&'static str],
) -> FormTypeSpec {
    FormTypeSpec {
        category,
        allowed_validation,
        allowed_config,
        requires_options: false,
        ordered: false,
        media: false,
    }
}

const fn ordered(mut s: FormTypeSpec) -> FormTypeSpec {
    s.ordered = true;
    s
}
const fn options(mut s: FormTypeSpec) -> FormTypeSpec {
    s.requires_options = true;
    s
}
const fn media(mut s: FormTypeSpec) -> FormTypeSpec {
    s.media = true;
    s
}

use FormNodeCategory::{Container, Display, Leaf};

impl FormNodeType {
    /// Urutan `Object.entries(FORM_TYPE_SPECS)` / deklarasi enum.
    pub const ALL: [FormNodeType; 28] = [
        Self::Text,
        Self::Textarea,
        Self::Email,
        Self::Phone,
        Self::Url,
        Self::Number,
        Self::Currency,
        Self::Percent,
        Self::Date,
        Self::Time,
        Self::Datetime,
        Self::Select,
        Self::Radio,
        Self::Multiselect,
        Self::Checkbox,
        Self::Boolean,
        Self::Rating,
        Self::Signature,
        Self::Photo,
        Self::File,
        Self::Location,
        Self::Barcode,
        Self::Heading,
        Self::Paragraph,
        Self::Divider,
        Self::Section,
        Self::Group,
        Self::Repeater,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Text => "text",
            Self::Textarea => "textarea",
            Self::Email => "email",
            Self::Phone => "phone",
            Self::Url => "url",
            Self::Number => "number",
            Self::Currency => "currency",
            Self::Percent => "percent",
            Self::Date => "date",
            Self::Time => "time",
            Self::Datetime => "datetime",
            Self::Select => "select",
            Self::Radio => "radio",
            Self::Multiselect => "multiselect",
            Self::Checkbox => "checkbox",
            Self::Boolean => "boolean",
            Self::Rating => "rating",
            Self::Signature => "signature",
            Self::Photo => "photo",
            Self::File => "file",
            Self::Location => "location",
            Self::Barcode => "barcode",
            Self::Heading => "heading",
            Self::Paragraph => "paragraph",
            Self::Divider => "divider",
            Self::Section => "section",
            Self::Group => "group",
            Self::Repeater => "repeater",
        }
    }

    /// `lookupTypeSpec(type) !== undefined`.
    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|t| t.as_str() == s)
    }

    /// `typeSpecOf(type)` / `FORM_TYPE_SPECS[type]`.
    pub const fn spec(self) -> FormTypeSpec {
        match self {
            Self::Text => spec(Leaf, TEXT_VALIDATION, NO_CONFIG),
            Self::Textarea => spec(Leaf, TEXT_VALIDATION, &["rows"]),
            Self::Email | Self::Phone | Self::Url | Self::Barcode => {
                spec(Leaf, TEXT_VALIDATION, NO_CONFIG)
            }
            Self::Number | Self::Percent => ordered(spec(Leaf, NUMBER_VALIDATION, NO_CONFIG)),
            Self::Currency => ordered(spec(Leaf, NUMBER_VALIDATION, &["currencyCode"])),
            Self::Date | Self::Time | Self::Datetime => {
                ordered(spec(Leaf, DATE_VALIDATION, NO_CONFIG))
            }
            Self::Select | Self::Radio => options(spec(Leaf, NO_VALIDATION, NO_CONFIG)),
            Self::Multiselect | Self::Checkbox => options(spec(Leaf, MULTI_VALIDATION, NO_CONFIG)),
            Self::Boolean | Self::Location => spec(Leaf, NO_VALIDATION, NO_CONFIG),
            Self::Rating => ordered(spec(Leaf, NO_VALIDATION, &["max"])),
            Self::Signature => media(spec(Leaf, NO_VALIDATION, &["penColor", "aspectRatio"])),
            Self::Photo | Self::File => media(spec(Leaf, FILE_VALIDATION, NO_CONFIG)),
            Self::Heading | Self::Paragraph | Self::Divider => {
                spec(Display, NO_VALIDATION, NO_CONFIG)
            }
            Self::Section => spec(Container, NO_VALIDATION, NO_CONFIG),
            Self::Group => spec(Container, NO_VALIDATION, &["direction", "justify"]),
            Self::Repeater => spec(Container, &["minRows", "maxRows"], &["renderAs"]),
        }
    }

    pub const fn category(self) -> FormNodeCategory {
        self.spec().category
    }

    /// `isDisplayType`.
    pub fn is_display(self) -> bool {
        self.category() == Display
    }

    /// `isContainerType`.
    pub fn is_container(self) -> bool {
        self.category() == Container
    }

    pub fn is_leaf(self) -> bool {
        self.category() == Leaf
    }

    /// `SINGLE_CHOICE_TYPES`.
    pub fn is_single_choice(self) -> bool {
        matches!(self, Self::Select | Self::Radio)
    }

    /// `MULTI_CHOICE_TYPES`.
    pub fn is_multi_choice(self) -> bool {
        matches!(self, Self::Multiselect | Self::Checkbox)
    }

    /// `TEXT_LIKE_TYPES`.
    pub fn is_text_like(self) -> bool {
        matches!(
            self,
            Self::Text | Self::Textarea | Self::Email | Self::Phone | Self::Url | Self::Barcode
        )
    }

    /// `NUMBER_LIKE_TYPES`.
    pub fn is_number_like(self) -> bool {
        matches!(self, Self::Number | Self::Currency | Self::Percent)
    }

    /// `MEDIA_TYPES`.
    pub fn is_media(self) -> bool {
        matches!(self, Self::Signature | Self::Photo | Self::File)
    }
}

/// `FormContainerContext`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FormContainerContext {
    Root,
    Section,
    Group,
    Repeater,
}

impl FormContainerContext {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Root => "root",
            Self::Section => "section",
            Self::Group => "group",
            Self::Repeater => "repeater",
        }
    }

    /// `ALLOWED_CHILD_TYPES[ctx]` (urutan sama dengan Node: leaf+display lalu container).
    pub fn allowed_child_types(self) -> Vec<FormNodeType> {
        let mut out: Vec<FormNodeType> = FormNodeType::ALL
            .into_iter()
            .filter(|t| !t.is_container())
            .collect();
        match self {
            Self::Root => out.extend([
                FormNodeType::Section,
                FormNodeType::Group,
                FormNodeType::Repeater,
            ]),
            Self::Section => out.extend([FormNodeType::Group, FormNodeType::Repeater]),
            Self::Group | Self::Repeater => {}
        }
        out
    }

    pub fn allows(self, t: FormNodeType) -> bool {
        if !t.is_container() {
            return true;
        }
        match self {
            Self::Root => true,
            Self::Section => matches!(t, FormNodeType::Group | FormNodeType::Repeater),
            Self::Group | Self::Repeater => false,
        }
    }
}

/// `FormVisibleIfOperator`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VisibleIfOp {
    Eq,
    Ne,
    In,
    Nin,
    Gt,
    Gte,
    Lt,
    Lte,
    Empty,
    NotEmpty,
}

impl VisibleIfOp {
    pub const ALL: [VisibleIfOp; 10] = [
        Self::Eq,
        Self::Ne,
        Self::In,
        Self::Nin,
        Self::Gt,
        Self::Gte,
        Self::Lt,
        Self::Lte,
        Self::Empty,
        Self::NotEmpty,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Eq => "eq",
            Self::Ne => "ne",
            Self::In => "in",
            Self::Nin => "nin",
            Self::Gt => "gt",
            Self::Gte => "gte",
            Self::Lt => "lt",
            Self::Lte => "lte",
            Self::Empty => "empty",
            Self::NotEmpty => "notEmpty",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|o| o.as_str() == s)
    }
}

pub const REPEATER_RENDER_AS: &[&str] = &["table", "card"];
pub const GROUP_DIRECTIONS: &[&str] = &["column", "row"];
pub const GROUP_JUSTIFY: &[&str] = &["stretch", "start", "center", "end", "between", "around"];
pub const OPTIONS_SOURCES: &[&str] = &["static", "product", "order"];

// ── Kop & kaki PDF ───────────────────────────────────────────────────────────

pub const LAYOUT_SECTIONS: &[&str] = &["headers", "footers"];
pub const LAYOUT_BLOCK_TYPES: &[&str] = &["text", "image", "divider", "spacer", "row"];
pub const LAYOUT_ROW_CHILD_TYPES: &[&str] = &["text", "image", "divider", "spacer"];
pub const LAYOUT_IMAGE_SOURCES: &[&str] = &["upload"];

/// `lookupLayoutBlockConfig(type)`.
pub fn lookup_layout_block_config(t: &str) -> Option<&'static [&'static str]> {
    Some(match t {
        "text" => &["size", "bold", "italic", "align", "color"],
        "image" => &["height", "align"],
        "divider" => &["thickness", "color", "style"],
        "spacer" => &["height"],
        "row" => &["justify"],
        _ => return None,
    })
}

pub const LAYOUT_TEXT_SIZES: &[&str] = &["xs", "sm", "md", "lg", "xl"];
pub const LAYOUT_ALIGNMENTS: &[&str] = &["left", "center", "right"];
pub const LAYOUT_DIVIDER_STYLES: &[&str] = &["solid", "dashed"];

pub const LAYOUT_TEXT_VARIABLES: &[&str] = &[
    "companyName",
    "templateName",
    "templateVersion",
    "status",
    "storeName",
    "storeCode",
    "salesName",
    "visitNumber",
    "checkInAt",
    "checkOutAt",
    "submittedAt",
    "generatedAt",
    "generatedBy",
    "page",
    "pageCount",
];

/// `LAYOUT_VARIABLE_PATTERN` `/\{\{ *([A-Za-z]{1,40}) *\}\}/g`.
pub static LAYOUT_VARIABLE_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| re(r"\{\{ *([A-Za-z]{1,40}) *\}\}"));

pub const MAX_LAYOUT_BLOCKS_PER_SECTION: usize = 20;
pub const MAX_LAYOUT_TEXT_LENGTH: usize = 300;

/// `{ min, max }` (pt).
#[derive(Debug, Clone, Copy)]
pub struct Range {
    pub min: f64,
    pub max: f64,
}

pub const LAYOUT_IMAGE_HEIGHT_RANGE: Range = Range {
    min: 12.0,
    max: 100.0,
};
pub const LAYOUT_SPACER_HEIGHT_RANGE: Range = Range {
    min: 2.0,
    max: 60.0,
};
pub const LAYOUT_DIVIDER_THICKNESS_RANGE: Range = Range {
    min: 0.25,
    max: 4.0,
};

pub const MAX_LAYOUT_HEADER_HEIGHT: f64 = 180.0;
pub const MAX_LAYOUT_FOOTER_HEIGHT: f64 = 120.0;

pub const LAYOUT_ASSET_CONTENT_TYPES: &[&str] = &["image/png", "image/jpeg"];
pub const MAX_LAYOUT_ASSET_BYTES: i64 = 1024 * 1024;

/// `StoreVisitFormStatus`.
pub mod status {
    pub const NOT_STARTED: &str = "notStarted";
    pub const DRAFT: &str = "draft";
    pub const SUBMITTED: &str = "submitted";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_pattern() {
        assert!(is_form_key("a"));
        assert!(is_form_key("unit_list2"));
        assert!(!is_form_key("2a"));
        assert!(!is_form_key("A"));
        assert!(!is_form_key(&"a".repeat(41)));
        assert!(is_form_key(&"a".repeat(MAX_KEY_LENGTH)));
    }

    #[test]
    fn formats() {
        assert!(FormFieldFormat::Email.matches("a@b.co"));
        assert!(!FormFieldFormat::Email.matches("a@b.c"));
        assert!(!FormFieldFormat::Email.matches("a b@c.co"));
        assert!(FormFieldFormat::PhoneId.matches("081234567890"));
        assert!(FormFieldFormat::PhoneId.matches("+6281234567"));
        assert!(!FormFieldFormat::PhoneId.matches("0712345678"));
        assert!(FormFieldFormat::Url.matches("https://x.id/a"));
        assert!(!FormFieldFormat::Url.matches("ftp://x"));
        assert!(!FormFieldFormat::Numeric.matches("١٢٣"));
        assert!(FormFieldFormat::Nik.matches("1234567890123456"));
        assert!(FormFieldFormat::Npwp.matches("123456789012345"));
        assert_eq!(
            FormFieldFormat::parse("phone_id"),
            Some(FormFieldFormat::PhoneId)
        );
    }

    #[test]
    fn shapes() {
        assert!(is_date_shape("2026-01-31"));
        assert!(!is_date_shape("2026-1-31"));
        assert!(is_time_shape("23:59"));
        assert!(!is_time_shape("24:00"));
        assert_eq!(parse_relative_offset("today-7d"), Some((true, 7)));
        assert_eq!(parse_relative_offset("today+30d"), Some((false, 30)));
        assert_eq!(parse_relative_offset("today+12345d"), None);
        assert_eq!(parse_relative_offset("today"), None);
    }

    #[test]
    fn catalog() {
        assert_eq!(FormNodeType::ALL.len(), 28);
        for t in FormNodeType::ALL {
            assert_eq!(FormNodeType::parse(t.as_str()), Some(t));
        }
        assert!(FormNodeType::Rating.spec().ordered);
        assert!(FormNodeType::Signature.spec().media);
        assert!(FormContainerContext::Section.allows(FormNodeType::Group));
        assert!(!FormContainerContext::Section.allows(FormNodeType::Section));
        assert!(!FormContainerContext::Group.allows(FormNodeType::Repeater));
        assert_eq!(FormContainerContext::Root.allowed_child_types().len(), 28);
    }
}
