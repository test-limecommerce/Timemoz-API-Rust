//! DTO `src/integrations/clickup/dto/*` + pesan class-validator (urutan = urutan
//! properti; dalam satu properti dekorator dibaca dari bawah ke atas).

use mz_core::extract::Validate;
use mz_core::iso8601::is_iso8601;
use serde::Deserialize;
use serde_json::Value;

use crate::util::str_to_number;

fn must_string(e: &mut Vec<String>, field: &str, v: Option<&Value>) {
    if !matches!(v, Some(Value::String(_))) {
        e.push(format!("{field} must be a string"));
    }
}

/// `@IsOptional() @IsString()`.
fn opt_string(e: &mut Vec<String>, field: &str, v: Option<&Value>) {
    if let Some(x) = v
        && !x.is_null()
        && !x.is_string()
    {
        e.push(format!("{field} must be a string"));
    }
}

fn str_of(v: Option<&Value>) -> String {
    v.and_then(Value::as_str).unwrap_or_default().to_owned()
}

fn opt_str_of(v: Option<&Value>) -> Option<String> {
    v.and_then(Value::as_str).map(str::to_owned)
}

/// `ChangeTaskStatusDto`.
#[derive(Debug, Deserialize)]
pub struct ChangeTaskStatusBody {
    status: Option<Value>,
}

impl Validate for ChangeTaskStatusBody {
    fn validate(&self) -> Vec<String> {
        let mut e = Vec::new();
        must_string(&mut e, "status", self.status.as_ref());
        e
    }
}

impl ChangeTaskStatusBody {
    pub fn status(&self) -> String {
        str_of(self.status.as_ref())
    }
}

/// `LinkProjectDto`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkProjectBody {
    project_uuid: Option<Value>,
    clickup_folder_id: Option<Value>,
    clickup_folder_name: Option<Value>,
    clickup_space_id: Option<Value>,
    clickup_space_name: Option<Value>,
}

impl Validate for LinkProjectBody {
    fn validate(&self) -> Vec<String> {
        let mut e = Vec::new();
        must_string(&mut e, "projectUuid", self.project_uuid.as_ref());
        must_string(&mut e, "clickupFolderId", self.clickup_folder_id.as_ref());
        must_string(
            &mut e,
            "clickupFolderName",
            self.clickup_folder_name.as_ref(),
        );
        must_string(&mut e, "clickupSpaceId", self.clickup_space_id.as_ref());
        must_string(&mut e, "clickupSpaceName", self.clickup_space_name.as_ref());
        e
    }
}

/// Isi `LinkProjectDto` yang sudah lolos validasi.
#[derive(Debug, Clone)]
pub struct LinkProject {
    pub project_uuid: String,
    pub clickup_folder_id: String,
    pub clickup_folder_name: String,
    pub clickup_space_id: String,
    pub clickup_space_name: String,
}

impl LinkProjectBody {
    pub fn into_dto(self) -> LinkProject {
        LinkProject {
            project_uuid: str_of(self.project_uuid.as_ref()),
            clickup_folder_id: str_of(self.clickup_folder_id.as_ref()),
            clickup_folder_name: str_of(self.clickup_folder_name.as_ref()),
            clickup_space_id: str_of(self.clickup_space_id.as_ref()),
            clickup_space_name: str_of(self.clickup_space_name.as_ref()),
        }
    }
}

/// `LinkUserDto`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkUserBody {
    user_uuid: Option<Value>,
    clickup_user_id: Option<Value>,
    clickup_username: Option<Value>,
    clickup_email: Option<Value>,
    clickup_avatar: Option<Value>,
}

impl Validate for LinkUserBody {
    fn validate(&self) -> Vec<String> {
        let mut e = Vec::new();
        must_string(&mut e, "userUuid", self.user_uuid.as_ref());
        must_string(&mut e, "clickupUserId", self.clickup_user_id.as_ref());
        opt_string(&mut e, "clickupUsername", self.clickup_username.as_ref());
        opt_string(&mut e, "clickupEmail", self.clickup_email.as_ref());
        opt_string(&mut e, "clickupAvatar", self.clickup_avatar.as_ref());
        e
    }
}

/// Isi `LinkUserDto` yang sudah lolos validasi.
#[derive(Debug, Clone)]
pub struct LinkUser {
    pub user_uuid: String,
    pub clickup_user_id: String,
    pub clickup_username: Option<String>,
    pub clickup_email: Option<String>,
    pub clickup_avatar: Option<String>,
}

impl LinkUserBody {
    pub fn into_dto(self) -> LinkUser {
        LinkUser {
            user_uuid: str_of(self.user_uuid.as_ref()),
            clickup_user_id: str_of(self.clickup_user_id.as_ref()),
            clickup_username: opt_str_of(self.clickup_username.as_ref()),
            clickup_email: opt_str_of(self.clickup_email.as_ref()),
            clickup_avatar: opt_str_of(self.clickup_avatar.as_ref()),
        }
    }
}

/// `null` tetap `Some(Value::Null)` (beda dengan kunci yang tidak ada).
fn present<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Value>, D::Error> {
    Value::deserialize(d).map(Some)
}

/// `SyncProjectsDto` (`lists: ClickupListItemDto[]`, `@IsArray @ValidateNested({each})`).
#[derive(Debug, Deserialize)]
pub struct SyncProjectsBody {
    #[serde(default, deserialize_with = "present")]
    lists: Option<Value>,
}

/// Properti `ClickupListItemDto` pada objek `o` (prefix = jalur induk).
fn item_messages(e: &mut Vec<String>, prefix: &str, o: &serde_json::Map<String, Value>) {
    if !matches!(o.get("clickupListId"), Some(Value::String(_))) {
        e.push(format!("{prefix}.clickupListId must be a string"));
    }
    if !matches!(o.get("name"), Some(Value::String(_))) {
        e.push(format!("{prefix}.name must be a string"));
    }
}

/// `ClickupListItemDto`.
#[derive(Debug, Clone)]
pub struct ListItem {
    pub clickup_list_id: String,
    pub name: String,
}

impl Validate for SyncProjectsBody {
    fn validate(&self) -> Vec<String> {
        let mut e = Vec::new();
        // customValidations (IsArray) dulu, lalu nestedValidations.
        match self.lists.as_ref() {
            Some(Value::Array(items)) => {
                for (i, item) in items.iter().enumerate() {
                    match item {
                        Value::Object(o) => item_messages(&mut e, &format!("lists.{i}"), o),
                        // `Array` juga lolos sebagai "object or array" di class-validator,
                        // propertinya tidak ada → dua pesan IsString.
                        Value::Array(_) => {
                            e.push(format!("lists.{i}.clickupListId must be a string"));
                            e.push(format!("lists.{i}.name must be a string"));
                        }
                        _ => e.push(format!(
                            "lists.each value in nested property {i} must be either object or array"
                        )),
                    }
                }
            }
            None => e.push("lists must be an array".into()),
            Some(Value::Object(o)) => {
                // Objek tunggal: IsArray gagal, lalu ValidateNested memvalidasi objek itu
                // sebagai `ClickupListItemDto`.
                e.push("lists must be an array".into());
                item_messages(&mut e, "lists", o);
            }
            Some(_) => {
                // Primitif atau `null` (nestedValidations hanya melewati `undefined`).
                e.push("lists must be an array".into());
                e.push("each value in nested property lists must be either object or array".into());
            }
        }
        e
    }
}

impl SyncProjectsBody {
    pub fn lists(&self) -> Vec<ListItem> {
        self.lists
            .as_ref()
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .map(|i| ListItem {
                        clickup_list_id: str_of(i.get("clickupListId")),
                        name: str_of(i.get("name")),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// `SyncTasksDto`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncTasksBody {
    include_closed: Option<Value>,
    include_subtasks: Option<Value>,
}

impl Validate for SyncTasksBody {
    fn validate(&self) -> Vec<String> {
        let mut e = Vec::new();
        for (field, v) in [
            ("includeClosed", &self.include_closed),
            ("includeSubtasks", &self.include_subtasks),
        ] {
            if let Some(x) = v
                && !x.is_null()
                && !x.is_boolean()
            {
                e.push(format!("{field} must be a boolean value"));
            }
        }
        e
    }
}

impl SyncTasksBody {
    /// `dto.includeClosed ?? true`, `dto.includeSubtasks ?? false`.
    pub fn flags(&self) -> (bool, bool) {
        (
            self.include_closed
                .as_ref()
                .and_then(Value::as_bool)
                .unwrap_or(true),
            self.include_subtasks
                .as_ref()
                .and_then(Value::as_bool)
                .unwrap_or(false),
        )
    }
}

use mz_core::qs::{QVal, get as q_get};

/// `GetProjectTasksDto` sesudah validasi.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectTasksQuery {
    pub search: Option<String>,
    pub status: Option<String>,
    pub assignee: Option<String>,
    pub start_date: Option<String>,
    pub end_date: Option<String>,
    pub page: i64,
    pub limit: i64,
}

fn q_string(e: &mut Vec<String>, m: &[(String, QVal)], k: &str) -> Option<String> {
    match q_get(m, k)? {
        QVal::One(s) => Some(s.clone()),
        QVal::Many(_) | QVal::Obj => {
            e.push(format!("{k} must be a string"));
            None
        }
    }
}

fn q_date(e: &mut Vec<String>, m: &[(String, QVal)], k: &str) -> Option<String> {
    match q_get(m, k)? {
        QVal::One(s) if is_iso8601(s) => Some(s.clone()),
        _ => {
            e.push(format!("{k} must be a valid ISO 8601 date string"));
            None
        }
    }
}

/// `@Type(() => Number) @IsInt() @Min(1) [@Max(n)]`; pesan: Max, Min, IsInt.
fn q_int(
    e: &mut Vec<String>,
    m: &[(String, QVal)],
    k: &str,
    default: i64,
    max: Option<f64>,
) -> i64 {
    let value = match q_get(m, k) {
        None => return default,
        Some(QVal::One(s)) => str_to_number(s),
        Some(QVal::Many(_) | QVal::Obj) => f64::NAN,
    };
    if let Some(max) = max
        && (value.is_nan() || value > max)
    {
        e.push(format!("{k} must not be greater than {max}"));
    }
    if value.is_nan() || value < 1.0 {
        e.push(format!("{k} must not be less than 1"));
    }
    if !(value.is_finite() && value.fract() == 0.0) {
        e.push(format!("{k} must be an integer number"));
    }
    #[allow(clippy::cast_possible_truncation)]
    let v = value.clamp(1.0, 9.0e15) as i64;
    v
}

/// Parse + validasi query `GET projects/:projectUuid/tasks`.
pub fn project_tasks_query(raw: Option<&str>) -> Result<ProjectTasksQuery, Vec<String>> {
    let m = mz_core::qs::parse(raw.unwrap_or_default());
    let mut e = Vec::new();
    let q = ProjectTasksQuery {
        search: q_string(&mut e, &m, "search"),
        status: q_string(&mut e, &m, "status"),
        assignee: q_string(&mut e, &m, "assignee"),
        start_date: q_date(&mut e, &m, "startDate"),
        end_date: q_date(&mut e, &m, "endDate"),
        page: q_int(&mut e, &m, "page", 1, None),
        limit: q_int(&mut e, &m, "limit", 20, Some(100.0)),
    };
    if e.is_empty() { Ok(q) } else { Err(e) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sync_projects(v: Value) -> Vec<String> {
        serde_json::from_value::<SyncProjectsBody>(v)
            .map(|b| b.validate())
            .unwrap_or_default()
    }

    #[test]
    fn sync_projects_messages() {
        assert_eq!(sync_projects(json!({})), vec!["lists must be an array"]);
        assert_eq!(
            sync_projects(json!({"lists": "x"})),
            vec![
                "lists must be an array",
                "each value in nested property lists must be either object or array"
            ]
        );
        assert_eq!(
            sync_projects(json!({"lists": [{"clickupListId": 1, "name": "a"}]})),
            vec!["lists.0.clickupListId must be a string"]
        );
        assert!(sync_projects(json!({"lists": [{"clickupListId": "1", "name": "a"}]})).is_empty());
        assert_eq!(sync_projects(json!({"lists": null})).len(), 2);
        assert_eq!(
            sync_projects(json!({"lists": {"name": "x"}})),
            vec![
                "lists must be an array",
                "lists.clickupListId must be a string"
            ]
        );
    }

    #[test]
    fn link_user_messages() {
        let b: LinkUserBody = serde_json::from_value(
            json!({"clickupUserId": 5, "clickupEmail": null, "clickupAvatar": 1}),
        )
        .unwrap();
        assert_eq!(
            b.validate(),
            vec![
                "userUuid must be a string",
                "clickupUserId must be a string",
                "clickupAvatar must be a string"
            ]
        );
    }

    #[test]
    fn sync_tasks_flags() {
        let b: Result<SyncTasksBody, _> = serde_json::from_value(json!({"includeClosed": "true"}));
        assert_eq!(
            b.map(|b| b.validate()).unwrap_or_default(),
            vec!["includeClosed must be a boolean value"]
        );
        let b: Result<SyncTasksBody, _> =
            serde_json::from_value(json!({"includeClosed": false, "includeSubtasks": null}));
        assert_eq!(b.map(|b| b.flags()).ok(), Some((false, false)));
    }

    #[test]
    fn project_tasks_query_parse() {
        let q = project_tasks_query(Some("search=bug&page=2&limit=5&startDate=2026-04-01"));
        assert_eq!(
            q,
            Ok(ProjectTasksQuery {
                search: Some("bug".into()),
                status: None,
                assignee: None,
                start_date: Some("2026-04-01".into()),
                end_date: None,
                page: 2,
                limit: 5,
            })
        );
        assert_eq!(
            project_tasks_query(Some("page=abc&limit=500&status=a&status=b&endDate=x")),
            Err(vec![
                "status must be a string".to_owned(),
                "endDate must be a valid ISO 8601 date string".to_owned(),
                "page must not be less than 1".to_owned(),
                "page must be an integer number".to_owned(),
                "limit must not be greater than 100".to_owned(),
            ])
        );
        assert_eq!(
            project_tasks_query(None).map(|q| (q.page, q.limit)),
            Ok((1, 20))
        );
    }
}
