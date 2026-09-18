//! Reconcile completed and abandoned staging after asynchronous Pod deletion.
use anyhow::Result;
use grading_executor::Config;
use k8s_openapi::api::core::v1::Pod;
use kube::{Api, api::ListParams};
use std::{
    collections::HashSet,
    path::Path,
    time::{Duration, SystemTime},
};

pub async fn reconcile(config: &Config, pods: &Api<Pod>) -> Result<()> {
    // Fail closed if Kubernetes cannot confirm which staging paths are still mounted.
    let pods = pods
        .list(&ListParams::default().labels("app=grading-sandbox"))
        .await?;
    let live = pods
        .items
        .into_iter()
        .filter_map(|pod| pod.metadata.labels?.get("grading-lease").cloned())
        .collect();
    let grace = Duration::from_secs(
        u64::from(
            config
                .registry
                .as_ref()
                .map_or(86400, |r| r.timeout_seconds),
        ) + 600,
    );
    sweep(
        &config.staging_root.join("runs"),
        &live,
        SystemTime::now(),
        grace,
    )
    .await
}

async fn sweep(
    root: &Path,
    live: &HashSet<String>,
    now: SystemTime,
    grace: Duration,
) -> Result<()> {
    let mut entries = match tokio::fs::read_dir(root).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    while let Some(entry) = entries.next_entry().await? {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !grading_core::security::valid_hex(&name, 32) || live.contains(&name) {
            continue;
        }
        // Never traverse symlink entries or unrelated files in the staging root.
        if !entry.file_type().await?.is_dir() {
            continue;
        }
        let metadata = entry.metadata().await?;
        let completed = tokio::fs::try_exists(entry.path().join(".finished")).await?;
        let abandoned = now
            .duration_since(metadata.modified()?)
            .is_ok_and(|age| age > grace);
        if completed || abandoned {
            tokio::fs::remove_dir_all(entry.path()).await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn cleanup_waits_for_pods_and_recovers_after_restart() {
        let root = std::env::temp_dir().join(format!("grading-cleanup-{}", uuid::Uuid::new_v4()));
        let active = "a".repeat(32);
        let finished = "b".repeat(32);
        let abandoned = "c".repeat(32);
        for name in [&active, &finished, &abandoned] {
            tokio::fs::create_dir_all(root.join(name)).await.unwrap();
        }
        for name in [&active, &finished] {
            tokio::fs::write(root.join(name).join(".finished"), b"")
                .await
                .unwrap();
        }
        let live = HashSet::from([active.clone()]);
        sweep(&root, &live, SystemTime::now(), Duration::from_secs(600))
            .await
            .unwrap();
        assert!(root.join(&active).exists());
        assert!(!root.join(&finished).exists());
        assert!(root.join(&abandoned).exists());
        sweep(
            &root,
            &HashSet::new(),
            SystemTime::now() + Duration::from_secs(601),
            Duration::from_secs(600),
        )
        .await
        .unwrap();
        assert!(!root.join(&active).exists());
        assert!(!root.join(&abandoned).exists());
        tokio::fs::remove_dir_all(root).await.unwrap();
    }
}
