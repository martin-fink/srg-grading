//! Worker-scoped leasing, provenance validation, and atomic result acceptance.
use crate::{artifacts::Artifacts, queue};
use anyhow::{Result, ensure};
use grading_core::{
    config::Resources,
    protocol::{Lease, Revision, RunResult},
    security::digest,
};
use serde_json::json;
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

#[derive(Debug, Clone, FromRow)]
pub struct Worker {
    pub id: String,
    pub profiles: Vec<String>,
    pub resource_caps: serde_json::Value,
}

pub async fn authenticate(pool: &PgPool, raw: &str) -> Result<Worker> {
    Ok(sqlx::query_as(
        "SELECT id,profiles,resource_caps FROM workers WHERE token_hash=$1 AND enabled",
    )
    .bind(digest(raw))
    .fetch_one(pool)
    .await?)
}

pub async fn lease(pool: &PgPool, worker: &Worker, requested: &[String]) -> Result<Option<Lease>> {
    let profiles: Vec<_> = worker
        .profiles
        .iter()
        .filter(|p| requested.contains(p))
        .cloned()
        .collect();
    let caps: Resources = serde_json::from_value(worker.resource_caps.clone())?;
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(704318)")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SELECT pg_advisory_xact_lock(704314,hashtext($1))")
        .bind(&worker.id)
        .fetch_one(&mut *tx)
        .await?;
    let busy: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM tasks WHERE lease_owner=$1 AND kind='grade' AND status='leased' AND lease_until>now())")
        .bind(&worker.id).fetch_one(&mut *tx).await?;
    if busy {
        return Ok(None);
    }
    let id: Option<Uuid> = sqlx::query_scalar(
        "SELECT t.id FROM tasks t JOIN grading_runs g ON g.id=(t.payload->>'run_id')::uuid JOIN assignment_revisions r ON r.digest=g.revision_digest
         WHERE t.kind='grade' AND t.attempts<8 AND ((t.status='pending' AND t.available_at<=now()) OR (t.status='leased' AND t.lease_until<now()))
         AND r.definition->'assignment'->>'execution_profile'=ANY($1)
         AND (r.definition->'assignment'->'resources'->>'cpu')::int<=$2
         AND (r.definition->'assignment'->'resources'->>'memory_gib')::int<=$3
         AND (r.definition->'assignment'->'resources'->>'storage_gib')::int<=$4
         AND NOT EXISTS(SELECT 1 FROM tasks busy JOIN grading_runs bg ON bg.id=(busy.payload->>'run_id')::uuid JOIN submissions bs ON bs.id=bg.submission_id JOIN submissions target ON target.id=g.submission_id JOIN student_repositories br ON br.id=bs.repository_id JOIN student_repositories tr ON tr.id=target.repository_id WHERE busy.kind='grade' AND busy.status='leased' AND busy.lease_until>now() AND br.enrollment_id=tr.enrollment_id)
         ORDER BY t.priority DESC,t.available_at,t.id FOR UPDATE OF t SKIP LOCKED LIMIT 1")
        .bind(&profiles).bind(caps.cpu as i32).bind(caps.memory_gib as i32).bind(caps.storage_gib as i32).fetch_optional(&mut *tx).await?;
    let Some(id) = id else {
        return Ok(None);
    };
    let token = Uuid::new_v4();
    sqlx::query("UPDATE tasks SET status='leased',lease_owner=$2,lease_token=$3,lease_until=now()+interval '120 seconds',attempts=attempts+1,updated_at=now() WHERE id=$1")
        .bind(id).bind(&worker.id).bind(token).execute(&mut *tx).await?;
    sqlx::query("UPDATE grading_runs SET status='running' WHERE id=(SELECT (payload->>'run_id')::uuid FROM tasks WHERE id=$1)").bind(id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(Some(owned_lease(pool, worker, id, token, false).await?))
}

pub async fn owned_lease(
    pool: &PgPool,
    worker: &Worker,
    task: Uuid,
    token: Uuid,
    allow_done: bool,
) -> Result<Lease> {
    #[derive(FromRow)]
    struct Row {
        id: Uuid,
        lease_token: Uuid,
        lease_until: chrono::DateTime<chrono::Utc>,
        run_id: Uuid,
        sha: String,
        revision_digest: String,
        source_digest: String,
        definition: serde_json::Value,
        baseline: Option<serde_json::Value>,
    }
    let row: Row = sqlx::query_as("SELECT t.id,t.lease_token,t.lease_until,g.id AS run_id,s.sha,g.revision_digest,s.source_digest,r.definition,
        CASE WHEN g.public_run_id IS NULL THEN NULL ELSE jsonb_build_object('run_id',b.id,'points',COALESCE(b.public_points,b.points),'deadline',COALESCE(x.deadline,original.deadline)) END AS baseline
        FROM tasks t JOIN grading_runs g ON g.id=(t.payload->>'run_id')::uuid JOIN submissions s ON s.id=g.submission_id
        JOIN assignment_revisions r ON r.digest=g.revision_digest
        JOIN student_repositories sr ON sr.id=s.repository_id
        JOIN assignment_revisions original ON original.digest=sr.revision_digest
        LEFT JOIN extensions x ON x.repository_id=sr.id
        LEFT JOIN grading_runs b ON b.id=g.public_run_id
        WHERE (g.public_run_id IS NULL OR COALESCE(x.deadline,original.deadline)<now()) AND t.id=$1 AND t.kind='grade' AND t.lease_owner=$2 AND t.lease_token=$3
        AND ((t.status='leased' AND t.lease_until>now()) OR ($4 AND t.status='done'))")
        .bind(task).bind(&worker.id).bind(token).bind(allow_done).fetch_one(pool).await?;
    let revision: Revision = serde_json::from_value(row.definition)?;
    let caps: Resources = serde_json::from_value(worker.resource_caps.clone())?;
    revision.validate()?;
    ensure!(
        worker
            .profiles
            .contains(&revision.assignment.execution_profile)
            && revision.assignment.resources.fits(&caps),
        "worker profile or resource caps mismatch"
    );
    ensure!(
        revision.digest()? == row.revision_digest,
        "stored revision digest mismatch"
    );
    Ok(Lease {
        baseline: row.baseline.map(serde_json::from_value).transpose()?,
        schema_version: 1,
        task_id: row.id,
        run_id: row.run_id,
        lease_token: row.lease_token,
        expires_at: row.lease_until,
        sha: row.sha,
        revision_digest: row.revision_digest,
        source_digest: row.source_digest,
        revision,
    })
}

pub async fn accept(
    pool: &PgPool,
    artifacts: &Artifacts,
    worker: &Worker,
    task: Uuid,
    result: &RunResult,
) -> Result<()> {
    let lease = owned_lease(pool, worker, task, result.lease_token, true).await?;
    let points = result.validate(&lease)?;
    let bytes = serde_json::to_vec(result)?;
    let hash = digest(&bytes);
    let existing: Option<String> =
        sqlx::query_scalar("SELECT result_digest FROM grading_runs WHERE id=$1")
            .bind(result.run_id)
            .fetch_one(pool)
            .await?;
    if let Some(existing) = existing {
        ensure!(existing == hash, "conflicting replay");
        return Ok(());
    }
    let report = artifacts.put(pool, "report", &bytes).await?;
    let mut tx = pool.begin().await?;
    let (status, live): (String,bool) = sqlx::query_as("SELECT status,lease_until>now() FROM tasks WHERE id=$1 AND lease_token=$2 AND lease_owner=$3 FOR UPDATE")
        .bind(task).bind(result.lease_token).bind(&worker.id).fetch_one(&mut *tx).await?;
    if status == "done" {
        let previous: Option<String> =
            sqlx::query_scalar("SELECT result_digest FROM grading_runs WHERE id=$1")
                .bind(result.run_id)
                .fetch_one(&mut *tx)
                .await?;
        ensure!(
            previous.as_deref() == Some(hash.as_str()),
            "conflicting replay"
        );
        return Ok(());
    }
    ensure!(
        status == "leased" && live,
        "lease lost before result acceptance"
    );
    let status = serde_json::to_value(&result.status)?
        .as_str()
        .unwrap_or("infrastructure_failed")
        .to_owned();
    let public_points = lease.baseline.as_ref().map(|b| b.points).or(points);
    sqlx::query("UPDATE grading_runs SET status=$2,points=$3,report_digest=$4,result_digest=$5,completed_at=now(),public_points=$6 WHERE id=$1")
        .bind(result.run_id).bind(status).bind(points).bind(report).bind(hash).bind(public_points).execute(&mut *tx).await?;
    if result.status == grading_core::protocol::RunStatus::Invalidated {
        sqlx::query("UPDATE student_repositories SET needs_review=true WHERE id=(SELECT s.repository_id FROM submissions s JOIN grading_runs g ON g.submission_id=s.id WHERE g.id=$1)").bind(result.run_id).execute(&mut *tx).await?;
    }
    for finding in &result.findings {
        sqlx::query("INSERT INTO integrity_findings(run_id,path,reason) VALUES($1,$2,$3) ON CONFLICT DO NOTHING").bind(result.run_id).bind(&finding.path).bind(&finding.reason).execute(&mut *tx).await?;
    }
    sqlx::query("UPDATE tasks SET status='done',updated_at=now() WHERE id=$1")
        .bind(task)
        .execute(&mut *tx)
        .await?;
    sqlx::query("INSERT INTO outbox(run_id) VALUES($1) ON CONFLICT DO NOTHING")
        .bind(result.run_id)
        .execute(&mut *tx)
        .await?;
    queue::enqueue(
        &mut tx,
        "publish",
        json!({"run_id":result.run_id}),
        &format!("publish:{}", result.run_id),
        30,
    )
    .await?;
    tx.commit().await?;
    Ok(())
}

pub async fn enqueue_run(pool: &PgPool, submission: Uuid, regrade: bool) -> Result<Uuid> {
    let mut tx = pool.begin().await?;
    let (repository, revision, source, closed, final_id): (Uuid,String,Option<String>,bool,Option<Uuid>) = sqlx::query_as(
        "SELECT r.id,r.grading_revision,s.source_digest,r.closure_due,r.final_submission_id FROM submissions s JOIN student_repositories r ON r.id=s.repository_id WHERE s.id=$1 FOR UPDATE OF r")
        .bind(submission).fetch_one(&mut *tx).await?;
    ensure!(source.is_some(), "source is not retained yet");
    if !regrade
        && let Some(id) = sqlx::query_scalar(
            "SELECT id FROM grading_runs WHERE submission_id=$1 AND public_run_id IS NULL ORDER BY attempt DESC LIMIT 1",
        )
        .bind(submission)
        .fetch_optional(&mut *tx)
        .await?
    {
        return Ok(id);
    }
    let latest: Uuid = sqlx::query_scalar("SELECT id FROM submissions WHERE repository_id=$1 ORDER BY received_at DESC,id DESC LIMIT 1").bind(repository).fetch_one(&mut *tx).await?;
    let is_final = closed && final_id == Some(submission);
    let public_runs: i64 = sqlx::query_scalar("SELECT count(*) FROM grading_runs g JOIN submissions s ON s.id=g.submission_id WHERE s.repository_id=$1 AND g.public_run_id IS NULL AND g.status<>'superseded'")
        .bind(repository).fetch_one(&mut *tx).await?;
    let superseded =
        !regrade && !is_final && (closed || latest != submission || public_runs >= 200);
    let attempt: i32 = sqlx::query_scalar(
        "SELECT COALESCE(max(attempt),0)+1 FROM grading_runs WHERE submission_id=$1",
    )
    .bind(submission)
    .fetch_one(&mut *tx)
    .await?;
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO grading_runs(id,submission_id,revision_digest,attempt,status) VALUES($1,$2,$3,$4,$5)")
        .bind(id).bind(submission).bind(revision).bind(attempt).bind(if superseded { "superseded" } else { "pending" }).execute(&mut *tx).await?;
    if !superseded {
        sqlx::query("UPDATE tasks SET status='cancelled' WHERE kind='grade' AND status='pending' AND payload->>'run_id' IN (SELECT g.id::text FROM grading_runs g JOIN submissions s ON s.id=g.submission_id WHERE s.repository_id=$1 AND s.id<>$2 AND g.public_run_id IS NULL)")
            .bind(repository).bind(submission).execute(&mut *tx).await?;
        sqlx::query("UPDATE grading_runs SET status='superseded' WHERE status='pending' AND id IN (SELECT (payload->>'run_id')::uuid FROM tasks WHERE status='cancelled' AND kind='grade')").execute(&mut *tx).await?;
        queue::enqueue(
            &mut tx,
            "grade",
            json!({"run_id":id}),
            &format!("grade:{id}"),
            if is_final { 100 } else { 0 },
        )
        .await?;
    }
    tx.commit().await?;
    Ok(id)
}

/// Private grading is an explicit operator action over closed, final public submissions.
pub async fn enqueue_private(
    pool: &PgPool,
    course: &str,
    exercise: &str,
    operator: &str,
    reason: &str,
) -> Result<Vec<(Uuid, Option<Uuid>, String)>> {
    ensure!(
        !reason.trim().is_empty(),
        "private grading requires a reason"
    );
    let ids: Vec<Uuid> = sqlx::query_scalar("SELECT r.id FROM student_repositories r JOIN assignments a ON a.id=r.assignment_id WHERE a.course_id=$1 AND a.slug=$2 ORDER BY r.id").bind(course).bind(exercise).fetch_all(pool).await?;
    ensure!(!ids.is_empty(), "exercise has no student repositories");
    let mut outcomes = Vec::new();
    for repository in ids {
        crate::submissions::close(pool, repository).await?;
        let mut tx = pool.begin().await?;
        let (revision,final_id,due):(String,Option<Uuid>,bool)=sqlx::query_as("SELECT r.grading_revision,r.final_submission_id,COALESCE(x.deadline,v.deadline)<now() FROM student_repositories r JOIN assignment_revisions v ON v.digest=r.revision_digest LEFT JOIN extensions x ON x.repository_id=r.id WHERE r.id=$1 FOR UPDATE OF r").bind(repository).fetch_one(&mut *tx).await?;
        if !due || final_id.is_none() {
            outcomes.push((
                repository,
                None,
                if due {
                    "No final submission"
                } else {
                    "Effective deadline has not passed"
                }
                .into(),
            ));
            continue;
        }
        let submission = final_id.unwrap();
        let baseline:Option<(Uuid,String,Option<i32>)>=sqlx::query_as("SELECT id,status,points FROM grading_runs WHERE submission_id=$1 AND public_run_id IS NULL ORDER BY attempt DESC LIMIT 1").bind(submission).fetch_optional(&mut *tx).await?;
        let Some((baseline, status, Some(_))) = baseline else {
            outcomes.push((
                repository,
                None,
                "Public grading must complete first".into(),
            ));
            continue;
        };
        if status != "completed" {
            outcomes.push((
                repository,
                None,
                "Public grading must complete first".into(),
            ));
            continue;
        }
        let definition: serde_json::Value =
            sqlx::query_scalar("SELECT definition FROM assignment_revisions WHERE digest=$1")
                .bind(&revision)
                .fetch_one(&mut *tx)
                .await?;
        let definition: Revision = serde_json::from_value(definition)?;
        ensure!(
            definition.grader.as_ref().is_some_and(|g| g
                .workflow
                .as_ref()
                .is_some_and(|w| w.private_command.is_some())),
            "exercise has no private grading command"
        );
        if let Some(run)=sqlx::query_scalar("SELECT id FROM grading_runs WHERE public_run_id=$1 AND revision_digest=$2 ORDER BY attempt DESC LIMIT 1").bind(baseline).bind(&revision).fetch_optional(&mut *tx).await? {
            outcomes.push((repository,Some(run),"Already scheduled; update the grader revision to run a correction".into()));continue;
        }
        let attempt: i32 = sqlx::query_scalar(
            "SELECT COALESCE(max(attempt),0)+1 FROM grading_runs WHERE submission_id=$1",
        )
        .bind(submission)
        .fetch_one(&mut *tx)
        .await?;
        let run = Uuid::new_v4();
        sqlx::query("INSERT INTO grading_runs(id,submission_id,revision_digest,attempt,public_run_id) VALUES($1,$2,$3,$4,$5)").bind(run).bind(submission).bind(&revision).bind(attempt).bind(baseline).execute(&mut *tx).await?;
        queue::enqueue(
            &mut tx,
            "grade",
            json!({"run_id":run}),
            &format!("grade:{run}"),
            100,
        )
        .await?;
        sqlx::query("INSERT INTO audit_events(operator,action,target,reason) VALUES($1,'grading.private',$2,$3)").bind(operator).bind(run.to_string()).bind(format!("{reason}; public_run={baseline}")).execute(&mut *tx).await?;
        tx.commit().await?;
        outcomes.push((repository, Some(run), "Queued private grading".into()));
    }
    Ok(outcomes)
}

/// Retry a failed private run without changing its pinned grader or public baseline.
pub async fn retry_private(
    pool: &PgPool,
    failed: Uuid,
    operator: &str,
    reason: &str,
) -> Result<Uuid> {
    ensure!(!reason.trim().is_empty(), "private retry reason required");
    let mut tx = pool.begin().await?;
    let (repository, submission): (Uuid, Uuid) = sqlx::query_as("SELECT s.repository_id,s.id FROM grading_runs g JOIN submissions s ON s.id=g.submission_id WHERE g.id=$1")
        .bind(failed).fetch_one(&mut *tx).await?;
    sqlx::query("SELECT id FROM student_repositories WHERE id=$1 FOR UPDATE")
        .bind(repository)
        .execute(&mut *tx)
        .await?;
    let (latest, status, revision, baseline, attempt): (Uuid, String, String, Option<Uuid>, i32) = sqlx::query_as("SELECT id,status,revision_digest,public_run_id,attempt FROM grading_runs WHERE submission_id=$1 ORDER BY attempt DESC LIMIT 1")
        .bind(submission).fetch_one(&mut *tx).await?;
    ensure!(
        latest == failed
            && baseline.is_some()
            && matches!(status.as_str(), "infrastructure_failed" | "timed_out"),
        "only the latest failed private run can be retried"
    );
    let run = Uuid::new_v4();
    sqlx::query("INSERT INTO grading_runs(id,submission_id,revision_digest,attempt,public_run_id) VALUES($1,$2,$3,$4,$5)")
        .bind(run).bind(submission).bind(revision).bind(attempt + 1).bind(baseline).execute(&mut *tx).await?;
    queue::enqueue(
        &mut tx,
        "grade",
        json!({"run_id":run}),
        &format!("grade:{run}"),
        100,
    )
    .await?;
    crate::submissions::audit(
        &mut tx,
        operator,
        "grading.private_retry",
        run,
        &format!("{reason}; previous_run={failed}"),
    )
    .await?;
    tx.commit().await?;
    Ok(run)
}
