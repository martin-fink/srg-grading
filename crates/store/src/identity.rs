//! Single-use login state, hashed sessions, and audited local administrator changes.
use anyhow::{Result, ensure};
use grading_core::security::{digest, token};
use sqlx::{FromRow, PgPool};

#[derive(Debug, Clone, FromRow)]
pub struct Session {
    pub github_id: i64,
    pub login: String,
    pub csrf: String,
    pub admin: bool,
}

pub async fn begin_login(pool: &PgPool, state: &str, browser: &str, verifier: &str) -> Result<()> {
    sqlx::query("INSERT INTO login_states(state_hash,browser_hash,verifier,expires_at) VALUES($1,$2,$3,now()+interval '5 minutes')")
        .bind(digest(state)).bind(digest(browser)).bind(verifier).execute(pool).await?;
    Ok(())
}

pub async fn consume_login(pool: &PgPool, state: &str, browser: &str) -> Result<String> {
    Ok(sqlx::query_scalar("DELETE FROM login_states WHERE state_hash=$1 AND browser_hash=$2 AND expires_at>now() RETURNING verifier")
        .bind(digest(state)).bind(digest(browser)).fetch_one(pool).await?)
}

pub async fn new_session(
    pool: &PgPool,
    github_id: i64,
    login: &str,
    previous: Option<&str>,
) -> Result<String> {
    let raw = token();
    let mut tx = pool.begin().await?;
    sqlx::query("INSERT INTO users(github_id,login) VALUES($1,$2) ON CONFLICT(github_id) DO UPDATE SET login=$2,updated_at=now()")
        .bind(github_id).bind(login).execute(&mut *tx).await?;
    if let Some(previous) = previous {
        sqlx::query("DELETE FROM sessions WHERE token_hash=$1")
            .bind(digest(previous))
            .execute(&mut *tx)
            .await?;
    }
    // The users upsert above serializes session creation for each account.
    sqlx::query("DELETE FROM sessions WHERE token_hash IN (SELECT token_hash FROM sessions WHERE github_id=$1 ORDER BY expires_at DESC,token_hash OFFSET 4)")
        .bind(github_id).execute(&mut *tx).await?;
    sqlx::query("INSERT INTO sessions(token_hash,github_id,csrf,expires_at) VALUES($1,$2,$3,now()+interval '12 hours')")
        .bind(digest(&raw)).bind(github_id).bind(token()).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(raw)
}

pub async fn session(pool: &PgPool, raw: &str) -> Result<Option<Session>> {
    Ok(sqlx::query_as("SELECT s.github_id,u.login,s.csrf,EXISTS(SELECT 1 FROM admins a WHERE a.github_id=s.github_id) AS admin FROM sessions s JOIN users u ON u.github_id=s.github_id WHERE s.token_hash=$1 AND s.expires_at>now()")
        .bind(digest(raw)).fetch_optional(pool).await?)
}

pub async fn logout(pool: &PgPool, raw: &str) -> Result<()> {
    sqlx::query("DELETE FROM sessions WHERE token_hash=$1")
        .bind(digest(raw))
        .execute(pool)
        .await?;
    Ok(())
}

pub async fn admin_change(
    pool: &PgPool,
    id: i64,
    grant: bool,
    recovery: bool,
    operator: &str,
    reason: &str,
) -> Result<()> {
    ensure!(
        id > 0 && !reason.trim().is_empty() && !operator.trim().is_empty(),
        "identity, operator and reason are required"
    );
    let mut tx = pool.begin().await?;
    sqlx::query("SELECT pg_advisory_xact_lock(704311)")
        .execute(&mut *tx)
        .await?;
    if grant {
        sqlx::query("INSERT INTO admins(github_id) VALUES($1) ON CONFLICT DO NOTHING")
            .bind(id)
            .execute(&mut *tx)
            .await?;
    } else {
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM admins WHERE github_id<>$1")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
        ensure!(
            count > 0 || recovery,
            "refusing to revoke final administrator without --recovery-override"
        );
        sqlx::query("DELETE FROM admins WHERE github_id=$1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    sqlx::query("INSERT INTO audit_events(operator,action,target,reason) VALUES($1,$2,$3,$4)")
        .bind(operator)
        .bind(if grant { "admin.grant" } else { "admin.revoke" })
        .bind(id.to_string())
        .bind(reason)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

/// Bounded periodic maintenance, never performed on the anonymous login hot path.
pub async fn cleanup_expired(pool: &PgPool) -> Result<()> {
    sqlx::query("DELETE FROM login_states WHERE state_hash IN (SELECT state_hash FROM login_states WHERE expires_at<now() LIMIT 1000)").execute(pool).await?;
    sqlx::query("DELETE FROM sessions WHERE token_hash IN (SELECT token_hash FROM sessions WHERE expires_at<now() LIMIT 1000)").execute(pool).await?;
    Ok(())
}
