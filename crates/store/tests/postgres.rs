//! Integration coverage runs against the disposable SCRAM database in tests/database.sh.
#[path = "support/exercises.rs"]
mod exercises;
use anyhow::Result;
use chrono::{Duration, Utc};
use grading_core::{
    config::CourseConfig,
    integrity::{Blob, Manifest, Snapshot},
    protocol::{Grader, Revision, RunResult, RunStatus, ScriptScore, Workflow},
    security,
};
use grading_store::{
    artifacts::Artifacts,
    courses::{self, ResolvedStudent, RosterRow},
    grading, identity, queue, submissions,
};
use serde_json::json;
use sqlx::PgPool;
use std::collections::BTreeMap;
use uuid::Uuid;

#[tokio::test]
async fn database_invariants_and_recovery() -> Result<()> {
    let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
        eprintln!("PostgreSQL integration skipped; use just test");
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    let web = PgPool::connect(&std::env::var("TEST_WEB_DATABASE_URL")?).await?;
    let credentials = tempfile::NamedTempFile::new()?;
    tokio::fs::write(credentials.path(), std::env::var("TEST_WEB_DATABASE_URL")?).await?;
    let bounded_pool = grading_store::connect(credentials.path()).await?;
    for (setting, expected) in [
        ("statement_timeout", "15s"),
        ("lock_timeout", "3s"),
        ("idle_in_transaction_session_timeout", "30s"),
    ] {
        let actual: String = sqlx::query_scalar("SELECT current_setting($1)")
            .bind(setting)
            .fetch_one(&bounded_pool)
            .await?;
        assert_eq!(actual, expected);
    }
    bounded_pool.close().await;
    let admin = PgPool::connect(&std::env::var("TEST_ADMIN_DATABASE_URL")?).await?;
    let operator = PgPool::connect(&std::env::var("TEST_OPERATOR_DATABASE_URL")?).await?;
    assert!(
        sqlx::query("INSERT INTO admins(github_id) VALUES(77)")
            .execute(&web)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("SET ROLE grading_admin")
            .execute(&web)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("INSERT INTO admins(github_id) VALUES(77)")
            .execute(&operator)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("SELECT * FROM sessions")
            .execute(&admin)
            .await
            .is_err()
    );
    identity::admin_change(&admin, 100, true, false, "test-root", "initial grant").await?;
    assert!(
        identity::admin_change(&admin, 100, false, false, "test-root", "last admin")
            .await
            .is_err()
    );
    let raw = identity::new_session(&web, 100, "original-login", None).await?;
    assert!(identity::session(&web, &raw).await?.unwrap().admin);
    identity::admin_change(&admin, 101, true, false, "test-root", "second admin").await?;
    identity::admin_change(&admin, 100, false, false, "test-root", "revocation").await?;
    assert!(!identity::session(&web, &raw).await?.unwrap().admin);
    let rotated = identity::new_session(&web, 100, "renamed-login", Some(&raw)).await?;
    assert!(identity::session(&web, &raw).await?.is_none());
    assert_eq!(
        identity::session(&web, &rotated).await?.unwrap().login,
        "renamed-login"
    );
    assert!(submissions::admit_registration(&web, 100).await?);
    assert!(!submissions::admit_registration(&web, 100).await?);
    identity::begin_login(&web, "state", "browser", "verifier").await?;
    assert!(
        identity::consume_login(&web, "state", "other-browser")
            .await
            .is_err()
    );
    assert_eq!(
        identity::consume_login(&web, "state", "browser").await?,
        "verifier"
    );
    assert!(
        identity::consume_login(&web, "state", "browser")
            .await
            .is_err()
    );
    sqlx::query("UPDATE sessions SET expires_at=now()-interval '1 second' WHERE token_hash=$1")
        .bind(security::digest(&rotated))
        .execute(&web)
        .await?;
    assert!(identity::session(&web, &rotated).await?.is_none());

    for _ in 0..10 {
        identity::new_session(&web, 101, "session-cap", None).await?;
    }
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions WHERE github_id=101")
        .fetch_one(&pool)
        .await?;
    assert_eq!(count, 5);
    sqlx::query("UPDATE sessions SET expires_at=now()-interval '1 second' WHERE github_id=101")
        .execute(&pool)
        .await?;
    identity::cleanup_expired(&web).await?;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM sessions WHERE github_id=101")
        .fetch_one(&pool)
        .await?;
    assert_eq!(count, 0);
    let config = CourseConfig::parse(include_str!("../../../tests/fixtures/course.toml"))?;
    let mut assignment: grading_core::config::Assignment =
        toml::from_str(include_str!("../../../tests/fixtures/assignment.toml"))?;
    assignment.opens_at = Utc::now() - Duration::hours(1);
    assignment.deadline = Utc::now() + Duration::hours(1);
    let source = Snapshot {
        sha: "a".repeat(40),
        files: BTreeMap::from([(
            "tests/public.json".into(),
            Blob {
                mode: "100644".into(),
                data: "".into(),
            },
        )]),
    };
    let manifest = Manifest::generate(&source, vec!["src/".into()])?;
    let grader_digest = Artifacts::new(std::env::var("TEST_ARTIFACT_ROOT")?)
        .await?
        .put(&operator, "source", b"grader fixture")
        .await?;
    let revision = Revision {
        tests: Default::default(),
        grader: Some(Grader {
            repository: "org/grader".into(),
            revision: "c".repeat(40),
            image: assignment.image.clone(),
            source_digest: Some(grader_digest),
            workflow: Some(Workflow {
                public_command: vec!["/bin/python3".into(), "/grader/public.py".into()],
                private_command: None,
            }),
        }),
        course_id: config.course.id.clone(),
        assignment_id: "echo".into(),
        assignment: assignment.clone(),
        manifest,
    };
    courses::apply_course(
        &operator,
        &config,
        &format!("sha256:{}", "b".repeat(64)),
        true,
        "test-root",
    )
    .await?;
    let count: i64 = sqlx::query_scalar("SELECT count(*) FROM courses")
        .fetch_one(&pool)
        .await?;
    assert_eq!(count, 0);
    courses::apply_course(
        &operator,
        &config,
        &format!("sha256:{}", "b".repeat(64)),
        false,
        "test-root",
    )
    .await?;
    courses::apply_course(
        &operator,
        &config,
        &format!("sha256:{}", "b".repeat(64)),
        false,
        "test-root",
    )
    .await?;
    grading_store::exercises::publish(
        &operator,
        grading_store::exercises::Publication {
            revision: &revision,
            expected: None,
            existing: false,
            dry_run: false,
            operator: "fixture",
            reason: "setup",
        },
    )
    .await?;
    assert!(
        sqlx::query("UPDATE assignment_revisions SET max_points=1000")
            .execute(&pool)
            .await
            .is_err()
    );
    let student = ResolvedStudent {
        row: RosterRow {
            student_id: "fictional-1".into(),
            name: "Example Student".into(),
            github_username: "renamed-login".into(),
        },
        github_id: 100,
        login: "renamed-login".into(),
    };
    courses::import_roster(
        &operator,
        &config.course.id,
        std::slice::from_ref(&student),
        false,
        "test-root",
    )
    .await?;
    courses::import_roster(&operator, &config.course.id, &[], false, "test-root").await?;
    let mut hijack = student.clone();
    hijack.github_id = 999;
    assert!(
        courses::import_roster(&operator, &config.course.id, &[hijack], false, "test-root")
            .await
            .is_err()
    );
    let mut alias = student.clone();
    alias.row.student_id = "another-student".into();
    assert!(
        courses::import_roster(&operator, &config.course.id, &[alias], false, "test-root")
            .await
            .is_err()
    );
    assert!(courses::dashboard(&web, 999).await?.is_empty());
    let assignment: Uuid = sqlx::query_scalar("SELECT id FROM assignments WHERE slug='echo'")
        .fetch_one(&pool)
        .await?;
    let (first, second) = tokio::join!(
        courses::request_repository(&web, 100, assignment),
        courses::request_repository(&web, 100, assignment)
    );
    let repository = first?;
    assert_eq!(repository, second?);
    assert!(
        courses::request_repository(&web, 999, assignment)
            .await
            .is_err()
    );

    let (task_a, task_b) = tokio::join!(
        queue::lease(&web, "control-a", &["provision"]),
        queue::lease(&web, "control-b", &["provision"])
    );
    let task_a = task_a?;
    let task_b = task_b?;
    assert_ne!(task_a.is_some(), task_b.is_some());
    let task = task_a.or(task_b).unwrap();
    assert!(
        queue::heartbeat(&web, task.id, Uuid::new_v4(), "control-a")
            .await
            .is_err()
    );
    sqlx::query("UPDATE tasks SET lease_until=now()-interval '1 second' WHERE id=$1")
        .bind(task.id)
        .execute(&pool)
        .await?;
    let retry = queue::lease(&web, "replacement", &["provision"])
        .await?
        .unwrap();
    assert_ne!(task.lease_token, retry.lease_token);
    assert!(queue::finish(&web, &task).await.is_err());
    queue::finish(&web, &retry).await?;

    let accepted_time = Utc::now();
    let mut tx = web.begin().await?;
    let submission = submissions::record(
        &mut tx,
        repository,
        &"c".repeat(40),
        accepted_time,
        "registration",
        None,
    )
    .await?
    .unwrap();
    let duplicate = submissions::record(
        &mut tx,
        repository,
        &"c".repeat(40),
        Utc::now(),
        "registration",
        None,
    )
    .await?;
    assert_eq!(duplicate, Some(submission));
    let late = submissions::record(
        &mut tx,
        repository,
        &"d".repeat(40),
        accepted_time + Duration::days(1),
        "webhook",
        None,
    )
    .await?;
    assert!(late.is_none());
    tx.commit().await?;
    let mut burst = web.begin().await?;
    for index in 0..40 {
        submissions::record(
            &mut burst,
            repository,
            &format!("{index:040x}"),
            Utc::now(),
            "registration",
            None,
        )
        .await?;
    }
    let pending: i64 =
        sqlx::query_scalar("SELECT count(*) FROM tasks WHERE kind='snapshot' AND status='pending'")
            .fetch_one(&mut *burst)
            .await?;
    assert_eq!(pending, 1);
    burst.rollback().await?;
    let directory = tempfile::tempdir()?;
    let artifact_root = std::env::var_os("TEST_ARTIFACT_ROOT")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| directory.path().to_owned());
    let artifacts = Artifacts::new(artifact_root).await?;
    let mut submitted = source.clone();
    submitted.sha = "c".repeat(40);
    let hash = artifacts
        .put(&web, "source", &serde_json::to_vec(&submitted)?)
        .await?;
    sqlx::query("UPDATE submissions SET source_digest=$2 WHERE id=$1")
        .bind(submission)
        .bind(&hash)
        .execute(&web)
        .await?;
    let run = grading::enqueue_run(&web, submission, false).await?;
    assert_eq!(grading::enqueue_run(&web, submission, false).await?, run);
    let raw_worker = security::token();
    sqlx::query("INSERT INTO workers(id,token_hash,profiles,resource_caps) VALUES('test-worker',$1,ARRAY['registered-v1'],$2)").bind(security::digest(&raw_worker)).bind(serde_json::to_value(&revision.assignment.resources)?).execute(&operator).await?;
    let worker = grading::authenticate(&web, &raw_worker).await?;
    assert!(
        grading::lease(&web, &worker, &["unapproved".into()])
            .await?
            .is_none()
    );
    let lease = grading::lease(&web, &worker, &worker.profiles)
        .await?
        .unwrap();
    assert!(
        grading::lease(&web, &worker, &worker.profiles)
            .await?
            .is_none()
    );
    assert!(
        grading::owned_lease(&web, &worker, lease.task_id, Uuid::new_v4(), false)
            .await
            .is_err()
    );
    let result = RunResult {
        logs: vec![],
        score: Some(ScriptScore {
            schema_version: 1,
            points: 10,
            invalidated: false,
            reason: String::new(),
        }),
        schema_version: 1,
        lease_token: lease.lease_token,
        run_id: run,
        sha: lease.sha.clone(),
        revision_digest: lease.revision_digest.clone(),
        image: revision.assignment.image.clone(),
        resources: revision.assignment.resources.clone(),
        status: RunStatus::Completed,
        findings: vec![],
    };
    let mut forged = result.clone();
    forged.sha = "f".repeat(40);
    assert!(
        grading::accept(&web, &artifacts, &worker, lease.task_id, &forged)
            .await
            .is_err()
    );
    forged = result.clone();
    forged.score.as_mut().unwrap().points = 100_000;
    assert!(
        grading::accept(&web, &artifacts, &worker, lease.task_id, &forged)
            .await
            .is_err()
    );
    grading::accept(&web, &artifacts, &worker, lease.task_id, &result).await?;
    grading::accept(&web, &artifacts, &worker, lease.task_id, &result).await?;
    forged = result.clone();
    forged.score.as_mut().unwrap().points = 20;
    assert!(
        grading::accept(&web, &artifacts, &worker, lease.task_id, &forged)
            .await
            .is_err()
    );
    let score: Option<i32> = sqlx::query_scalar("SELECT points FROM grading_runs WHERE id=$1")
        .bind(run)
        .fetch_one(&pool)
        .await?;
    assert_eq!(score, Some(10));
    let rows = courses::dashboard(&web, 100).await?;
    assert_eq!(rows[0].points, Some(10));
    assert_eq!(rows[0].sha.as_deref(), Some("c".repeat(40).as_str()));
    assert!(
        submissions::override_grade(&operator, repository, 21, "test-root", "invalid")
            .await
            .is_err()
    );
    submissions::override_grade(&operator, repository, 12, "test-root", "manual correction")
        .await?;
    assert_eq!(
        courses::dashboard(&web, 100).await?[0].override_points,
        Some(12)
    );

    sqlx::query("INSERT INTO extensions(repository_id,deadline,reason) VALUES($1,$2,'test clock')")
        .bind(repository)
        .bind(Utc::now() - Duration::seconds(1))
        .execute(&operator)
        .await?;
    submissions::close(&operator, repository).await?;
    submissions::close(&operator, repository).await?;
    let closed = courses::repository(&web, repository).await?;
    assert!(closed.closure_due);
    assert_eq!(closed.final_submission_id, Some(submission));
    assert!(closed.locked_at.is_none());
    let mut tx = web.begin().await?;
    assert!(
        submissions::record(
            &mut tx,
            repository,
            &"e".repeat(40),
            Utc::now(),
            "registration",
            None
        )
        .await?
        .is_none()
    );
    tx.commit().await?;
    assert_eq!(
        courses::repository(&web, repository)
            .await?
            .final_submission_id,
        Some(submission)
    );
    assert!(
        submissions::extend(
            &operator,
            repository,
            Utc::now() + Duration::days(2),
            "test-root",
            "closed"
        )
        .await
        .is_err()
    );
    assert!(
        sqlx::query("UPDATE submission_events SET received_at=now()")
            .execute(&pool)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("DELETE FROM audit_events")
            .execute(&pool)
            .await
            .is_err()
    );
    assert_eq!(artifacts.get(&hash).await?, serde_json::to_vec(&submitted)?);
    assert!(artifacts.get("../../passwd").await.is_err());
    let mut tx = pool.begin().await?;
    queue::enqueue(
        &mut tx,
        "lock",
        json!({"repository_id":repository}),
        "failed-fixture",
        0,
    )
    .await?;
    tx.commit().await?;
    sqlx::query("UPDATE tasks SET status='leased',attempts=8,lease_until=now()-interval '1 second' WHERE dedup_key='failed-fixture'").execute(&pool).await?;
    queue::expire_exhausted(&pool).await?;
    let status: String =
        sqlx::query_scalar("SELECT status FROM tasks WHERE dedup_key='failed-fixture'")
            .fetch_one(&pool)
            .await?;
    assert_eq!(status, "failed");
    queue::retry(
        &operator,
        sqlx::query_scalar("SELECT id FROM tasks WHERE dedup_key='failed-fixture'")
            .fetch_one(&pool)
            .await?,
        "test",
        "Recovered infrastructure",
    )
    .await?;
    let task = queue::lease(&pool, "terminal-test", &["lock"])
        .await?
        .unwrap();
    queue::fail_permanently(&pool, &task).await?;
    assert!(
        queue::retry(&web, task.id, "student", "unauthorized")
            .await
            .is_err()
    );
    let status: String = sqlx::query_scalar("SELECT status FROM tasks WHERE id=$1")
        .bind(task.id)
        .fetch_one(&pool)
        .await?;
    assert_eq!(status, "failed");
    exercises::publication_rollout_and_permissions().await?;
    Ok(())
}
