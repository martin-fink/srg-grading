//! Receipt-time registration, immutable final selection, and source retention.
use crate::queue;
use anyhow::{Result, ensure};
use chrono::{DateTime, Utc};
use grading_core::security::valid_hex;
use serde_json::json;
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

pub async fn record(
    tx: &mut Transaction<'_, Postgres>,
    repository: Uuid,
    sha: &str,
    received: DateTime<Utc>,
    source: &str,
    delivery: Option<Uuid>,
) -> Result<Option<Uuid>> {
    ensure!(valid_hex(sha, 40), "invalid submission SHA");
    let (opens, deadline, closed): (DateTime<Utc>, DateTime<Utc>, bool) = sqlx::query_as("SELECT v.opens_at,COALESCE(x.deadline,v.deadline),r.closure_due FROM student_repositories r JOIN assignment_revisions v ON v.digest=r.revision_digest LEFT JOIN extensions x ON x.repository_id=r.id WHERE r.id=$1 FOR UPDATE OF r")
        .bind(repository).fetch_one(&mut **tx).await?;
    let eligible =
        received >= opens && received <= deadline && source != "reconciliation" && !closed;
    let event = Uuid::new_v4();
    sqlx::query("INSERT INTO submission_events(id,repository_id,sha,received_at,source,eligible,delivery_id) VALUES($1,$2,$3,$4,$5,$6,$7)")
        .bind(event).bind(repository).bind(sha).bind(received).bind(source).bind(eligible).bind(delivery).execute(&mut **tx).await?;
    if !eligible {
        if source == "reconciliation" || closed || (source == "webhook" && received > deadline) {
            sqlx::query("UPDATE student_repositories SET needs_review=true WHERE id=$1")
                .bind(repository)
                .execute(&mut **tx)
                .await?;
        }
        return Ok(None);
    }
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO submissions(id,repository_id,event_id,sha,received_at) VALUES($1,$2,$3,$4,$5)",
    )
    .bind(id)
    .bind(repository)
    .bind(event)
    .bind(sha)
    .bind(received)
    .execute(&mut **tx)
    .await?;
    schedule_snapshots(tx, repository).await?;
    Ok(Some(id))
}

/// Preserve every receipt while limiting the active source-fetch queue per enrollment.
pub async fn schedule_snapshots(
    tx: &mut Transaction<'_, Postgres>,
    repository: Uuid,
) -> Result<()> {
    let enrollment: Uuid =
        sqlx::query_scalar("SELECT enrollment_id FROM student_repositories WHERE id=$1")
            .bind(repository)
            .fetch_one(&mut **tx)
            .await?;
    sqlx::query("SELECT pg_advisory_xact_lock(704316,hashtext($1))")
        .bind(enrollment.to_string())
        .execute(&mut **tx)
        .await?;
    let active: i64 = sqlx::query_scalar("SELECT count(*) FROM tasks t JOIN submissions s ON s.id=(t.payload->>'submission_id')::uuid JOIN student_repositories r ON r.id=s.repository_id WHERE t.kind='snapshot' AND t.status IN ('pending','leased') AND r.enrollment_id=$1").bind(enrollment).fetch_one(&mut **tx).await?;
    let pending: Vec<(Uuid,bool)> = sqlx::query_as("SELECT s.id,s.id IS NOT DISTINCT FROM r.final_submission_id FROM submissions s JOIN student_repositories r ON r.id=s.repository_id WHERE r.enrollment_id=$1 AND s.source_digest IS NULL AND NOT EXISTS(SELECT 1 FROM tasks t WHERE t.dedup_key='snapshot:'||s.id::text) ORDER BY (s.id IS NOT DISTINCT FROM r.final_submission_id) DESC,s.received_at DESC LIMIT $2")
        .bind(enrollment).bind((32-active).max(0)).fetch_all(&mut **tx).await?;
    for (id, final_submission) in pending {
        queue::enqueue(
            tx,
            "snapshot",
            json!({"submission_id":id}),
            &format!("snapshot:{id}"),
            if final_submission { 100 } else { 20 },
        )
        .await?;
    }
    Ok(())
}

pub async fn refill_snapshots(pool: &PgPool) -> Result<()> {
    let ids: Vec<Uuid> = sqlx::query_scalar("SELECT DISTINCT s.repository_id FROM submissions s WHERE s.source_digest IS NULL AND NOT EXISTS(SELECT 1 FROM tasks t WHERE t.dedup_key='snapshot:'||s.id::text) LIMIT 100").fetch_all(pool).await?;
    for id in ids {
        let mut tx = pool.begin().await?;
        schedule_snapshots(&mut tx, id).await?;
        tx.commit().await?;
    }
    Ok(())
}

pub async fn close(pool: &PgPool, repository: Uuid) -> Result<()> {
    let mut tx = pool.begin().await?;
    let (due, closed): (bool,bool) = sqlx::query_as("SELECT COALESCE(x.deadline,v.deadline)<now(),r.closure_due FROM student_repositories r JOIN assignment_revisions v ON v.digest=r.revision_digest LEFT JOIN extensions x ON x.repository_id=r.id WHERE r.id=$1 FOR UPDATE OF r")
        .bind(repository).fetch_one(&mut *tx).await?;
    if due && !closed {
        let submission: Option<Uuid> = sqlx::query_scalar("SELECT id FROM submissions WHERE repository_id=$1 ORDER BY received_at DESC,id DESC LIMIT 1").bind(repository).fetch_optional(&mut *tx).await?;
        sqlx::query(
            "UPDATE student_repositories SET closure_due=true,final_submission_id=$2 WHERE id=$1",
        )
        .bind(repository)
        .bind(submission)
        .execute(&mut *tx)
        .await?;
        if let Some(submission) = submission {
            sqlx::query("UPDATE tasks SET priority=100 WHERE (kind='snapshot' AND payload->>'submission_id'=$1) OR (kind='grade' AND payload->>'run_id' IN (SELECT id::text FROM grading_runs WHERE submission_id=$2))")
                .bind(submission.to_string()).bind(submission).execute(&mut *tx).await?;
        }
    }
    if due {
        queue::enqueue(
            &mut tx,
            "lock",
            json!({"repository_id":repository}),
            &format!("lock:{repository}"),
            100,
        )
        .await?;
        sqlx::query("UPDATE tasks SET status='pending',attempts=0,available_at=now() WHERE dedup_key=$1 AND status='failed'").bind(format!("lock:{repository}")).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    Ok(())
}

pub async fn extend(
    pool: &PgPool,
    repository: Uuid,
    deadline: DateTime<Utc>,
    operator: &str,
    reason: &str,
) -> Result<()> {
    ensure!(!reason.trim().is_empty(), "reason required");
    let mut tx = pool.begin().await?;
    let (closed, previous): (bool,DateTime<Utc>) = sqlx::query_as("SELECT r.closure_due,COALESCE(x.deadline,v.deadline) FROM student_repositories r JOIN assignment_revisions v ON v.digest=r.revision_digest LEFT JOIN extensions x ON x.repository_id=r.id WHERE r.id=$1 FOR UPDATE OF r")
        .bind(repository).fetch_one(&mut *tx).await?;
    ensure!(
        !closed && deadline > previous && deadline > Utc::now(),
        "extension must advance an assignment that has not been closed; closed submissions require an audited selection override"
    );
    sqlx::query("INSERT INTO extensions(repository_id,deadline,reason) VALUES($1,$2,$3) ON CONFLICT(repository_id) DO UPDATE SET deadline=$2,reason=$3,updated_at=now()")
        .bind(repository).bind(deadline).bind(reason).execute(&mut *tx).await?;
    audit(
        &mut tx,
        operator,
        "extension",
        repository,
        &format!("{deadline}: {reason}"),
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn override_grade(
    pool: &PgPool,
    repository: Uuid,
    points: i32,
    operator: &str,
    reason: &str,
) -> Result<()> {
    ensure!(!reason.trim().is_empty(), "reason required");
    let mut tx = pool.begin().await?;
    let max: i32 = sqlx::query_scalar("SELECT v.max_points FROM student_repositories r JOIN assignment_revisions v ON v.digest=r.revision_digest WHERE r.id=$1 FOR UPDATE OF r").bind(repository).fetch_one(&mut *tx).await?;
    ensure!((0..=max).contains(&points), "override outside point bounds");
    sqlx::query("INSERT INTO grade_overrides(id,repository_id,points,reason,operator) VALUES($1,$2,$3,$4,$5)").bind(Uuid::new_v4()).bind(repository).bind(points).bind(reason).bind(operator).execute(&mut *tx).await?;
    audit(&mut tx, operator, "grade.override", repository, reason).await?;
    tx.commit().await?;
    Ok(())
}

pub async fn audit(
    tx: &mut Transaction<'_, Postgres>,
    operator: &str,
    action: &str,
    target: Uuid,
    reason: &str,
) -> Result<()> {
    sqlx::query("INSERT INTO audit_events(operator,action,target,reason) VALUES($1,$2,$3,$4)")
        .bind(operator)
        .bind(action)
        .bind(target.to_string())
        .bind(reason)
        .execute(&mut **tx)
        .await?;
    Ok(())
}
