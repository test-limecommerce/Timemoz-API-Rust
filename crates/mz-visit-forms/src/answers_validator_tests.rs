//! Port `form-answers.validator.spec.ts` (semua kasus).

use super::*;
use serde_json::json;

const TZ: &str = "Asia/Jakarta";
const DRAFT: ValidateAnswersOptions<'static> = ValidateAnswersOptions {
    mode: AnswerValidationMode::Draft,
    time_zone: TZ,
};
const FINAL: ValidateAnswersOptions<'static> = ValidateAnswersOptions {
    mode: AnswerValidationMode::Final,
    time_zone: TZ,
};
const UUID_A: &str = "8f1c2d3e-4a5b-4c6d-8e9f-0a1b2c3d4e5f";
const UUID_B: &str = "3ab70000-1111-4222-8333-444455556666";

fn schema(fields: Value) -> FormDocument {
    FormDocument::from_value(&json!({ "schemaVersion": 1, "fields": fields }))
}

fn run(fields: Value, answers: Option<Value>, o: ValidateAnswersOptions) -> ValidateAnswersResult {
    validate_answers(&schema(fields), answers.as_ref(), &o)
}

fn run_at(fields: Value, answers: Value, o: ValidateAnswersOptions, now: &str) -> Vec<String> {
    let now = chrono::DateTime::parse_from_rfc3339(now)
        .unwrap()
        .with_timezone(&Utc);
    validate_answers_at(&schema(fields), Some(&answers), &o, now)
        .issues
        .iter()
        .map(|i| format!("{}: {}", i.key, i.message))
        .collect()
}

fn msgs(fields: Value, answers: Value, o: ValidateAnswersOptions) -> Vec<String> {
    run(fields, Some(answers), o)
        .issues
        .iter()
        .map(|i| format!("{}: {}", i.key, i.message))
        .collect()
}

fn has(v: &[String], s: &str) -> bool {
    v.iter().any(|x| x == s)
}

fn select(key: &str, values: &[&str]) -> Value {
    json!({
        "key": key, "type": "select", "label": key,
        "options": {"source": "static", "items": values.iter().map(|v| json!({"value": v, "label": v})).collect::<Vec<_>>()}
    })
}

fn obj(v: Value) -> Map<String, Value> {
    v.as_object().unwrap().clone()
}

// ── bentuk masukan ──

#[test]
fn null_undefined_are_empty() {
    let f = json!([{"key": "a", "type": "text", "label": "A"}]);
    let r = run(f.clone(), Some(json!(null)), DRAFT);
    assert!(r.answers.is_empty() && r.issues.is_empty() && r.files.is_empty());
    assert!(r.product_order_refs.is_empty());
    assert!(run(f, None, DRAFT).answers.is_empty());
}

#[test]
fn rejects_non_object() {
    assert_eq!(
        msgs(json!([]), json!("{}"), DRAFT),
        vec![": answers must be an object"]
    );
}

#[test]
fn does_not_mutate_input() {
    let fields = json!([
        select("items", &["a", "b"]),
        {"key": "alasan", "type": "text", "label": "Alasan",
         "visibleIf": {"all": [{"field": "items", "op": "eq", "value": "b"}]}}
    ]);
    let input = json!({"items": "a", "alasan": "terbawa"});
    let r = validate_answers(&schema(fields), Some(&input), &FINAL);
    assert_eq!(r.answers, obj(json!({"items": "a"})));
    assert_eq!(input, json!({"items": "a", "alasan": "terbawa"}));
}

// ── key tidak dikenal ──

fn unknown_fields() -> Value {
    json!([
        {"key": "a", "type": "text", "label": "A"},
        {"key": "h", "type": "heading", "label": "Judul"},
        {"key": "sec", "type": "section", "label": "S", "fields": [{"key": "b", "type": "text", "label": "B"}]},
        {"key": "g", "type": "group", "label": "G", "fields": [{"key": "c", "type": "text", "label": "C"}]},
        {"key": "r", "type": "repeater", "label": "R", "validation": {"minRows": 0, "maxRows": 5},
         "fields": [{"key": "d", "type": "text", "label": "D"}]}
    ])
}

#[test]
fn unknown_keys() {
    let f = unknown_fields();
    assert!(has(
        &msgs(f.clone(), json!({"zz": 1}), DRAFT),
        "zz: unknown field \"zz\""
    ));
    assert!(has(
        &msgs(f.clone(), json!({"h": "x"}), DRAFT),
        "h: unknown field \"h\""
    ));
    assert!(has(
        &msgs(f.clone(), json!({"sec": {}}), DRAFT),
        "sec: unknown field \"sec\""
    ));
    assert!(msgs(f.clone(), json!({"b": "halo"}), DRAFT).is_empty());
    assert!(has(
        &msgs(f.clone(), json!({"g": {"zz": 1}}), DRAFT),
        "g.zz: unknown field \"zz\""
    ));
    assert!(has(
        &msgs(f, json!({"r": [{"d": "x"}, {"zz": 1}]}), DRAFT),
        "r.1.zz: unknown field \"zz\""
    ));
}

#[test]
fn unknown_keys_follow_js_key_order() {
    let r = msgs(json!([]), json!({"b": 1, "10": 1, "a": 1, "2": 1}), DRAFT);
    assert_eq!(
        r,
        vec![
            "2: unknown field \"2\"",
            "10: unknown field \"10\"",
            "b: unknown field \"b\"",
            "a: unknown field \"a\""
        ]
    );
}

// ── bentuk nilai per type ──

fn check(node: Value, value: Value) -> Vec<String> {
    let mut n = obj(node);
    n.insert("label".into(), json!("X"));
    let key = n["key"].as_str().unwrap().to_owned();
    msgs(json!([n]), json!({ key: value }), DRAFT)
}

#[test]
fn text_rules() {
    assert!(has(
        &check(json!({"key": "t", "type": "text"}), json!(5)),
        "t: value must be a string"
    ));
    assert!(has(
        &check(
            json!({"key": "t", "type": "text", "validation": {"maxLength": 3}}),
            json!("abcd")
        ),
        "t: value must be at most 3 characters"
    ));
    assert!(
        check(
            json!({"key": "t", "type": "text", "validation": {"maxLength": 2}}),
            json!("👍👍")
        )
        .is_empty()
    );
}

#[test]
fn implicit_formats() {
    assert!(has(
        &check(json!({"key": "e", "type": "email"}), json!("bukan-email")),
        "e: value does not match the email format"
    ));
    assert!(check(json!({"key": "e", "type": "email"}), json!("a@b.co")).is_empty());
    assert!(has(
        &check(json!({"key": "u", "type": "url"}), json!("ftp://x")),
        "u: value does not match the url format"
    ));
    assert!(check(json!({"key": "p", "type": "phone"}), json!("081234567890")).is_empty());
    assert_eq!(
        check(json!({"key": "p", "type": "phone"}), json!("12345")).len(),
        1
    );
}

#[test]
fn literal_formats() {
    for (format, ok, bad) in [
        ("nik", "1234567890123456", "123"),
        ("npwp", "123456789012345", "12345"),
        ("numeric", "12345", "12a"),
        ("alphanumeric", "abc123", "abc-123"),
    ] {
        let n = json!({"key": "t", "type": "text", "validation": {"format": format}});
        assert!(check(n.clone(), json!(ok)).is_empty(), "{format}");
        assert!(has(
            &check(n, json!(bad)),
            &format!("t: value does not match the {format} format")
        ));
    }
}

#[test]
fn number_rules() {
    assert!(has(
        &check(json!({"key": "n", "type": "number"}), json!("5")),
        "n: value must be a finite number"
    ));
    // NaN tidak ada di JSON; `null` diperlakukan kosong — cukup uji bentuk non-angka lain.
    assert_eq!(
        check(json!({"key": "n", "type": "number"}), json!(true)).len(),
        1
    );
    assert!(has(
        &check(
            json!({"key": "n", "type": "number", "validation": {"decimals": 0}}),
            json!(1.5)
        ),
        "n: value must be a whole number"
    ));
    assert!(has(
        &check(
            json!({"key": "n", "type": "number", "validation": {"decimals": 2}}),
            json!(1.234)
        ),
        "n: value must have at most 2 decimal places"
    ));
    assert!(
        check(
            json!({"key": "n", "type": "number", "validation": {"decimals": 2}}),
            json!(1.23)
        )
        .is_empty()
    );
}

#[test]
fn date_time_datetime_boolean() {
    assert!(has(
        &check(json!({"key": "d", "type": "date"}), json!("11-08-2026")),
        "d: value must be a YYYY-MM-DD date"
    ));
    assert_eq!(
        check(json!({"key": "d", "type": "date"}), json!("2026-02-30")).len(),
        1
    );
    assert!(check(json!({"key": "d", "type": "date"}), json!("2026-02-28")).is_empty());
    assert!(check(json!({"key": "t", "type": "time"}), json!("09:30")).is_empty());
    assert_eq!(
        check(json!({"key": "t", "type": "time"}), json!("24:00")).len(),
        1
    );
    assert_eq!(
        check(json!({"key": "t", "type": "time"}), json!("9:30")).len(),
        1
    );
    assert!(
        check(
            json!({"key": "dt", "type": "datetime"}),
            json!(1_770_000_000_000_i64)
        )
        .is_empty()
    );
    assert!(has(
        &check(
            json!({"key": "dt", "type": "datetime"}),
            json!("2026-08-11T00:00:00Z")
        ),
        "dt: value must be epoch milliseconds"
    ));
    assert!(check(json!({"key": "b", "type": "boolean"}), json!(true)).is_empty());
    assert!(has(
        &check(json!({"key": "b", "type": "boolean"}), json!("true")),
        "b: value must be a boolean"
    ));
}

#[test]
fn rating_rules() {
    assert!(check(json!({"key": "r", "type": "rating"}), json!(5)).is_empty());
    assert!(has(
        &check(json!({"key": "r", "type": "rating"}), json!(6)),
        "r: rating must be between 0 and 5"
    ));
    assert!(
        check(
            json!({"key": "r", "type": "rating", "config": {"max": 10}}),
            json!(10)
        )
        .is_empty()
    );
    assert!(has(
        &check(json!({"key": "r", "type": "rating"}), json!(4.5)),
        "r: value must be a whole number"
    ));
}

#[test]
fn select_and_radio() {
    assert!(has(
        &msgs(json!([select("s", &["a", "b"])]), json!({"s": "z"}), DRAFT),
        "s: \"z\" is not an allowed option"
    ));
    let radio = json!([{"key": "s", "type": "radio", "label": "S",
        "options": {"source": "static", "items": [{"value": "a", "label": "A"}]}}]);
    assert!(has(
        &msgs(radio, json!({"s": "z"}), DRAFT),
        "s: \"z\" is not an allowed option"
    ));
}

#[test]
fn multiselect_rules() {
    let n = json!([{"key": "m", "type": "multiselect", "label": "M",
        "options": {"source": "static", "items": [{"value": "a", "label": "A"}, {"value": "b", "label": "B"}]},
        "validation": {"minItems": 2, "maxItems": 2}}]);
    assert!(has(
        &msgs(n.clone(), json!({"m": "a"}), DRAFT),
        "m: value must be an array of option values"
    ));
    assert!(has(
        &msgs(n.clone(), json!({"m": ["a", "a"]}), DRAFT),
        "m: \"a\" is selected more than once"
    ));
    assert!(has(
        &msgs(n.clone(), json!({"m": ["a", "b", "a"]}), DRAFT),
        "m: select at most 2 options"
    ));
    assert!(msgs(n, json!({"m": ["a", "b"]}), DRAFT).is_empty());
}

#[test]
fn location_rules() {
    let n = json!([{"key": "loc", "type": "location", "label": "Loc"}]);
    assert!(
        msgs(
            n.clone(),
            json!({"loc": {"lat": -6.2, "lng": 106.8}}),
            DRAFT
        )
        .is_empty()
    );
    assert!(
        msgs(
            n.clone(),
            json!({"loc": {"lat": -6.2, "lng": 106.8, "accuracy": 12, "capturedAt": 1}}),
            DRAFT
        )
        .is_empty()
    );
    assert!(has(
        &msgs(n.clone(), json!({"loc": {"lat": 91, "lng": 0}}), DRAFT),
        "loc: lat must be a number between -90 and 90"
    ));
    assert!(has(
        &msgs(n, json!({"loc": {"lat": 0, "lng": 0, "alt": 5}}), DRAFT),
        "loc: unknown location property \"alt\""
    ));
}

#[test]
fn media_rules() {
    let n = json!([{"key": "ttd", "type": "signature", "label": "TTD"}]);
    assert!(msgs(n.clone(), json!({"ttd": {"fileUuid": UUID_A}}), DRAFT).is_empty());
    assert!(has(
        &msgs(
            n.clone(),
            json!({"ttd": {"fileUuid": UUID_A, "objectKey": "svform/x.png"}}),
            DRAFT
        ),
        "ttd: objectKey is resolved by the server and must not be sent"
    ));
    assert!(has(
        &msgs(n.clone(), json!({"ttd": {"fileUuid": "abc"}}), DRAFT),
        "ttd: fileUuid must be a uuid"
    ));
    assert!(has(
        &msgs(n, json!({"ttd": UUID_A}), DRAFT),
        "ttd: value must be an object with fileUuid"
    ));
}

// ── kosong (§7.2) ──

#[test]
fn empties_count_as_missing() {
    let f = json!([{"key": "a", "type": "text", "label": "A", "required": true}]);
    for answers in [
        json!({}),
        json!({"a": null}),
        json!({"a": ""}),
        json!({"a": "   "}),
    ] {
        assert_eq!(
            msgs(f.clone(), answers, FINAL),
            vec!["a: this field is required"]
        );
    }
    let multi = json!([{"key": "m", "type": "multiselect", "label": "M", "required": true,
        "options": {"source": "static", "items": [{"value": "a", "label": "A"}]}}]);
    assert_eq!(
        msgs(multi, json!({"m": []}), FINAL),
        vec!["m: this field is required"]
    );
}

#[test]
fn zero_and_false_are_answers() {
    assert!(
        msgs(
            json!([{"key": "n", "type": "number", "label": "N", "required": true}]),
            json!({"n": 0}),
            FINAL
        )
        .is_empty()
    );
    assert!(
        msgs(
            json!([{"key": "b", "type": "boolean", "label": "B", "required": true}]),
            json!({"b": false}),
            FINAL
        )
        .is_empty()
    );
}

#[test]
fn never_fills_default_value() {
    let r = run(
        json!([{"key": "a", "type": "text", "label": "A", "defaultValue": "bawaan"}]),
        Some(json!({})),
        FINAL,
    );
    assert!(r.answers.is_empty());
}

// ── draft vs submit ──

fn bounds_fields() -> Value {
    json!([
        {"key": "t", "type": "text", "label": "T", "required": true, "validation": {"minLength": 5, "maxLength": 8}},
        {"key": "n", "type": "number", "label": "N", "validation": {"min": 10, "max": 20}}
    ])
}

#[test]
fn draft_skips_required_and_lower_bounds() {
    assert!(msgs(bounds_fields(), json!({"t": "ab", "n": 1}), DRAFT).is_empty());
    assert!(msgs(bounds_fields(), json!({}), DRAFT).is_empty());
}

#[test]
fn draft_enforces_upper_bounds() {
    assert_eq!(
        msgs(bounds_fields(), json!({"t": "abcdefghi", "n": 21}), DRAFT),
        vec![
            "t: value must be at most 8 characters",
            "n: value must be at most 20"
        ]
    );
}

#[test]
fn submit_enforces_required_and_lower_bounds() {
    assert_eq!(
        msgs(bounds_fields(), json!({"n": 1}), FINAL),
        vec!["t: this field is required", "n: value must be at least 10"]
    );
}

// ── repeater ──

fn repeater_fields() -> Value {
    json!([{"key": "unit_list", "type": "repeater", "label": "Daftar unit",
        "validation": {"minRows": 2, "maxRows": 3},
        "fields": [{"key": "unit", "type": "text", "label": "Unit", "required": true},
                   {"key": "qty", "type": "number", "label": "Qty"}]}])
}

#[test]
fn repeater_rows() {
    let f = repeater_fields();
    let rows = json!([{"unit": "a"}, {"unit": "b"}, {"unit": "c"}, {"unit": "d"}]);
    assert!(has(
        &msgs(f.clone(), json!({"unit_list": rows}), DRAFT),
        "unit_list: at most 3 rows are allowed"
    ));
    assert!(msgs(f.clone(), json!({"unit_list": [{"unit": "a"}]}), DRAFT).is_empty());
    assert_eq!(
        msgs(f.clone(), json!({"unit_list": [{"unit": "a"}]}), FINAL),
        vec!["unit_list: at least 2 rows are required"]
    );
    assert_eq!(
        msgs(f.clone(), json!({}), FINAL),
        vec!["unit_list: at least 2 rows are required"]
    );
    assert_eq!(
        msgs(
            f.clone(),
            json!({"unit_list": [{"unit": "a"}, {"unit": "b", "qty": "dua"}]}),
            FINAL
        ),
        vec!["unit_list.1.qty: value must be a finite number"]
    );
    assert_eq!(
        msgs(f.clone(), json!({"unit_list": "x"}), DRAFT),
        vec!["unit_list: value must be an array of rows"]
    );
    assert_eq!(
        msgs(f.clone(), json!({"unit_list": ["x", "y"]}), DRAFT),
        vec![
            "unit_list.0: row must be an object",
            "unit_list.1: row must be an object"
        ]
    );
    assert_eq!(
        msgs(f, json!({"unit_list": [{"qty": 1}, {"unit": "b"}]}), FINAL),
        vec!["unit_list.0.unit: this field is required"]
    );
}

#[test]
fn required_repeater() {
    let f = json!([{"key": "r", "type": "repeater", "label": "R", "required": true,
        "validation": {"minRows": 0, "maxRows": 5}, "fields": [{"key": "a", "type": "text", "label": "A"}]}]);
    assert_eq!(
        msgs(f, json!({"r": []}), FINAL),
        vec!["r: at least one row is required"]
    );
}

// ── group ──

#[test]
fn group_rules() {
    let f = json!([{"key": "diserahkan", "type": "group", "label": "Diserahkan oleh",
        "fields": [{"key": "nama", "type": "text", "label": "Nama", "required": true},
                   {"key": "ttd", "type": "signature", "label": "TTD"}]}]);
    assert_eq!(
        msgs(f.clone(), json!({"diserahkan": "Budi"}), DRAFT),
        vec!["diserahkan: value must be an object"]
    );
    assert_eq!(
        msgs(f.clone(), json!({}), FINAL),
        vec!["diserahkan.nama: this field is required"]
    );
    assert!(msgs(f, json!({}), DRAFT).is_empty());
}

// ── visibilitas ──

fn conditional() -> Value {
    json!([
        select("items", &["ac_split", "kulkas"]),
        {"key": "alasan", "type": "text", "label": "Alasan", "required": true,
         "visibleIf": {"all": [{"field": "items", "op": "eq", "value": "ac_split"}]}}
    ])
}

#[test]
fn hidden_required_ignored_and_pruned() {
    let r = run(
        conditional(),
        Some(json!({"items": "kulkas", "alasan": "basi"})),
        FINAL,
    );
    assert!(r.issues.is_empty());
    assert_eq!(r.answers, obj(json!({"items": "kulkas"})));
    assert_eq!(
        msgs(conditional(), json!({"items": "ac_split"}), FINAL),
        vec!["alasan: this field is required"]
    );
    let d = run(
        conditional(),
        Some(json!({"items": "kulkas", "alasan": "basi"})),
        DRAFT,
    );
    assert_eq!(d.answers, obj(json!({"items": "kulkas", "alasan": "basi"})));
}

#[test]
fn hidden_container_hides_descendants() {
    let f = json!([
        {"key": "gate", "type": "boolean", "label": "Gate"},
        {"key": "sec", "type": "section", "label": "S",
         "visibleIf": {"all": [{"field": "gate", "op": "eq", "value": true}]},
         "fields": [
            {"key": "a", "type": "text", "label": "A", "required": true},
            {"key": "g", "type": "group", "label": "G", "fields": [{"key": "b", "type": "text", "label": "B", "required": true}]}
         ]}
    ]);
    let hidden = run(
        f.clone(),
        Some(json!({"gate": false, "a": "x", "g": {"b": "y"}})),
        FINAL,
    );
    assert!(hidden.issues.is_empty());
    assert_eq!(hidden.answers, obj(json!({"gate": false})));
    let shown = run(f, Some(json!({"gate": true})), FINAL);
    let keys: Vec<&str> = shown.issues.iter().map(|i| i.key.as_str()).collect();
    assert_eq!(keys, vec!["a", "g.b"]);
}

#[test]
fn reaches_fixed_point() {
    let f = json!([
        {"key": "b", "type": "text", "label": "B", "visibleIf": {"all": [{"field": "a", "op": "notEmpty"}]}},
        {"key": "a", "type": "text", "label": "A", "visibleIf": {"all": [{"field": "gate", "op": "eq", "value": true}]}},
        {"key": "gate", "type": "boolean", "label": "Gate"}
    ]);
    let r = run(f, Some(json!({"gate": false, "a": "x", "b": "y"})), FINAL);
    assert_eq!(r.answers, obj(json!({"gate": false})));
}

#[test]
fn visibility_per_repeater_row() {
    let f = json!([{"key": "r", "type": "repeater", "label": "R", "validation": {"minRows": 0, "maxRows": 5},
        "fields": [{"key": "jenis", "type": "boolean", "label": "Jenis"},
                   {"key": "catatan", "type": "text", "label": "Catatan",
                    "visibleIf": {"all": [{"field": "jenis", "op": "eq", "value": true}]}}]}]);
    let r = run(
        f,
        Some(
            json!({"r": [{"jenis": true, "catatan": "disimpan"}, {"jenis": false, "catatan": "dibuang"}]}),
        ),
        FINAL,
    );
    assert_eq!(
        r.answers,
        obj(json!({"r": [{"jenis": true, "catatan": "disimpan"}, {"jenis": false}]}))
    );
}

fn with_condition(op: &str, value: Option<Value>, target: &str) -> Value {
    let cond = match value {
        None => json!({"field": "x", "op": op}),
        Some(v) => json!({"field": "x", "op": op, "value": v}),
    };
    json!([
        {"key": "x", "type": target, "label": "X"},
        {"key": "y", "type": "text", "label": "Y", "visibleIf": {"all": [cond]}}
    ])
}

fn visible(fields: Value, answers: Value) -> bool {
    let mut a = obj(answers);
    a.insert("y".into(), json!("ada"));
    run(fields, Some(Value::Object(a)), FINAL)
        .answers
        .contains_key("y")
}

#[test]
fn operators() {
    let n = |op: &str| with_condition(op, Some(json!(5)), "number");
    assert!(visible(n("eq"), json!({"x": 5})));
    assert!(!visible(n("eq"), json!({"x": 6})));
    assert!(visible(n("ne"), json!({"x": 6})));
    assert!(visible(n("gt"), json!({"x": 6})));
    assert!(!visible(n("gt"), json!({"x": 5})));
    assert!(visible(n("gte"), json!({"x": 5})));
    assert!(visible(n("lt"), json!({"x": 4})));
    assert!(visible(n("lte"), json!({"x": 5})));
    assert!(!visible(n("gt"), json!({})));

    let inn = json!([select("x", &["a", "b", "c"]),
        {"key": "y", "type": "text", "label": "Y", "visibleIf": {"all": [{"field": "x", "op": "in", "value": ["a", "b"]}]}}]);
    assert!(visible(inn.clone(), json!({"x": "a"})));
    assert!(!visible(inn, json!({"x": "c"})));

    let multi = |cond: Value| {
        json!([{"key": "x", "type": "multiselect", "label": "X",
            "options": {"source": "static", "items": [{"value": "a", "label": "A"}, {"value": "b", "label": "B"}, {"value": "c", "label": "C"}]}},
            {"key": "y", "type": "text", "label": "Y", "visibleIf": {"all": [cond]}}])
    };
    let m_in = multi(json!({"field": "x", "op": "in", "value": ["a"]}));
    assert!(visible(m_in.clone(), json!({"x": ["a", "c"]})));
    assert!(!visible(m_in, json!({"x": ["b", "c"]})));
    let m_eq = multi(json!({"field": "x", "op": "eq", "value": ["a", "b"]}));
    assert!(visible(m_eq.clone(), json!({"x": ["b", "a"]})));
    assert!(!visible(m_eq, json!({"x": ["a"]})));

    let empty = with_condition("empty", None, "text");
    assert!(visible(empty.clone(), json!({})));
    assert!(visible(empty.clone(), json!({"x": ""})));
    assert!(visible(empty.clone(), json!({"x": null})));
    assert!(!visible(empty, json!({"x": "ada"})));
    assert!(!visible(
        with_condition("empty", None, "number"),
        json!({"x": 0})
    ));
    assert!(!visible(
        with_condition("empty", None, "boolean"),
        json!({"x": false})
    ));

    let both = json!([
        {"key": "x", "type": "number", "label": "X"},
        {"key": "z", "type": "number", "label": "Z"},
        {"key": "y", "type": "text", "label": "Y", "visibleIf": {
            "all": [{"field": "x", "op": "gte", "value": 5}],
            "any": [{"field": "z", "op": "eq", "value": 1}, {"field": "z", "op": "eq", "value": 2}]}}
    ]);
    assert!(visible(both.clone(), json!({"x": 5, "z": 2})));
    assert!(!visible(both.clone(), json!({"x": 4, "z": 2})));
    assert!(!visible(both, json!({"x": 5, "z": 3})));
}

// ── rujukan file ──

fn file_fields() -> Value {
    json!([
        {"key": "ttd", "type": "signature", "label": "TTD"},
        {"key": "r", "type": "repeater", "label": "R", "validation": {"minRows": 0, "maxRows": 5},
         "fields": [{"key": "foto", "type": "photo", "label": "Foto"}]}
    ])
}

fn file_refs(r: &ValidateAnswersResult) -> Vec<(String, String)> {
    r.files
        .iter()
        .map(|f| (f.file_uuid.clone(), f.field_path.clone()))
        .collect()
}

#[test]
fn collects_files() {
    let r = run(
        file_fields(),
        Some(json!({"ttd": {"fileUuid": UUID_A}, "r": [{}, {}, {"foto": {"fileUuid": UUID_B}}]})),
        FINAL,
    );
    assert_eq!(
        file_refs(&r),
        vec![
            (UUID_A.into(), "ttd".into()),
            (UUID_B.into(), "r.2.foto".into())
        ]
    );
    let d = run(
        file_fields(),
        Some(json!({"ttd": {"fileUuid": UUID_A}})),
        DRAFT,
    );
    assert_eq!(file_refs(&d), vec![(UUID_A.into(), "ttd".into())]);
}

#[test]
fn hidden_files_not_collected() {
    let f = json!([
        {"key": "gate", "type": "boolean", "label": "Gate"},
        {"key": "ttd", "type": "signature", "label": "TTD", "visibleIf": {"all": [{"field": "gate", "op": "eq", "value": true}]}}
    ]);
    let r = run(
        f,
        Some(json!({"gate": false, "ttd": {"fileUuid": UUID_A}})),
        FINAL,
    );
    assert!(r.files.is_empty());
    assert_eq!(r.answers, obj(json!({"gate": false})));
}

// ── product/order ──

fn product_select(key: &str, extra: Value) -> Value {
    let mut n =
        obj(json!({"key": key, "type": "select", "label": key, "options": {"source": "product"}}));
    n.extend(obj(extra));
    Value::Object(n)
}

fn order_multi(key: &str, extra: Value) -> Value {
    let mut n = obj(
        json!({"key": key, "type": "multiselect", "label": key, "options": {"source": "order"}}),
    );
    n.extend(obj(extra));
    Value::Object(n)
}

#[test]
fn product_order_fields() {
    let f = json!([product_select("produk", json!({}))]);
    assert!(msgs(f.clone(), json!({"produk": UUID_A}), FINAL).is_empty());
    assert!(has(
        &msgs(f, json!({"produk": 123}), FINAL),
        "produk: value must be a string"
    ));

    let f = json!([order_multi("orders", json!({}))]);
    assert!(msgs(f.clone(), json!({"orders": [UUID_A, UUID_B]}), FINAL).is_empty());
    assert!(has(
        &msgs(f.clone(), json!({"orders": [UUID_A, UUID_A]}), FINAL),
        &format!("orders: \"{UUID_A}\" is selected more than once")
    ));
    assert!(has(
        &msgs(f, json!({"orders": [1]}), FINAL),
        "orders: every selected option must be a string"
    ));

    let f = json!([order_multi(
        "orders",
        json!({"validation": {"minItems": 2}})
    )]);
    assert!(has(
        &msgs(f, json!({"orders": [UUID_A]}), FINAL),
        "orders: select at least 2 options"
    ));
    let f = json!([order_multi(
        "orders",
        json!({"validation": {"maxItems": 1}})
    )]);
    assert!(has(
        &msgs(f, json!({"orders": [UUID_A, UUID_B]}), FINAL),
        "orders: select at most 1 options"
    ));
}

#[test]
fn collects_product_order_refs() {
    let f = json!([
        product_select("produk", json!({})),
        {"key": "unit_list", "type": "repeater", "label": "Unit", "validation": {"minRows": 0, "maxRows": 5},
         "fields": [order_multi("produk", json!({}))]}
    ]);
    let r = run(
        f,
        Some(json!({"produk": UUID_A, "unit_list": [{"produk": [UUID_B]}]})),
        FINAL,
    );
    assert_eq!(
        r.product_order_refs,
        vec![
            AnswerProductOrderReference {
                source: OptionsSource::Product,
                field_path: "produk".into(),
                ids: vec![UUID_A.into()]
            },
            AnswerProductOrderReference {
                source: OptionsSource::Order,
                field_path: "unit_list.0.produk".into(),
                ids: vec![UUID_B.into()]
            },
        ]
    );
}

// ── timezone ──

#[test]
fn date_bounds_use_company_day() {
    let f = json!([{"key": "d", "type": "date", "label": "D", "validation": {"minDate": "today-7d", "maxDate": "today"}}]);
    let now = "2026-08-11T18:00:00Z";
    assert!(run_at(f.clone(), json!({"d": "2026-08-12"}), DRAFT, now).is_empty());
    assert_eq!(
        run_at(f.clone(), json!({"d": "2026-08-13"}), DRAFT, now),
        vec!["d: date must be on or before 2026-08-12"]
    );
    assert!(run_at(f.clone(), json!({"d": "2026-08-05"}), DRAFT, now).is_empty());
    assert_eq!(
        run_at(f, json!({"d": "2026-08-04"}), DRAFT, now),
        vec!["d: date must be on or after 2026-08-05"]
    );
}

#[test]
fn datetime_max_is_end_of_day() {
    let f = json!([{"key": "dt", "type": "datetime", "label": "DT", "validation": {"maxDate": "today"}}]);
    let now = "2026-08-11T05:00:00Z";
    let ms = |s: &str| {
        chrono::DateTime::parse_from_rfc3339(s)
            .unwrap()
            .timestamp_millis()
    };
    assert!(
        run_at(
            f.clone(),
            json!({"dt": ms("2026-08-11T16:59:00Z")}),
            DRAFT,
            now
        )
        .is_empty()
    );
    assert_eq!(
        run_at(f, json!({"dt": ms("2026-08-11T17:30:00Z")}), DRAFT, now).len(),
        1
    );
}

#[test]
fn no_duplicate_messages() {
    let f = json!([{"key": "t", "type": "text", "label": "T", "validation": {"minLength": 5, "maxLength": 3}}]);
    let issues = msgs(f, json!({"t": "abcd"}), FINAL);
    let set: HashSet<&String> = issues.iter().collect();
    assert_eq!(set.len(), issues.len());
}

#[test]
fn byte_size() {
    assert_eq!(answers_byte_size(Some(&json!({}))), 2);
    assert_eq!(
        answers_byte_size(Some(&json!({"a": "é"}))),
        "{\"a\":\"é\"}".len()
    );
    assert_eq!(answers_byte_size(Some(&json!(null))), 2);
    assert_eq!(answers_byte_size(None), 2);
}

#[test]
fn normalizes_numbers_like_json_parse() {
    let f = json!([{"key": "n", "type": "number", "label": "N"}]);
    let input: Value = serde_json::from_str(r#"{"n": 1.0}"#).unwrap();
    let r = run(f, Some(input), DRAFT);
    assert_eq!(serde_json::to_string(&r.answers).unwrap(), r#"{"n":1}"#);
}
