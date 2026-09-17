//! Exercise updates preserve student Git trees and immutable historical runs.
use anyhow::Result;
use chrono::{Duration, Utc};
use grading_core::{
    config::CourseConfig,
    integrity::{Manifest, ProtectedFile},
    protocol::{Grader, Revision},
};
use grading_store::{
    courses,
    exercises::{self, Publication},
    grading,
};
use sqlx::PgPool;
use std::collections::BTreeMap;
use uuid::Uuid;

pub async fn publication_rollout_and_permissions() -> Result<()> {
    let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
        return Ok(());
    };
    let owner = PgPool::connect(&url).await?;
    let operator = PgPool::connect(&std::env::var("TEST_OPERATOR_DATABASE_URL")?).await?;
    let web = PgPool::connect(&std::env::var("TEST_WEB_DATABASE_URL")?).await?;
    let mut config = CourseConfig::parse(include_str!("../../../../tests/fixtures/course.toml"))?;
    config.course.id = "registered-course".into();
    let mut assignment = config.assignments["echo"].clone();
    assignment.opens_at = Utc::now() - Duration::hours(1);
    assignment.deadline = Utc::now() + Duration::hours(1);
    assignment.execution_profile = "registered-v1".into();
    config.assignments.clear();
    courses::apply_course(&operator, &config, &"a".repeat(40), &[], false, "fixture").await?;
    let first = Revision {
        course_id: config.course.id.clone(),
        assignment_id: "echo".into(),
        manifest: Manifest {
            schema_version: 1,
            template_revision: assignment.template_revision.clone(),
            editable: vec!["src/".into()],
            files: BTreeMap::from([(
                "tests/cases.toml".into(),
                ProtectedFile {
                    mode: "100644".into(),
                    sha256: "a".repeat(64),
                },
            )]),
        },
        assignment,
        tests: super::toml_suite()?,
        grader: Some(Grader {
            source_digest: None,
            workflow: None,
            tests: vec![grading_core::protocol::PrivateTest {
                id: "private-one".into(),
                stdin: "private input".into(),
                stdout: "private answer".into(),
            }],
            repository: "org/private".into(),
            revision: "b".repeat(40),
            image: format!("registry.example/grading/checker@sha256:{}", "c".repeat(64)),
        }),
    };
    let publication = |revision, expected, existing, dry_run| Publication {
        revision,
        expected,
        existing,
        dry_run,
        operator: "fixture",
        reason: "test update",
    };
    assert!(
        exercises::publish(&web, publication(&first, None, false, false))
            .await
            .is_err()
    );
    exercises::publish(&operator, publication(&first, None, false, true)).await?;
    assert!(
        exercises::current(&operator, &config.course.id, "echo")
            .await?
            .is_none()
    );
    exercises::publish(&operator, publication(&first, None, false, false)).await?;
    assert!(
        exercises::publish(&operator, publication(&first, None, false, false))
            .await
            .is_err()
    );
    let students: Vec<_> = [8001, 8002]
        .into_iter()
        .map(|id| courses::ResolvedStudent {
            row: courses::RosterRow {
                student_id: format!("registered-{id}"),
                name: "Fixture".into(),
                github_username: format!("user-{id}"),
            },
            github_id: id,
            login: format!("user-{id}"),
        })
        .collect();
    courses::import_roster(&operator, &config.course.id, &students, false, "fixture").await?;
    let assignment_id: Uuid = sqlx::query_scalar("SELECT id FROM assignments WHERE course_id=$1")
        .bind(&config.course.id)
        .fetch_one(&owner)
        .await?;
    let repository = courses::request_repository(&web, 8001, assignment_id).await?;
    let first_hash = first.digest()?;
    let mut second = first.clone();
    second.assignment.template_revision = "d".repeat(40);
    second.manifest.template_revision = second.assignment.template_revision.clone();
    second
        .manifest
        .files
        .get_mut("tests/cases.toml")
        .unwrap()
        .sha256 = "e".repeat(64);
    second.grader.as_mut().unwrap().revision = "f".repeat(40);
    exercises::publish(
        &operator,
        publication(&second, Some(&first_hash), false, false),
    )
    .await?;
    let pins: (String, String) = sqlx::query_as(
        "SELECT revision_digest,grading_revision FROM student_repositories WHERE id=$1",
    )
    .bind(repository)
    .fetch_one(&owner)
    .await?;
    assert_eq!(pins, (first_hash.clone(), first_hash.clone()));
    let new_repository = courses::request_repository(&web, 8002, assignment_id).await?;
    assert_eq!(
        courses::repository(&web, new_repository)
            .await?
            .revision_digest,
        second.digest()?
    );
    let second_hash = second.digest()?;
    exercises::publish(
        &operator,
        publication(&second, Some(&second_hash), true, false),
    )
    .await?;
    let (template_pin,definition): (String,serde_json::Value) = sqlx::query_as("SELECT s.revision_digest,r.definition FROM student_repositories s JOIN assignment_revisions r ON r.digest=s.grading_revision WHERE s.id=$1").bind(repository).fetch_one(&owner).await?;
    let hybrid: Revision = serde_json::from_value(definition)?;
    assert_eq!(template_pin, first_hash);
    assert_eq!(
        hybrid.manifest.template_revision,
        first.manifest.template_revision
    );
    assert_eq!(
        hybrid.manifest.files["tests/cases.toml"].sha256,
        first.manifest.files["tests/cases.toml"].sha256
    );
    assert_eq!(hybrid.grader.unwrap().revision, "f".repeat(40));
    // New attempts bind to the new grading revision; previous attempts keep their pin.
    let artifacts =
        grading_store::artifacts::Artifacts::new(std::env::var("TEST_ARTIFACT_ROOT")?).await?;
    let artifact = artifacts
        .put(
            &owner,
            "source",
            b"registered exercise retained source fixture",
        )
        .await?;
    let mut tx = owner.begin().await?;
    let submission = grading_store::submissions::record(
        &mut tx,
        repository,
        &"a".repeat(40),
        Utc::now(),
        "registration",
        None,
    )
    .await?
    .unwrap();
    tx.commit().await?;
    sqlx::query("UPDATE submissions SET source_digest=$2 WHERE id=$1")
        .bind(submission)
        .bind(&artifact)
        .execute(&owner)
        .await?;
    let run = grading::enqueue_run(&web, submission, false).await?;
    let run_pin: String =
        sqlx::query_scalar("SELECT revision_digest FROM grading_runs WHERE id=$1")
            .bind(run)
            .fetch_one(&owner)
            .await?;
    let mut third = second.clone();
    third.grader.as_mut().unwrap().revision = "1".repeat(40);
    third.tests.tests.clear();
    third.grader.as_mut().unwrap().tests.clear();
    third.grader.as_mut().unwrap().workflow = Some(grading_core::protocol::Workflow {
        public_command: vec!["/bin/public".into()],
        private_command: Some(vec!["/bin/private".into()]),
    });
    let grader_snapshot = grading_core::integrity::Snapshot {
        sha: third.grader.as_ref().unwrap().revision.clone(),
        files: BTreeMap::from([(
            "private/cases.json".into(),
            grading_core::integrity::Blob {
                mode: "100644".into(),
                data: "cHJpdmF0ZS10ZXN0LW1hcmtlcg==".into(),
            },
        )]),
    };
    let grader_digest = artifacts
        .put(&operator, "source", &serde_json::to_vec(&grader_snapshot)?)
        .await?;
    third.grader.as_mut().unwrap().source_digest = Some(grader_digest.clone());
    third.grader.as_mut().unwrap().image = third.assignment.image.clone();
    exercises::publish(
        &operator,
        publication(&third, Some(&second_hash), true, false),
    )
    .await?;
    let retained: String =
        sqlx::query_scalar("SELECT grader_source_digest FROM assignment_revisions WHERE digest=$1")
            .bind(third.digest()?)
            .fetch_one(&owner)
            .await?;
    assert_eq!(retained, grader_digest);
    assert!(
        sqlx::query("DELETE FROM artifacts WHERE digest=$1")
            .bind(&grader_digest)
            .execute(&owner)
            .await
            .is_err()
    );
    let regrade = grading::enqueue_run(&web, submission, true).await?;
    let old_pin: String =
        sqlx::query_scalar("SELECT revision_digest FROM grading_runs WHERE id=$1")
            .bind(run)
            .fetch_one(&owner)
            .await?;
    let new_pin: String =
        sqlx::query_scalar("SELECT revision_digest FROM grading_runs WHERE id=$1")
            .bind(regrade)
            .fetch_one(&owner)
            .await?;
    assert_eq!(old_pin, run_pin);
    assert_ne!(new_pin, run_pin);
    let token = grading_core::security::token();
    sqlx::query("INSERT INTO workers(id,token_hash,profiles,resource_caps) VALUES('registered-worker',$1,ARRAY['registered-v1'],$2)").bind(grading_core::security::digest(&token)).bind(serde_json::to_value(&third.assignment.resources)?).execute(&operator).await?;
    let worker = grading::authenticate(&web, &token).await?;
    for _ in 0..2 {
        let lease = grading::lease(&web, &worker, &worker.profiles)
            .await?
            .unwrap();
        assert!(lease.baseline.is_none());
        let scripted = lease.revision.grader.as_ref().unwrap().workflow.is_some();
        let result = grading_core::protocol::RunResult {
            logs: vec![
                grading_core::protocol::RunLog {
                    student_visible: true,
                    text: "compiler-feedback <script>alert(1)</script>".into(),
                },
                grading_core::protocol::RunLog {
                    student_visible: false,
                    text: "controller-private-marker".into(),
                },
            ],
            schema_version: 1,
            lease_token: lease.lease_token,
            run_id: lease.run_id,
            sha: lease.sha.clone(),
            revision_digest: lease.revision_digest.clone(),
            image: lease.revision.assignment.image.clone(),
            resources: lease.revision.assignment.resources.clone(),
            status: grading_core::protocol::RunStatus::Completed,
            tests: lease
                .revision
                .tests
                .tests
                .iter()
                .map(|t| grading_core::protocol::TestResult {
                    id: t.id.clone(),
                    passed: true,
                    log: String::new(),
                })
                .collect(),
            findings: vec![],
            private: None,
            private_tests: vec![],
            score: scripted.then_some(grading_core::protocol::ScriptScore {
                schema_version: 1,
                points: 18,
                invalidated: false,
                reason: String::new(),
            }),
        };
        grading::accept(&web, &artifacts, &worker, lease.task_id, &result).await?;
    }
    let before = grading::enqueue_private(
        &operator,
        &config.course.id,
        "echo",
        "fixture",
        "private review",
    )
    .await?;
    assert!(before.iter().all(|(_, run, _)| run.is_none()));
    let private_id = Uuid::new_v4();
    assert!(sqlx::query("INSERT INTO grading_runs(id,submission_id,revision_digest,attempt,public_run_id) VALUES($1,$2,$3,3,$4)").bind(private_id).bind(submission).bind(&new_pin).bind(regrade).execute(&web).await.is_err());
    assert!(sqlx::query("INSERT INTO grading_runs(id,submission_id,revision_digest,attempt,public_run_id) VALUES($1,$2,$3,3,$4)").bind(private_id).bind(submission).bind(&new_pin).bind(regrade).execute(&owner).await.is_err());
    // Advance the effective deadline in this fixture without changing immutable revisions.
    sqlx::query("INSERT INTO extensions(repository_id,deadline,reason) VALUES($1,now()-interval '1 second','simulated elapsed deadline')").bind(repository).execute(&owner).await?;
    let scheduled = grading::enqueue_private(
        &operator,
        &config.course.id,
        "echo",
        "fixture",
        "private review",
    )
    .await?;
    let private_run = scheduled
        .iter()
        .find(|(id, _, _)| *id == repository)
        .unwrap()
        .1
        .unwrap();
    assert!(
        scheduled
            .iter()
            .find(|(id, _, _)| *id == new_repository)
            .unwrap()
            .1
            .is_none()
    );
    let again = grading::enqueue_private(
        &operator,
        &config.course.id,
        "echo",
        "fixture",
        "private review",
    )
    .await?;
    assert_eq!(
        again.iter().find(|(id, _, _)| *id == repository).unwrap().1,
        Some(private_run)
    );
    let lease = grading::lease(&web, &worker, &worker.profiles)
        .await?
        .unwrap();
    let baseline = lease.baseline.as_ref().unwrap();
    assert_eq!(baseline.run_id, regrade);
    assert_eq!(baseline.points, 18);
    let result = grading_core::protocol::RunResult {
        logs: vec![grading_core::protocol::RunLog {
            student_visible: false,
            text: "private-grader-log-marker".into(),
        }],
        schema_version: 1,
        lease_token: lease.lease_token,
        run_id: lease.run_id,
        sha: lease.sha.clone(),
        revision_digest: lease.revision_digest.clone(),
        image: lease.revision.assignment.image.clone(),
        resources: lease.revision.assignment.resources.clone(),
        status: grading_core::protocol::RunStatus::Completed,
        tests: vec![],
        findings: vec![],
        private: None,
        private_tests: vec![],
        score: Some(grading_core::protocol::ScriptScore {
            schema_version: 1,
            points: baseline.points / 2,
            invalidated: false,
            reason: "private-input-marker: additional tests failed; halved public score".into(),
        }),
    };
    grading::accept(&web, &artifacts, &worker, lease.task_id, &result).await?;
    grading::accept(&web, &artifacts, &worker, lease.task_id, &result).await?;
    let (public, official): (i32, i32) =
        sqlx::query_as("SELECT public_points,points FROM grading_runs WHERE id=$1")
            .bind(private_run)
            .fetch_one(&owner)
            .await?;
    assert_eq!((public, official), (18, 9));
    let unchanged: i32 = sqlx::query_scalar("SELECT points FROM grading_runs WHERE id=$1")
        .bind(regrade)
        .fetch_one(&owner)
        .await?;
    assert_eq!(unchanged, 18);
    let mut conflict = result.clone();
    conflict.score.as_mut().unwrap().points = 0;
    assert!(
        grading::accept(&web, &artifacts, &worker, lease.task_id, &conflict)
            .await
            .is_err()
    );
    assert!(
        sqlx::query("UPDATE grading_runs SET public_run_id=$2 WHERE id=$1")
            .bind(run)
            .bind(private_run)
            .execute(&owner)
            .await
            .is_err()
    );
    assert!(sqlx::query("INSERT INTO grading_runs(id,submission_id,revision_digest,attempt,public_run_id) VALUES($1,$2,$3,4,$4)").bind(Uuid::new_v4()).bind(submission).bind(new_pin).bind(private_run).execute(&operator).await.is_err());
    let before = BTreeMap::from([(
        config.course.id.clone(),
        exercises::catalog(&operator, &config.course.id).await?,
    )]);
    let mut extra = third.clone();
    extra.assignment_id = "extra".into();
    assert!(
        exercises::apply_set(
            &operator,
            &before,
            &[
                publication(&extra, None, false, false),
                publication(&third, None, false, false),
            ],
            "fixture",
            "atomic failure",
            false
        )
        .await
        .is_err()
    );
    assert!(
        exercises::current(&operator, &config.course.id, "extra")
            .await?
            .is_none()
    );
    exercises::apply_set(&operator, &before, &[], "fixture", "preview retire", true).await?;
    assert_eq!(
        exercises::catalog(&operator, &config.course.id).await?,
        before[&config.course.id]
    );
    exercises::apply_set(
        &operator,
        &before,
        &[],
        "fixture",
        "confirmed retire",
        false,
    )
    .await?;
    assert!(
        exercises::catalog(&operator, &config.course.id)
            .await?
            .iter()
            .all(|e| e.archived)
    );
    assert!(
        courses::request_repository(&web, 8002, assignment_id)
            .await
            .is_err()
    );
    assert!(!courses::dashboard(&web, 8001).await?.is_empty());
    assert_eq!(
        sqlx::query_scalar::<_, i32>("SELECT points FROM grading_runs WHERE id=$1")
            .bind(private_run)
            .fetch_one(&owner)
            .await?,
        9
    );
    assert!(
        exercises::apply_set(&operator, &before, &[], "fixture", "stale preview", false)
            .await
            .is_err()
    );
    let archived = BTreeMap::from([(
        config.course.id.clone(),
        exercises::catalog(&operator, &config.course.id).await?,
    )]);
    exercises::apply_set(
        &operator,
        &archived,
        &[publication(&third, Some(&third.digest()?), false, false)],
        "fixture",
        "restore exercise",
        false,
    )
    .await?;
    assert!(
        exercises::catalog(&operator, &config.course.id)
            .await?
            .iter()
            .all(|e| !e.archived)
    );
    Ok(())
}
