//! Administrator-only forms, multipart input and durable review pages.
use crate::routes::{AppState, HttpError, authenticated, csrf};
use askama::Template;
use axum::{
    Form,
    extract::{Multipart, Path, State},
    http::{HeaderMap, StatusCode, header},
    response::{Html, IntoResponse, Redirect, Response},
};
use grading_core::{
    admin::{self, Input},
    security,
};
use grading_store::{admin as operations, identity::Session};
use serde::Deserialize;
use std::collections::BTreeMap;
use uuid::Uuid;
type Result<T> = std::result::Result<T, HttpError>;
async fn authorized(state: &AppState, headers: &HeaderMap) -> Result<Session> {
    let session = authenticated(state, headers).await?;
    if !session.admin {
        return Err(HttpError(StatusCode::FORBIDDEN));
    }
    Ok(session)
}
#[derive(Template)]
#[template(path = "admin_form.html")]
struct ActionPage {
    login: String,
    csrf: String,
    action: &'static admin::Action,
    fields: Vec<Field>,
    content: String,
    reason: String,
    error: String,
}
struct Field {
    name: String,
    label: String,
    value: String,
}
fn form(session: &Session, input: Input, error: String) -> Result<Html<String>> {
    let action = admin::action(&input.action).ok_or(HttpError(StatusCode::NOT_FOUND))?;
    let fields = action
        .fields
        .iter()
        .map(|(name, label)| Field {
            name: (*name).into(),
            label: (*label).into(),
            value: input.fields.get(*name).cloned().unwrap_or_default(),
        })
        .collect();
    Ok(Html(
        ActionPage {
            login: session.login.clone(),
            csrf: session.csrf.clone(),
            action,
            fields,
            content: if input.action == "worker_register" {
                String::new()
            } else {
                input.content
            },
            reason: input.reason,
            error,
        }
        .render()
        .map_err(anyhow::Error::from)?,
    ))
}
pub async fn new(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(action): Path<String>,
) -> Result<Html<String>> {
    let session = authorized(&state, &headers).await?;
    form(
        &session,
        Input {
            action,
            fields: BTreeMap::new(),
            content: String::new(),
            reason: String::new(),
        },
        String::new(),
    )
}
pub async fn validate(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(action): Path<String>,
    mut multipart: Multipart,
) -> Result<Response> {
    let session = authorized(&state, &headers).await?;
    let mut values = BTreeMap::new();
    let mut uploaded = String::new();
    let mut error = String::new();
    while let Some(field) = multipart
        .next_field()
        .await
        .map_err(|_| HttpError(StatusCode::BAD_REQUEST))?
    {
        let name = field.name().unwrap_or("").to_owned();
        let bytes = field
            .bytes()
            .await
            .map_err(|_| HttpError(StatusCode::PAYLOAD_TOO_LARGE))?;
        if bytes.len() > admin::MAX_INPUT {
            return Err(HttpError(StatusCode::PAYLOAD_TOO_LARGE));
        }
        let text = match String::from_utf8(bytes.to_vec()) {
            Ok(s) => s,
            Err(_) => {
                error="The uploaded file must contain UTF-8 text (TOML or CSV), not a binary document.".into();
                String::new()
            }
        };
        if name == "upload" {
            uploaded = text;
        } else if values.insert(name, text).is_some() {
            error = "Duplicate form field. Reload the form and try again.".into();
        }
    }
    csrf(
        &state,
        &headers,
        &session,
        values.get("csrf").map_or("", String::as_str),
    )?;
    let content = values.remove("content").unwrap_or_default();
    if !uploaded.trim().is_empty() && !content.trim().is_empty() {
        error = "Choose one input: upload a file or paste text, not both.".into();
    }
    let mut input = Input {
        action,
        content: if uploaded.trim().is_empty() {
            content
        } else {
            uploaded
        },
        reason: values.remove("reason").unwrap_or_default(),
        fields: values
            .into_iter()
            .filter_map(|(k, v)| k.strip_prefix("field_").map(|name| (name.to_owned(), v)))
            .collect(),
    };
    if error.is_empty()
        && let Err(e) = input.validate()
    {
        error = e.to_string();
    }
    if input.action == "worker_register" && error.is_empty() {
        if input.content.trim().len() < 43 {
            error = "Provide a random worker token of at least 43 characters.".into();
        } else {
            input.content = security::digest(input.content.trim());
        }
    }
    if !error.is_empty() {
        return Ok((
            StatusCode::UNPROCESSABLE_ENTITY,
            form(&session, input, error)?,
        )
            .into_response());
    }
    match operations::submit(&state.pool, session.github_id, &session.csrf, &input).await {
        Ok(id) => Ok(Redirect::to(&format!("/admin/operations/{id}")).into_response()),
        Err(error) => {
            let message = if error.downcast_ref::<sqlx::Error>().is_some() {
                "Unable to queue validation. Check administrator service diagnostics and retry."
                    .into()
            } else {
                error.to_string()
            };
            Ok((StatusCode::CONFLICT, form(&session, input, message)?).into_response())
        }
    }
}
#[derive(Template)]
#[template(path = "admin_operation.html")]
struct OperationPage {
    login: String,
    csrf: String,
    id: String,
    title: String,
    state: String,
    output: String,
    token: String,
    busy: bool,
    ready: bool,
    download: bool,
    worker_online: bool,
}
pub async fn show(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Html<String>> {
    let session = authorized(&state, &headers).await?;
    let op = operations::get(&state.pool, id, session.github_id).await?;
    let worker_online:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM admin_worker_status WHERE heartbeat>now()-interval '30 seconds')").fetch_one(&state.pool).await?;
    let title = admin::action(op.input["action"].as_str().unwrap_or(""))
        .map_or("Administrative operation", |a| a.title);
    Ok(Html(
        OperationPage {
            login: session.login,
            csrf: session.csrf,
            id: id.to_string(),
            title: title.into(),
            busy: matches!(
                op.state.as_str(),
                "pending_validation" | "validating" | "queued" | "applying"
            ),
            ready: op.confirmable,
            state: if op.state == "ready" && !op.confirmable {
                "Validation expired; validate again.".into()
            } else {
                op.state.replace('_', " ")
            },
            output: op.output,
            token: op.confirmation_token.unwrap_or_default(),
            download: op.download.is_some() && op.state == "succeeded",
            worker_online,
        }
        .render()
        .map_err(anyhow::Error::from)?,
    ))
}
pub async fn edit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Html<String>> {
    let session = authorized(&state, &headers).await?;
    let op = operations::get(&state.pool, id, session.github_id).await?;
    let input: Input = serde_json::from_value(op.input).map_err(anyhow::Error::from)?;
    form(&session, input, String::new())
}
#[derive(Deserialize)]
pub struct Confirm {
    csrf: String,
    token: String,
}
pub async fn confirm(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Form(form): Form<Confirm>,
) -> Result<Response> {
    let session = authorized(&state, &headers).await?;
    csrf(&state, &headers, &session, &form.csrf)?;
    if !operations::confirm(
        &state.pool,
        id,
        session.github_id,
        &session.csrf,
        &form.token,
    )
    .await?
    {
        return Ok((StatusCode::CONFLICT,Html("<p>Confirmation was not accepted: validation expired, this session changed, or the operation was already confirmed. No new operation was queued. <a href=\"/admin\">Return to administration</a>.</p>".to_owned())).into_response());
    }
    Ok(Redirect::to(&format!("/admin/operations/{id}")).into_response())
}
#[derive(Deserialize)]
pub struct Retry {
    csrf: String,
}
pub async fn retry(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Form(form): Form<Retry>,
) -> Result<Redirect> {
    let session = authorized(&state, &headers).await?;
    csrf(&state, &headers, &session, &form.csrf)?;
    let op = operations::get(&state.pool, id, session.github_id).await?;
    if matches!(
        op.state.as_str(),
        "queued" | "applying" | "pending_validation" | "validating"
    ) {
        return Err(HttpError(StatusCode::CONFLICT));
    }
    let input: Input = serde_json::from_value(op.input).map_err(anyhow::Error::from)?;
    let next = operations::submit(&state.pool, session.github_id, &session.csrf, &input).await?;
    Ok(Redirect::to(&format!("/admin/operations/{next}")))
}
pub async fn download(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Response> {
    let session = authorized(&state, &headers).await?;
    let op = operations::get(&state.pool, id, session.github_id).await?;
    if op.state != "succeeded" {
        return Err(HttpError(StatusCode::CONFLICT));
    }
    let csv = op.download.ok_or(HttpError(StatusCode::NOT_FOUND))?;
    Ok((
        [
            (header::CONTENT_TYPE, "text/csv; charset=utf-8"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=grades.csv",
            ),
            (header::CACHE_CONTROL, "no-store"),
        ],
        csv,
    )
        .into_response())
}
#[derive(Template)]
#[template(path = "admin_data.html")]
struct DataPage {
    login: String,
    title: String,
    text: String,
}
pub async fn data(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(view): Path<String>,
) -> Result<Html<String>> {
    let session = authorized(&state, &headers).await?;
    let query = match view.as_str() {
        "courses" => {
            "SELECT to_jsonb(t) FROM (SELECT id,title,organization,timezone FROM courses ORDER BY id) t"
        }
        "students" => {
            "SELECT to_jsonb(t) FROM (SELECT e.course_id,e.student_id,e.name,u.login,e.github_id FROM enrollments e JOIN users u ON u.github_id=e.github_id ORDER BY e.course_id,e.student_id LIMIT 1000) t"
        }
        "repositories" => {
            "SELECT to_jsonb(t) FROM (SELECT r.id,a.course_id,a.slug,r.name,r.state,r.final_submission_id,r.needs_review,r.last_error FROM student_repositories r JOIN assignments a ON a.id=r.assignment_id ORDER BY a.course_id,r.name LIMIT 1000) t"
        }
        "submissions" => {
            "SELECT to_jsonb(t) FROM (SELECT id,repository_id,event_id,sha,received_at FROM submissions ORDER BY received_at DESC LIMIT 1000) t"
        }
        "events" => {
            "SELECT to_jsonb(t) FROM (SELECT id,repository_id,sha,received_at FROM submission_events ORDER BY received_at DESC LIMIT 1000) t"
        }
        "runs" => {
            "SELECT to_jsonb(t) FROM (SELECT g.id,g.submission_id,g.status,g.points,g.public_run_id,g.attempt FROM grading_runs g JOIN submissions s ON s.id=g.submission_id ORDER BY s.received_at DESC,g.attempt DESC LIMIT 1000) t"
        }
        "tasks" => {
            "SELECT to_jsonb(t) FROM (SELECT id,kind,status,attempts,last_error,payload FROM tasks ORDER BY created_at DESC LIMIT 1000) t"
        }
        "workers" => {
            "SELECT to_jsonb(t) FROM (SELECT id,profiles,resource_caps,enabled FROM workers ORDER BY id) t"
        }
        _ => return Err(HttpError(StatusCode::NOT_FOUND)),
    };
    let rows: Vec<serde_json::Value> = sqlx::query_scalar(query).fetch_all(&state.pool).await?;
    Ok(Html(
        DataPage {
            login: session.login,
            title: view,
            text: serde_json::to_string_pretty(&rows).map_err(anyhow::Error::from)?,
        }
        .render()
        .map_err(anyhow::Error::from)?,
    ))
}
