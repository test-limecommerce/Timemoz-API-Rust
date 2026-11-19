//! Port bagian MURNI `audit-log.middleware.ts`: dari `(before, after)` ke entri audit.
//! Pembacaan DB (induk, nama anggota, kode toko) ada di [`crate::handle`].

use std::collections::HashMap;

use serde_json::{Value, json};

use crate::policy::{diff_snapshots, is_excluded_field, project_row, sanitize_where};
use crate::row::{Row, as_int, as_text};
use crate::scope::{
    AUDIT_BULK_ENTITY_ID, AUDIT_BULK_THRESHOLD, Action, EntityType, MemberKind, Model, Spec,
};
use crate::signature::{detect_transition, has_required_fields};

/// `AuditEntryDraft` — satu entri sebelum pelaku/jejak request diketahui.
#[derive(Debug, Clone, PartialEq)]
pub struct AuditEntry {
    pub entity_type: EntityType,
    pub entity_id: i32,
    pub entity_uuid: Option<String>,
    pub entity_label: Option<String>,
    /// `None` = perubahan pada tabel utama entitas itu sendiri.
    pub sub_entity: Option<String>,
    pub action: Action,
    /// `None` = ditentukan saat tulis (urutan §4.1) atau dibuang.
    pub company_id: Option<i32>,
    pub before: Option<Value>,
    pub after: Option<Value>,
    pub changed_fields: Vec<String>,
    pub is_restorable: bool,
}

/// Hasil pra-baca (`PreRead`).
#[derive(Debug, Clone, PartialEq)]
pub enum PreRead {
    /// `AUDIT_LOG_ENABLED=false`: tidak ada pra-baca, tidak ada perekaman.
    Disabled,
    /// Pra-baca gagal (sudah di-`warn`); barisnya dicatat tanpa `before`.
    Failed,
    Rows {
        rows: Vec<Row>,
        truncated: bool,
    },
}

impl PreRead {
    /// Pra-baca yang berhasil (untuk baris yang dibangun pemanggil sendiri).
    pub fn rows(rows: Vec<Row>) -> Self {
        let truncated = rows.len() > AUDIT_BULK_THRESHOLD;
        PreRead::Rows { rows, truncated }
    }
    pub fn is_ok(&self) -> bool {
        matches!(self, PreRead::Rows { .. })
    }
    pub fn as_rows(&self) -> &[Row] {
        match self {
            PreRead::Rows { rows, .. } => rows,
            _ => &[],
        }
    }
    pub fn first(&self) -> Option<&Row> {
        self.as_rows().first()
    }
    fn truncated(&self) -> bool {
        matches!(
            self,
            PreRead::Rows {
                truncated: true,
                ..
            }
        )
    }
}

pub(crate) struct Built {
    pub entry: AuditEntry,
    pub raw: Row,
}

fn to_i32(v: Option<i64>) -> Option<i32> {
    v.and_then(|v| i32::try_from(v).ok())
}

/// `toEntry(beforeRow, afterRow, forcedAction)`.
pub(crate) fn to_entry(
    model: Model,
    before_row: Option<&Row>,
    after_row: Option<&Row>,
    forced: Option<Action>,
) -> Option<Built> {
    let spec = model.spec();
    let entity_type = spec.entity_type();
    let before = project_row(model, before_row);
    let after = project_row(model, after_row);
    let raw = after_row.or(before_row)?;

    let action = forced
        .or_else(|| detect_transition(entity_type, before.as_ref(), after.as_ref()))
        .unwrap_or(Action::Update);

    let changed_fields = if action == Action::Create {
        after
            .as_ref()
            .map(|a| a.keys().map(str::to_owned).collect())
            .unwrap_or_default()
    } else {
        diff_snapshots(before.as_ref(), after.as_ref())
    };

    // §4.3 — `update` tanpa kolom berarti dibuang, KECUALI `before`-nya hilang.
    if action == Action::Update && before.is_some() && changed_fields.is_empty() {
        return None;
    }

    let company_id_of = |row: Option<&Row>| row.and_then(|r| spec.company_id_of(r));
    let (entity_id, entity_uuid, entity_label, sub_entity, restorable) = match spec {
        Spec::Companion(c) => (
            raw.int(c.parent_id_field),
            None,
            None,
            Some(c.sub_entity.to_owned()),
            false,
        ),
        Spec::Entity(e) => (
            raw.int("id"),
            raw.get("uuid").and_then(Value::as_str).map(str::to_owned),
            (e.label_of)(raw),
            None,
            e.restorable,
        ),
    };
    let entity_id = to_i32(entity_id)?;

    let is_restorable = action == Action::Delete
        && restorable
        && after_row.is_some()
        && has_required_fields(entity_type, before.as_ref());

    Some(Built {
        raw: raw.clone(),
        entry: AuditEntry {
            entity_type,
            entity_id,
            entity_uuid,
            entity_label,
            sub_entity,
            action,
            company_id: to_i32(company_id_of(after_row).or_else(|| company_id_of(before_row))),
            before: before.map(|r| r.to_json()),
            after: after.map(|r| r.to_json()),
            changed_fields,
            is_restorable,
        },
    })
}

/// Baris ringkasan massal (`bulkEntry`, §3.5).
fn bulk_entry(
    model: Model,
    action: Action,
    affected: u64,
    company_id: Option<i64>,
    where_: &Value,
    sample_ids: &[i64],
    reason: &str,
) -> AuditEntry {
    tracing::info!(
        "{affected} baris {model}.{action} diringkas jadi satu baris audit — {reason} (§3.5)"
    );
    AuditEntry {
        entity_type: model.spec().entity_type(),
        entity_id: AUDIT_BULK_ENTITY_ID,
        entity_uuid: None,
        entity_label: None,
        sub_entity: Some("bulk".to_owned()),
        action,
        company_id: to_i32(company_id),
        before: None,
        after: Some(json!({
            "affected": affected,
            "where": sanitize_where(where_),
            "sampleIds": sample_ids.iter().take(10).collect::<Vec<_>>(),
        })),
        changed_fields: Vec::new(),
        is_restorable: false,
    }
}

fn sample_ids(pre: &PreRead) -> Vec<i64> {
    pre.as_rows().iter().filter_map(|r| r.int("id")).collect()
}

fn bulk_reason(pre: &PreRead) -> String {
    if pre.is_ok() {
        format!("melewati ambang {AUDIT_BULK_THRESHOLD}")
    } else {
        "pra-bacanya gagal, jadi barisnya tidak bisa dirinci".to_owned()
    }
}

/// `create`.
pub(crate) fn build_create(model: Model, result: &Row) -> Vec<Built> {
    to_entry(model, None, Some(result), Some(Action::Create))
        .into_iter()
        .collect()
}

/// `createMany` — `data` = argumen `data` Prisma, `affected` = `count`.
pub(crate) fn build_create_many(model: Model, data: &[Row], affected: u64) -> Vec<Built> {
    if affected == 0 {
        return Vec::new();
    }
    let spec = model.spec();
    if matches!(spec, Spec::Companion(_)) && data.len() <= AUDIT_BULK_THRESHOLD {
        return data
            .iter()
            .filter_map(|d| to_entry(model, None, Some(d), Some(Action::Create)))
            .collect();
    }
    let first = data.first();
    let mut entry = bulk_entry(
        model,
        Action::Create,
        affected,
        first.and_then(|r| spec.company_id_of(r)),
        &Value::Null,
        &[],
        "createMany tidak mengembalikan id sama sekali di Prisma 5.5.2",
    );
    let sample: Vec<Value> = data
        .iter()
        .take(10)
        .map(|d| project_row(model, Some(d)).map_or(Value::Null, |r| r.to_json()))
        .collect();
    entry.after = Some(json!({ "affected": affected, "sample": sample }));
    vec![Built {
        entry,
        raw: first.cloned().unwrap_or_default(),
    }]
}

/// `update` / `upsert` — `result` = baris yang dikembalikan tulisannya.
pub(crate) fn build_update(model: Model, pre: &PreRead, result: &Row, upsert: bool) -> Vec<Built> {
    let before = pre.first();
    let inserted = upsert && pre.is_ok() && before.is_none();
    to_entry(
        model,
        before,
        Some(result),
        inserted.then_some(Action::Create),
    )
    .into_iter()
    .collect()
}

/// `data` `updateMany` setelah `splitUpdateData`: nilai yang bisa diturunkan, dan kolom
/// yang harus dibaca ulang (`{ increment }`, objek tanpa `set`).
pub(crate) fn split_update_data(model: Model, data: &Row) -> (Row, Vec<String>) {
    let mut plain = Row::new();
    let mut pending = Vec::new();
    for (field, value) in data.iter() {
        if is_excluded_field(model, field) {
            continue;
        }
        if let Value::Object(obj) = value {
            if let Some(set) = obj.get("set") {
                plain.set(field, set.clone());
            } else {
                pending.push(field.to_owned());
            }
            continue;
        }
        plain.set(field, value.clone());
    }
    (plain, pending)
}

/// `updateMany`. `post` = baris hasil pembacaan ulang per id (hanya bila ada `pending`).
pub(crate) fn build_update_many(
    model: Model,
    pre: &PreRead,
    affected: u64,
    plain: &Row,
    pending: &[String],
    post: &HashMap<i64, Row>,
    where_: &Value,
) -> Vec<Built> {
    if affected == 0 {
        return Vec::new();
    }
    let spec = model.spec();
    if !pre.is_ok() || affected as usize > AUDIT_BULK_THRESHOLD || pre.truncated() {
        let company_id = pre
            .first()
            .and_then(|r| spec.company_id_of(r))
            .or_else(|| where_.get("companyId").and_then(as_int));
        let entry = bulk_entry(
            model,
            Action::Update,
            affected,
            company_id,
            where_,
            &sample_ids(pre),
            &bulk_reason(pre),
        );
        return vec![Built {
            entry,
            raw: pre.first().cloned().unwrap_or_default(),
        }];
    }
    let mut out = Vec::new();
    for before in pre.as_rows() {
        let mut after = before.merged(plain);
        if let Some(post_row) = before.int("id").and_then(|id| post.get(&id)) {
            for field in pending {
                after.set(field, post_row.get(field).cloned().unwrap_or(Value::Null));
            }
        }
        out.extend(to_entry(model, Some(before), Some(&after), None));
    }
    out
}

/// `delete` — `result` = baris yang dikembalikan `delete` (pengganti pra-baca yang gagal).
pub(crate) fn build_delete(model: Model, pre: &PreRead, result: Option<&Row>) -> Vec<Built> {
    let before = pre.first().or(result);
    to_entry(model, before, None, Some(Action::Delete))
        .into_iter()
        .collect()
}

/// `deleteMany`.
pub(crate) fn build_delete_many(
    model: Model,
    pre: &PreRead,
    affected: u64,
    where_: &Value,
) -> Vec<Built> {
    if affected == 0 {
        return Vec::new();
    }
    let spec = model.spec();
    if !pre.is_ok() || affected as usize > AUDIT_BULK_THRESHOLD || pre.truncated() {
        let company_id = pre
            .first()
            .and_then(|r| spec.company_id_of(r))
            .or_else(|| where_.get("companyId").and_then(as_int));
        let entry = bulk_entry(
            model,
            Action::Delete,
            affected,
            company_id,
            where_,
            &sample_ids(pre),
            &bulk_reason(pre),
        );
        return vec![Built {
            entry,
            raw: pre.first().cloned().unwrap_or_default(),
        }];
    }
    pre.as_rows()
        .iter()
        .filter_map(|b| to_entry(model, Some(b), None, Some(Action::Delete)))
        .collect()
}

// ── Keanggotaan (§4.2) ───────────────────────────────────────────────────────

/// Pemisah kunci majemuk (NUL tidak bisa disimpan di kolom `text`).
const SEP: char = '\u{0}';

/// Arah perubahan keanggotaan (`membershipEffect`; `none` = jangan panggil sama sekali).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberEffect {
    /// `create`/`createMany`, atau `update*` yang menulis `deletedAt: null`.
    Add,
    /// `delete*`, atau `update*` yang mengisi `deletedAt`.
    Remove,
}

/// `MembershipShape.select`.
pub(crate) fn member_select(kind: MemberKind) -> &'static [&'static str] {
    match kind {
        MemberKind::User => &["id", "userId", "deletedAt"],
        MemberKind::UserHardDelete => &["id", "userId"],
        MemberKind::Grant => &["id", "featureKey", "action"],
        MemberKind::UserRule => &["id", "userId", "deletedAt", "limitMinutes"],
        MemberKind::UserRole => &["id", "userId", "deletedAt", "role"],
    }
}

/// `activeWhere` = `{ deletedAt: null }`?
pub(crate) fn member_has_soft_delete(kind: MemberKind) -> bool {
    matches!(
        kind,
        MemberKind::User | MemberKind::UserRule | MemberKind::UserRole
    )
}

fn grant_key(get: impl Fn(&str) -> Option<String>) -> Option<String> {
    let f = get("featureKey")?;
    let a = get("action")?;
    Some(format!("{f}{SEP}{a}"))
}

fn rule_key(user_id: Option<i64>, limit: Option<&Value>) -> Option<String> {
    let id = user_id?;
    let limit = limit
        .and_then(as_int)
        .map_or_else(|| "always".to_owned(), |l| l.to_string());
    Some(format!("{id}{SEP}{limit}"))
}

fn role_key(user_id: Option<i64>, role: Option<&Value>) -> Option<String> {
    let id = user_id?;
    let role = role.and_then(Value::as_str).filter(|r| !r.is_empty())?;
    Some(format!("{id}{SEP}{role}"))
}

/// `keyOfRow` (bentuk TABEL).
pub(crate) fn key_of_row(kind: MemberKind, row: &Row) -> Option<String> {
    match kind {
        MemberKind::User | MemberKind::UserHardDelete => row.int("userId").map(|i| i.to_string()),
        MemberKind::Grant => grant_key(|f| row.text(f).map(str::to_owned)),
        MemberKind::UserRule => rule_key(row.int("userId"), row.get("limitMinutes")),
        MemberKind::UserRole => role_key(row.int("userId"), row.get("role")),
    }
}

/// `keyOfMember` (bentuk SNAPSHOT).
pub(crate) fn key_of_member(kind: MemberKind, member: &Value) -> Option<String> {
    let int = |f: &str| member.get(f).and_then(as_int);
    match kind {
        MemberKind::User | MemberKind::UserHardDelete => int("id").map(|i| i.to_string()),
        MemberKind::Grant => grant_key(|f| member.get(f).and_then(as_text).map(str::to_owned)),
        MemberKind::UserRule => rule_key(int("id"), member.get("limitMinutes")),
        MemberKind::UserRole => role_key(int("id"), member.get("role")),
    }
}

/// `Number(key)` untuk bagian id sebuah kunci.
fn key_id(key: &str) -> Option<i64> {
    key.split(SEP).next().and_then(|s| s.parse().ok())
}

/// Id user yang perlu dibaca namanya untuk `hydrate`.
pub(crate) fn hydrate_ids(kind: MemberKind, keys: &[String]) -> Vec<i64> {
    if kind == MemberKind::Grant {
        return Vec::new();
    }
    let mut ids: Vec<i64> = keys.iter().filter_map(|k| key_id(k)).collect();
    ids.sort_unstable();
    ids.dedup();
    ids
}

/// `withNames` — `names = None` berarti pembacaannya gagal (anggota ditulis tanpa nama).
fn with_names(ids: &[i64], names: Option<&HashMap<i64, Option<String>>>) -> Vec<(i64, Value)> {
    let mut unique = ids.to_vec();
    unique.sort_unstable();
    unique.dedup();
    unique
        .into_iter()
        .map(|id| {
            let v = match names {
                Some(n) => json!({ "id": id, "name": n.get(&id).cloned().flatten() }),
                None => json!({ "id": id }),
            };
            (id, v)
        })
        .collect()
}

/// `hydrate(keys)` — kunci → daftar anggota siap tulis, terurut stabil.
pub(crate) fn hydrate(
    kind: MemberKind,
    keys: &[String],
    names: Option<&HashMap<i64, Option<String>>>,
) -> Vec<Value> {
    match kind {
        MemberKind::Grant => {
            let mut sorted = keys.to_vec();
            sorted.sort();
            sorted
                .iter()
                .map(|k| {
                    let mut parts = k.split(SEP);
                    json!({ "featureKey": parts.next(), "action": parts.next() })
                })
                .collect()
        }
        MemberKind::User | MemberKind::UserHardDelete => {
            let ids: Vec<i64> = keys.iter().filter_map(|k| key_id(k)).collect();
            with_names(&ids, names)
                .into_iter()
                .map(|(_, v)| v)
                .collect()
        }
        MemberKind::UserRule | MemberKind::UserRole => {
            let mut parsed: Vec<(i64, String)> = keys
                .iter()
                .filter_map(|k| {
                    let mut it = k.split(SEP);
                    let id = it.next()?.parse().ok()?;
                    Some((id, it.next().unwrap_or("").to_owned()))
                })
                .collect();
            let ids: Vec<i64> = parsed.iter().map(|(id, _)| *id).collect();
            let named: HashMap<i64, Value> = with_names(&ids, names).into_iter().collect();
            if kind == MemberKind::UserRule {
                let limit = |s: &str| {
                    if s == "always" {
                        None
                    } else {
                        s.parse::<i64>().ok()
                    }
                };
                parsed.sort_by(|a, b| {
                    a.0.cmp(&b.0)
                        .then(limit(&a.1).unwrap_or(-1).cmp(&limit(&b.1).unwrap_or(-1)))
                });
                parsed
                    .into_iter()
                    .map(|(id, l)| {
                        let mut v = named
                            .get(&id)
                            .cloned()
                            .unwrap_or_else(|| json!({ "id": id }));
                        if let Some(obj) = v.as_object_mut() {
                            obj.insert(
                                "limitMinutes".into(),
                                limit(&l).map_or(Value::Null, Value::from),
                            );
                        }
                        v
                    })
                    .collect()
            } else {
                parsed.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
                parsed
                    .into_iter()
                    .map(|(id, role)| {
                        let mut v = named
                            .get(&id)
                            .cloned()
                            .unwrap_or_else(|| json!({ "id": id }));
                        if let Some(obj) = v.as_object_mut() {
                            obj.insert("role".into(), Value::String(role));
                        }
                        v
                    })
                    .collect()
            }
        }
    }
}

/// `membersOf(snapshot)`.
pub(crate) fn members_of(snapshot: Option<&Value>) -> Vec<Value> {
    snapshot
        .and_then(|s| s.get("members"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

/// `sameMembers`.
pub(crate) fn same_members(kind: MemberKind, a: &[Value], b: &[Value]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut left: Vec<String> = a.iter().filter_map(|m| key_of_member(kind, m)).collect();
    let mut right: Vec<String> = b.iter().filter_map(|m| key_of_member(kind, m)).collect();
    left.sort();
    right.sort();
    left.iter()
        .enumerate()
        .all(|(i, k)| right.get(i) == Some(k))
}

/// `nextKeys` — kunci anggota sesudah perubahan.
pub(crate) fn next_keys(
    kind: MemberKind,
    base: &[Value],
    touched: &[String],
    effect: MemberEffect,
) -> Vec<String> {
    let mut keys: Vec<String> = Vec::new();
    for k in base.iter().filter_map(|m| key_of_member(kind, m)) {
        if !keys.contains(&k) {
            keys.push(k);
        }
    }
    for k in touched {
        match effect {
            MemberEffect::Add => {
                if !keys.contains(k) {
                    keys.push(k.clone());
                }
            }
            MemberEffect::Remove => keys.retain(|x| x != k),
        }
    }
    keys
}

/// Entri keanggotaan baru (sebelum identitas induk diisi).
pub(crate) fn membership_entry(
    entity_type: EntityType,
    parent_id: i32,
    sub_entity: &str,
    before: Vec<Value>,
    after: Vec<Value>,
) -> AuditEntry {
    AuditEntry {
        entity_type,
        entity_id: parent_id,
        entity_uuid: None,
        entity_label: None,
        sub_entity: Some(sub_entity.to_owned()),
        action: Action::Update,
        company_id: None,
        before: Some(json!({ "members": before })),
        after: Some(json!({ "members": after })),
        changed_fields: vec!["members".to_owned()],
        is_restorable: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store_row(active: bool, deleted: Option<&str>) -> Row {
        Row::new()
            .with("id", 9)
            .with("uuid", "aaaaaaaa-0000-4000-8000-000000000009")
            .with("companyId", 7)
            .with("code", "A1")
            .with("name", "Toko A")
            .with("isActive", active)
            .with("deletedAt", deleted.map_or(Value::Null, Value::from))
    }

    fn only(built: Vec<Built>) -> AuditEntry {
        assert_eq!(built.len(), 1);
        built
            .into_iter()
            .next()
            .map(|b| b.entry)
            .unwrap_or_else(|| membership_entry(EntityType::Store, 0, "", vec![], vec![]))
    }

    #[test]
    fn create_entry_from_result() {
        let e = only(build_create(Model::Store, &store_row(true, None)));
        assert_eq!(e.action, Action::Create);
        assert_eq!(e.entity_type, EntityType::Store);
        assert_eq!(e.entity_id, 9);
        assert_eq!(
            e.entity_uuid.as_deref(),
            Some("aaaaaaaa-0000-4000-8000-000000000009")
        );
        assert_eq!(e.entity_label.as_deref(), Some("A1 — Toko A"));
        assert_eq!(e.company_id, Some(7));
        assert_eq!(e.before, None);
        // Urutan kolom hasil (bukan alfabetis), tanpa `id`.
        assert_eq!(
            e.changed_fields,
            vec!["uuid", "companyId", "code", "name", "isActive", "deletedAt"]
        );
        assert!(!e.is_restorable);
    }

    #[test]
    fn create_with_narrow_select_mirrors_prisma_result() {
        // `company.create({ select: { id, uuid } })` (register): label null, after {uuid}.
        let e = only(build_create(
            Model::Company,
            &Row::new().with("id", 3).with("uuid", "u-3"),
        ));
        assert_eq!(e.company_id, Some(3));
        assert_eq!(e.entity_label, None);
        assert_eq!(e.after, Some(json!({ "uuid": "u-3" })));
        assert_eq!(e.changed_fields, vec!["uuid"]);
    }

    #[test]
    fn create_many_entity_is_summarised() {
        let data = vec![
            Row::new()
                .with("companyId", 7)
                .with("userId", 3)
                .with("storeId", 11)
                .with("visitDate", "2026-08-20"),
            Row::new()
                .with("companyId", 7)
                .with("userId", 3)
                .with("storeId", 12)
                .with("visitDate", "2026-08-20"),
        ];
        let e = only(build_create_many(Model::StoreVisitPlan, &data, 2));
        assert_eq!(e.action, Action::Create);
        assert_eq!(e.sub_entity.as_deref(), Some("bulk"));
        assert_eq!(e.entity_id, AUDIT_BULK_ENTITY_ID);
        assert_eq!(e.entity_uuid, None);
        assert_eq!(e.company_id, Some(7));
        assert_eq!(
            e.after.as_ref().and_then(|a| a.get("affected")),
            Some(&json!(2))
        );
        assert!(build_create_many(Model::StoreVisitPlan, &data, 0).is_empty());
    }

    #[test]
    fn create_many_companion_is_itemised() {
        let data = vec![
            Row::new()
                .with("userId", 5)
                .with("accessLevelId", 2)
                .with("companyId", 7)
                .with("status", 1),
        ];
        let e = only(build_create_many(Model::UserAccessLevel, &data, 1));
        assert_eq!(e.entity_type, EntityType::Employee);
        assert_eq!(e.entity_id, 5);
        assert_eq!(e.sub_entity.as_deref(), Some("accessLevel"));
        assert_eq!(e.company_id, Some(7));
        assert_eq!(
            e.after,
            Some(json!({ "accessLevelId": 2, "companyId": 7, "status": 1 }))
        );
        assert_eq!(
            e.changed_fields,
            vec!["accessLevelId", "companyId", "status"]
        );
    }

    #[test]
    fn update_diff_and_label_at_event_time() {
        let before = Row::new()
            .with("id", 4)
            .with("uuid", "c4")
            .with("companyId", 7)
            .with("code", "JOG")
            .with("name", "Jogja Barat")
            .with("isActive", true);
        let after = before.clone().with("name", "Jogja Barat Daya");
        let e = only(build_update(
            Model::StoreGroup,
            &PreRead::rows(vec![before]),
            &after,
            false,
        ));
        assert_eq!(e.action, Action::Update);
        assert_eq!(e.changed_fields, vec!["name"]);
        assert_eq!(
            e.before.as_ref().and_then(|b| b.get("name")),
            Some(&json!("Jogja Barat"))
        );
        assert_eq!(e.entity_label.as_deref(), Some("JOG — Jogja Barat Daya"));
    }

    #[test]
    fn soft_delete_update_becomes_restorable_delete() {
        let before = store_row(true, None);
        let after = store_row(false, Some("2026-08-19T00:00:00.000Z"));
        let e = only(build_update(
            Model::Store,
            &PreRead::rows(vec![before]),
            &after,
            false,
        ));
        assert_eq!(e.action, Action::Delete);
        assert!(e.is_restorable);
        assert_eq!(
            e.before.as_ref().and_then(|b| b.get("isActive")),
            Some(&json!(true))
        );
    }

    #[test]
    fn upsert_that_inserts_is_create_but_failed_preread_is_not() {
        let row = Row::new()
            .with("id", 1)
            .with("uuid", "n1")
            .with("companyId", 7)
            .with("name", "Z");
        let e = only(build_update(
            Model::NegativeZone,
            &PreRead::rows(vec![]),
            &row,
            true,
        ));
        assert_eq!(e.action, Action::Create);
        let e = only(build_update(
            Model::NegativeZone,
            &PreRead::Failed,
            &row,
            true,
        ));
        assert_eq!(e.action, Action::Update);
        assert_eq!(e.before, None);
        assert!(e.changed_fields.is_empty());
    }

    #[test]
    fn noise_only_update_is_dropped() {
        let before = store_row(true, None).with("totalVisits", 1);
        let after = before
            .clone()
            .with("totalVisits", 1000)
            .with("lastVisitAt", "2026-08-19T00:00:00.000Z");
        assert!(build_update(Model::Store, &PreRead::rows(vec![before]), &after, false).is_empty());
    }

    #[test]
    fn secret_only_update_is_dropped_without_leaking() {
        let before = Row::new()
            .with("id", 7)
            .with("uuid", "c7")
            .with("name", "Acme")
            .with("jiraAccessToken", "a");
        let after = before.clone().with("jiraAccessToken", "b");
        assert!(
            build_update(
                Model::Company,
                &PreRead::rows(vec![before.clone()]),
                &after,
                false
            )
            .is_empty()
        );
        // Pra-baca gagal → tetap dicatat, tanpa before, tanpa nilai rahasia.
        let e = only(build_update(
            Model::Company,
            &PreRead::Failed,
            &after,
            false,
        ));
        assert_eq!(e.after, Some(json!({ "uuid": "c7", "name": "Acme" })));
    }

    #[test]
    fn users_password_update_leaves_no_row() {
        // `users.update({ data: { password } })` — hanya kolom allowlist yang dibandingkan.
        let before = Row::new()
            .with("username", "a@b.c")
            .with("name", "A")
            .with("status", 1)
            .with("id", 5)
            .with("uuid", "u5");
        let after = before.clone().with("password", "hash");
        assert!(build_update(Model::Users, &PreRead::rows(vec![before]), &after, false).is_empty());
    }

    #[test]
    fn update_many_derives_after_from_data() {
        let rows: Vec<Row> = (1..=3)
            .map(|i| {
                Row::new()
                    .with("id", i)
                    .with("uuid", format!("g{i}"))
                    .with("companyId", 7)
                    .with("name", "G")
                    .with("isActive", true)
            })
            .collect();
        let (plain, pending) = split_update_data(
            Model::StoreGroup,
            &Row::new().with("isActive", false).with("updatedAt", "x"),
        );
        assert!(pending.is_empty());
        let built = build_update_many(
            Model::StoreGroup,
            &PreRead::rows(rows),
            3,
            &plain,
            &pending,
            &HashMap::new(),
            &json!({}),
        );
        assert_eq!(built.len(), 3);
        assert!(built.iter().all(|b| b.entry.action == Action::Delete));
    }

    #[test]
    fn update_many_pending_fields_come_from_post_read() {
        let before = Row::new()
            .with("id", 1)
            .with("uuid", "d1")
            .with("companyId", 7)
            .with("name", "Div")
            .with("budget", 1);
        let data = Row::new().with("budget", json!({ "increment": 1 }));
        let (plain, pending) = split_update_data(Model::StoreGroup, &data);
        assert_eq!(pending, vec!["budget"]);
        let mut post = HashMap::new();
        post.insert(1, before.clone().with("budget", 2));
        let e = only(build_update_many(
            Model::StoreGroup,
            &PreRead::rows(vec![before]),
            1,
            &plain,
            &pending,
            &post,
            &json!({}),
        ));
        assert_eq!(e.changed_fields, vec!["budget"]);
        // `{ set: x }` diturunkan langsung.
        let (plain, pending) = split_update_data(
            Model::StoreGroup,
            &Row::new().with("name", json!({ "set": "X" })),
        );
        assert_eq!(plain.get("name"), Some(&json!("X")));
        assert!(pending.is_empty());
    }

    #[test]
    fn update_many_over_threshold_or_failed_is_one_summary() {
        let rows: Vec<Row> = (1..=501)
            .map(|i| Row::new().with("id", i).with("companyId", 7))
            .collect();
        let pre = PreRead::rows(rows);
        let w = json!({ "companyId": 7, "qrToken": "s" });
        let e = only(build_update_many(
            Model::Store,
            &pre,
            501,
            &Row::new(),
            &[],
            &HashMap::new(),
            &w,
        ));
        assert_eq!(e.sub_entity.as_deref(), Some("bulk"));
        assert_eq!(e.company_id, Some(7));
        let after = e.after.unwrap_or_default();
        assert_eq!(
            after.get("sampleIds"),
            Some(&json!([1, 2, 3, 4, 5, 6, 7, 8, 9, 10]))
        );
        assert_eq!(after.get("where"), Some(&json!({ "companyId": 7 })));
        let e = only(build_update_many(
            Model::Store,
            &PreRead::Failed,
            2,
            &Row::new(),
            &[],
            &HashMap::new(),
            &w,
        ));
        assert_eq!(e.company_id, Some(7));
        assert!(
            build_update_many(
                Model::Store,
                &PreRead::Failed,
                0,
                &Row::new(),
                &[],
                &HashMap::new(),
                &w
            )
            .is_empty()
        );
    }

    #[test]
    fn hard_delete_records_before_but_is_not_restorable() {
        let e = only(build_delete(
            Model::Store,
            &PreRead::rows(vec![store_row(true, None)]),
            None,
        ));
        assert_eq!(e.action, Action::Delete);
        assert!(e.before.is_some());
        assert_eq!(e.after, None);
        assert!(e.changed_fields.is_empty());
        assert!(!e.is_restorable);
        // Pra-baca gagal → baris hasil `delete` jadi penggantinya.
        let e = only(build_delete(
            Model::Store,
            &PreRead::Failed,
            Some(&store_row(true, None)),
        ));
        assert!(e.before.is_some());
        let built = build_delete_many(
            Model::Store,
            &PreRead::rows(vec![store_row(true, None), store_row(true, None)]),
            2,
            &json!({}),
        );
        assert_eq!(built.len(), 2);
    }

    #[test]
    fn role_status_zero_is_delete_but_never_restorable() {
        let before = Row::new()
            .with("id", 3)
            .with("uuid", "r3")
            .with("companyId", 7)
            .with("name", "SPV")
            .with("status", 1);
        let after = before.clone().with("status", 0);
        let e = only(build_update(
            Model::AccessLevel,
            &PreRead::rows(vec![before]),
            &after,
            false,
        ));
        assert_eq!(e.action, Action::Delete);
        assert!(!e.is_restorable);
        assert_eq!(e.entity_type, EntityType::Role);
    }

    #[test]
    fn employee_entry_leaves_company_empty() {
        let before = Row::new()
            .with("username", "a@b.c")
            .with("name", "A")
            .with("status", 2)
            .with("id", 5)
            .with("uuid", "u5");
        let after = before.clone().with("status", 1);
        let e = only(build_update(
            Model::Users,
            &PreRead::rows(vec![before]),
            &after,
            false,
        ));
        assert_eq!(e.company_id, None);
        assert_eq!(e.entity_type, EntityType::Employee);
        assert_eq!(e.changed_fields, vec!["status"]);
        assert_eq!(e.entity_label.as_deref(), Some("A"));
    }

    #[test]
    fn membership_keys_and_hydration() {
        let mut names = HashMap::new();
        names.insert(3, Some("Ani".to_owned()));
        names.insert(5, Some("Budi".to_owned()));
        let base = hydrate(
            MemberKind::UserRole,
            &["5\u{0}MEMBER".to_owned(), "3\u{0}LEADER".to_owned()],
            Some(&names),
        );
        assert_eq!(
            base,
            vec![
                json!({"id": 3, "name": "Ani", "role": "LEADER"}),
                json!({"id": 5, "name": "Budi", "role": "MEMBER"})
            ]
        );
        // MEMBER -> LEADER (cabut + tambah) terbaca sebagai perubahan.
        let removed = next_keys(
            MemberKind::UserRole,
            &base,
            &["5\u{0}MEMBER".to_owned()],
            MemberEffect::Remove,
        );
        let readded = next_keys(
            MemberKind::UserRole,
            &hydrate(MemberKind::UserRole, &removed, Some(&names)),
            &["5\u{0}LEADER".to_owned()],
            MemberEffect::Add,
        );
        let after = hydrate(MemberKind::UserRole, &readded, Some(&names));
        assert!(!same_members(MemberKind::UserRole, &base, &after));
        // Identik → sama.
        let same = next_keys(
            MemberKind::UserRole,
            &base,
            &["5\u{0}MEMBER".to_owned()],
            MemberEffect::Add,
        );
        assert!(same_members(
            MemberKind::UserRole,
            &base,
            &hydrate(MemberKind::UserRole, &same, Some(&names))
        ));
        // Nama gagal dibaca → anggota tanpa `name`.
        assert_eq!(
            hydrate(MemberKind::User, &["7".to_owned()], None),
            vec![json!({"id": 7})]
        );
        assert_eq!(
            hydrate(MemberKind::User, &["7".to_owned()], Some(&HashMap::new())),
            vec![json!({"id": 7, "name": null})]
        );
    }

    #[test]
    fn grant_members_are_composite() {
        let keys = vec!["users\u{0}WRITE".to_owned(), "users\u{0}READ".to_owned()];
        let members = hydrate(MemberKind::Grant, &keys, None);
        assert_eq!(
            members,
            vec![
                json!({"featureKey": "users", "action": "READ"}),
                json!({"featureKey": "users", "action": "WRITE"})
            ]
        );
        let row = Row::new()
            .with("featureKey", "users")
            .with("action", "WRITE");
        let next = next_keys(
            MemberKind::Grant,
            &members,
            &[key_of_row(MemberKind::Grant, &row).unwrap_or_default()],
            MemberEffect::Remove,
        );
        assert_eq!(
            hydrate(MemberKind::Grant, &next, None),
            vec![json!({"featureKey": "users", "action": "READ"})]
        );
    }

    #[test]
    fn user_rule_keys_include_limit() {
        let mut names = HashMap::new();
        names.insert(4, Some("Cici".to_owned()));
        let before = hydrate(
            MemberKind::UserRule,
            &["4\u{0}360".to_owned()],
            Some(&names),
        );
        assert_eq!(
            before,
            vec![json!({"id": 4, "name": "Cici", "limitMinutes": 360})]
        );
        let always = Row::new()
            .with("userId", 4)
            .with("limitMinutes", Value::Null);
        assert_eq!(
            key_of_row(MemberKind::UserRule, &always).as_deref(),
            Some("4\u{0}always")
        );
        let after = hydrate(
            MemberKind::UserRule,
            &["4\u{0}480".to_owned()],
            Some(&names),
        );
        assert!(!same_members(MemberKind::UserRule, &before, &after));
    }
}
