//! Public browser routes, signed webhook ingestion, and the separate worker listener.
use crate::pages::{AdminPage, AdminRow, Dashboard};
use anyhow::Result;
use askama::Template;
use axum::{
    Form, Json, Router,
    body::Bytes,
    extract::Request,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
};
use chrono::Utc;
use grading_core::{
    protocol::{Heartbeat, LeaseRequest, Revision, RunResult},
    security,
};
use grading_github::GitHub;
use grading_store::{
    artifacts::Artifacts,
    courses, grading,
    identity::{self, Session},
    queue, submissions,
};
use serde::Deserialize;
use sqlx::PgPool;
use std::sync::Arc;
use uuid::Uuid;

const SESSION_COOKIE: &str = "__Host-grading-session";
const LOGIN_COOKIE: &str = "__Host-grading-login";

#[derive(Clone)]
pub struct AppState {
    pub pool: PgPool,
    pub github: GitHub,
    pub artifacts: Artifacts,
    pub webhook_secret: Arc<Vec<u8>>,
    pub public_origin: String,
}

pub struct HttpError(pub StatusCode);
impl From<anyhow::Error> for HttpError {
    fn from(_: anyhow::Error) -> Self {
        Self(StatusCode::INTERNAL_SERVER_ERROR)
    }
}
impl From<sqlx::Error> for HttpError {
    fn from(error: sqlx::Error) -> Self {
        Self(if matches!(error, sqlx::Error::RowNotFound) {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        })
    }
}
impl IntoResponse for HttpError {
    fn into_response(self) -> Response {
        (
            self.0,
            match self.0 {
                StatusCode::UNAUTHORIZED => "Please sign in.",
                StatusCode::FORBIDDEN => "This request is not permitted.",
                StatusCode::NOT_FOUND => "Not found.",
                StatusCode::BAD_REQUEST => "Invalid request.",
                StatusCode::CONFLICT => "This operation cannot be accepted in its current state.",
                _ => "The operation failed. Please retry or contact your instructor.",
            },
        )
            .into_response()
    }
}
type HttpResult<T> = Result<T, HttpError>;

pub fn public_router(state: AppState) -> Router {
    Router::new()
        .route("/", get(dashboard))
        .route("/login", get(login))
        .route("/auth/callback", get(callback))
        .route("/logout", post(logout))
        .route("/admin", get(admin))
        .route("/assignments/{id}/repository", post(create_repository))
        .route("/repositories/{id}/submit", post(submit))
        .route("/runs/{id}/report", get(report))
        .route(
            "/webhooks/github",
            post(webhook).layer(DefaultBodyLimit::max(2 * 1024 * 1024)),
        )
        .route("/healthz", get(|| async { "ok" }))
        .route("/readyz", get(ready))
        .route("/static/style.css", get(css))
        .layer(DefaultBodyLimit::max(65536))
        .layer(middleware::from_fn(headers))
        .with_state(state)
}

pub fn internal_router(state: AppState) -> Router {
    Router::new()
        .route("/internal/lease", post(lease))
        .route("/internal/tasks/{id}/heartbeat", post(heartbeat))
        .route("/internal/tasks/{id}/source", get(source))
        .route("/internal/tasks/{id}/result", post(result))
        .route("/internal/metrics", get(metrics))
        .layer(DefaultBodyLimit::max(8 * 1024 * 1024))
        .with_state(state)
}

pub fn preview_router() -> Router {
    Router::new()
        .route(
            "/",
            get(|| async { Html(crate::pages::preview().render().expect("preview template")) }),
        )
        .route("/static/style.css", get(css))
        .layer(middleware::from_fn(headers))
}

async fn headers(request: Request, next: Next) -> Response {
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_SECURITY_POLICY,"default-src 'none'; style-src 'self'; img-src 'self'; form-action 'self'; frame-ancestors 'none'; base-uri 'none'".parse().expect("static header"));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        "nosniff".parse().expect("static header"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        "same-origin".parse().expect("static header"),
    );
    headers.insert(
        header::CACHE_CONTROL,
        "no-store".parse().expect("static header"),
    );
    response
}
async fn css() -> impl IntoResponse {
    (
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        include_str!("../../../static/style.css"),
    )
}
async fn ready(State(state): State<AppState>) -> HttpResult<&'static str> {
    grading_store::healthy(&state.pool).await?;
    Ok("ready")
}

fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .find_map(|part| {
            part.trim()
                .split_once('=')
                .filter(|(key, _)| *key == name)
                .map(|(_, value)| value)
        })
}
fn set_cookie(response: &mut Response, name: &str, value: &str, age: u32) {
    response.headers_mut().append(
        header::SET_COOKIE,
        format!("{name}={value}; Path=/; Secure; HttpOnly; SameSite=Lax; Max-Age={age}")
            .parse()
            .expect("generated cookie"),
    );
}
async fn authenticated(state: &AppState, headers: &HeaderMap) -> HttpResult<Session> {
    let raw = cookie(headers, SESSION_COOKIE).ok_or(HttpError(StatusCode::UNAUTHORIZED))?;
    identity::session(&state.pool, raw)
        .await?
        .ok_or(HttpError(StatusCode::UNAUTHORIZED))
}
fn csrf(
    state: &AppState,
    headers: &HeaderMap,
    session: &Session,
    submitted: &str,
) -> HttpResult<()> {
    let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());
    if !security::equal(&session.csrf, submitted)
        || origin.is_some_and(|value| value != state.public_origin)
    {
        return Err(HttpError(StatusCode::FORBIDDEN));
    }
    Ok(())
}

async fn dashboard(State(state): State<AppState>, headers: HeaderMap) -> HttpResult<Html<String>> {
    let session = if let Some(raw) = cookie(&headers, SESSION_COOKIE) {
        identity::session(&state.pool, raw).await?
    } else {
        None
    };
    let page = if let Some(session) = session {
        Dashboard {
            login: session.login,
            csrf: session.csrf,
            authenticated: true,
            admin: session.admin,
            preview: false,
            rows: courses::dashboard(&state.pool, session.github_id)
                .await?
                .into_iter()
                .map(Into::into)
                .collect(),
        }
    } else {
        Dashboard {
            login: String::new(),
            csrf: String::new(),
            authenticated: false,
            admin: false,
            preview: false,
            rows: vec![],
        }
    };
    Ok(Html(page.render().map_err(anyhow::Error::from)?))
}

async fn login(State(state): State<AppState>) -> HttpResult<Response> {
    let login_state = security::token();
    let browser = security::token();
    let verifier = security::token();
    identity::begin_login(&state.pool, &login_state, &browser, &verifier).await?;
    let mut response =
        Redirect::to(&state.github.authorize_url(&login_state, &verifier)).into_response();
    set_cookie(&mut response, LOGIN_COOKIE, &browser, 300);
    Ok(response)
}
#[derive(Deserialize)]
struct Callback {
    code: String,
    state: String,
}
async fn callback(
    State(state): State<AppState>,
    headers: HeaderMap,
    query: Result<Query<Callback>, axum::extract::rejection::QueryRejection>,
) -> HttpResult<Response> {
    let Query(query) = query.map_err(|_| callback_error("query_parse", StatusCode::BAD_REQUEST))?;
    let browser = cookie(&headers, LOGIN_COOKIE)
        .ok_or_else(|| callback_error("login_cookie", StatusCode::FORBIDDEN))?;
    let verifier = identity::consume_login(&state.pool, &query.state, browser)
        .await
        .map_err(|error| {
            if matches!(
                error.downcast_ref::<sqlx::Error>(),
                Some(sqlx::Error::RowNotFound)
            ) {
                callback_error("login_state", StatusCode::FORBIDDEN)
            } else {
                callback_error("login_state_store", StatusCode::INTERNAL_SERVER_ERROR)
            }
        })?;
    let account = state
        .github
        .login(&query.code, &verifier)
        .await
        .map_err(|error| {
            tracing::warn!(
                stage = error.stage(),
                reason = error.reason(),
                upstream_status = error.upstream_status(),
                status = 500,
                "GitHub login callback failed"
            );
            HttpError(StatusCode::INTERNAL_SERVER_ERROR)
        })?;
    let session = identity::new_session(
        &state.pool,
        account.id,
        &account.login,
        cookie(&headers, SESSION_COOKIE),
    )
    .await
    .map_err(|_| callback_error("session_create", StatusCode::INTERNAL_SERVER_ERROR))?;
    let mut response = Redirect::to("/").into_response();
    set_cookie(&mut response, SESSION_COOKIE, &session, 43200);
    set_cookie(&mut response, LOGIN_COOKIE, "", 0);
    Ok(response)
}

fn callback_error(stage: &'static str, status: StatusCode) -> HttpError {
    tracing::warn!(
        stage,
        status = status.as_u16(),
        "GitHub login callback failed"
    );
    HttpError(status)
}
#[derive(Deserialize)]
struct CsrfForm {
    csrf: String,
}
async fn logout(
    State(state): State<AppState>,
    headers: HeaderMap,
    Form(form): Form<CsrfForm>,
) -> HttpResult<Response> {
    let session = authenticated(&state, &headers).await?;
    csrf(&state, &headers, &session, &form.csrf)?;
    if let Some(raw) = cookie(&headers, SESSION_COOKIE) {
        identity::logout(&state.pool, raw).await?;
    }
    let mut response = Redirect::to("/").into_response();
    set_cookie(&mut response, SESSION_COOKIE, "", 0);
    Ok(response)
}
async fn create_repository(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Form(form): Form<CsrfForm>,
) -> HttpResult<Redirect> {
    let session = authenticated(&state, &headers).await?;
    csrf(&state, &headers, &session, &form.csrf)?;
    courses::request_repository(&state.pool, session.github_id, id)
        .await
        .map_err(|_| HttpError(StatusCode::CONFLICT))?;
    Ok(Redirect::to("/"))
}
async fn submit(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Form(form): Form<CsrfForm>,
) -> HttpResult<Redirect> {
    let session = authenticated(&state, &headers).await?;
    csrf(&state, &headers, &session, &form.csrf)?;
    let owner: Option<i64>=sqlx::query_scalar("SELECT e.github_id FROM student_repositories r JOIN enrollments e ON e.id=r.enrollment_id WHERE r.id=$1 AND e.github_id=$2").bind(id).bind(session.github_id).fetch_optional(&state.pool).await?;
    if owner.is_none() {
        return Err(HttpError(StatusCode::NOT_FOUND));
    }
    let repository = courses::repository(&state.pool, id).await?;
    if repository.state != "ready" || repository.closure_due || repository.deadline < Utc::now() {
        return Err(HttpError(StatusCode::CONFLICT));
    }
    let revision: Revision =
        serde_json::from_value(repository.definition).map_err(anyhow::Error::from)?;
    let full_name = format!("{}/{}", repository.organization, repository.name);
    state
        .github
        .verify_repository(
            &full_name,
            repository
                .github_repo_id
                .ok_or(HttpError(StatusCode::CONFLICT))?,
        )
        .await?;
    let sha = state
        .github
        .branch_sha(&full_name, &revision.assignment.branch)
        .await?;
    let mut tx = state.pool.begin().await?;
    let received = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *tx)
        .await?;
    let accepted = submissions::record(&mut tx, id, &sha, received, "registration", None).await?;
    tx.commit().await?;
    if accepted.is_none() {
        return Err(HttpError(StatusCode::CONFLICT));
    }
    Ok(Redirect::to("/"))
}

#[derive(Deserialize)]
struct Push {
    after: String,
    #[serde(rename = "ref")]
    reference: String,
    deleted: bool,
    repository: PushRepository,
}
#[derive(Deserialize)]
struct PushRepository {
    id: i64,
}
async fn webhook(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> HttpResult<StatusCode> {
    let received = Utc::now();
    let signature = headers
        .get("x-hub-signature-256")
        .and_then(|v| v.to_str().ok())
        .ok_or(HttpError(StatusCode::FORBIDDEN))?;
    security::verify_webhook(&state.webhook_secret, signature, &body)
        .map_err(|_| HttpError(StatusCode::FORBIDDEN))?;
    let delivery = headers
        .get("x-github-delivery")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<Uuid>().ok())
        .ok_or(HttpError(StatusCode::BAD_REQUEST))?;
    let event = headers
        .get("x-github-event")
        .and_then(|v| v.to_str().ok())
        .ok_or(HttpError(StatusCode::BAD_REQUEST))?;
    let mut tx = state.pool.begin().await?;
    let hash = security::digest(&body);
    let inserted=sqlx::query("INSERT INTO webhook_deliveries(delivery_id,body_digest,received_at) VALUES($1,$2,$3) ON CONFLICT DO NOTHING").bind(delivery).bind(&hash).bind(received).execute(&mut *tx).await?.rows_affected();
    if inserted == 0 {
        let previous: String =
            sqlx::query_scalar("SELECT body_digest FROM webhook_deliveries WHERE delivery_id=$1")
                .bind(delivery)
                .fetch_one(&mut *tx)
                .await?;
        if previous != hash {
            return Err(HttpError(StatusCode::CONFLICT));
        }
        return Ok(StatusCode::OK);
    }
    if event == "push" {
        let push: Push =
            serde_json::from_slice(&body).map_err(|_| HttpError(StatusCode::BAD_REQUEST))?;
        let repository:Option<(Uuid,serde_json::Value)>=sqlx::query_as("SELECT r.id,v.definition FROM student_repositories r JOIN assignment_revisions v ON v.digest=r.revision_digest WHERE r.github_repo_id=$1")
            .bind(push.repository.id).fetch_optional(&mut *tx).await?;
        if let Some((id, definition)) = repository {
            let revision: Revision =
                serde_json::from_value(definition).map_err(anyhow::Error::from)?;
            if push.reference == format!("refs/heads/{}", revision.assignment.branch) {
                if push.deleted {
                    sqlx::query("UPDATE student_repositories SET needs_review=true WHERE id=$1")
                        .bind(id)
                        .execute(&mut *tx)
                        .await?;
                } else {
                    submissions::record(
                        &mut tx,
                        id,
                        &push.after,
                        received,
                        "webhook",
                        Some(delivery),
                    )
                    .await
                    .map_err(|_| HttpError(StatusCode::BAD_REQUEST))?;
                }
            }
        }
    }
    tx.commit().await?;
    Ok(StatusCode::ACCEPTED)
}

async fn report(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> HttpResult<Response> {
    let session = authenticated(&state, &headers).await?;
    let hash:Option<String>=sqlx::query_scalar("SELECT g.report_digest FROM grading_runs g JOIN submissions s ON s.id=g.submission_id JOIN student_repositories r ON r.id=s.repository_id JOIN enrollments e ON e.id=r.enrollment_id WHERE g.id=$1 AND (e.github_id=$2 OR EXISTS(SELECT 1 FROM admins WHERE github_id=$2))")
        .bind(id).bind(session.github_id).fetch_one(&state.pool).await?;
    let hash = hash.ok_or(HttpError(StatusCode::NOT_FOUND))?;
    let bytes = state.artifacts.get(&hash).await?;
    Ok((
        [
            (header::CONTENT_TYPE, "text/plain; charset=utf-8"),
            (header::CONTENT_DISPOSITION, "inline"),
        ],
        bytes,
    )
        .into_response())
}
async fn admin(State(state): State<AppState>, headers: HeaderMap) -> HttpResult<Html<String>> {
    let session = authenticated(&state, &headers).await?;
    if !session.admin {
        return Err(HttpError(StatusCode::FORBIDDEN));
    }
    let rows:Vec<(String,i64,i64,i64,i64)>=sqlx::query_as("SELECT c.title,(SELECT count(*) FROM enrollments e WHERE e.course_id=c.id),(SELECT count(*) FROM student_repositories r JOIN enrollments e ON e.id=r.enrollment_id WHERE e.course_id=c.id),(SELECT count(*) FROM student_repositories r JOIN enrollments e ON e.id=r.enrollment_id WHERE e.course_id=c.id AND r.needs_review),(SELECT count(*) FROM tasks t LEFT JOIN submissions s ON s.id::text=t.payload->>'submission_id' LEFT JOIN grading_runs g ON g.id::text=t.payload->>'run_id' LEFT JOIN submissions gs ON gs.id=g.submission_id JOIN student_repositories rr ON rr.id::text=t.payload->>'repository_id' OR rr.id=s.repository_id OR rr.id=gs.repository_id JOIN enrollments ee ON ee.id=rr.enrollment_id WHERE t.status='failed' AND ee.course_id=c.id) FROM courses c ORDER BY c.id").fetch_all(&state.pool).await?;
    Ok(Html(
        AdminPage {
            login: session.login,
            rows: rows
                .into_iter()
                .map(
                    |(course, enrolled, repositories, needs_review, failed_tasks)| AdminRow {
                        course,
                        enrolled,
                        repositories,
                        needs_review,
                        failed_tasks,
                    },
                )
                .collect(),
        }
        .render()
        .map_err(anyhow::Error::from)?,
    ))
}
async fn worker(state: &AppState, headers: &HeaderMap) -> HttpResult<grading::Worker> {
    let raw = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(HttpError(StatusCode::UNAUTHORIZED))?;
    grading::authenticate(&state.pool, raw)
        .await
        .map_err(|_| HttpError(StatusCode::UNAUTHORIZED))
}
async fn lease(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(request): Json<LeaseRequest>,
) -> HttpResult<Json<Option<grading_core::protocol::Lease>>> {
    let worker = worker(&state, &headers).await?;
    Ok(Json(
        grading::lease(&state.pool, &worker, &request.profiles).await?,
    ))
}
async fn heartbeat(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(body): Json<Heartbeat>,
) -> HttpResult<StatusCode> {
    let worker = worker(&state, &headers).await?;
    grading::owned_lease(&state.pool, &worker, id, body.lease_token, false)
        .await
        .map_err(|_| HttpError(StatusCode::CONFLICT))?;
    queue::heartbeat(&state.pool, id, body.lease_token, &worker.id)
        .await
        .map_err(|_| HttpError(StatusCode::CONFLICT))?;
    Ok(StatusCode::NO_CONTENT)
}
#[derive(Deserialize)]
struct SourceQuery {
    lease_token: Uuid,
}
async fn source(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Query(query): Query<SourceQuery>,
) -> HttpResult<Response> {
    let worker = worker(&state, &headers).await?;
    let lease = grading::owned_lease(&state.pool, &worker, id, query.lease_token, false)
        .await
        .map_err(|_| HttpError(StatusCode::NOT_FOUND))?;
    Ok((
        [(header::CONTENT_TYPE, "application/json")],
        state.artifacts.get(&lease.source_digest).await?,
    )
        .into_response())
}
async fn result(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
    Json(result): Json<RunResult>,
) -> HttpResult<StatusCode> {
    let worker = worker(&state, &headers).await?;
    grading::accept(&state.pool, &state.artifacts, &worker, id, &result)
        .await
        .map_err(|_| HttpError(StatusCode::CONFLICT))?;
    Ok(StatusCode::NO_CONTENT)
}
async fn metrics(State(state): State<AppState>, headers: HeaderMap) -> HttpResult<String> {
    worker(&state, &headers).await?;
    let (pending,failed,stale):(i64,i64,i64)=sqlx::query_as("SELECT count(*) FILTER(WHERE status='pending'),count(*) FILTER(WHERE status='failed'),count(*) FILTER(WHERE status='leased' AND lease_until<now()) FROM tasks").fetch_one(&state.pool).await?;
    let age:i64=sqlx::query_scalar("SELECT COALESCE(EXTRACT(EPOCH FROM now()-min(created_at))::bigint,0) FROM tasks WHERE status='pending'").fetch_one(&state.pool).await?;
    let locks: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM student_repositories WHERE closure_due AND locked_at IS NULL",
    )
    .fetch_one(&state.pool)
    .await?;
    let provisioning: i64 =
        sqlx::query_scalar("SELECT count(*) FROM tasks WHERE kind='provision' AND status='failed'")
            .fetch_one(&state.pool)
            .await?;
    let integrity: i64 =
        sqlx::query_scalar("SELECT count(*) FROM grading_runs WHERE status='integrity_failed'")
            .fetch_one(&state.pool)
            .await?;
    Ok(format!(
        "grading_tasks_pending {pending}\ngrading_tasks_failed {failed}\ngrading_leases_stale {stale}\ngrading_queue_oldest_seconds {age}\ngrading_locks_pending {locks}\ngrading_provisioning_failed {provisioning}\ngrading_integrity_failed {integrity}\n"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tower::ServiceExt;
    #[tokio::test]
    async fn preview_has_security_headers_and_no_mutating_routes() {
        let app = preview_router();
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            response
                .headers()
                .contains_key(header::CONTENT_SECURITY_POLICY)
        );
        let response = app
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/assignments/anything/repository")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }
    #[test]
    fn templates_escape_account_and_course_text() {
        let mut page = crate::pages::preview();
        page.login = "<script>alert(1)</script>".into();
        let html = page.render().unwrap();
        assert!(!html.contains("<script>"));
        assert!(html.contains("alert(1)"));
    }
}
