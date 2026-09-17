//! PostgreSQL persistence and durable work leasing.
pub mod artifacts;
pub mod courses;
pub mod grading;
pub mod identity;
pub mod queue;
pub mod submissions;

use anyhow::{Context, Result};
use sqlx::{PgPool, postgres::PgPoolOptions};
use std::{path::Path, time::Duration};

pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../../migrations");

pub async fn connect(secret_file: &Path) -> Result<PgPool> {
    let url = tokio::fs::read_to_string(secret_file)
        .await
        .context("reading database credential file")?;
    Ok(PgPoolOptions::new()
        .max_connections(10)
        .acquire_timeout(Duration::from_secs(5))
        .connect(url.trim())
        .await?)
}

pub async fn healthy(pool: &PgPool) -> Result<()> {
    sqlx::query!("SELECT 1 AS \"ok!\"").fetch_one(pool).await?;
    Ok(())
}
