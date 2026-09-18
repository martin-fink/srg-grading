//! Durable, session-bound validation and confirmation requests.
use anyhow::{Result, ensure};
use grading_core::{admin::Input, security};
use serde_json::Value;
use sqlx::{FromRow, PgPool};
use uuid::Uuid;
#[derive(Debug, FromRow)]
pub struct Operation {
    pub id: Uuid,
    pub actor: i64,
    pub input: Value,
    pub state: String,
    pub plan: Option<Value>,
    pub output: String,
    pub download: Option<String>,
    pub confirmation_token: Option<String>,
    pub confirmable: bool,
}
pub async fn submit(pool: &PgPool, actor: i64, csrf: &str, input: &Input) -> Result<Uuid> {
    input.validate()?;
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(704320,hashtext($1))")
        .bind(actor.to_string())
        .execute(&mut *tx)
        .await?;
    let allowed:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM admins WHERE github_id=$1) AND (SELECT count(*) FROM admin_operations WHERE actor=$1 AND created_at>now()-interval '1 hour')<60 AND (SELECT count(*) FROM admin_operations WHERE actor=$1 AND state IN ('pending_validation','validating','queued','applying'))<4").bind(actor).fetch_one(&mut *tx).await?;
    ensure!(
        allowed,
        "Administrator access is required; at most four operations may be active and 60 submitted per hour."
    );
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO admin_operations(id,actor,session_hash,input) VALUES($1,$2,$3,$4)")
        .bind(id)
        .bind(actor)
        .bind(security::digest(csrf))
        .bind(serde_json::to_value(input)?)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(id)
}
pub async fn get(pool: &PgPool, id: Uuid, actor: i64) -> Result<Operation> {
    Ok(sqlx::query_as("SELECT id,actor,input,state,plan,output,download,confirmation_token,state='ready' AND validated_until>now() AS confirmable FROM admin_operations WHERE id=$1 AND actor=$2").bind(id).bind(actor).fetch_one(pool).await?)
}
pub async fn confirm(pool: &PgPool, id: Uuid, actor: i64, csrf: &str, token: &str) -> Result<bool> {
    Ok(
        sqlx::query_scalar("SELECT confirm_admin_operation($1,$2,$3,$4)")
            .bind(id)
            .bind(actor)
            .bind(security::digest(csrf))
            .bind(security::digest(token))
            .fetch_one(pool)
            .await?,
    )
}
pub async fn progress(pool: &PgPool, id: Uuid, message: &str) -> Result<()> {
    let message = truncate(message, 400_000);
    sqlx::query("UPDATE admin_operations SET output=left(output,100000)||E'\n\n'||$2,updated_at=now() WHERE id=$1")
        .bind(id)
        .bind(message)
        .execute(pool)
        .await?;
    Ok(())
}
pub fn truncate(input: &str, max: usize) -> &str {
    let mut end = input.len().min(max);
    while !input.is_char_boundary(end) {
        end -= 1;
    }
    &input[..end]
}
