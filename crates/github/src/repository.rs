//! Pinned template seeding, exact snapshots, invitations, and permission verification.
use crate::{Account, GitHub};
use anyhow::{Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use grading_core::{
    config::identifier,
    integrity::{Blob, MAX_FILE_BYTES, MAX_FILES, MAX_SNAPSHOT_BYTES, Snapshot, safe_path},
    security::valid_hex,
};
use reqwest::Method;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use uuid::Uuid;

fn verify_base_permission(organization: &Value) -> Result<()> {
    let permission = match organization.get("default_repository_permission") {
        Some(Value::String(value)) => match value.as_str() {
            "none" => return Ok(()),
            "read" => "read",
            "write" => "write",
            "admin" => "admin",
            _ => "unknown",
        },
        None | Some(Value::Null) => "unavailable",
        _ => "unknown",
    };
    tracing::warn!(
        stage = "organization_permissions",
        base_permission = permission,
        "cannot verify organization base permission is none"
    );
    match permission {
        "unavailable" => anyhow::bail!("organization base permission unavailable"),
        "unknown" => anyhow::bail!("organization base permission unrecognized"),
        _ => anyhow::bail!("organization base permission must be none"),
    }
}

#[derive(Debug, Deserialize)]
pub struct Repository {
    pub id: i64,
    pub name: String,
    pub owner: Account,
    pub private: bool,
    pub description: Option<String>,
    pub default_branch: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_permission_requires_explicit_none() {
        assert!(verify_base_permission(&json!({"default_repository_permission":"none"})).is_ok());
        for value in [json!({}), json!({"default_repository_permission":null})] {
            assert_eq!(
                verify_base_permission(&value).unwrap_err().to_string(),
                "organization base permission unavailable"
            );
        }
        for permission in ["read", "write", "admin"] {
            assert_eq!(
                verify_base_permission(&json!({"default_repository_permission":permission}))
                    .unwrap_err()
                    .to_string(),
                "organization base permission must be none"
            );
        }
        let error =
            verify_base_permission(&json!({"default_repository_permission":"SECRET"})).unwrap_err();
        assert_eq!(
            error.to_string(),
            "organization base permission unrecognized"
        );
    }

    use axum::{
        Json, Router,
        extract::State,
        http::StatusCode,
        response::IntoResponse,
        routing::{get, post},
    };
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[derive(Clone)]
    struct Fixture {
        nonce: Uuid,
        creates: Arc<AtomicUsize>,
    }
    fn response(nonce: Uuid) -> Value {
        json!({"id":55,"name":"student-repo","owner":{"id":1,"login":"course","type":"Organization"},"private":true,"description":format!("grading-provision:{nonce}"),"default_branch":"main"})
    }
    async fn existing(State(state): State<Fixture>) -> axum::response::Response {
        if state.creates.load(Ordering::SeqCst) == 0 {
            StatusCode::NOT_FOUND.into_response()
        } else {
            Json(response(state.nonce)).into_response()
        }
    }
    async fn create(State(state): State<Fixture>) -> StatusCode {
        state.creates.fetch_add(1, Ordering::SeqCst);
        StatusCode::SERVICE_UNAVAILABLE
    }

    #[tokio::test]
    async fn recovery_requires_provisioning_marker_and_never_duplicates_creation() {
        let fixture = Fixture {
            nonce: Uuid::new_v4(),
            creates: Arc::new(AtomicUsize::new(0)),
        };
        let app = Router::new()
            .route(
                "/orgs/course",
                get(|| async { Json(json!({"id":1,"default_repository_permission":"none"})) }),
            )
            .route("/orgs/course/repos", post(create))
            .route("/repos/course/student-repo", get(existing))
            .route(
                "/repos/course/student-repo/teams",
                get(|| async { Json(json!([])) }),
            )
            .with_state(fixture.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let github = GitHub::fixture(format!("http://{}", listener.local_addr().unwrap()));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        assert!(
            github
                .ensure_private_repository("course", "student-repo", fixture.nonce)
                .await
                .is_err()
        );
        let recovered = github
            .ensure_private_repository("course", "student-repo", fixture.nonce)
            .await
            .unwrap();
        assert_eq!(recovered.id, 55);
        assert_eq!(fixture.creates.load(Ordering::SeqCst), 1);
        assert!(
            github
                .ensure_private_repository("course", "student-repo", Uuid::new_v4())
                .await
                .is_err()
        );
        server.abort();
    }

    #[tokio::test]
    async fn rate_limits_set_a_retry_deadline() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let github = GitHub::fixture(format!("http://{}", listener.local_addr().unwrap()));
        let app = Router::new().route(
            "/limited",
            get(|| async {
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    [("retry-after", "120")],
                    "limited",
                )
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        assert!(
            github
                .request::<Value>(Method::GET, "/limited", None)
                .await
                .is_err()
        );
        assert!(
            github.retry_at().await.unwrap() > chrono::Utc::now() + chrono::Duration::seconds(110)
        );
        server.abort();
    }
    #[tokio::test]
    async fn student_snapshot_rejects_large_trees_before_blobs_and_reuses_blobs() {
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = calls.clone();
        let sha = "a".repeat(40);
        let app = Router::new()
            .route("/repos/course/repo/git/commits/{sha}", get(|| async {
                Json(json!({"sha":"a".repeat(40), "tree":{"sha":"b".repeat(40)}}))
            }))
            .route("/repos/course/repo/git/trees/{sha}", get(|| async {
                Json(json!({"truncated":false,"tree":[
                    {"path":"src/a", "type":"blob","mode":"100644","sha":"c".repeat(40),"size":1},
                    {"path":"src/b", "type":"blob","mode":"100755","sha":"c".repeat(40),"size":1}
                ]}))
            }))
            .route("/repos/course/repo/git/blobs/{sha}", get(move || {
                counter.fetch_add(1, Ordering::SeqCst);
                async { Json(json!({"content":"eA==","encoding":"base64","size":1})) }
            }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let github = GitHub::fixture(format!("http://{}", listener.local_addr().unwrap()));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        assert!(
            github
                .snapshot_with_limits("course/repo", &sha, 1, 1024)
                .await
                .is_err()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(
            github
                .snapshot_with_limits("course/repo", &sha, 10, 1)
                .await
                .is_err()
        );
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        for _ in 0..2 {
            let snapshot = github.student_snapshot("course/repo", &sha).await.unwrap();
            assert_eq!(snapshot.files["src/b"].mode, "100755");
            assert_eq!(snapshot.files["src/a"].bytes().unwrap(), b"x");
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        server.abort();
    }
}

#[derive(Deserialize)]
struct Object {
    sha: String,
}
#[derive(Deserialize)]
struct GitRef {
    object: Object,
}
#[derive(Deserialize)]
struct Commit {
    sha: String,
    tree: Object,
}
#[derive(Deserialize)]
struct Tree {
    tree: Vec<TreeEntry>,
    truncated: bool,
}
#[derive(Deserialize)]
struct TreeEntry {
    path: String,
    mode: String,
    sha: String,
    #[serde(rename = "type")]
    kind: String,
    size: Option<usize>,
}
#[derive(Deserialize)]
struct ApiBlob {
    content: String,
    encoding: String,
    size: usize,
}

fn validate_tree(tree: &Tree, max_files: usize, max_bytes: usize) -> Result<()> {
    ensure!(
        !tree.truncated && tree.tree.len() <= max_files * 2,
        "source tree exceeds limits"
    );
    let mut files = 0;
    let mut bytes = 0usize;
    for entry in &tree.tree {
        safe_path(&entry.path)?;
        if entry.kind == "tree" {
            continue;
        }
        files += 1;
        let size = if entry.kind == "commit" && entry.mode == "160000" {
            40
        } else {
            ensure!(
                entry.kind == "blob"
                    && matches!(entry.mode.as_str(), "100644" | "100755" | "120000"),
                "unsupported Git object"
            );
            entry
                .size
                .ok_or_else(|| anyhow::anyhow!("missing blob size"))?
        };
        ensure!(size <= MAX_FILE_BYTES, "source file too large");
        bytes = bytes
            .checked_add(size)
            .ok_or_else(|| anyhow::anyhow!("snapshot exceeds limits"))?;
        ensure!(
            files <= max_files && bytes <= max_bytes,
            "snapshot exceeds limits"
        );
    }
    Ok(())
}

pub fn repo_path(repository: &str) -> Result<String> {
    ensure!(
        grading_core::config::github_repository(repository),
        "invalid repository name"
    );
    Ok(format!("/repos/{repository}"))
}

impl GitHub {
    pub async fn snapshot(&self, repository: &str, sha: &str) -> Result<Snapshot> {
        self.snapshot_with_limits(repository, sha, MAX_FILES, MAX_SNAPSHOT_BYTES)
            .await
    }

    /// Conservative student budgets; trusted template/grader imports retain platform limits.
    pub async fn student_snapshot(&self, repository: &str, sha: &str) -> Result<Snapshot> {
        self.snapshot_with_limits(repository, sha, 512, 8 * 1024 * 1024)
            .await
    }

    async fn snapshot_with_limits(
        &self,
        repository: &str,
        sha: &str,
        files: usize,
        bytes: usize,
    ) -> Result<Snapshot> {
        tokio::time::timeout(
            std::time::Duration::from_secs(120),
            self.fetch_snapshot(repository, sha, files, bytes),
        )
        .await?
    }

    async fn fetch_snapshot(
        &self,
        repository: &str,
        sha: &str,
        max_files: usize,
        max_bytes: usize,
    ) -> Result<Snapshot> {
        ensure!(valid_hex(sha, 40), "invalid commit SHA");
        let path = repo_path(repository)?;
        let commit: Commit = self
            .request(Method::GET, &format!("{path}/git/commits/{sha}"), None)
            .await?;
        ensure!(commit.sha == sha, "commit identity mismatch");
        let tree: Tree = self
            .request(
                Method::GET,
                &format!("{path}/git/trees/{}?recursive=1", commit.tree.sha),
                None,
            )
            .await?;
        validate_tree(&tree, max_files, max_bytes)?;
        let mut files = BTreeMap::new();
        let mut total = 0;
        for entry in tree.tree {
            safe_path(&entry.path)?;
            if entry.kind == "tree" {
                continue;
            }
            if entry.kind == "commit" && entry.mode == "160000" {
                files.insert(
                    entry.path,
                    Blob {
                        mode: entry.mode,
                        data: STANDARD.encode(entry.sha.as_bytes()),
                    },
                );
                continue;
            }
            ensure!(
                entry.kind == "blob"
                    && matches!(entry.mode.as_str(), "100644" | "100755" | "120000"),
                "unsupported Git object"
            );
            ensure!(
                entry.size.is_some_and(|size| size <= MAX_FILE_BYTES),
                "source file too large"
            );
            total += entry.size.unwrap_or(0);
            ensure!(
                total <= MAX_SNAPSHOT_BYTES && files.len() < MAX_FILES,
                "snapshot exceeds limits"
            );
            let cache_key = format!("{repository}:{}", entry.sha);
            let cached = self.blobs.lock().await.get(&cache_key).cloned();
            let blob = if let Some(blob) = cached {
                blob
            } else {
                let blob: ApiBlob = self
                    .request(
                        Method::GET,
                        &format!("{path}/git/blobs/{}", entry.sha),
                        None,
                    )
                    .await?;
                ensure!(
                    blob.encoding == "base64" && blob.size <= MAX_FILE_BYTES,
                    "invalid blob encoding or size"
                );
                let bytes = STANDARD.decode(blob.content.replace(['\r', '\n'], ""))?;
                ensure!(
                    bytes.len() == blob.size && Some(blob.size) == entry.size,
                    "invalid blob size"
                );
                let blob = Blob {
                    mode: entry.mode.clone(),
                    data: STANDARD.encode(bytes),
                };
                let mut cache = self.blobs.lock().await;
                if cache.len() >= 512
                    || cache.values().map(|b| b.data.len()).sum::<usize>() + blob.data.len()
                        > 16 * 1024 * 1024
                {
                    cache.clear();
                }
                cache.insert(cache_key, blob.clone());
                blob
            };
            files.insert(
                entry.path,
                Blob {
                    mode: entry.mode,
                    data: blob.data,
                },
            );
        }
        let snapshot = Snapshot {
            sha: sha.to_owned(),
            files,
        };
        snapshot.validate_structure()?;
        Ok(snapshot)
    }

    pub async fn branch_sha(&self, repository: &str, branch: &str) -> Result<String> {
        ensure!(identifier(branch), "invalid branch");
        let reference: GitRef = self
            .request(
                Method::GET,
                &format!("{}/git/ref/heads/{branch}", repo_path(repository)?),
                None,
            )
            .await?;
        ensure!(valid_hex(&reference.object.sha, 40), "invalid branch SHA");
        Ok(reference.object.sha)
    }

    pub async fn verify_repository(&self, full_name: &str, id: i64) -> Result<Repository> {
        let repository: Repository = self
            .request(Method::GET, &repo_path(full_name)?, None)
            .await?;
        ensure!(
            repository.id == id
                && repository.private
                && format!("{}/{}", repository.owner.login, repository.name)
                    .eq_ignore_ascii_case(full_name),
            "repository identity or privacy changed"
        );
        Ok(repository)
    }

    pub async fn ensure_private_repository(
        &self,
        org: &str,
        name: &str,
        nonce: Uuid,
    ) -> Result<Repository> {
        ensure!(
            identifier(org) && grading_core::config::github_repository(&format!("{org}/{name}")),
            "invalid repository allocation"
        );
        let organization: Value = self
            .request(Method::GET, &format!("/orgs/{org}"), None)
            .await?;
        verify_base_permission(&organization)?;
        let marker = format!("grading-provision:{nonce}");
        let repository = match self.optional::<Repository>(&format!("/repos/{org}/{name}")).await? {
            Some(repository) => repository,
            None => self.request(Method::POST,&format!("/orgs/{org}/repos"),Some(json!({"name":name,"private":true,"description":marker,"auto_init":true,"has_issues":false,"has_projects":false,"has_wiki":false}))).await?,
        };
        ensure!(
            repository.private
                && repository.owner.id == organization["id"].as_i64().unwrap_or(-1)
                && repository.description.as_deref() == Some(&marker),
            "refusing to adopt unverified repository"
        );
        self.verify_no_teams(&format!("{org}/{name}")).await?;
        Ok(repository)
    }

    pub async fn verify_no_teams(&self, repository: &str) -> Result<()> {
        let teams: Vec<Value> = self
            .request(
                Method::GET,
                &format!("{}/teams?per_page=100", repo_path(repository)?),
                None,
            )
            .await?;
        ensure!(
            teams.is_empty(),
            "repository inherits team access; instructor review required"
        );
        Ok(())
    }

    pub async fn seed(&self, repository: &str, branch: &str, source: &Snapshot) -> Result<()> {
        source.validate()?;
        let path = repo_path(repository)?;
        self.empty(
            Method::PUT,
            &format!("{path}/actions/permissions"),
            Some(json!({"enabled":false})),
        )
        .await?;
        let info: Repository = self.request(Method::GET, &path, None).await?;
        let parent = self.branch_sha(repository, &info.default_branch).await?;
        let mut tree = Vec::new();
        for (name, blob) in &source.files {
            let object: Object = self
                .request(
                    Method::POST,
                    &format!("{path}/git/blobs"),
                    Some(json!({"content":blob.data,"encoding":"base64"})),
                )
                .await?;
            tree.push(json!({"path":name,"mode":blob.mode,"type":"blob","sha":object.sha}));
        }
        let object: Object = self
            .request(
                Method::POST,
                &format!("{path}/git/trees"),
                Some(json!({"tree":tree})),
            )
            .await?;
        let commit: Object = self.request(Method::POST,&format!("{path}/git/commits"),Some(json!({"message":format!("Approved template {}",source.sha),"tree":object.sha,"parents":[parent]}))).await?;
        let reference = format!("{path}/git/refs/heads/{branch}");
        if self
            .optional::<Value>(&format!("{path}/git/ref/heads/{branch}"))
            .await?
            .is_some()
        {
            self.empty(
                Method::PATCH,
                &reference,
                Some(json!({"sha":commit.sha,"force":false})),
            )
            .await?;
        } else {
            self.empty(
                Method::POST,
                &format!("{path}/git/refs"),
                Some(json!({"ref":format!("refs/heads/{branch}"),"sha":commit.sha})),
            )
            .await?;
        }
        self.empty(Method::PATCH, &path, Some(json!({"default_branch":branch})))
            .await?;
        let seeded = self
            .snapshot(repository, &self.branch_sha(repository, branch).await?)
            .await?;
        ensure!(
            serde_json::to_vec(&seeded.files)? == serde_json::to_vec(&source.files)?,
            "seeded files do not match pinned template"
        );
        self.empty(Method::PUT,&format!("{path}/actions/permissions/workflow"),Some(json!({"default_workflow_permissions":"read","can_approve_pull_request_reviews":false}))).await?;
        Ok(())
    }

    /// Student pushes must never execute outside the grading sandbox.
    pub async fn disable_actions(&self, repository: &str) -> Result<()> {
        self.empty(
            Method::PUT,
            &format!("{}/actions/permissions", repo_path(repository)?),
            Some(json!({"enabled":false})),
        )
        .await
    }

    pub async fn invite(&self, repository: &str, github_id: i64) -> Result<Option<String>> {
        self.disable_actions(repository).await?;
        let account = self.account(github_id).await?;
        let path = repo_path(repository)?;
        let permission: Option<Value> = self
            .optional(&format!(
                "{path}/collaborators/{}/permission",
                account.login
            ))
            .await?;
        if permission
            .as_ref()
            .is_some_and(|p| p["permission"] == "write")
        {
            return Ok(None);
        }
        let invitations: Vec<Value> = self
            .request(
                Method::GET,
                &format!("{path}/invitations?per_page=100"),
                None,
            )
            .await?;
        ensure!(invitations.len() < 100, "unexpected invitation count");
        if invitations
            .iter()
            .any(|invitation| invitation["invitee"]["id"].as_i64() == Some(github_id))
        {
            return Ok(Some(format!("https://github.com/{repository}/invitations")));
        }
        self.empty(
            Method::PUT,
            &format!("{path}/collaborators/{}", account.login),
            Some(json!({"permission":"push"})),
        )
        .await?;
        Ok(Some(format!("https://github.com/{repository}/invitations")))
    }

    pub async fn permission(&self, repository: &str, github_id: i64) -> Result<String> {
        let account = self.account(github_id).await?;
        let permission: Option<Value> = self
            .optional(&format!(
                "{}/collaborators/{}/permission",
                repo_path(repository)?,
                account.login
            ))
            .await?;
        Ok(permission
            .and_then(|v| {
                v["role_name"]
                    .as_str()
                    .or_else(|| v["permission"].as_str())
                    .map(str::to_owned)
            })
            .unwrap_or_else(|| "none".into()))
    }

    pub async fn lock(&self, repository: &str, github_id: i64) -> Result<()> {
        let path = repo_path(repository)?;
        let account = self.account(github_id).await?;
        self.verify_no_teams(repository).await?;
        let invitations: Vec<Value> = self
            .request(
                Method::GET,
                &format!("{path}/invitations?per_page=100"),
                None,
            )
            .await?;
        ensure!(invitations.len() < 100, "unexpected invitation count");
        for invitation in invitations {
            if invitation["invitee"]["id"].as_i64() == Some(github_id) {
                let id = invitation["id"]
                    .as_i64()
                    .ok_or_else(|| anyhow::anyhow!("invalid invitation"))?;
                self.empty(
                    Method::PATCH,
                    &format!("{path}/invitations/{id}"),
                    Some(json!({"permissions":"read"})),
                )
                .await?;
            }
        }
        if self.permission(repository, github_id).await? != "none" {
            self.empty(
                Method::PUT,
                &format!("{path}/collaborators/{}", account.login),
                Some(json!({"permission":"pull"})),
            )
            .await?;
        }
        let permission = self.permission(repository, github_id).await?;
        ensure!(
            permission == "read" || permission == "none",
            "effective write access remains; instructor review required"
        );
        Ok(())
    }

    pub async fn publish_check(
        &self,
        repository: &str,
        sha: &str,
        run: Uuid,
        status: &str,
        points: Option<i32>,
        existing: Option<i64>,
    ) -> Result<i64> {
        let path = repo_path(repository)?;
        let mut found = existing;
        if found.is_none() {
            let checks: Value = self
                .request(
                    Method::GET,
                    &format!(
                        "{path}/commits/{sha}/check-runs?check_name=Official%20grade&per_page=100"
                    ),
                    None,
                )
                .await?;
            if let Some(checks) = checks["check_runs"].as_array() {
                found = checks
                    .iter()
                    .find(|c| c["external_id"] == run.to_string())
                    .and_then(|c| c["id"].as_i64());
            }
        }
        let summary = match points {
            Some(points) => format!(
                "Official points: {points}. See the portal report for public-test results and any private adjustment."
            ),
            None => format!(
                "Official result: {status}. No score awarded; this is not a misconduct finding."
            ),
        };
        let body = json!({"name":"Official grade","head_sha":sha,"external_id":run.to_string(),"status":"completed","conclusion":if status=="completed" {"success"} else {"neutral"},"output":{"title":"Official grading result","summary":summary}});
        let result: Value = if let Some(id) = found {
            self.request(
                Method::PATCH,
                &format!("{path}/check-runs/{id}"),
                Some(body),
            )
            .await?
        } else {
            self.request(Method::POST, &format!("{path}/check-runs"), Some(body))
                .await?
        };
        result["id"]
            .as_i64()
            .ok_or_else(|| anyhow::anyhow!("missing Check ID"))
    }
}
