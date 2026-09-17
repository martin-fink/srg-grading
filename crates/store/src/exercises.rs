//! Atomic exercise publication and explicit grading revision rollouts.
use anyhow::{Result, ensure};
use grading_core::protocol::Revision;
use sqlx::{PgPool, Postgres, Transaction};
use uuid::Uuid;

pub async fn current(pool: &PgPool, course: &str, name: &str) -> Result<Option<Revision>> {
    let value: Option<serde_json::Value> = sqlx::query_scalar("SELECT r.definition FROM assignments a JOIN assignment_revisions r ON r.digest=a.current_revision WHERE a.course_id=$1 AND a.slug=$2")
        .bind(course).bind(name).fetch_optional(pool).await?;
    value
        .map(serde_json::from_value)
        .transpose()
        .map_err(Into::into)
}

async fn insert(
    tx: &mut Transaction<'_, Postgres>,
    id: Uuid,
    revision: &Revision,
) -> Result<String> {
    revision.validate()?;
    let digest = revision.digest()?;
    let git = &revision
        .grader
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("missing grader"))?
        .revision;
    sqlx::query("INSERT INTO assignment_revisions(digest,assignment_id,config_revision,definition,opens_at,deadline,max_points) VALUES($1,$2,$3,$4,$5,$6,$7) ON CONFLICT DO NOTHING")
        .bind(&digest).bind(id).bind(git).bind(serde_json::to_value(revision)?).bind(revision.assignment.opens_at).bind(revision.assignment.deadline).bind(revision.assignment.max_points).execute(&mut **tx).await?;
    Ok(digest)
}

pub struct Publication<'a> {
    pub revision: &'a Revision,
    pub expected: Option<&'a str>,
    pub existing: bool,
    pub dry_run: bool,
    pub operator: &'a str,
    pub reason: &'a str,
}

pub async fn publish(pool: &PgPool, publication: Publication<'_>) -> Result<()> {
    let Publication {
        revision,
        expected,
        existing,
        dry_run,
        operator,
        reason,
    } = publication;
    revision.validate()?;
    ensure!(!reason.trim().is_empty(), "publication requires a reason");
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(704312)")
        .execute(&mut *tx)
        .await?;
    let previous: Option<(Uuid, Option<String>)> = sqlx::query_as(
        "SELECT id,current_revision FROM assignments WHERE course_id=$1 AND slug=$2 FOR UPDATE",
    )
    .bind(&revision.course_id)
    .bind(&revision.assignment_id)
    .fetch_optional(&mut *tx)
    .await?;
    ensure!(
        previous.as_ref().and_then(|(_, r)| r.as_deref()) == expected,
        "exercise changed while building; retry against the current revision"
    );
    let id = if let Some((id, _)) = previous {
        id
    } else {
        let id = Uuid::new_v4();
        sqlx::query("INSERT INTO assignments(id,course_id,slug) VALUES($1,$2,$3)")
            .bind(id)
            .bind(&revision.course_id)
            .bind(&revision.assignment_id)
            .execute(&mut *tx)
            .await?;
        id
    };
    let digest = insert(&mut tx, id, revision).await?;
    sqlx::query("UPDATE assignments SET current_revision=$2 WHERE id=$1")
        .bind(id)
        .bind(&digest)
        .execute(&mut *tx)
        .await?;
    if existing {
        let rows: Vec<(Uuid,serde_json::Value)> = sqlx::query_as("SELECT s.id,r.definition FROM student_repositories s JOIN assignment_revisions r ON r.digest=s.revision_digest WHERE s.assignment_id=$1 FOR UPDATE OF s")
            .bind(id).fetch_all(&mut *tx).await?;
        for (repository, definition) in rows {
            let mut combined: Revision = serde_json::from_value(definition)?;
            // Preserve the original template manifest and dates; only grading changes.
            combined.grader = revision.grader.clone();
            combined.tests = revision.tests.clone();
            combined.assignment.image = revision.assignment.image.clone();
            combined.assignment.execution_profile = revision.assignment.execution_profile.clone();
            combined.assignment.resources = revision.assignment.resources.clone();
            combined.assignment.timeout_seconds = revision.assignment.timeout_seconds;
            combined.assignment.max_points = revision.assignment.max_points;
            let hash = insert(&mut tx, id, &combined).await?;
            sqlx::query("UPDATE student_repositories SET grading_revision=$2 WHERE id=$1")
                .bind(repository)
                .bind(hash)
                .execute(&mut *tx)
                .await?;
        }
    }
    sqlx::query("INSERT INTO audit_events(operator,action,target,reason) VALUES($1,'exercise.publish',$2,$3)")
        .bind(operator).bind(format!("{}/{}",revision.course_id,revision.assignment_id))
        .bind(format!("{reason}; revision={digest}; existing={existing}")).execute(&mut *tx).await?;
    if dry_run {
        tx.rollback().await?;
    } else {
        tx.commit().await?;
    }
    Ok(())
}
