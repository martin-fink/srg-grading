//! Idempotent course and roster imports, repository allocation, and score views.
use crate::queue;
use anyhow::{Result, ensure};
use chrono::{DateTime, Utc};
use grading_core::{config::CourseConfig, security::valid_hex};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RosterRow {
    pub student_id: String,
    pub name: String,
    pub github_username: String,
}

#[derive(Debug, Clone)]
pub struct ResolvedStudent {
    pub row: RosterRow,
    pub github_id: i64,
    pub login: String,
}

pub async fn apply_course(
    pool: &PgPool,
    config: &CourseConfig,
    config_revision: &str,
    dry_run: bool,
    operator: &str,
) -> Result<()> {
    config.validate()?;
    ensure!(
        config_revision
            .strip_prefix("sha256:")
            .is_some_and(|s| valid_hex(s, 64)),
        "config revision must be a SHA-256 content digest"
    );
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(704312)")
        .execute(&mut *tx)
        .await?;
    let org: Option<String> = sqlx::query_scalar("SELECT organization FROM courses WHERE id=$1")
        .bind(&config.course.id)
        .fetch_optional(&mut *tx)
        .await?;
    ensure!(
        org.as_ref()
            .is_none_or(|org| org == &config.course.github_organization),
        "cannot change course organization"
    );
    sqlx::query("INSERT INTO courses(id,title,organization,timezone,config_revision) VALUES($1,$2,$3,$4,$5) ON CONFLICT(id) DO UPDATE SET title=$2,timezone=$4,config_revision=$5,updated_at=now()")
        .bind(&config.course.id).bind(&config.course.title).bind(&config.course.github_organization).bind(&config.course.timezone).bind(config_revision).execute(&mut *tx).await?;
    sqlx::query(
        "INSERT INTO audit_events(operator,action,target,reason) VALUES($1,'course.apply',$2,$3)",
    )
    .bind(operator)
    .bind(&config.course.id)
    .bind(config_revision)
    .execute(&mut *tx)
    .await?;
    if dry_run {
        tx.rollback().await?;
    } else {
        tx.commit().await?;
    }
    Ok(())
}

pub async fn import_roster(
    pool: &PgPool,
    course: &str,
    rows: &[ResolvedStudent],
    dry_run: bool,
    operator: &str,
) -> Result<()> {
    let mut ids = std::collections::BTreeSet::new();
    let mut students = std::collections::BTreeSet::new();
    for row in rows {
        ensure!(
            row.github_id > 0 && ids.insert(row.github_id) && students.insert(&row.row.student_id),
            "duplicate enrollment"
        );
        ensure!(
            !row.row.student_id.is_empty()
                && !row.row.name.is_empty()
                && row.row.student_id.len() <= 128
                && row.row.name.len() <= 200,
            "invalid roster fields"
        );
    }
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT id FROM courses WHERE id=$1 FOR UPDATE")
        .bind(course)
        .fetch_one(&mut *tx)
        .await?;
    for row in rows {
        sqlx::query("INSERT INTO users(github_id,login) VALUES($1,$2) ON CONFLICT(github_id) DO UPDATE SET login=$2,updated_at=now()")
            .bind(row.github_id).bind(&row.login).execute(&mut *tx).await?;
        let existing: Option<i64> = sqlx::query_scalar(
            "SELECT github_id FROM enrollments WHERE course_id=$1 AND student_id=$2",
        )
        .bind(course)
        .bind(&row.row.student_id)
        .fetch_optional(&mut *tx)
        .await?;
        ensure!(
            existing.is_none_or(|id| id == row.github_id),
            "student ID already belongs to a different GitHub account"
        );
        let existing_student: Option<String> = sqlx::query_scalar(
            "SELECT student_id FROM enrollments WHERE course_id=$1 AND github_id=$2",
        )
        .bind(course)
        .bind(row.github_id)
        .fetch_optional(&mut *tx)
        .await?;
        ensure!(
            existing_student
                .as_ref()
                .is_none_or(|id| id == &row.row.student_id),
            "GitHub account is already enrolled under another student ID"
        );
        sqlx::query("INSERT INTO enrollments(id,course_id,github_id,student_id,name) VALUES($1,$2,$3,$4,$5) ON CONFLICT(course_id,github_id) DO UPDATE SET name=$5")
            .bind(Uuid::new_v4()).bind(course).bind(row.github_id).bind(&row.row.student_id).bind(&row.row.name).execute(&mut *tx).await?;
    }
    sqlx::query(
        "INSERT INTO audit_events(operator,action,target,reason) VALUES($1,'roster.import',$2,$3)",
    )
    .bind(operator)
    .bind(course)
    .bind(format!("{} records; absent rows retained", rows.len()))
    .execute(&mut *tx)
    .await?;
    if dry_run {
        tx.rollback().await?;
    } else {
        tx.commit().await?;
    }
    Ok(())
}

pub async fn request_repository(pool: &PgPool, github_id: i64, assignment: Uuid) -> Result<Uuid> {
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock_shared(704312)")
        .execute(&mut *tx)
        .await?;
    sqlx::query("SELECT pg_advisory_xact_lock(704315,hashtext($1))")
        .bind(format!("{github_id}:{assignment}"))
        .execute(&mut *tx)
        .await?;
    let (enrollment, revision, opens, deadline): (Uuid, String, DateTime<Utc>, DateTime<Utc>) = sqlx::query_as(
        "SELECT e.id,r.digest,r.opens_at,r.deadline FROM enrollments e JOIN assignments a ON a.course_id=e.course_id JOIN assignment_revisions r ON r.digest=a.current_revision WHERE e.github_id=$1 AND a.id=$2 AND NOT a.archived")
        .bind(github_id).bind(assignment).fetch_one(&mut *tx).await?;
    if let Some(id) = sqlx::query_scalar(
        "SELECT id FROM student_repositories WHERE enrollment_id=$1 AND assignment_id=$2",
    )
    .bind(enrollment)
    .bind(assignment)
    .fetch_optional(&mut *tx)
    .await?
    {
        return Ok(id);
    }
    let now: DateTime<Utc> = sqlx::query_scalar("SELECT clock_timestamp()")
        .fetch_one(&mut *tx)
        .await?;
    ensure!(now >= opens && now <= deadline, "assignment is not open");
    let id = Uuid::new_v4();
    let (student_id, template): (String, String) = sqlx::query_as(
        "SELECT e.student_id,r.definition->'assignment'->>'template' FROM enrollments e CROSS JOIN assignment_revisions r WHERE e.id=$1 AND r.digest=$2",
    ).bind(enrollment).bind(&revision).fetch_one(&mut *tx).await?;
    let name = repository_name(&template, &student_id, id);
    sqlx::query("INSERT INTO student_repositories(id,enrollment_id,assignment_id,revision_digest,grading_revision,name,provisioning_nonce) VALUES($1,$2,$3,$4,$4,$5,$6)")
        .bind(id).bind(enrollment).bind(assignment).bind(revision).bind(name).bind(Uuid::new_v4()).execute(&mut *tx).await?;
    queue::enqueue(
        &mut tx,
        "provision",
        json!({"repository_id":id}),
        &format!("provision:{id}"),
        10,
    )
    .await?;
    tx.commit().await?;
    Ok(id)
}

#[derive(Debug, Clone, FromRow)]
pub struct Repository {
    pub id: Uuid,
    pub enrollment_id: Uuid,
    pub assignment_id: Uuid,
    pub revision_digest: String,
    pub github_repo_id: Option<i64>,
    pub name: String,
    pub provisioning_nonce: Uuid,
    pub state: String,
    pub invitation_url: Option<String>,
    pub observed_sha: Option<String>,
    pub closure_due: bool,
    pub final_submission_id: Option<Uuid>,
    pub locked_at: Option<DateTime<Utc>>,
    pub needs_review: bool,
    pub last_error: Option<String>,
    pub github_id: i64,
    pub login: String,
    pub organization: String,
    pub definition: serde_json::Value,
    pub deadline: DateTime<Utc>,
}

pub async fn repository(pool: &PgPool, id: Uuid) -> Result<Repository> {
    Ok(sqlx::query_as("SELECT r.*,e.github_id,u.login,c.organization,v.definition,COALESCE(x.deadline,v.deadline) AS deadline FROM student_repositories r JOIN enrollments e ON e.id=r.enrollment_id JOIN users u ON u.github_id=e.github_id JOIN courses c ON c.id=e.course_id JOIN assignment_revisions v ON v.digest=r.revision_digest LEFT JOIN extensions x ON x.repository_id=r.id WHERE r.id=$1")
        .bind(id).fetch_one(pool).await?)
}

#[derive(Debug, FromRow)]
pub struct DashboardRow {
    pub assignment_id: Uuid,
    pub course: String,
    pub title: String,
    pub timezone: String,
    pub opens_at: DateTime<Utc>,
    pub deadline: DateTime<Utc>,
    pub max_points: i32,
    pub repository_id: Option<Uuid>,
    pub repository_name: Option<String>,
    pub organization: String,
    pub state: Option<String>,
    pub invitation_url: Option<String>,
    pub locked_at: Option<DateTime<Utc>>,
    pub needs_review: Option<bool>,
    pub last_error: Option<String>,
    pub sha: Option<String>,
    pub status: Option<String>,
    pub points: Option<i32>,
    pub public_points: Option<i32>,
    pub private_grading: bool,
    pub run_id: Option<Uuid>,
    pub override_points: Option<i32>,
    pub closure_due: Option<bool>,
}

pub async fn dashboard(pool: &PgPool, github_id: i64) -> Result<Vec<DashboardRow>> {
    Ok(sqlx::query_as(
        "SELECT a.id AS assignment_id,c.title AS course,c.organization,c.timezone,v.definition->'assignment'->>'title' AS title,v.opens_at,COALESCE(x.deadline,v.deadline) AS deadline,CASE WHEN o.points IS NOT NULL THEN COALESCE(gv.max_points,v.max_points) ELSE COALESCE(rv.max_points,gv.max_points,v.max_points) END AS max_points,
         r.id AS repository_id,r.name AS repository_name,r.state,r.invitation_url,r.locked_at,r.needs_review,r.last_error,r.closure_due,
         s.sha,g.status,g.points,COALESCE(g.public_points,b.public_points,b.points) AS public_points,g.public_run_id IS NOT NULL AS private_grading,CASE WHEN g.report_digest IS NOT NULL THEN g.id END AS run_id,o.points AS override_points
         FROM enrollments e JOIN courses c ON c.id=e.course_id JOIN assignments a ON a.course_id=c.id
         LEFT JOIN student_repositories r ON r.enrollment_id=e.id AND r.assignment_id=a.id
         JOIN assignment_revisions v ON v.digest=COALESCE(r.revision_digest,a.current_revision)
         LEFT JOIN assignment_revisions gv ON gv.digest=r.grading_revision
         LEFT JOIN extensions x ON x.repository_id=r.id
         LEFT JOIN LATERAL (SELECT * FROM submissions s WHERE s.repository_id=r.id AND (NOT r.closure_due OR s.id=r.final_submission_id) ORDER BY s.received_at DESC,s.id DESC LIMIT 1) s ON true
         LEFT JOIN LATERAL (SELECT * FROM grading_runs g WHERE g.submission_id=s.id ORDER BY g.attempt DESC LIMIT 1) g ON true
         LEFT JOIN grading_runs b ON b.id=g.public_run_id
         LEFT JOIN assignment_revisions rv ON rv.digest=g.revision_digest
         LEFT JOIN LATERAL (SELECT points FROM grade_overrides o WHERE o.repository_id=r.id ORDER BY o.created_at DESC,o.id DESC LIMIT 1) o ON true
         WHERE e.github_id=$1 AND (NOT a.archived OR r.id IS NOT NULL) ORDER BY c.id,a.slug")
        .bind(github_id).fetch_all(pool).await?)
}

// Keep the full UUID while fitting descriptive components into GitHub's name limit.
fn repository_name(template: &str, student_id: &str, id: Uuid) -> String {
    let sanitize = |value: &str| -> String {
        let value: String = value
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || "-_.".contains(c) {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        if value.is_empty() {
            "student".into()
        } else {
            value
        }
    };
    let mut template = sanitize(template.rsplit('/').next().unwrap_or(template));
    let mut student = sanitize(student_id);
    while template.len() + student.len() > 62 {
        if template.len() > student.len() {
            template.pop();
        } else {
            student.pop();
        }
    }
    format!("{template}-{student}-{id}")
}

#[cfg(test)]
mod naming_tests {
    use super::*;

    #[test]
    fn repository_names_preserve_uuid_and_fit_github_limits() {
        let id = Uuid::new_v4();
        assert_eq!(
            repository_name("org/echo-template", "123456", id),
            format!("echo-template-123456-{id}")
        );
        let name = repository_name(&format!("org/{}", "t".repeat(100)), &"ü /".repeat(100), id);
        assert!(grading_core::config::github_repository(&format!(
            "org/{name}"
        )));
        assert!(name.ends_with(&id.to_string()));
        assert_eq!(name.len(), 100);
    }
}
