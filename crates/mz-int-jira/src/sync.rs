//! Sinkronisasi issue Jira → tabel `task` (`syncIssuesInternal`, `upsertIssuePage`,
//! `buildTaskData`, `handleWebhookEvent`).

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use chrono::NaiveDateTime;
use mz_core::AppState;
use mz_core::time::now_naive;
use reqwest::Method;
use serde_json::{Value, json};
use sqlx::types::Json;
use sqlx::{Postgres, QueryBuilder};

use crate::http::{self, Error, HttpErr, Res};
use crate::util::{js_string, parse_jira_datetime, path, truthy};

const SEARCH_PAGE_SIZE: u32 = 100;
/// Batas aman paginasi (100 × 500 = 50.000 issue per project per sync).
const SEARCH_MAX_PAGES: usize = 500;
/// Toleransi selisih jam server Jira vs server kita.
const SYNC_CLOCK_SKEW_MS: i64 = 5 * 60 * 1000;
const SYNC_TX_TIMEOUT: Duration = Duration::from_secs(30);
const FIELDS: &str = "summary,description,status,priority,duedate,assignee,labels,issuetype,timeoriginalestimate,project,updated";

/// Hasil `syncIssuesInternal` (urutan kunci = objek literal Node).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct SyncCounts {
    pub created: u64,
    pub updated: u64,
    pub skipped: u64,
    pub total: u64,
}

/// `extractDescription(adf)`.
pub(crate) fn extract_description(adf: Option<&Value>) -> Option<String> {
    let adf = adf.filter(|v| truthy(Some(v)))?;
    if let Value::String(s) = adf {
        return Some(s.clone());
    }
    if !truthy(adf.get("content")) {
        return None;
    }
    fn traverse(node: &Value, out: &mut Vec<String>) {
        if node.get("type").and_then(Value::as_str) == Some("text")
            && let Some(t) = node.get("text").filter(|t| truthy(Some(t)))
        {
            out.push(js_string(t));
        }
        if let Some(Value::Array(children)) = node.get("content") {
            for c in children {
                traverse(c, out);
            }
        }
    }
    let mut texts = Vec::new();
    traverse(adf, &mut texts);
    let joined = texts.join(" ");
    (!joined.is_empty()).then_some(joined)
}

/// Data tulis satu task (`buildTaskData`).
#[derive(Debug, Clone)]
pub(crate) struct TaskData {
    pub project_id: i32,
    pub company_id: i32,
    pub external_url: Option<String>,
    pub external_meta: Value,
    pub name: String,
    pub description: Option<String>,
    pub status: Option<String>,
    pub priority: Option<i32>,
    pub priority_label: Option<String>,
    pub due_date: Option<NaiveDateTime>,
    pub time_estimate: Option<i32>,
    pub assignees: Value,
    pub tags: Value,
    pub now: NaiveDateTime,
}

fn opt_str(v: Option<&Value>) -> Option<String> {
    v.map(js_string)
}

/// `buildTaskData(issue, projectId, companyId, jiraProjectId, siteUrl)`.
pub(crate) fn build_task_data(
    issue: &Value,
    project_id: i32,
    company_id: i32,
    jira_project_id: &str,
    site_url: Option<&str>,
) -> TaskData {
    let fields = issue.get("fields").unwrap_or(&Value::Null);
    let assignees = match path(fields, &["assignee"]) {
        Some(a) => json!([{
            "externalUserId": a.get("accountId").cloned().unwrap_or(Value::Null),
            "username": path(a, &["displayName"]).cloned().unwrap_or(Value::Null),
            "email": path(a, &["emailAddress"]).cloned().unwrap_or(Value::Null),
        }]),
        None => json!([]),
    };
    let priority_label = opt_str(path(fields, &["priority", "name"]));
    let priority = priority_label.as_deref().and_then(|l| match l {
        "Highest" => Some(1),
        "High" => Some(2),
        "Medium" => Some(3),
        "Low" => Some(4),
        "Lowest" => Some(5),
        _ => None,
    });
    let key = issue.get("key").map(js_string).unwrap_or_default();
    let time_estimate = fields
        .get("timeoriginalestimate")
        .filter(|v| truthy(Some(v)))
        .and_then(|v| match v {
            Value::Number(n) => n.as_f64(),
            Value::String(s) => s.trim().parse().ok(),
            _ => None,
        })
        .and_then(|f| {
            #[allow(clippy::cast_possible_truncation)]
            let i = f as i64;
            i32::try_from(i).ok()
        });
    TaskData {
        project_id,
        company_id,
        external_url: site_url
            .filter(|s| !s.is_empty())
            .map(|s| format!("{s}/browse/{key}")),
        external_meta: json!({
            "issueKey": issue.get("key").cloned().unwrap_or(Value::Null),
            "issueType": path(fields, &["issuetype", "name"]).cloned().unwrap_or(Value::Null),
            "projectKey": path(fields, &["project", "key"]).cloned().unwrap_or(Value::Null),
            "projectId": jira_project_id,
        }),
        name: path(fields, &["summary"]).map_or_else(|| "(no name)".to_owned(), js_string),
        description: extract_description(fields.get("description")),
        status: opt_str(path(fields, &["status", "name"])),
        priority,
        priority_label,
        due_date: fields
            .get("duedate")
            .filter(|v| truthy(Some(v)))
            .and_then(|v| mz_core::time::parse_js_date(&js_string(v))),
        time_estimate,
        assignees,
        tags: path(fields, &["labels"])
            .cloned()
            .unwrap_or_else(|| json!([])),
        now: now_naive(),
    }
}

/// `SET` untuk `task.update({ data: buildTaskData(...) })`.
fn push_update_sets(qb: &mut QueryBuilder<'_, Postgres>, d: &TaskData) {
    qb.push(r#""projectId" = "#).push_bind(d.project_id);
    qb.push(r#", "companyId" = "#).push_bind(d.company_id);
    qb.push(r#", source = 'jira', "externalUrl" = "#)
        .push_bind(d.external_url.clone());
    qb.push(r#", "externalMeta" = "#)
        .push_bind(Json(d.external_meta.clone()));
    qb.push(", name = ").push_bind(d.name.clone());
    qb.push(", description = ").push_bind(d.description.clone());
    qb.push(", status = ").push_bind(d.status.clone());
    qb.push(", priority = ").push_bind(d.priority);
    qb.push(r#", "priorityLabel" = "#)
        .push_bind(d.priority_label.clone());
    qb.push(r#", "dueDate" = "#).push_bind(d.due_date);
    qb.push(r#", "startDate" = NULL, "timeEstimate" = "#)
        .push_bind(d.time_estimate);
    qb.push(", assignees = ")
        .push_bind(Json(d.assignees.clone()));
    qb.push(", tags = ").push_bind(Json(d.tags.clone()));
    qb.push(r#", "lastSyncedAt" = "#).push_bind(d.now);
    qb.push(r#", "updatedAt" = "#).push_bind(d.now);
}

const INSERT_COLS: &str = r#"INSERT INTO task ("projectId", "companyId", source, "externalId",
    "externalUrl", "externalMeta", name, description, status, priority, "priorityLabel",
    "dueDate", "startDate", "timeEstimate", assignees, tags, "lastSyncedAt", "updatedAt") "#;

fn push_insert_row(
    mut b: sqlx::query_builder::Separated<'_, '_, Postgres, &'static str>,
    d: &TaskData,
    external_id: &str,
) {
    b.push_bind(d.project_id)
        .push_bind(d.company_id)
        .push("'jira'")
        .push_bind(external_id.to_owned())
        .push_bind(d.external_url.clone())
        .push_bind(Json(d.external_meta.clone()))
        .push_bind(d.name.clone())
        .push_bind(d.description.clone())
        .push_bind(d.status.clone())
        .push_bind(d.priority)
        .push_bind(d.priority_label.clone())
        .push_bind(d.due_date)
        .push("NULL")
        .push_bind(d.time_estimate)
        .push_bind(Json(d.assignees.clone()))
        .push_bind(Json(d.tags.clone()))
        .push_bind(d.now)
        .push_bind(d.now);
}

#[derive(sqlx::FromRow)]
struct Existing {
    id: i32,
    #[sqlx(rename = "externalId")]
    external_id: Option<String>,
    #[sqlx(rename = "projectId")]
    project_id: i32,
    #[sqlx(rename = "lastSyncedAt")]
    last_synced_at: Option<NaiveDateTime>,
}

/// `upsertIssuePage(...)` dalam satu transaksi (timeout 30 dtk).
async fn upsert_issue_page(
    state: &AppState,
    issues: &[&Value],
    company_id: i32,
    project_id: i32,
    jira_project_id: &str,
    site_url: Option<&str>,
) -> Res<(u64, u64, u64)> {
    let fut = async {
        let mut ext_ids: Vec<String> = Vec::new();
        for i in issues {
            let id = js_string(i.get("id").unwrap_or(&Value::Null));
            if !ext_ids.contains(&id) {
                ext_ids.push(id);
            }
        }
        let mut tx = state.db.begin().await?;
        let rows: Vec<Existing> = sqlx::query_as(
            r#"SELECT id, "externalId", "projectId", "lastSyncedAt" FROM task
               WHERE "companyId" = $1 AND source = 'jira' AND "externalId" = ANY($2)"#,
        )
        .bind(company_id)
        .bind(&ext_ids)
        .fetch_all(&mut *tx)
        .await?;
        let mut by_ext: HashMap<String, Existing> = HashMap::new();
        for r in rows {
            if let Some(ext) = r.external_id.clone() {
                by_ext.entry(ext).or_insert(r);
            }
        }

        let mut to_create: Vec<(String, TaskData)> = Vec::new();
        let mut to_update: Vec<(i32, TaskData)> = Vec::new();
        let mut skipped = 0_u64;
        let mut seen: HashSet<String> = HashSet::new();
        for issue in issues {
            let ext = js_string(issue.get("id").unwrap_or(&Value::Null));
            if !seen.insert(ext.clone()) {
                continue;
            }
            let Some(existing) = by_ext.get(&ext) else {
                let d = build_task_data(issue, project_id, company_id, jira_project_id, site_url);
                to_create.push((ext, d));
                continue;
            };
            let jira_updated = path(issue, &["fields", "updated"])
                .filter(|v| truthy(Some(v)))
                .and_then(|v| parse_jira_datetime(&js_string(v)));
            let unchanged = existing.project_id == project_id
                && match (existing.last_synced_at, jira_updated) {
                    (Some(last), Some(upd)) => {
                        upd.and_utc().timestamp_millis()
                            < last.and_utc().timestamp_millis() - SYNC_CLOCK_SKEW_MS
                    }
                    _ => false,
                };
            if unchanged {
                skipped += 1;
                continue;
            }
            to_update.push((
                existing.id,
                build_task_data(issue, project_id, company_id, jira_project_id, site_url),
            ));
        }

        let mut created = 0_u64;
        if !to_create.is_empty() {
            let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(INSERT_COLS);
            qb.push_values(&to_create, |b, (ext, d)| push_insert_row(b, d, ext));
            qb.push(" ON CONFLICT DO NOTHING");
            created = qb.build().execute(&mut *tx).await?.rows_affected();
        }
        for (id, d) in &to_update {
            let mut qb: QueryBuilder<Postgres> = QueryBuilder::new("UPDATE task SET ");
            push_update_sets(&mut qb, d);
            qb.push(" WHERE id = ").push_bind(*id);
            qb.build().execute(&mut *tx).await?;
        }
        tx.commit().await?;
        Ok::<_, Error>((created, to_update.len() as u64, skipped))
    };
    tokio::time::timeout(SYNC_TX_TIMEOUT, fut)
        .await
        .unwrap_or_else(|_| {
            Err(Error::Msg(
                "Transaction already closed: A query cannot be executed on an expired transaction. The timeout for this transaction was 30000 ms.".to_owned(),
            ))
        })
}

/// Parameter `syncIssuesInternal`.
#[derive(Debug, Clone)]
pub(crate) struct SyncTarget<'a> {
    pub company_id: i32,
    pub project_id: i32,
    pub jira_project_id: &'a str,
    pub jira_project_key: &'a str,
    pub site_url: Option<&'a str>,
    pub access_token: &'a str,
    pub cloud_id: &'a str,
}

/// JQL `syncIssuesInternal`.
pub(crate) fn build_jql(project_key: &str, include_closed: bool, incremental: bool) -> String {
    let mut conds = vec![format!("project = \"{project_key}\"")];
    if !include_closed {
        conds.push("statusCategory != Done".to_owned());
    }
    if incremental {
        conds.push("updated >= \"-25h\"".to_owned());
    }
    let order = if incremental {
        "ORDER BY updated ASC"
    } else {
        "ORDER BY created DESC"
    };
    format!("{} {order}", conds.join(" AND "))
}

/// `syncIssuesInternal(...)`.
pub(crate) async fn sync_issues_internal(
    state: &AppState,
    t: &SyncTarget<'_>,
    include_closed: bool,
    incremental: bool,
) -> Res<SyncCounts> {
    let jql = build_jql(t.jira_project_key, include_closed, incremental);
    let url = http::jira_url(state, t.cloud_id, "/search/jql");
    let mut counts = SyncCounts::default();
    let mut next_page_token: Option<String> = None;
    for page in 0..SEARCH_MAX_PAGES {
        let mut params: Vec<(&str, String)> = vec![
            ("jql", jql.clone()),
            ("maxResults", SEARCH_PAGE_SIZE.to_string()),
            ("fields", FIELDS.to_owned()),
        ];
        if let Some(tok) = &next_page_token {
            params.push(("nextPageToken", tok.clone()));
        }
        let data =
            match http::send(http::bearer(state, Method::GET, &url, t.access_token).query(&params))
                .await
            {
                Ok(d) => d,
                Err(err) if err.status() == Some(410) => {
                    let detail = match &err {
                        HttpErr::Status { data, .. } => data
                            .get("errorMessages")
                            .and_then(Value::as_array)
                            .map(|a| a.iter().map(js_string).collect::<Vec<_>>().join("; ")),
                        _ => None,
                    }
                    .unwrap_or_else(|| err.message());
                    return Err(Error::Msg(format!(
                        "Jira sync failed for project '{}' (410 Gone): {detail}",
                        t.jira_project_key
                    )));
                }
                Err(err) => return Err(err.into()),
            };
        let issues: Vec<&Value> = data
            .get("issues")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter(|i| truthy(i.get("id")) && truthy(i.get("fields")))
                    .collect()
            })
            .unwrap_or_default();
        if !issues.is_empty() {
            let (c, u, s) = upsert_issue_page(
                state,
                &issues,
                t.company_id,
                t.project_id,
                t.jira_project_id,
                t.site_url,
            )
            .await?;
            counts.created += c;
            counts.updated += u;
            counts.skipped += s;
            counts.total += issues.len() as u64;
        }
        next_page_token = path(&data, &["nextPageToken"])
            .filter(|v| truthy(Some(v)))
            .map(js_string);
        if data.get("isLast") == Some(&Value::Bool(true)) || next_page_token.is_none() {
            break;
        }
        if page == SEARCH_MAX_PAGES - 1 {
            tracing::warn!(
                "Jira sync for project '{}' (company {}) hit the {SEARCH_MAX_PAGES}-page cap; remaining issues not synced",
                t.jira_project_key,
                t.company_id
            );
        }
    }
    Ok(counts)
}

/// `handleWebhookEvent(secret, payload)`.
pub(crate) async fn handle_webhook_event(
    state: &AppState,
    secret: &str,
    payload: &Value,
) -> Res<()> {
    if secret.is_empty() {
        return Ok(());
    }
    let company: Option<(i32, Option<String>)> = sqlx::query_as(
        r#"SELECT id, "jiraSiteUrl" FROM company WHERE "jiraWebhookSecret" = $1 LIMIT 1"#,
    )
    .bind(secret)
    .fetch_optional(&state.db)
    .await?;
    let Some((company_id, site_url)) = company else {
        return Ok(());
    };
    let event = path(payload, &["webhookEvent"])
        .map(js_string)
        .unwrap_or_default();
    let Some(issue) = payload.get("issue").filter(|i| truthy(i.get("id"))) else {
        return Ok(());
    };
    let ext_id = js_string(issue.get("id").unwrap_or(&Value::Null));
    let now = now_naive();

    if event == "jira:issue_deleted" {
        sqlx::query(
            r#"UPDATE task SET status = 'Deleted', "lastSyncedAt" = $3, "updatedAt" = $3
               WHERE "externalId" = $1 AND "companyId" = $2 AND source = 'jira'"#,
        )
        .bind(&ext_id)
        .bind(company_id)
        .bind(now)
        .execute(&state.db)
        .await?;
        return Ok(());
    }
    if event != "jira:issue_created" && event != "jira:issue_updated" {
        return Ok(());
    }
    let Some(jira_project_id) = path(issue, &["fields", "project", "id"])
        .filter(|v| truthy(Some(v)))
        .map(js_string)
    else {
        return Ok(());
    };
    let mapping: Option<i32> = sqlx::query_scalar(
        r#"SELECT "projectId" FROM "projectJiraMapping"
           WHERE "jiraProjectId" = $1 AND "companyId" = $2 AND status = 1 LIMIT 1"#,
    )
    .bind(&jira_project_id)
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?;
    let Some(project_id) = mapping else {
        return Ok(());
    };
    let data = build_task_data(
        issue,
        project_id,
        company_id,
        &jira_project_id,
        site_url.as_deref(),
    );
    let existing: Option<i32> = sqlx::query_scalar(
        r#"SELECT id FROM task WHERE "externalId" = $1 AND "companyId" = $2 AND source = 'jira'
           LIMIT 1"#,
    )
    .bind(&ext_id)
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?;
    if let Some(id) = existing {
        let mut qb: QueryBuilder<Postgres> = QueryBuilder::new("UPDATE task SET ");
        push_update_sets(&mut qb, &data);
        qb.push(" WHERE id = ").push_bind(id);
        qb.build().execute(&state.db).await?;
    } else if event == "jira:issue_created" {
        let mut qb: QueryBuilder<Postgres> = QueryBuilder::new(INSERT_COLS);
        qb.push_values(std::iter::once(&data), |b, d| {
            push_insert_row(b, d, &ext_id)
        });
        qb.build().execute(&state.db).await?;
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn description_from_adf() {
        let adf = json!({"type":"doc","content":[
            {"type":"paragraph","content":[{"type":"text","text":"Hello"},{"type":"text","text":"world"}]},
            {"type":"paragraph","content":[{"type":"text","text":""}]}
        ]});
        assert_eq!(
            extract_description(Some(&adf)).as_deref(),
            Some("Hello world")
        );
        assert_eq!(
            extract_description(Some(&json!("plain"))).as_deref(),
            Some("plain")
        );
        assert_eq!(extract_description(Some(&json!({"type":"doc"}))), None);
        assert_eq!(extract_description(Some(&json!({"content":[]}))), None);
        assert_eq!(extract_description(None), None);
    }

    #[test]
    fn task_data_shape() {
        let issue = json!({"id":"100","key":"ENG-1","fields":{
            "summary":"Fix","priority":{"name":"High"},"duedate":"2026-04-30",
            "assignee":{"accountId":"acc","displayName":"John"},
            "labels":["a"],"timeoriginalestimate":3600,
            "issuetype":{"name":"Bug"},"project":{"id":"10000","key":"ENG"},
            "status":{"name":"To Do"}}});
        let d = build_task_data(&issue, 1, 2, "10000", Some("https://x.atlassian.net"));
        assert_eq!(
            d.external_url.as_deref(),
            Some("https://x.atlassian.net/browse/ENG-1")
        );
        assert_eq!(d.priority, Some(2));
        assert_eq!(d.priority_label.as_deref(), Some("High"));
        assert_eq!(d.time_estimate, Some(3600));
        assert_eq!(d.due_date.unwrap().to_string(), "2026-04-30 00:00:00");
        assert_eq!(
            d.assignees,
            json!([{"externalUserId":"acc","username":"John","email":null}])
        );
        assert_eq!(d.external_meta["projectKey"], json!("ENG"));
        let bare = build_task_data(&json!({"id":1,"key":"K-1","fields":{}}), 1, 2, "9", None);
        assert_eq!(bare.name, "(no name)");
        assert_eq!(bare.external_url, None);
        assert_eq!(bare.priority, None);
        assert_eq!(bare.tags, json!([]));
    }

    #[test]
    fn jql_variants() {
        assert_eq!(
            build_jql("ENG", true, false),
            r#"project = "ENG" ORDER BY created DESC"#
        );
        assert_eq!(
            build_jql("ENG", false, true),
            r#"project = "ENG" AND statusCategory != Done AND updated >= "-25h" ORDER BY updated ASC"#
        );
    }
}
