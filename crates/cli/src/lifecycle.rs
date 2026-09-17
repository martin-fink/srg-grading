//! Retriable GitHub tasks and daily synchronization with durable per-repository observations.
use anyhow::{Context, Result, ensure};
use chrono::Utc;
use grading_core::{integrity::Snapshot, protocol::Revision};
use grading_github::GitHub;
use grading_store::{
    artifacts::Artifacts,
    courses, grading,
    queue::{self, Task},
    submissions,
};
use serde_json::json;
use sqlx::PgPool;
use tracing::Instrument;
use uuid::Uuid;

pub struct ContextData {
    pub pool: PgPool,
    pub github: GitHub,
    pub artifacts: Artifacts,
}

use grading_core::diagnostics::Stage;

fn failure_details(error: &anyhow::Error) -> (&'static str, &'static str, Option<u16>) {
    let stage = error
        .downcast_ref::<Stage>()
        .map_or("task_process", |s| s.0);
    if let Some(status) = error.downcast_ref::<grading_github::client::ApiStatus>() {
        return (stage, "github_http", Some(status.0));
    }
    if error.downcast_ref::<reqwest::Error>().is_some() {
        return (stage, "github_transport", None);
    }
    for cause in error.chain() {
        let reason = match cause.to_string().as_str() {
            "organization base permission must be none" => "organization_base_permissions",
            "repository inherits team access; instructor review required" => {
                "inherited_team_access"
            }
            "refusing to adopt unverified repository" => "repository_ownership_or_marker",
            "template does not match approved manifest" => "template_integrity",
            "assignment expired before provisioning" | "assignment closed before invitation" => {
                "assignment_closed"
            }
            "repository identity changed" => "repository_identity_changed",
            "student has excessive permissions" => "excessive_student_permissions",
            "GitHub rate limit is active" => "github_rate_limit",
            "lease or assignment closed during seeding" => "lease_or_assignment_closed",
            _ => continue,
        };
        return (stage, reason, None);
    }
    let details = grading_store::diagnostics(error);
    (stage, details.reason, details.upstream_status)
}

fn payload_id(task: &Task, name: &str) -> Result<Uuid> {
    task.payload[name]
        .as_str()
        .context("task is missing an ID")?
        .parse()
        .map_err(Into::into)
}

pub async fn work(context: &ContextData, once: bool) -> Result<()> {
    let owner = format!("control-{}", Uuid::new_v4());
    loop {
        queue::expire_exhausted(&context.pool).await?;
        submissions::refill_snapshots(&context.pool).await?;
        if let Some(task) = queue::lease(
            &context.pool,
            &owner,
            &["provision", "snapshot", "publish", "lock"],
        )
        .await?
        {
            tracing::info!(task_id=%task.id, kind=%task.kind, attempt=task.attempts, "task leased");
            let outcome = {
                let processing = process(context, &task).instrument(
                    tracing::info_span!("background_task", task_id=%task.id, kind=%task.kind),
                );
                tokio::pin!(processing);
                let mut heartbeat = tokio::time::interval(std::time::Duration::from_secs(30));
                loop {
                    tokio::select! {
                        result=&mut processing=>break result,
                        _=heartbeat.tick()=>{if let Err(error)=queue::heartbeat(&context.pool,task.id,task.lease_token,&owner).await {break Err(error.context(Stage("task_heartbeat")));}}
                    }
                }
            };
            match outcome {
                Ok(()) => {
                    queue::finish(&context.pool, &task).await?;
                    tracing::info!(task_id=%task.id, kind=%task.kind, "task completed");
                    if task.kind == "provision" {
                        let repository =
                            courses::repository(&context.pool, payload_id(&task, "repository_id")?)
                                .await?;
                        if repository.state == "invitation_pending"
                            && !repository.closure_due
                            && repository.deadline > Utc::now()
                        {
                            sqlx::query("UPDATE tasks SET status='pending',attempts=0,available_at=now()+interval '5 minutes' WHERE id=$1 AND lease_token=$2 AND status='done'").bind(task.id).bind(task.lease_token).execute(&context.pool).await?;
                        }
                    }
                }
                Err(error) => {
                    let (stage, reason, upstream_status) = failure_details(&error);
                    tracing::warn!(task_id=%task.id,kind=%task.kind,attempt=task.attempts,stage,reason,upstream_status,"task failed; bounded retry scheduled");
                    let message = format!(
                        "stage={stage}; reason={reason}; upstream_status={upstream_status:?}"
                    );
                    queue::fail(&context.pool, &task, &message).await?;
                    if let Some(retry_at) = context.github.retry_at().await {
                        sqlx::query("UPDATE tasks SET available_at=GREATEST(available_at,$2) WHERE id=$1 AND lease_token=$3 AND status='pending'").bind(task.id).bind(retry_at).bind(task.lease_token).execute(&context.pool).await?;
                    }
                    if let Ok(id) = payload_id(&task, "repository_id") {
                        sqlx::query("UPDATE student_repositories SET last_error='Operation failed; retry scheduled' WHERE id=$1").bind(id).execute(&context.pool).await?;
                    }
                }
            }
        } else if once {
            break;
        } else {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        }
        if once {
            break;
        }
    }
    Ok(())
}

async fn process(context: &ContextData, task: &Task) -> Result<()> {
    let _repository_guard = if matches!(task.kind.as_str(), "provision" | "lock") {
        let mut connection = context.pool.acquire().await?.detach();
        sqlx::query("SELECT pg_advisory_lock(704317,hashtext($1))")
            .bind(payload_id(task, "repository_id")?.to_string())
            .execute(&mut connection)
            .await?;
        Some(connection)
    } else {
        None
    };
    match task.kind.as_str() {
        "provision" => provision(context, task).await,
        "snapshot" => snapshot(context, payload_id(task, "submission_id")?)
            .await
            .context(Stage("submission_snapshot")),
        "publish" => publish(context, payload_id(task, "run_id")?)
            .await
            .context(Stage("check_publish")),
        "lock" => lock(context, payload_id(task, "repository_id")?)
            .await
            .context(Stage("repository_lock")),
        _ => anyhow::bail!("unsupported control task"),
    }
}

async fn provision(context: &ContextData, task: &Task) -> Result<()> {
    let id = payload_id(task, "repository_id")?;
    let repository = courses::repository(&context.pool, id)
        .await
        .context(Stage("repository_load"))?;
    if repository.state == "ready" {
        return Ok(());
    }
    let revision: Revision =
        serde_json::from_value(repository.definition.clone()).context(Stage("revision_decode"))?;
    let full_name = format!("{}/{}", repository.organization, repository.name);
    if repository.state == "creating" {
        ensure!(
            !repository.closure_due && Utc::now() <= repository.deadline,
            "assignment expired before provisioning"
        );
        let source = context
            .github
            .snapshot(
                &revision.assignment.template,
                &revision.assignment.template_revision,
            )
            .await
            .context(Stage("template_fetch"))?;
        ensure!(
            revision.manifest.check(&source)?.is_empty(),
            "template does not match approved manifest"
        );
        let created = context
            .github
            .ensure_private_repository(
                &repository.organization,
                &repository.name,
                repository.provisioning_nonce,
            )
            .await
            .context(Stage("repository_create"))?;
        ensure!(
            repository.github_repo_id.is_none_or(|id| id == created.id),
            "repository identity changed"
        );
        sqlx::query("UPDATE student_repositories SET github_repo_id=$2 WHERE id=$1 AND (github_repo_id IS NULL OR github_repo_id=$2)").bind(id).bind(created.id).execute(&context.pool).await?;
        context
            .github
            .seed(&full_name, &revision.assignment.branch, &source)
            .await
            .context(Stage("repository_seed"))?;
        let updated=sqlx::query("UPDATE student_repositories SET state='invitation_pending',last_error=NULL WHERE id=$1 AND NOT closure_due AND EXISTS(SELECT 1 FROM tasks WHERE id=$2 AND lease_token=$3 AND status='leased' AND lease_until>now())")
            .bind(id).bind(task.id).bind(task.lease_token).execute(&context.pool).await?.rows_affected();
        ensure!(updated == 1, "lease or assignment closed during seeding");
    }
    let repository = courses::repository(&context.pool, id)
        .await
        .context(Stage("repository_load"))?;
    ensure!(
        !repository.closure_due && Utc::now() <= repository.deadline,
        "assignment closed before invitation"
    );
    context
        .github
        .verify_repository(
            &full_name,
            repository.github_repo_id.context("missing repository ID")?,
        )
        .await
        .context(Stage("repository_verify"))?;
    context
        .github
        .verify_no_teams(&full_name)
        .await
        .context(Stage("team_permissions"))?;
    let invitation = context
        .github
        .invite(&full_name, repository.github_id)
        .await
        .context(Stage("student_invitation"))?;
    let permission = context
        .github
        .permission(&full_name, repository.github_id)
        .await
        .context(Stage("student_permissions"))?;
    ensure!(
        permission == "none" || permission == "read" || permission == "write",
        "student has excessive permissions"
    );
    sqlx::query("UPDATE student_repositories SET state=$2,invitation_url=$3,last_error=NULL WHERE id=$1 AND NOT closure_due")
        .bind(id).bind(if permission=="write"{"ready"}else{"invitation_pending"}).bind(invitation).execute(&context.pool).await?;
    Ok(())
}

async fn snapshot(context: &ContextData, id: Uuid) -> Result<()> {
    let (repository_id, sha, existing): (Uuid, String, Option<String>) =
        sqlx::query_as("SELECT repository_id,sha,source_digest FROM submissions WHERE id=$1")
            .bind(id)
            .fetch_one(&context.pool)
            .await?;
    if existing.is_none() {
        let repository = courses::repository(&context.pool, repository_id).await?;
        let full_name = format!("{}/{}", repository.organization, repository.name);
        context
            .github
            .verify_repository(
                &full_name,
                repository.github_repo_id.context("missing repository ID")?,
            )
            .await?;
        let source: Snapshot = context.github.snapshot(&full_name, &sha).await?;
        let hash = context
            .artifacts
            .put(&context.pool, "source", &serde_json::to_vec(&source)?)
            .await?;
        sqlx::query(
            "UPDATE submissions SET source_digest=$2 WHERE id=$1 AND source_digest IS NULL",
        )
        .bind(id)
        .bind(hash)
        .execute(&context.pool)
        .await?;
    }
    grading::enqueue_run(&context.pool, id, false).await?;
    Ok(())
}

async fn publish(context: &ContextData, run: Uuid) -> Result<()> {
    let (repository,sha,status,points,existing):(Uuid,String,String,Option<i32>,Option<i64>)=sqlx::query_as("SELECT s.repository_id,s.sha,g.status,g.points,o.github_check_id FROM grading_runs g JOIN submissions s ON s.id=g.submission_id JOIN outbox o ON o.run_id=g.id WHERE g.id=$1")
        .bind(run).fetch_one(&context.pool).await?;
    let repository = courses::repository(&context.pool, repository).await?;
    let full_name = format!("{}/{}", repository.organization, repository.name);
    context
        .github
        .verify_repository(
            &full_name,
            repository.github_repo_id.context("missing repository ID")?,
        )
        .await?;
    let check = context
        .github
        .publish_check(&full_name, &sha, run, &status, points, existing)
        .await?;
    sqlx::query("UPDATE outbox SET github_check_id=$2,published_at=now() WHERE run_id=$1")
        .bind(run)
        .bind(check)
        .execute(&context.pool)
        .await?;
    Ok(())
}

async fn lock(context: &ContextData, id: Uuid) -> Result<()> {
    let repository = courses::repository(&context.pool, id).await?;
    ensure!(repository.closure_due, "assignment is not due for closure");
    let full_name = format!("{}/{}", repository.organization, repository.name);
    context
        .github
        .verify_repository(
            &full_name,
            repository
                .github_repo_id
                .context("repository not created")?,
        )
        .await?;
    context
        .github
        .lock(&full_name, repository.github_id)
        .await?;
    sqlx::query("UPDATE student_repositories SET locked_at=COALESCE(locked_at,now()),last_error=NULL WHERE id=$1").bind(id).execute(&context.pool).await?;
    Ok(())
}

pub async fn sync(context: &ContextData) -> Result<()> {
    let mut connection = context.pool.acquire().await?.detach();
    let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock(704313)")
        .fetch_one(&mut connection)
        .await?;
    ensure!(locked, "repository sync is already running");
    let outcome = sync_inner(context).await;
    sqlx::query("SELECT pg_advisory_unlock(704313)")
        .execute(&mut connection)
        .await?;
    outcome
}

async fn sync_inner(context: &ContextData) -> Result<()> {
    queue::expire_exhausted(&context.pool).await?;
    submissions::refill_snapshots(&context.pool).await?;
    sqlx::query("UPDATE tasks SET status='pending',attempts=0,available_at=now() WHERE status='failed' AND kind IN ('provision','snapshot','publish','lock')").execute(&context.pool).await?;
    let ids: Vec<Uuid> = sqlx::query_scalar("SELECT id FROM student_repositories ORDER BY id")
        .fetch_all(&context.pool)
        .await?;
    for id in ids {
        let result = observe(context, id)
            .instrument(tracing::info_span!("repository_sync", repository_id=%id))
            .await;
        sqlx::query("INSERT INTO reconciliation_observations(repository_id,success,detail) VALUES($1,$2,$3)")
            .bind(id).bind(result.is_ok()).bind(if result.is_ok(){"Identity, branch and permissions observed"}else{"Observation failed; retry required"}).execute(&context.pool).await?;
        if let Err(error) = &result {
            let (stage, reason, upstream_status) = failure_details(error);
            tracing::warn!(repository_id=%id, stage, reason, upstream_status, "repository sync observation failed");
        }
        submissions::close(&context.pool, id).await?;
    }
    let missing:Vec<Uuid>=sqlx::query_scalar("SELECT s.id FROM submissions s WHERE source_digest IS NOT NULL AND NOT EXISTS(SELECT 1 FROM grading_runs g WHERE g.submission_id=s.id)").fetch_all(&context.pool).await?;
    for submission in missing {
        grading::enqueue_run(&context.pool, submission, false).await?;
    }
    let finals:Vec<Uuid>=sqlx::query_scalar("SELECT r.final_submission_id FROM student_repositories r JOIN submissions s ON s.id=r.final_submission_id WHERE s.source_digest IS NOT NULL AND EXISTS(SELECT 1 FROM grading_runs g WHERE g.submission_id=s.id AND g.status='superseded') AND NOT EXISTS(SELECT 1 FROM grading_runs g WHERE g.submission_id=s.id AND g.status<>'superseded')").fetch_all(&context.pool).await?;
    for submission in finals {
        grading::enqueue_run(&context.pool, submission, true).await?;
    }
    sqlx::query("UPDATE grading_runs SET status='infrastructure_failed' WHERE status IN ('pending','running') AND id IN (SELECT (payload->>'run_id')::uuid FROM tasks WHERE kind='grade' AND status='failed')").execute(&context.pool).await?;
    sqlx::query("UPDATE student_repositories SET needs_review=true WHERE id IN (SELECT s.repository_id FROM submissions s JOIN tasks t ON t.payload->>'submission_id'=s.id::text WHERE t.kind='snapshot' AND t.status='failed')").execute(&context.pool).await?;
    sqlx::query("DELETE FROM sessions WHERE expires_at<now()")
        .execute(&context.pool)
        .await?;
    Ok(())
}

async fn observe(context: &ContextData, id: Uuid) -> Result<()> {
    let repository = courses::repository(&context.pool, id).await?;
    if repository.github_repo_id.is_none() {
        let mut tx = context.pool.begin().await?;
        queue::enqueue(
            &mut tx,
            "provision",
            json!({"repository_id":id}),
            &format!("provision:{id}"),
            10,
        )
        .await?;
        sqlx::query("UPDATE tasks SET status='pending',attempts=0,available_at=now() WHERE dedup_key=$1 AND status='failed'").bind(format!("provision:{id}")).execute(&mut *tx).await?;
        tx.commit().await?;
        return Ok(());
    }
    let revision: Revision = serde_json::from_value(repository.definition)?;
    let full_name = format!("{}/{}", repository.organization, repository.name);
    context
        .github
        .verify_repository(&full_name, repository.github_repo_id.context("missing ID")?)
        .await?;
    context.github.verify_no_teams(&full_name).await?;
    let permission = context
        .github
        .permission(&full_name, repository.github_id)
        .await?;
    let sha = context
        .github
        .branch_sha(&full_name, &revision.assignment.branch)
        .await?;
    let mut tx = context.pool.begin().await?;
    let known: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM submission_events WHERE repository_id=$1 AND sha=$2)",
    )
    .bind(id)
    .bind(&sha)
    .fetch_one(&mut *tx)
    .await?;
    if !known && repository.observed_sha.as_deref() != Some(&sha) {
        submissions::record(&mut tx, id, &sha, Utc::now(), "reconciliation", None).await?;
    }
    sqlx::query("UPDATE student_repositories SET observed_sha=$2,observed_at=now(),state=CASE WHEN state='invitation_pending' AND $3='write' THEN 'ready' ELSE state END,invitation_url=CASE WHEN $3='write' THEN NULL ELSE invitation_url END WHERE id=$1")
        .bind(id).bind(&sha).bind(&permission).execute(&mut *tx).await?;
    if repository.closure_due && permission != "read" && permission != "none" {
        sqlx::query("UPDATE student_repositories SET locked_at=NULL,needs_review=true WHERE id=$1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("UPDATE tasks SET status='pending',attempts=0,available_at=now() WHERE dedup_key=$1 AND status IN ('done','failed')").bind(format!("lock:{id}")).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    ensure!(
        permission == "none" || permission == "read" || permission == "write",
        "excessive effective permissions"
    );
    Ok(())
}

#[cfg(test)]
mod diagnostics_tests {
    use super::*;

    #[test]
    fn failures_keep_stages_and_status_without_raw_error_text() {
        let error = anyhow::Error::new(grading_github::client::ApiStatus(403))
            .context("SECRET URL and response")
            .context(Stage("repository_create"));
        assert_eq!(
            failure_details(&error),
            ("repository_create", "github_http", Some(403))
        );
        let error = anyhow::anyhow!("organization base permission must be none")
            .context(Stage("repository_create"));
        assert_eq!(
            failure_details(&error),
            ("repository_create", "organization_base_permissions", None)
        );
        let error =
            anyhow::anyhow!("SECRET credential and source code").context(Stage("template_fetch"));
        assert_eq!(
            failure_details(&error),
            ("template_fetch", "validation_or_internal", None)
        );
    }
}
