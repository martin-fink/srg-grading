//! Browser security and webhook replay tests using the isolated PostgreSQL fixture.
use anyhow::Result;
use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use grading_github::{AppConfig, GitHub};
use grading_store::{artifacts::Artifacts, identity};
use grading_web::routes::{AppState, internal_router, public_router};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use sqlx::PgPool;
use std::sync::Arc;
use tower::ServiceExt;
use tracing::instrument::WithSubscriber;
use uuid::Uuid;

#[derive(Clone, Default)]
struct LogCapture(Arc<std::sync::Mutex<Vec<u8>>>);
impl std::io::Write for LogCapture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[tokio::test]
async fn browser_and_worker_boundaries() -> Result<()> {
    let Ok(url) = std::env::var("TEST_WEB_DATABASE_URL") else {
        eprintln!("HTTP database tests skipped; use just test");
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let admin = PgPool::connect(&std::env::var("TEST_ADMIN_DATABASE_URL")?).await?;
    let github = GitHub::new(AppConfig {
        app_id: "123".into(),
        installation_id: 123,
        client_id: "fixture".into(),
        client_secret: "fixture-only".into(),
        private_key_file: std::env::var("TEST_APP_KEY")?.into(),
        callback_url: "https://grading.example/auth/callback".into(),
    })
    .await?;
    let state = AppState {
        pool: pool.clone(),
        github,
        artifacts: Artifacts::new(std::env::var("TEST_ARTIFACT_ROOT")?).await?,
        webhook_secret: Arc::new(vec![7; 32]),
        public_origin: "https://grading.example".into(),
    };
    let app = public_router(state.clone());
    let worker = internal_router(state);
    let capture = LogCapture::default();
    let writer = capture.clone();
    let subscriber = tracing::Dispatch::new(
        tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_max_level(tracing::Level::INFO)
            .with_writer(move || writer.clone())
            .finish(),
    );
    for (query, browser, expected) in [
        ("code=secret-code", "", StatusCode::BAD_REQUEST),
        (
            "code=secret-code&state=secret-state",
            "",
            StatusCode::FORBIDDEN,
        ),
        (
            "code=secret-code&state=secret-state",
            "secret-browser",
            StatusCode::FORBIDDEN,
        ),
    ] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/auth/callback?{query}"))
                .header(
                    "cookie",
                    if browser.is_empty() {
                        "__Host-grading-session=secret-session".to_owned()
                    } else {
                        format!("__Host-grading-login={browser}; __Host-grading-session=secret-session")
                    },
                    )
                    .body(Body::empty())?,
            )
            .with_subscriber(subscriber.clone())
            .await?;
        assert_eq!(response.status(), expected);
        let body = to_bytes(response.into_body(), 4096).await?;
        assert!(!String::from_utf8_lossy(&body).contains("secret-"));
    }
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/secret-path?token=secret-query")
                .header("authorization", "Bearer secret-token")
                .body(Body::empty())?,
        )
        .with_subscriber(subscriber.clone())
        .await?;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    assert!(Uuid::parse_str(response.headers()["x-request-id"].to_str()?).is_ok());
    let logs = String::from_utf8(capture.0.lock().unwrap().clone())?;
    for stage in ["query_parse", "login_cookie", "login_state"] {
        assert!(logs.contains(stage), "missing callback stage {stage}");
    }
    for sensitive in ["secret-", "code=", "state=", "/auth/callback?"] {
        assert!(!logs.contains(sensitive));
    }
    assert!(logs.contains("request_id="));
    assert!(logs.contains("unmatched"));
    let unknown = identity::new_session(&pool, 200, "unknown-account", None).await?;
    let cookie = format!("__Host-grading-session={unknown}");
    let session = identity::session(&pool, &unknown).await?.unwrap();
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/")
                .header("cookie", &cookie)
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body = String::from_utf8(to_bytes(response.into_body(), 1024 * 1024).await?.to_vec())?;
    assert!(body.contains("No courses yet"));
    assert!(!body.contains("Create repository"));
    for origin in ["https://evil.example", "https://grading.example"] {
        let csrf = if origin.ends_with("evil.example") {
            session.csrf.as_str()
        } else {
            "forged"
        };
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/assignments/{}/repository", Uuid::new_v4()))
                    .header("cookie", &cookie)
                    .header("origin", origin)
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(Body::from(format!("csrf={csrf}")))?,
            )
            .await?;
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/admin")
                .header("cookie", &cookie)
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    identity::admin_change(&admin, 200, true, false, "test-root", "HTTP admin test").await?;
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/admin")
                .header("cookie", &cookie)
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    identity::admin_change(
        &admin,
        200,
        false,
        true,
        "test-root",
        "HTTP revocation test",
    )
    .await?;
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri("/admin")
                .header("cookie", &cookie)
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let other_run: Option<Uuid> = sqlx::query_scalar("SELECT id FROM grading_runs LIMIT 1")
        .fetch_optional(&pool)
        .await?;
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/runs/{}/report",
                    other_run.unwrap_or_else(Uuid::new_v4)
                ))
                .header("cookie", &cookie)
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    shared_runner_boundaries(&pool, &admin, &app, &worker).await?;
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/internal/lease")
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    let response = worker
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/internal/lease")
                .header("content-type", "application/json")
                .body(Body::from("{\"profiles\":[]}"))?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
    let delivery = Uuid::new_v4().to_string();
    let body = "{\"zen\":\"fixture\"}";
    for pass in 0..3 {
        let payload = if pass == 2 { "changed" } else { body };
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/webhooks/github")
                    .header("x-github-delivery", &delivery)
                    .header("x-github-event", "ping")
                    .header("x-hub-signature-256", signature(payload))
                    .body(Body::from(payload))?,
            )
            .await?;
        assert_eq!(
            response.status(),
            [StatusCode::ACCEPTED, StatusCode::OK, StatusCode::CONFLICT][pass]
        );
    }
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/webhooks/github")
                .header("x-github-delivery", Uuid::new_v4().to_string())
                .header("x-github-event", "ping")
                .header("x-hub-signature-256", signature(body))
                .body(Body::from("forged"))?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    let response = app
        .oneshot(Request::builder().uri("/login").body(Body::empty())?)
        .await?;
    assert_eq!(response.status(), StatusCode::SEE_OTHER);
    let cookie = response.headers()["set-cookie"].to_str()?;
    assert!(cookie.contains("Secure; HttpOnly; SameSite=Lax"));
    let redirect = response.headers()["location"].to_str()?;
    assert!(redirect.contains("code_challenge_method=S256"));
    assert!(redirect.contains("allow_signup=false"));
    Ok(())
}

fn signature(body: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(&[7; 32]).unwrap();
    mac.update(body.as_bytes());
    format!("sha256={}", hex::encode(mac.finalize().into_bytes()))
}

async fn shared_runner_boundaries(
    pool: &PgPool,
    admin: &PgPool,
    app: &axum::Router,
    internal: &axum::Router,
) -> Result<()> {
    let row: Option<(Uuid, Uuid)> = sqlx::query_as("SELECT g.id,g.submission_id FROM grading_runs g JOIN assignment_revisions v ON v.digest=g.revision_digest WHERE g.public_run_id IS NOT NULL AND v.grader_source_digest IS NOT NULL LIMIT 1").fetch_optional(pool).await?;
    let Some((private_run, submission)) = row else {
        anyhow::bail!(
            "shared runner fixture missing; run tests/database.sh for the complete fixture"
        );
    };
    let student = identity::new_session(pool, 8001, "user-8001", None).await?;
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/runs/{private_run}/report"))
                .header("cookie", format!("__Host-grading-session={student}"))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 65536).await?;
    let report: serde_json::Value = serde_json::from_slice(&body)?;
    assert_eq!(report["points"], 9);
    assert_eq!(report["public_points"], 18);
    assert!(!String::from_utf8_lossy(&body).contains("private-input-marker"));
    assert!(report.get("score").is_none());
    let administrator = identity::new_session(pool, 200, "administrator", None).await?;
    identity::admin_change(admin, 200, true, false, "fixture", "private report review").await?;
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/runs/{private_run}/report"))
                .header("cookie", format!("__Host-grading-session={administrator}"))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(
        String::from_utf8_lossy(&to_bytes(response.into_body(), 65536).await?)
            .contains("private-input-marker")
    );
    identity::admin_change(admin, 200, false, true, "fixture", "review complete").await?;

    let operator = PgPool::connect(&std::env::var("TEST_OPERATOR_DATABASE_URL")?).await?;
    let token = grading_core::security::token();
    sqlx::query("INSERT INTO workers(id,token_hash,profiles,resource_caps) VALUES('snapshot-worker',$1,ARRAY['registered-v1'],'{\"cpu\":10,\"memory_gib\":32,\"storage_gib\":64}')")
        .bind(grading_core::security::digest(&token)).execute(&operator).await?;
    let identity = grading_store::grading::authenticate(pool, &token).await?;
    grading_store::grading::enqueue_run(pool, submission, true).await?;
    let lease = grading_store::grading::lease(pool, &identity, &identity.profiles)
        .await?
        .unwrap();
    for (lease_token, authorized, expected) in [
        (lease.lease_token, false, StatusCode::UNAUTHORIZED),
        (Uuid::new_v4(), true, StatusCode::NOT_FOUND),
        (lease.lease_token, true, StatusCode::OK),
    ] {
        let mut request = Request::builder().uri(format!(
            "/internal/tasks/{}/grader?lease_token={lease_token}",
            lease.task_id
        ));
        if authorized {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        let response = internal
            .clone()
            .oneshot(request.body(Body::empty())?)
            .await?;
        assert_eq!(response.status(), expected);
        if expected == StatusCode::OK {
            let bytes = to_bytes(response.into_body(), 65536).await?;
            assert_eq!(
                grading_core::security::digest(&bytes),
                lease
                    .revision
                    .grader
                    .as_ref()
                    .unwrap()
                    .source_digest
                    .as_ref()
                    .unwrap()
                    .as_str()
            );
            let snapshot: grading_core::integrity::Snapshot = serde_json::from_slice(&bytes)?;
            assert_eq!(
                snapshot.files["private/cases.json"].bytes()?,
                b"private-test-marker"
            );
        }
    }
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/internal/tasks/{}/grader?lease_token={}",
                    lease.task_id, lease.lease_token
                ))
                .header("cookie", format!("__Host-grading-session={student}"))
                .body(Body::empty())?,
        )
        .await?;
    assert_eq!(response.status(), StatusCode::NOT_FOUND);
    Ok(())
}
