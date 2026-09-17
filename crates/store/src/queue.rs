//! At-least-once work queue with expiring, fenced leases and bounded backoff.
use anyhow::{Result, ensure};
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::{FromRow, PgPool, Postgres, Transaction};
use uuid::Uuid;

#[derive(Debug, Clone, FromRow)]
pub struct Task {
    pub id: Uuid,
    pub kind: String,
    pub payload: Value,
    pub lease_token: Uuid,
    pub lease_until: DateTime<Utc>,
    pub attempts: i32,
}

pub async fn enqueue(
    tx: &mut Transaction<'_, Postgres>,
    kind: &str,
    payload: Value,
    key: &str,
    priority: i32,
) -> Result<()> {
    sqlx::query("INSERT INTO tasks(id,kind,payload,dedup_key,priority) VALUES($1,$2,$3,$4,$5) ON CONFLICT(dedup_key) DO NOTHING")
        .bind(Uuid::new_v4()).bind(kind).bind(payload).bind(key).bind(priority).execute(&mut **tx).await?;
    Ok(())
}

pub async fn lease(pool: &PgPool, owner: &str, kinds: &[&str]) -> Result<Option<Task>> {
    let task = sqlx::query_as::<_, Task>(
        "WITH candidate AS (SELECT id FROM tasks WHERE kind=ANY($1) AND attempts < 8
         AND ((status='pending' AND available_at<=now()) OR (status='leased' AND lease_until<now()))
         ORDER BY priority DESC, available_at, id FOR UPDATE SKIP LOCKED LIMIT 1)
         UPDATE tasks t SET status='leased', lease_owner=$2, lease_token=$3,
         lease_until=now()+interval '120 seconds', attempts=attempts+1, updated_at=now()
         FROM candidate c WHERE t.id=c.id RETURNING t.id,t.kind,t.payload,t.lease_token,t.lease_until,t.attempts")
        .bind(kinds).bind(owner).bind(Uuid::new_v4()).fetch_optional(pool).await?;
    Ok(task)
}

pub async fn heartbeat(pool: &PgPool, id: Uuid, token: Uuid, owner: &str) -> Result<()> {
    let count = sqlx::query("UPDATE tasks SET lease_until=now()+interval '120 seconds',updated_at=now() WHERE id=$1 AND lease_token=$2 AND lease_owner=$3 AND status='leased' AND lease_until>now()")
        .bind(id).bind(token).bind(owner).execute(pool).await?.rows_affected();
    ensure!(count == 1, "lease expired or belongs to another worker");
    Ok(())
}

pub async fn finish(pool: &PgPool, task: &Task) -> Result<()> {
    let count = sqlx::query("UPDATE tasks SET status='done',updated_at=now() WHERE id=$1 AND lease_token=$2 AND status='leased' AND lease_until>now()")
        .bind(task.id).bind(task.lease_token).execute(pool).await?.rows_affected();
    ensure!(count == 1, "lease lost before completion");
    Ok(())
}

pub async fn fail(pool: &PgPool, task: &Task, message: &str) -> Result<()> {
    let delay = 30_i32 * (1 << task.attempts.min(8));
    sqlx::query(
        "UPDATE tasks SET status=CASE WHEN attempts>=8 THEN 'failed' ELSE 'pending' END,
        available_at=now()+make_interval(secs=>$3),last_error=$4,updated_at=now()
        WHERE id=$1 AND lease_token=$2 AND status='leased' AND lease_until>now()",
    )
    .bind(task.id)
    .bind(task.lease_token)
    .bind(f64::from(delay))
    .bind(message)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn expire_exhausted(pool: &PgPool) -> Result<()> {
    sqlx::query("UPDATE tasks SET status='failed',last_error='retry budget exhausted',updated_at=now() WHERE status='leased' AND lease_until<now() AND attempts>=8").execute(pool).await?;
    Ok(())
}
