//! DTO `src/roles/dto/*` + ValidationPipe (`transform: true`, tanpa `whitelist`).
//!
//! Field dibaca mentah (`serde_json::Value`) supaya tiga keadaan class-validator terbedakan:
//! tidak ada (`undefined`), `null`, dan nilai. Pesan & urutannya disalin dari keluaran Node
//! sungguhan (dekorator dievaluasi dari bawah ke atas; `ValidateNested` sesudah validator biasa;
//! pohon galat diratakan seperti `ValidationPipe.flattenValidationErrors`).

use mz_core::enums::{BaseRole, PermissionAction};
use mz_core::extract::{Validate, validator_js_length};
use serde::{Deserialize, Deserializer};
use serde_json::Value;

use crate::escalation::PermissionInput;
use mz_core::feature_registry::feature_keys;

/// `Some(Value::Null)` untuk `null` eksplisit, `None` untuk kunci yang tidak ada.
fn present<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(d).map(Some)
}

/// Nilai field opsional sesudah validasi: tidak dikirim / `null` / nilai.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Field<T> {
    Absent,
    Null,
    Set(T),
}

impl<T> Field<T> {
    /// `dto.x !== undefined`.
    pub fn is_defined(&self) -> bool {
        !matches!(self, Field::Absent)
    }
}

// ── pemeriksa gaya class-validator (`v = None` = undefined) ───────────────────

fn is_nullish(v: Option<&Value>) -> bool {
    matches!(v, None | Some(Value::Null))
}

fn as_str(v: Option<&Value>) -> Option<&str> {
    v.and_then(Value::as_str)
}

/// `@IsString() @IsNotEmpty() @MaxLength(n)` (pesan: MaxLength, IsNotEmpty, IsString).
fn check_string(
    out: &mut Vec<String>,
    field: &str,
    v: Option<&Value>,
    max: usize,
    not_empty: bool,
) {
    let s = as_str(v);
    if s.is_none_or(|s| validator_js_length(s) > max) {
        out.push(format!(
            "{field} must be shorter than or equal to {max} characters"
        ));
    }
    if not_empty && (is_nullish(v) || s == Some("")) {
        out.push(format!("{field} should not be empty"));
    }
    if s.is_none() {
        out.push(format!("{field} must be a string"));
    }
}

const BASE_ROLE_VALUES: &str = "ADMIN, MANAGER, STAFF";
const ACTION_VALUES: &str = "READ, WRITE, DELETE";

fn check_base_role(out: &mut Vec<String>, v: Option<&Value>) {
    if as_str(v).and_then(|s| s.parse::<BaseRole>().ok()).is_none() {
        out.push(format!(
            "baseRole must be one of the following values: {BASE_ROLE_VALUES}"
        ));
    }
}

fn check_uuid(out: &mut Vec<String>, field: &str, v: Option<&Value>) {
    if !as_str(v).is_some_and(mz_core::uuid_rules::is_uuid) {
        out.push(format!("{field} must be a UUID"));
    }
}

/// Satu simpul `ValidationError` (properti, constraint, anak).
#[derive(Debug, Default)]
struct VErr {
    property: String,
    constraints: Vec<String>,
    children: Vec<VErr>,
}

impl VErr {
    fn new(property: impl Into<String>) -> Self {
        Self {
            property: property.into(),
            ..Self::default()
        }
    }
    fn is_empty(&self) -> bool {
        self.constraints.is_empty() && self.children.is_empty()
    }
}

/// `mapChildrenToValidationErrors` + `prependConstraintsWithParentProp` Nest.
fn flatten(err: &VErr, parent: Option<&str>, out: &mut Vec<String>) {
    if err.children.is_empty() {
        out.extend(err.constraints.iter().cloned());
        return;
    }
    let path = match parent {
        Some(p) => format!("{p}.{}", err.property),
        None => err.property.clone(),
    };
    for item in &err.children {
        if !item.children.is_empty() {
            let mut nested = Vec::new();
            flatten_children(item, &path, &mut nested);
            out.extend(nested);
        }
        out.extend(item.constraints.iter().map(|c| format!("{path}.{c}")));
    }
}

/// Rekursi `mapChildrenToValidationErrors(item, parentPath)` untuk anak yang punya anak.
fn flatten_children(err: &VErr, parent: &str, out: &mut Vec<String>) {
    let path = format!("{parent}.{}", err.property);
    for item in &err.children {
        if !item.children.is_empty() {
            flatten_children(item, &path, out);
        }
        out.extend(item.constraints.iter().map(|c| format!("{path}.{c}")));
    }
}

/// `RolePermissionInputDto` atas satu objek.
fn validate_permission_object(obj: &serde_json::Map<String, Value>, into: &mut Vec<VErr>) {
    let fk = obj.get("featureKey");
    let mut e = VErr::new("featureKey");
    let fk_str = as_str(fk);
    if !fk_str.is_some_and(|k| feature_keys().any(|x| x == k)) {
        let all: Vec<&str> = feature_keys().collect();
        e.constraints.push(format!(
            "featureKey must be one of the following values: {}",
            all.join(", ")
        ));
    }
    if fk_str.is_none() {
        e.constraints.push("featureKey must be a string".into());
    }
    if !e.is_empty() {
        into.push(e);
    }

    let actions = obj.get("actions");
    let mut e = VErr::new("actions");
    let valid_action = |v: &Value| {
        v.as_str()
            .is_some_and(|s| s.parse::<PermissionAction>().is_ok())
    };
    let each_ok = match actions {
        Some(Value::Array(items)) => items.iter().all(valid_action),
        Some(v) => valid_action(v),
        None => false,
    };
    if !each_ok {
        e.constraints.push(format!(
            "each value in actions must be one of the following values: {ACTION_VALUES}"
        ));
    }
    if !matches!(actions, Some(Value::Array(items)) if !items.is_empty()) {
        e.constraints.push("actions should not be empty".into());
    }
    if !matches!(actions, Some(Value::Array(_))) {
        e.constraints.push("actions must be an array".into());
    }
    if !e.is_empty() {
        into.push(e);
    }
}

const NESTED_MSG: &str = "each value in nested property permissions must be either object or array";

/// `nestedValidations` untuk satu nilai di dalam array (`performValidations` → anak `index`).
fn nested_item(index: usize, value: &Value) -> VErr {
    let mut e = VErr::new(index.to_string());
    match value {
        Value::Array(items) => {
            for (i, sub) in items.iter().enumerate() {
                let child = nested_item(i, sub);
                if !child.is_empty() {
                    e.children.push(child);
                }
            }
        }
        Value::Object(obj) => validate_permission_object(obj, &mut e.children),
        _ => e.constraints.push(NESTED_MSG.into()),
    }
    e
}

/// `@IsArray() @ValidateNested({ each: true }) @Type(() => RolePermissionInputDto) permissions`.
fn check_permissions(out: &mut Vec<String>, v: Option<&Value>) {
    let mut e = VErr::new("permissions");
    if !matches!(v, Some(Value::Array(_))) {
        e.constraints.push("permissions must be an array".into());
    }
    match v {
        None => {}
        Some(Value::Array(items)) => {
            for (i, sub) in items.iter().enumerate() {
                let child = nested_item(i, sub);
                if !child.is_empty() {
                    e.children.push(child);
                }
            }
        }
        Some(Value::Object(obj)) => validate_permission_object(obj, &mut e.children),
        Some(_) => e.constraints.push(NESTED_MSG.into()),
    }
    if !e.is_empty() {
        flatten(&e, None, out);
    }
}

/// Bentuk tervalidasi `permissions[]` (aman: dipanggil hanya sesudah validasi lolos).
fn parse_permissions(v: Option<&Value>) -> Vec<PermissionInput> {
    let Some(Value::Array(items)) = v else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|item| {
            let feature_key = item.get("featureKey")?.as_str()?.to_owned();
            let actions = item
                .get("actions")?
                .as_array()?
                .iter()
                .filter_map(|a| a.as_str()?.parse().ok())
                .collect();
            Some(PermissionInput {
                feature_key,
                actions,
            })
        })
        .collect()
}

fn parse_base_role(v: Option<&Value>) -> Option<BaseRole> {
    as_str(v).and_then(|s| s.parse().ok())
}

fn field_string(v: Option<&Value>) -> Field<String> {
    match v {
        None => Field::Absent,
        Some(Value::String(s)) => Field::Set(s.clone()),
        Some(_) => Field::Null,
    }
}

// ── DTO ──────────────────────────────────────────────────────────────────────

/// `CreateRoleDto`.
#[derive(Debug, Deserialize)]
pub struct CreateRoleDto {
    #[serde(default, deserialize_with = "present")]
    name: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    description: Option<Value>,
    #[serde(default, rename = "baseRole", deserialize_with = "present")]
    base_role: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    permissions: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    code: Option<Value>,
}

impl Validate for CreateRoleDto {
    fn validate(&self) -> Vec<String> {
        let mut out = Vec::new();
        check_string(&mut out, "name", self.name.as_ref(), 255, true);
        if !is_nullish(self.description.as_ref()) {
            check_string(
                &mut out,
                "description",
                self.description.as_ref(),
                1000,
                false,
            );
        }
        check_base_role(&mut out, self.base_role.as_ref());
        if !is_nullish(self.permissions.as_ref()) {
            check_permissions(&mut out, self.permissions.as_ref());
        }
        out
    }
}

impl CreateRoleDto {
    pub fn has_code(&self) -> bool {
        self.code.is_some()
    }
    pub fn name(&self) -> String {
        as_str(self.name.as_ref()).unwrap_or_default().to_owned()
    }
    /// `dto.description ?? null`.
    pub fn description(&self) -> Option<String> {
        as_str(self.description.as_ref()).map(str::to_owned)
    }
    pub fn base_role(&self) -> Option<BaseRole> {
        parse_base_role(self.base_role.as_ref())
    }
    /// `dto.permissions ?? []`.
    pub fn permissions(&self) -> Vec<PermissionInput> {
        parse_permissions(self.permissions.as_ref())
    }
}

/// `UpdateRoleDto`.
#[derive(Debug, Deserialize)]
pub struct UpdateRoleDto {
    #[serde(default, deserialize_with = "present")]
    name: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    description: Option<Value>,
    #[serde(default, rename = "baseRole", deserialize_with = "present")]
    base_role: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    code: Option<Value>,
}

impl Validate for UpdateRoleDto {
    fn validate(&self) -> Vec<String> {
        let mut out = Vec::new();
        if !is_nullish(self.name.as_ref()) {
            check_string(&mut out, "name", self.name.as_ref(), 255, true);
        }
        if !is_nullish(self.description.as_ref()) {
            check_string(
                &mut out,
                "description",
                self.description.as_ref(),
                1000,
                false,
            );
        }
        if !is_nullish(self.base_role.as_ref()) {
            check_base_role(&mut out, self.base_role.as_ref());
        }
        out
    }
}

impl UpdateRoleDto {
    pub fn has_code(&self) -> bool {
        self.code.is_some()
    }
    pub fn name(&self) -> Field<String> {
        field_string(self.name.as_ref())
    }
    pub fn description(&self) -> Field<String> {
        field_string(self.description.as_ref())
    }
    pub fn base_role(&self) -> Field<BaseRole> {
        match self.base_role.as_ref() {
            None => Field::Absent,
            Some(v) => parse_base_role(Some(v)).map_or(Field::Null, Field::Set),
        }
    }
}

/// `CloneRoleDto`.
#[derive(Debug, Deserialize)]
pub struct CloneRoleDto {
    #[serde(default, deserialize_with = "present")]
    name: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    code: Option<Value>,
}

impl Validate for CloneRoleDto {
    fn validate(&self) -> Vec<String> {
        let mut out = Vec::new();
        check_string(&mut out, "name", self.name.as_ref(), 255, true);
        out
    }
}

impl CloneRoleDto {
    pub fn has_code(&self) -> bool {
        self.code.is_some()
    }
    pub fn name(&self) -> String {
        as_str(self.name.as_ref()).unwrap_or_default().to_owned()
    }
}

/// `PreviewRoleDto`.
#[derive(Debug, Deserialize)]
pub struct PreviewRoleDto {
    #[serde(default, rename = "baseRole", deserialize_with = "present")]
    base_role: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    permissions: Option<Value>,
    #[serde(default, rename = "roleUuid", deserialize_with = "present")]
    role_uuid: Option<Value>,
}

impl Validate for PreviewRoleDto {
    fn validate(&self) -> Vec<String> {
        let mut out = Vec::new();
        check_base_role(&mut out, self.base_role.as_ref());
        check_permissions(&mut out, self.permissions.as_ref());
        if !is_nullish(self.role_uuid.as_ref()) {
            check_uuid(&mut out, "roleUuid", self.role_uuid.as_ref());
        }
        out
    }
}

impl PreviewRoleDto {
    pub fn base_role(&self) -> Option<BaseRole> {
        parse_base_role(self.base_role.as_ref())
    }
    pub fn permissions(&self) -> Vec<PermissionInput> {
        parse_permissions(self.permissions.as_ref())
    }
    /// `if (dto.roleUuid)` — string kosong tidak mungkin lolos `@IsUUID`.
    pub fn role_uuid(&self) -> Option<String> {
        as_str(self.role_uuid.as_ref())
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    }
}

/// `ReplaceRolePermissionsDto` (dipakai `PUT :uuid/permissions` dan `PUT :uuid/customize`).
#[derive(Debug, Deserialize)]
pub struct ReplaceRolePermissionsDto {
    #[serde(default, deserialize_with = "present")]
    permissions: Option<Value>,
}

impl Validate for ReplaceRolePermissionsDto {
    fn validate(&self) -> Vec<String> {
        let mut out = Vec::new();
        check_permissions(&mut out, self.permissions.as_ref());
        out
    }
}

impl ReplaceRolePermissionsDto {
    pub fn permissions(&self) -> Vec<PermissionInput> {
        parse_permissions(self.permissions.as_ref())
    }
}

/// `DeleteRoleQueryDto` (`?reassignTo=<uuid>`).
#[derive(Debug, Deserialize)]
pub struct DeleteRoleQueryDto {
    #[serde(default, rename = "reassignTo", deserialize_with = "present")]
    reassign_to: Option<Value>,
}

impl Validate for DeleteRoleQueryDto {
    fn validate(&self) -> Vec<String> {
        let mut out = Vec::new();
        if !is_nullish(self.reassign_to.as_ref()) {
            check_uuid(&mut out, "reassignTo", self.reassign_to.as_ref());
        }
        out
    }
}

impl DeleteRoleQueryDto {
    pub fn reassign_to(&self) -> Option<String> {
        as_str(self.reassign_to.as_ref())
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
    }
}

#[cfg(test)]
#[allow(clippy::indexing_slicing)]
mod tests {
    use super::*;
    use serde::de::DeserializeOwned;
    use serde_json::json;

    fn msgs<T: DeserializeOwned + Validate>(body: Value) -> Vec<String> {
        serde_json::from_value::<T>(body)
            .map(|d| d.validate())
            .unwrap_or_else(|e| vec![format!("serde: {e}")])
    }

    const KEYS: &str = "users, projects, roles, clock, geofences, negative-zones, kyc, leaves, leave-config, timesheets, screenshots, activity-logs, manual-time-approvals, manual-time-settings, divisions, overtime, overtime-report, overtime-settings, stores, store-groups, store-visits, store-visit-plans, visit-masters, store-visit-forms, customers, customer-masters, products, product-masters, price-lists, order-masters, orders, order-approvals, order-deliveries, reports, audit-logs, billing, invoices, company-settings, integrations, calendar, table-fields, access-levels";

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| (*x).to_owned()).collect()
    }

    /// Golden: keluaran `ValidationPipe({ transform: true })` Node untuk DTO yang sama.
    #[test]
    fn create_messages_match_node() {
        assert_eq!(
            msgs::<CreateRoleDto>(json!({})),
            s(&[
                "name must be shorter than or equal to 255 characters",
                "name should not be empty",
                "name must be a string",
                "baseRole must be one of the following values: ADMIN, MANAGER, STAFF"
            ])
        );
        assert_eq!(
            msgs::<CreateRoleDto>(
                json!({"name": 5, "description": 7, "baseRole": "X", "permissions": "no"})
            ),
            s(&[
                "name must be shorter than or equal to 255 characters",
                "name must be a string",
                "description must be shorter than or equal to 1000 characters",
                "description must be a string",
                "baseRole must be one of the following values: ADMIN, MANAGER, STAFF",
                "permissions must be an array",
                "each value in nested property permissions must be either object or array"
            ])
        );
        assert_eq!(
            msgs::<CreateRoleDto>(json!({"name": "", "baseRole": "MANAGER"})),
            s(&["name should not be empty"])
        );
        assert_eq!(
            msgs::<CreateRoleDto>(
                json!({"name": null, "baseRole": null, "description": null, "permissions": null})
            ),
            s(&[
                "name must be shorter than or equal to 255 characters",
                "name should not be empty",
                "name must be a string",
                "baseRole must be one of the following values: ADMIN, MANAGER, STAFF"
            ])
        );
        assert_eq!(
            msgs::<CreateRoleDto>(
                json!({"name": "a", "baseRole": "MANAGER", "permissions": [1, "x", null]})
            ),
            vec![format!("permissions.{NESTED_MSG}"); 3]
        );
        assert_eq!(
            msgs::<CreateRoleDto>(json!({"name": "a", "baseRole": "MANAGER", "permissions": [{}]})),
            vec![
                format!("permissions.0.featureKey must be one of the following values: {KEYS}"),
                "permissions.0.featureKey must be a string".into(),
                "permissions.0.each value in actions must be one of the following values: READ, WRITE, DELETE".into(),
                "permissions.0.actions should not be empty".into(),
                "permissions.0.actions must be an array".into(),
            ]
        );
        assert_eq!(
            msgs::<CreateRoleDto>(
                json!({"name": "a", "baseRole": "MANAGER", "permissions": [{"featureKey": 3, "actions": "READ"}]})
            ),
            vec![
                format!("permissions.0.featureKey must be one of the following values: {KEYS}"),
                "permissions.0.featureKey must be a string".into(),
                "permissions.0.actions should not be empty".into(),
                "permissions.0.actions must be an array".into(),
            ]
        );
        assert_eq!(
            msgs::<CreateRoleDto>(json!({"name": "a", "baseRole": "MANAGER", "permissions": [
                {"featureKey": "nope", "actions": []},
                {"featureKey": "users", "actions": ["READ", "X"]}
            ]})),
            vec![
                format!("permissions.0.featureKey must be one of the following values: {KEYS}"),
                "permissions.0.actions should not be empty".into(),
                "permissions.1.each value in actions must be one of the following values: READ, WRITE, DELETE".into(),
            ]
        );
        assert_eq!(
            msgs::<CreateRoleDto>(
                json!({"name": "a", "baseRole": "MANAGER", "permissions": [[1]]})
            ),
            vec![format!("permissions.0.{NESTED_MSG}")]
        );
        assert_eq!(
            msgs::<CreateRoleDto>(
                json!({"name": "a", "baseRole": "MANAGER", "permissions": {"featureKey": "users", "actions": ["READ"]}})
            ),
            s(&["permissions must be an array"])
        );
        assert_eq!(
            msgs::<CreateRoleDto>(json!({"name": ["a"], "baseRole": ["MANAGER"]})),
            s(&[
                "name must be shorter than or equal to 255 characters",
                "name must be a string",
                "baseRole must be one of the following values: ADMIN, MANAGER, STAFF"
            ])
        );
        let ok: Option<CreateRoleDto> =
            serde_json::from_value(json!({"name": "a", "baseRole": "MANAGER", "code": null})).ok();
        assert!(
            ok.as_ref()
                .is_some_and(|d| d.validate().is_empty() && d.has_code())
        );
    }

    #[test]
    fn other_dtos_match_node() {
        assert!(msgs::<UpdateRoleDto>(json!({})).is_empty());
        assert!(msgs::<UpdateRoleDto>(json!({"name": null, "baseRole": null})).is_empty());
        assert_eq!(
            msgs::<UpdateRoleDto>(json!({"name": "", "description": 1, "baseRole": "GUEST"})),
            s(&[
                "name should not be empty",
                "description must be shorter than or equal to 1000 characters",
                "description must be a string",
                "baseRole must be one of the following values: ADMIN, MANAGER, STAFF"
            ])
        );
        assert_eq!(
            msgs::<CloneRoleDto>(json!({"name": "x".repeat(256)})),
            s(&["name must be shorter than or equal to 255 characters"])
        );
        assert_eq!(
            msgs::<PreviewRoleDto>(json!({})),
            s(&[
                "baseRole must be one of the following values: ADMIN, MANAGER, STAFF",
                "permissions must be an array"
            ])
        );
        assert_eq!(
            msgs::<PreviewRoleDto>(
                json!({"baseRole": "STAFF", "permissions": [], "roleUuid": "abc"})
            ),
            s(&["roleUuid must be a UUID"])
        );
        assert_eq!(
            msgs::<ReplaceRolePermissionsDto>(json!({"permissions": null})),
            s(&["permissions must be an array", NESTED_MSG])
        );
        assert_eq!(
            msgs::<DeleteRoleQueryDto>(json!({"reassignTo": ""})),
            s(&["reassignTo must be a UUID"])
        );
        assert_eq!(
            msgs::<DeleteRoleQueryDto>(json!({"reassignTo": ["a", "b"]})),
            s(&["reassignTo must be a UUID"])
        );
    }

    #[test]
    fn update_tri_state() {
        let d: Option<UpdateRoleDto> =
            serde_json::from_value(json!({"name": null, "baseRole": null})).ok();
        assert!(d.is_some());
        let Some(d) = d else { return };
        assert_eq!(d.name(), Field::Null);
        assert_eq!(d.base_role(), Field::Null);
        assert_eq!(d.description(), Field::Absent);
        assert!(!d.has_code());
    }
}
