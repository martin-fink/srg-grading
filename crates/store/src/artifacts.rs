//! Content-addressed source and report files, published atomically before DB references.
use anyhow::{Result, ensure};
use grading_core::{
    integrity::MAX_SNAPSHOT_BYTES,
    security::{digest, valid_hex},
};
use sqlx::PgPool;
use std::path::{Path, PathBuf};
use tokio::io::AsyncWriteExt;
use uuid::Uuid;

#[derive(Clone)]
pub struct Artifacts {
    root: PathBuf,
}

impl Artifacts {
    pub async fn new(root: impl AsRef<Path>) -> Result<Self> {
        tokio::fs::create_dir_all(root.as_ref()).await?;
        Ok(Self {
            root: tokio::fs::canonicalize(root).await?,
        })
    }

    pub async fn put(&self, pool: &PgPool, kind: &str, bytes: &[u8]) -> Result<String> {
        ensure!(
            bytes.len() <= MAX_SNAPSHOT_BYTES * 2,
            "artifact exceeds limit"
        );
        let hash = digest(bytes);
        let destination = self.root.join(&hash);
        let temporary = self.root.join(format!(".tmp-{}", Uuid::new_v4()));
        let mut file = tokio::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)
            .await?;
        file.write_all(bytes).await?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&temporary, destination).await?;
        let directory = tokio::fs::File::open(&self.root).await?;
        directory.sync_all().await?;
        sqlx::query(
            "INSERT INTO artifacts(digest,bytes,kind) VALUES($1,$2,$3) ON CONFLICT DO NOTHING",
        )
        .bind(&hash)
        .bind(bytes.len() as i64)
        .bind(kind)
        .execute(pool)
        .await?;
        Ok(hash)
    }

    pub async fn get(&self, hash: &str) -> Result<Vec<u8>> {
        ensure!(valid_hex(hash, 64), "invalid artifact digest");
        let path = self.root.join(hash);
        ensure!(
            tokio::fs::metadata(&path).await?.len() <= (MAX_SNAPSHOT_BYTES * 2) as u64,
            "artifact exceeds limit"
        );
        let bytes = tokio::fs::read(path).await?;
        ensure!(digest(&bytes) == hash, "artifact digest mismatch");
        Ok(bytes)
    }
}
