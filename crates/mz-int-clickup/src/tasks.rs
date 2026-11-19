//! Sinkronisasi task ClickUp → tabel `task` (`buildTaskData`, `syncTasksInternal`,
//! `persistTaskPage`, `upsertSingleTask` di `clickup.service.ts:140-404`).

use std::time::Duration;

use chrono::NaiveDateTime;
use mz_core::AppState;
use mz_core::time::now_naive;
use serde::Serialize;
use serde_json::{Value, json};
use sqlx::types::Json;
use sqlx::{Postgres, QueryBuilder};

use crate::client::{Client, HttpErr};
use crate::util::{field, js_str, js_to_number, nn, opt_text, truthy};

/// `TASK_PAGE_TX_TIMEOUT_MS`.
const TASK_PAGE_TX_TIMEOUT: Duration = Duration::from_millis(30_000);

/// Galat yang dilempar (`throw`) — pemanggil memakai `message`-nya.
#[derive(Debug, Clone)]
pub(crate) struct Thrown(pub String);

impl From<sqlx::Error> for Thrown {
    fn from(e: sqlx::Error) -> Self {
        Self(e.to_string())
    }
}

impl From<HttpErr> for Thrown {
    fn from(e: HttpErr) -> Self {
        Self(e.message)
    }
}

/// Hasil `syncTasksInternal` (urutan kunci = objek Node).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct SyncTotals {
    pub created: u64,
    pub updated: u64,
    pub unchanged: u64,
    pub total: u64,
}

/// `buildTaskData(task, projectId, companyId, folderId)` (tanpa `externalId`).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TaskData {
    pub external_url: Option<String>,
    pub external_meta: Value,
    pub name: String,
    pub description: Option<String>,
    pub status: Option<String>,
    pub priority: Option<i32>,
    pub priority_label: Option<String>,
    pub due_date: Option<NaiveDateTime>,
    pub start_date: Option<NaiveDateTime>,
    pub time_estimate: Option<i32>,
    pub assignees: Value,
    pub tags: Value,
    pub now: NaiveDateTime,
}

/// `x ? new Date(Number(x)) : null` (Invalid Date → `None`).
fn ms_date(v: Option<&Value>) -> Option<NaiveDateTime> {
    if !truthy(v) {
        return None;
    }
    let ms = js_to_number(v);
    if !ms.is_finite() {
        return None;
    }
    #[allow(clippy::cast_possible_truncation)]
    chrono::DateTime::from_timestamp_millis(ms.trunc() as i64).map(|d| d.naive_utc())
}

fn array_of(v: Option<&Value>) -> &[Value] {
    nn(v).and_then(Value::as_array).map_or(&[], Vec::as_slice)
}

pub(crate) fn build_task_data(task: &Value, folder_id: &str, now: NaiveDateTime) -> TaskData {
    let t = Some(task);
    let assignees: Vec<Value> = array_of(field(t, "assignees"))
        .iter()
        .map(|a| {
            json!({
                "externalUserId": js_str(a.get("id")),
                "username": nn(a.get("username")).cloned().unwrap_or(Value::Null),
                "email": nn(a.get("email")).cloned().unwrap_or(Value::Null),
            })
        })
        .collect();
    // `t.name` undefined → `null` di dalam array JSON.
    let tags: Vec<Value> = array_of(field(t, "tags"))
        .iter()
        .map(|x| x.get("name").cloned().unwrap_or(Value::Null))
        .collect();

    let priority_label = opt_text(field(field(t, "priority"), "priority"));
    let priority = priority_label
        .as_deref()
        .filter(|l| !l.is_empty())
        .and_then(|l| match l {
            "urgent" => Some(1),
            "high" => Some(2),
            "normal" => Some(3),
            "low" => Some(4),
            _ => None,
        });

    let status = field(t, "status");
    let list = field(t, "list");
    let or_null = |v: Option<&Value>| nn(v).cloned().unwrap_or(Value::Null);
    let date_updated = nn(field(t, "date_updated")).map(|d| js_str(Some(d)));
    let external_meta = json!({
        "folderId": folder_id,
        "listId": or_null(field(list, "id")),
        "listName": or_null(field(list, "name")),
        "statusColor": or_null(field(status, "color")),
        "statusType": or_null(field(status, "type")),
        "dateUpdated": date_updated,
    });

    let time_estimate = if truthy(field(t, "time_estimate")) {
        let n = js_to_number(field(t, "time_estimate"));
        (n.is_finite() && n.fract() == 0.0 && n >= f64::from(i32::MIN) && n <= f64::from(i32::MAX))
            .then(|| {
                #[allow(clippy::cast_possible_truncation)]
                let v = n as i32;
                v
            })
    } else {
        None
    };

    TaskData {
        external_url: opt_text(field(t, "url")),
        external_meta,
        name: opt_text(field(t, "name")).unwrap_or_else(|| "(no name)".into()),
        description: opt_text(field(t, "description")),
        status: opt_text(field(status, "status")),
        priority,
        priority_label,
        due_date: ms_date(field(t, "due_date")),
        start_date: ms_date(field(t, "start_date")),
        time_estimate,
        assignees: Value::Array(assignees),
        tags: Value::Array(tags),
        now,
    }
}

/// Kolom tulis `task` (urutan sama dengan [`push_values`]).
const COLUMNS: &str = r#""projectId", "companyId", source, "externalUrl", "externalMeta", name, description, status, priority, "priorityLabel", "dueDate", "startDate", "timeEstimate", assignees, tags, "lastSyncedAt", "updatedAt""#;

fn push_values<'a>(
    b: &mut sqlx::query_builder::Separated<'_, 'a, Postgres, &'static str>,
    d: &'a TaskData,
    project_id: i32,
    company_id: i32,
) {
    b.push_bind(project_id)
        .push_bind(company_id)
        .push_bind("clickup")
        .push_bind(d.external_url.as_deref())
        .push_bind(Json(&d.external_meta))
        .push_bind(d.name.as_str())
        .push_bind(d.description.as_deref())
        .push_bind(d.status.as_deref())
        .push_bind(d.priority)
        .push_bind(d.priority_label.as_deref())
        .push_bind(d.due_date)
        .push_bind(d.start_date)
        .push_bind(d.time_estimate)
        .push_bind(Json(&d.assignees))
        .push_bind(Json(&d.tags))
        .push_bind(d.now)
        .push_bind(d.now);
}

/// `task.update({ where: { id }, data })` / `updateMany` per id → baris terpengaruh.
async fn update_task<'e, E>(
    ex: E,
    id: i32,
    d: &TaskData,
    project_id: i32,
    company_id: i32,
) -> sqlx::Result<u64>
where
    E: sqlx::PgExecutor<'e>,
{
    let r = sqlx::query(
        r#"UPDATE task SET "projectId" = $2, "companyId" = $3, source = 'clickup',
             "externalUrl" = $4, "externalMeta" = $5, name = $6, description = $7, status = $8,
             priority = $9, "priorityLabel" = $10, "dueDate" = $11, "startDate" = $12,
             "timeEstimate" = $13, assignees = $14, tags = $15, "lastSyncedAt" = $16,
             "updatedAt" = $16
           WHERE id = $1"#,
    )
    .bind(id)
    .bind(project_id)
    .bind(company_id)
    .bind(d.external_url.as_deref())
    .bind(Json(&d.external_meta))
    .bind(d.name.as_str())
    .bind(d.description.as_deref())
    .bind(d.status.as_deref())
    .bind(d.priority)
    .bind(d.priority_label.as_deref())
    .bind(d.due_date)
    .bind(d.start_date)
    .bind(d.time_estimate)
    .bind(Json(&d.assignees))
    .bind(Json(&d.tags))
    .bind(d.now)
    .execute(ex)
    .await?;
    Ok(r.rows_affected())
}

/// `persistTaskPage`: satu baca untuk baris yang ada, lalu satu transaksi
/// `createMany(skipDuplicates)` + `updateMany` per id (batas waktu 30 dtk).
async fn persist_task_page(
    state: &AppState,
    tasks: &[Value],
    company_id: i32,
    project_id: i32,
    folder_id: &str,
) -> Result<(u64, u64, u64), Thrown> {
    // `Map` JS: urutan sisip pertama, nilai terakhir menang.
    let mut by_external: Vec<(String, &Value)> = Vec::new();
    for t in tasks {
        if nn(t.get("id")).is_none() {
            continue;
        }
        let key = js_str(t.get("id"));
        if let Some(slot) = by_external.iter_mut().find(|(k, _)| *k == key) {
            slot.1 = t;
        } else {
            by_external.push((key, t));
        }
    }
    if by_external.is_empty() {
        return Ok((0, 0, 0));
    }

    let ids: Vec<String> = by_external.iter().map(|(k, _)| k.clone()).collect();
    let existing: Vec<(i32, Option<String>, Option<Value>)> = sqlx::query_as(
        r#"SELECT id, "externalId", "externalMeta" FROM task
           WHERE "externalId" = ANY($1) AND "projectId" = $2 AND "companyId" = $3
             AND source = 'clickup'"#,
    )
    .bind(&ids)
    .bind(project_id)
    .bind(company_id)
    .fetch_all(&state.db)
    .await?;

    let now = now_naive();
    let mut to_create: Vec<(String, TaskData)> = Vec::new();
    let mut to_update: Vec<(i32, TaskData)> = Vec::new();
    let mut unchanged = 0_u64;
    for (external_id, t) in &by_external {
        // `new Map(existing.map(...))`: id kembar → baris terakhir menang.
        let row = existing
            .iter()
            .rev()
            .find(|(_, e, _)| e.as_deref() == Some(external_id.as_str()));
        if let Some((id, _, meta)) = row {
            let stored = meta.as_ref();
            let incoming = nn(t.get("date_updated")).map(|d| js_str(Some(d)));
            if let Some(inc) = incoming
                && field(stored, "dateUpdated").and_then(Value::as_str) == Some(inc.as_str())
                && field(stored, "folderId").and_then(Value::as_str) == Some(folder_id)
            {
                unchanged += 1;
                continue;
            }
            to_update.push((*id, build_task_data(t, folder_id, now)));
        } else {
            to_create.push((external_id.clone(), build_task_data(t, folder_id, now)));
        }
    }
    if to_create.is_empty() && to_update.is_empty() {
        return Ok((0, 0, unchanged));
    }

    let work = async {
        let mut tx = state.db.begin().await?;
        let mut created = 0_u64;
        let mut updated = 0_u64;
        if !to_create.is_empty() {
            let mut qb: QueryBuilder<Postgres> =
                QueryBuilder::new(format!(r#"INSERT INTO task ({COLUMNS}, "externalId") "#));
            qb.push_values(to_create.iter(), |mut b, (ext, d)| {
                push_values(&mut b, d, project_id, company_id);
                b.push_bind(ext.as_str());
            });
            qb.push(" ON CONFLICT DO NOTHING");
            created = qb.build().execute(&mut *tx).await?.rows_affected();
        }
        for (id, d) in &to_update {
            updated += update_task(&mut *tx, *id, d, project_id, company_id).await?;
        }
        tx.commit().await?;
        Ok::<_, sqlx::Error>((created, updated, unchanged))
    };
    match tokio::time::timeout(TASK_PAGE_TX_TIMEOUT, work).await {
        Ok(r) => Ok(r?),
        Err(_) => Err(Thrown(format!(
            "Transaction already closed: The timeout for this transaction was {} ms.",
            TASK_PAGE_TX_TIMEOUT.as_millis()
        ))),
    }
}

/// `syncTasksInternal(companyId, projectId, folderId, token, includeClosed,
/// includeSubtasks, updatedAfterMs?, fromCron)`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn sync_tasks_internal(
    state: &AppState,
    company_id: i32,
    project_id: i32,
    folder_id: &str,
    token: &str,
    include_closed: bool,
    include_subtasks: bool,
    updated_after_ms: Option<i64>,
    from_cron: bool,
) -> Result<SyncTotals, Thrown> {
    let http = if from_cron {
        Client::cron(state, token)
    } else {
        Client::new(state, token)
    };
    let lists_res = match http
        .get(&format!("/folder/{folder_id}/list?archived=false"))
        .await
    {
        Ok(v) => v,
        Err(HttpErr {
            status: Some(401), ..
        }) => return Err(Thrown("Invalid ClickUp API token".into())),
        Err(HttpErr {
            status: Some(404), ..
        }) => return Err(Thrown("ClickUp folder not found".into())),
        Err(e) => return Err(e.into()),
    };

    let mut totals = SyncTotals::default();
    for list in array_of(lists_res.get("lists")) {
        let list_id = js_str(list.get("id"));
        let mut page = 0_u64;
        loop {
            let mut query = vec![
                ("archived", "false".to_owned()),
                ("include_closed", include_closed.to_string()),
                ("subtasks", include_subtasks.to_string()),
                ("page", page.to_string()),
            ];
            if let Some(ms) = updated_after_ms {
                query.push(("date_updated_gt", ms.to_string()));
            }
            let res = http
                .get_query(&format!("/list/{list_id}/task"), &query)
                .await?;
            let tasks = array_of(res.get("tasks"));
            if !tasks.is_empty() {
                let (c, u, n) =
                    persist_task_page(state, tasks, company_id, project_id, folder_id).await?;
                totals.created += c;
                totals.updated += u;
                totals.unchanged += n;
                totals.total += tasks.len() as u64;
            }
            if res.get("last_page") == Some(&Value::Bool(true)) || tasks.is_empty() {
                break;
            }
            page += 1;
        }
    }
    Ok(totals)
}

/// `upsertSingleTask(taskId, companyId, projectId, folderId, token)` (webhook).
pub(crate) async fn upsert_single_task(
    state: &AppState,
    task_id: &str,
    company_id: i32,
    project_id: i32,
    folder_id: &str,
    token: &str,
) -> Result<(), Thrown> {
    let task = Client::new(state, token)
        .get(&format!("/task/{task_id}"))
        .await?;
    let data = build_task_data(&task, folder_id, now_naive());
    let existing: Option<i32> = sqlx::query_scalar(
        r#"SELECT id FROM task WHERE "externalId" = $1 AND "projectId" = $2
             AND "companyId" = $3 AND source = 'clickup' LIMIT 1"#,
    )
    .bind(task_id)
    .bind(project_id)
    .bind(company_id)
    .fetch_optional(&state.db)
    .await?;
    if let Some(id) = existing {
        update_task(&state.db, id, &data, project_id, company_id).await?;
    } else {
        let mut qb: QueryBuilder<Postgres> =
            QueryBuilder::new(format!(r#"INSERT INTO task ({COLUMNS}, "externalId") "#));
        qb.push_values(std::iter::once(&data), |mut b, d| {
            push_values(&mut b, d, project_id, company_id);
            b.push_bind(task_id);
        });
        qb.build().execute(&state.db).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_data_from_clickup() {
        let now = now_naive();
        let t = json!({
            "id": "abc",
            "name": "Fix login",
            "url": "https://app.clickup.com/t/abc",
            "status": {"status": "in progress", "color": "#fff", "type": "custom"},
            "priority": {"priority": "high"},
            "due_date": "1767225600000",
            "start_date": null,
            "time_estimate": 3600000,
            "date_updated": "1700000000000",
            "list": {"id": "L1", "name": "Sprint"},
            "assignees": [{"id": 7, "username": "budi", "email": null}],
            "tags": [{"name": "bug"}, {}]
        });
        let d = build_task_data(&t, "F1", now);
        assert_eq!(d.name, "Fix login");
        assert_eq!(d.priority, Some(2));
        assert_eq!(d.priority_label.as_deref(), Some("high"));
        assert_eq!(d.status.as_deref(), Some("in progress"));
        assert_eq!(
            d.due_date.map(|x| x.to_string()).as_deref(),
            Some("2026-01-01 00:00:00")
        );
        assert_eq!(d.start_date, None);
        assert_eq!(d.time_estimate, Some(3_600_000));
        assert_eq!(
            d.assignees,
            json!([{"externalUserId": "7", "username": "budi", "email": null}])
        );
        assert_eq!(d.tags, json!(["bug", null]));
        assert_eq!(
            d.external_meta,
            json!({"folderId": "F1", "listId": "L1", "listName": "Sprint",
                   "statusColor": "#fff", "statusType": "custom", "dateUpdated": "1700000000000"})
        );

        let empty = build_task_data(&json!({"priority": {"priority": "weird"}}), "F", now);
        assert_eq!(empty.name, "(no name)");
        assert_eq!(empty.priority, None);
        assert_eq!(empty.priority_label.as_deref(), Some("weird"));
        assert_eq!(empty.external_meta.get("dateUpdated"), Some(&Value::Null));
        assert_eq!(empty.assignees, json!([]));
    }
}
