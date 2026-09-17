//! PostgreSQL persistence and durable work leasing.
pub mod artifacts;
pub mod courses;
pub mod exercises;
pub mod grading;
pub mod identity;
pub mod queue;
pub mod submissions;

use anyhow::{Context, Result};
use sqlx::{PgPool, postgres::PgPoolOptions};
use std::{path::Path, time::Duration};

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

pub fn diagnostics(error: &anyhow::Error) -> grading_core::diagnostics::Details {
    let mut details = grading_core::diagnostics::describe(error);
    if let Some(error) = error.downcast_ref::<sqlx::Error>() {
        details.reason = match error {
            sqlx::Error::RowNotFound => "record_not_found",
            sqlx::Error::PoolTimedOut => "database_pool_timeout",
            sqlx::Error::PoolClosed => "database_pool_closed",
            sqlx::Error::Database(db) => match db.code().as_deref() {
                Some("42501") => "database_permission_denied",
                Some("23505") => "database_unique_conflict",
                Some("23503") => "database_foreign_key",
                Some("23514") => "database_check_constraint",
                Some("40001") => "database_serialization_retry",
                Some("40P01") => "database_deadlock",
                _ => "database_error",
            },
            _ => "database_connection_or_protocol",
        };
    }
    details
}

pub async fn connect(secret_file: &Path) -> Result<PgPool> {
    let url =
        tokio::fs::read_to_string(secret_file)
            .await
            .context(grading_core::diagnostics::Stage(
                "database_credentials_read",
            ))?;
    PgPoolOptions::new()
        .max_connections(10)
        .acquire_timeout(Duration::from_secs(5))
        .connect(url.trim())
        .await
        .context(grading_core::diagnostics::Stage("database_connect"))
}

pub async fn healthy(pool: &PgPool) -> Result<()> {
    sqlx::query!("SELECT 1 AS \"ok!\"").fetch_one(pool).await?;
    Ok(())
}
