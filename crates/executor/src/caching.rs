//! Cache preparation runs during registration, before publishing an exercise revision.
use crate::Config;
use anyhow::{Context, Result, ensure};
use grading_core::{
    caching::{Caching, Seed},
    integrity::{Snapshot, safe_path},
    protocol::{Lease, Revision},
    security::digest,
};
use k8s_openapi::api::{batch::v1::Job, core::v1::Pod};
use kube::{
    Api, Client,
    api::{DeleteParams, ListParams, LogParams, PostParams},
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File, OpenOptions},
    io::Read,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::Path,
    time::Duration,
};

const MAX_ENTRIES: usize = 100_000;
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Entry {
    path: String,
    bytes: u64,
    executable: bool,
    hash: String,
}
#[derive(Serialize, Deserialize)]
struct Manifest {
    input_key: String,
    entries: Vec<Entry>,
}
fn directory(path: &Path) -> Result<()> {
    ensure!(
        fs::symlink_metadata(path)?.is_dir(),
        "cache path is not a real directory"
    );
    Ok(())
}
fn scan(
    root: &Path,
    path: &Path,
    entries: &mut Vec<Entry>,
    total: &mut u64,
    limit: u64,
    seal: bool,
) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(entries.len() < MAX_ENTRIES, "too many cache entries");
    let relative = path
        .strip_prefix(root)?
        .to_str()
        .context("non-UTF8 cache path")?
        .to_owned();
    safe_path(&relative)?;
    ensure!(
        relative.split('/').count() <= 32,
        "cache directory nesting exceeds limit"
    );
    if metadata.is_dir() {
        entries.push(Entry {
            path: relative,
            bytes: 0,
            executable: true,
            hash: String::new(),
        });
        let mut children = fs::read_dir(path)?
            .take(MAX_ENTRIES + 1)
            .collect::<std::io::Result<Vec<_>>>()?;
        ensure!(children.len() <= MAX_ENTRIES, "too many cache entries");
        children.sort_by_key(|entry| entry.file_name());
        for child in children {
            scan(root, &child.path(), entries, total, limit, seal)?;
        }
        if seal {
            fs::set_permissions(path, fs::Permissions::from_mode(0o555))?;
            File::open(path)?.sync_all()?;
        }
    } else {
        ensure!(
            metadata.is_file() && metadata.nlink() == 1,
            "cache exports must be regular files without hard links"
        );
        *total = total
            .checked_add(metadata.len())
            .context("cache size overflow")?;
        ensure!(*total <= limit, "cache export exceeds size limit");
        let mut file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
            .open(path)?;
        let mut hasher = Sha256::new();
        let mut buffer = [0u8; 65536];
        let mut length = 0;
        loop {
            let n = file.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            length += n as u64;
            ensure!(length <= metadata.len(), "cache changed during capture");
            hasher.update(&buffer[..n]);
        }
        ensure!(length == metadata.len(), "cache changed during capture");
        let executable = metadata.mode() & 0o111 != 0;
        entries.push(Entry {
            path: relative,
            bytes: length,
            executable,
            hash: hex::encode(hasher.finalize()),
        });
        if seal {
            file.set_permissions(fs::Permissions::from_mode(if executable {
                0o555
            } else {
                0o444
            }))?;
            file.sync_all()?;
        }
    }
    Ok(())
}
fn manifest(root: &Path, config: &Caching, key: &str, seal: bool) -> Result<Vec<u8>> {
    directory(root)?;
    let mut entries = Vec::new();
    for artifact in &config.artifacts {
        let path = root.join(&artifact.name);
        directory(&path)?;
        scan(
            root,
            &path,
            &mut entries,
            &mut 0,
            u64::from(artifact.max_size_gib) << 30,
            seal,
        )?;
    }
    Ok(serde_json::to_vec(&Manifest {
        input_key: key.into(),
        entries,
    })?)
}
/// Verify content rather than timestamps. This also detects accidental storage corruption.
pub fn verify(config: &Config, seed: &Seed) -> Result<()> {
    seed.validate()?;
    ensure!(
        seed.namespace == config.namespace && seed.source_pvc == config.source_pvc,
        "cache is on a different staging volume"
    );
    let root = config.staging_root.join("caches");
    directory(&root)?;
    let data = root.join(&seed.digest);
    directory(&data)?;
    ensure!(
        digest(manifest(&data, &seed.config, &seed.input_key, false)?) == seed.digest,
        "cache artifact digest mismatch"
    );
    Ok(())
}
/// Large seed verification must not block the administration worker's heartbeat.
pub async fn verify_async(config: &Config, seed: &Seed) -> Result<()> {
    let config = config.clone();
    let seed = seed.clone();
    tokio::task::spawn_blocking(move || verify(&config, &seed)).await?
}

fn stage(snapshot: &Snapshot, path: &Path) -> Result<()> {
    snapshot.validate()?;
    fs::create_dir_all(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
    for (name, blob) in &snapshot.files {
        let target = path.join(name);
        fs::create_dir_all(target.parent().context("snapshot parent")?)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(if blob.mode == "100755" { 0o755 } else { 0o644 })
            .open(target)?;
        std::io::Write::write_all(&mut file, &blob.bytes()?)?;
    }
    Ok(())
}
/// Uses the same image allowlist, gVisor isolation and resource caps as grading.
pub fn preparation_job(
    config: &Config,
    revision: &Revision,
    caching: &Caching,
    id: uuid::Uuid,
) -> Result<Job> {
    caching.validate()?;
    let mut revision = revision.clone();
    revision.grader.as_mut().context("missing grader")?.caching = None;
    revision.assignment.resources = caching.resources.clone();
    revision.assignment.timeout_seconds = caching.timeout_seconds;
    let lease = Lease {
        schema_version: 1,
        task_id: id,
        run_id: id,
        lease_token: id,
        expires_at: chrono::Utc::now(),
        sha: revision.assignment.template_revision.clone(),
        revision_digest: revision.digest()?,
        source_digest: "0".repeat(64),
        revision,
        baseline: None,
    };
    let mut value = serde_json::to_value(crate::job(
        config,
        &lease,
        "cache",
        caching.timeout_seconds,
    )?)?;
    let pod = &mut value["spec"]["template"]["spec"];
    pod["securityContext"]["runAsUser"] = json!(10004);
    pod["securityContext"]["runAsGroup"] = json!(10004);
    pod["securityContext"]["fsGroup"] = json!(10004);
    pod["nodeSelector"] = json!({"kubernetes.io/arch": caching.architecture});
    pod["volumes"][0]["persistentVolumeClaim"]["readOnly"] = json!(false);
    let container = &mut pod["containers"][0];
    container["command"] = json!(caching.command);
    container["workingDir"] = json!("/workspace");
    let root = format!("cache-builds/{}", id.simple());
    container["volumeMounts"] = json!([
        {"name":"source","mountPath":"/source","subPath":format!("{root}/source"),"readOnly":true},
        {"name":"source","mountPath":"/recipe","subPath":format!("{root}/recipe"),"readOnly":true},
        {"name":"source","mountPath":"/output","subPath":format!("{root}/output")},
        {"name":"workspace","mountPath":"/workspace"}, {"name":"tmp","mountPath":"/tmp"}
    ]);
    Ok(serde_json::from_value(value)?)
}
// Bound temporary output too; volume-level quotas remain necessary for burst writes.
fn output_budget(root: &Path, limit: u64) -> Result<()> {
    let mut pending = vec![(root.to_owned(), 0)];
    let mut count = 0;
    let mut bytes = 0u64;
    while let Some((path, depth)) = pending.pop() {
        ensure!(depth <= 32, "cache output nesting exceeds limit");
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            count += 1;
            ensure!(count <= MAX_ENTRIES, "too many temporary cache entries");
            let metadata = match fs::symlink_metadata(entry.path()) {
                Ok(m) => m,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            if metadata.is_dir() {
                pending.push((entry.path(), depth + 1));
            } else {
                bytes = bytes
                    .checked_add(metadata.len())
                    .context("cache output size overflow")?;
            }
            ensure!(
                bytes <= limit,
                "temporary cache output exceeds preparation storage budget"
            );
        }
    }
    Ok(())
}
async fn stopped(
    jobs: &Api<Job>,
    pods: &Api<Pod>,
    name: &str,
    selector: &ListParams,
) -> Result<()> {
    match jobs.delete(name, &DeleteParams::foreground()).await {
        Ok(_) => {}
        Err(kube::Error::Api(e)) if e.code == 404 => {}
        Err(e) => return Err(e.into()),
    }
    tokio::time::timeout(Duration::from_secs(120), async {
        loop {
            if pods.list(selector).await?.items.is_empty() {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    })
    .await
    .context("cache pods did not stop; output remains quarantined")??;
    Ok(())
}
/// Synchronous preparation deliberately precedes the existing atomic catalog publication.
/// A failed or interrupted invocation can be retried by repeating add/update/apply.
pub async fn prepare(
    config: &Config,
    revision: &Revision,
    caching: &Caching,
    source: &Snapshot,
    recipe: &Snapshot,
) -> Result<Seed> {
    prepare_with_client(config, revision, caching, source, recipe, None).await
}
async fn prepare_with_client(
    config: &Config,
    revision: &Revision,
    caching: &Caching,
    source: &Snapshot,
    recipe: &Snapshot,
    client: Option<Client>,
) -> Result<Seed> {
    let key = caching.key(&revision.assignment.image, source, recipe)?;
    let id = uuid::Uuid::new_v4();
    let mut job = preparation_job(config, revision, caching, id)?;
    let root = &config.staging_root;
    fs::create_dir_all(root.join("cache-refs"))?;
    fs::create_dir_all(root.join("caches"))?;
    // OS lock is released even on process death. Concurrent invocations wait by retrying.
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join("cache-refs").join(format!("{key}.lock")))?;
    lock.try_lock()
        .context("cache preparation already running; retry this command when it finishes")?;
    let reference = root.join("cache-refs").join(&key);
    if reference.exists() {
        let seed: Seed = serde_json::from_slice(&fs::read(&reference)?)?;
        ensure!(seed.input_key == key, "cache reference key mismatch");
        verify_async(config, &seed).await?;
        println!("Cache ready (reused): {}", seed.digest);
        return Ok(seed);
    }
    let cache_label = &key[..60];
    job.metadata
        .labels
        .get_or_insert_default()
        .insert("cache-key".into(), cache_label.into());
    job.spec
        .as_mut()
        .context("missing job spec")?
        .template
        .metadata
        .as_mut()
        .context("missing pod metadata")?
        .labels
        .get_or_insert_default()
        .insert("cache-key".into(), cache_label.into());
    let name = job.metadata.name.as_deref().context("cache job name")?;
    let build = root.join("cache-builds").join(id.simple().to_string());
    fs::create_dir_all(build.join("output"))?;
    fs::set_permissions(&build, fs::Permissions::from_mode(0o700))?;
    fs::set_permissions(build.join("output"), fs::Permissions::from_mode(0o700))?;
    fs::write(
        build.join("metadata.json"),
        serde_json::to_vec(
            &json!({"input_key":key,"job":name,"namespace":config.namespace,"started_at":chrono::Utc::now()}),
        )?,
    )?;
    stage(source, &build.join("source"))?;
    stage(recipe, &build.join("recipe"))?;
    println!(
        "Preparing cache {key}; job {name}; diagnostics {}",
        build.display()
    );
    let client = match client {
        Some(client) => client,
        None => Client::try_default().await?,
    };
    let jobs: Api<Job> = Api::namespaced(client.clone(), &config.namespace);
    let pods: Api<Pod> = Api::namespaced(client, &config.namespace);
    // Recover a killed registration process before starting another preparation for this key.
    let previous = ListParams::default().labels(&format!("cache-key={cache_label}"));
    for old in jobs.list(&previous).await?.items {
        if let Some(name) = old.metadata.name {
            stopped(&jobs, &pods, &name, &previous).await?;
        }
    }
    let selector = ListParams::default().labels(&format!("grading-lease={}", id.simple()));
    let outcome = tokio::time::timeout(
        Duration::from_secs(u64::from(caching.timeout_seconds) + 60),
        async {
            jobs.create(&PostParams::default(), &job).await?;
            loop {
                let status = jobs.get(name).await?.status.unwrap_or_default();
                if status.succeeded.unwrap_or(0) > 0 {
                    break;
                }
                ensure!(
                    status.failed.unwrap_or(0) == 0,
                    "cache preparation job failed"
                );
                output_budget(
                    &build.join("output"),
                    u64::from(caching.resources.storage_gib) << 30,
                )?;
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
            Ok::<_, anyhow::Error>(())
        },
    )
    .await
    .context("cache preparation timed out")
    .and_then(|r| r);
    // Diagnostic collection is best effort and must never prevent sandbox cleanup.
    let _ = tokio::time::timeout(Duration::from_secs(20), async {
        for pod in pods.list(&selector).await?.items {
            if let Some(status)=&pod.status {
                let states:Vec<_>=status.container_statuses.iter().flatten().map(|container|{
                    let state=container.state.as_ref();
                    json!({"name":container.name,"waiting_reason":state.and_then(|s|s.waiting.as_ref()).and_then(|s|s.reason.as_ref()),"termination_reason":state.and_then(|s|s.terminated.as_ref()).and_then(|s|s.reason.as_ref()),"exit_code":state.and_then(|s|s.terminated.as_ref()).map(|s|s.exit_code)})
                }).collect();
                let conditions:Vec<_>=status.conditions.iter().flatten().map(|c|json!({"type":c.type_,"status":c.status,"reason":c.reason})).collect();
                fs::write(build.join("status.json"),serde_json::to_vec_pretty(&json!({"phase":status.phase,"reason":status.reason,"conditions":conditions,"containers":states}))?)?;
            }
            if let Some(name) = pod.metadata.name {
                let log = pods
                    .logs(
                        &name,
                        &LogParams {
                            limit_bytes: Some(262144),
                            ..Default::default()
                        },
                    )
                    .await?;
                let mut file = OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .mode(0o600)
                    .open(build.join("preparation.log"))?;
                std::io::Write::write_all(&mut file, log.as_bytes())?;
            }
        }
        Ok::<_, anyhow::Error>(())
    })
    .await;
    stopped(&jobs, &pods, name, &selector).await?;
    outcome?;
    let exports = build.join("sealed");
    fs::create_dir(&exports)?;
    for artifact in &caching.artifacts {
        let mut path = build.join("output");
        for component in artifact.path.split('/') {
            path.push(component);
            directory(&path)?;
        }
        // Some filesystems require write permission to move a directory between parents.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
        fs::rename(path, exports.join(&artifact.name))?;
    }
    let capture_root = exports.clone();
    let capture_config = caching.clone();
    let capture_key = key.clone();
    let bytes = tokio::task::spawn_blocking(move || {
        manifest(&capture_root, &capture_config, &capture_key, true)
    })
    .await??;
    let hash = digest(&bytes);
    File::open(&exports)?.sync_all()?;
    let destination = root.join("caches").join(&hash);
    if !destination.exists() {
        fs::rename(&exports, &destination)?;
    }
    fs::set_permissions(&destination, fs::Permissions::from_mode(0o555))?;
    File::open(&destination)?.sync_all()?;
    File::open(root.join("caches"))?.sync_all()?;
    let seed = Seed {
        config: caching.clone(),
        input_key: key.clone(),
        digest: hash,
        namespace: config.namespace.clone(),
        source_pvc: config.source_pvc.clone(),
    };
    verify_async(config, &seed).await?;
    let temp = root.join("cache-refs").join(format!("{key}.{id}.tmp"));
    fs::write(&temp, serde_json::to_vec(&seed)?)?;
    File::open(&temp)?.sync_all()?;
    fs::rename(temp, reference)?;
    File::open(root.join("cache-refs"))?.sync_all()?;
    println!("Cache ready: {}", seed.digest);
    Ok(seed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use grading_core::{
        caching::{Artifact, Mode},
        config::Resources,
    };
    #[test]
    fn exports_reject_links_escape_special_files_and_overflow_and_ignore_mtime() {
        let root = std::env::temp_dir().join(format!("cache-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(root.join("compiler")).unwrap();
        let path = root.join("compiler/object");
        fs::write(&path, b"compiled").unwrap();
        let config = Caching {
            version: 1,
            recipe_dir: "cache".into(),
            command: vec!["/bin/true".into()],
            timeout_seconds: 1,
            resources: Resources {
                cpu: 1,
                memory_gib: 1,
                storage_gib: 1,
            },
            architecture: "amd64".into(),
            artifacts: vec![Artifact {
                name: "compiler".into(),
                path: "export".into(),
                mount_path: "/cache/compiler".into(),
                mode: Mode::ReadOnly,
                max_size_gib: 1,
            }],
        };
        let before = manifest(&root, &config, "key", false).unwrap();
        File::open(&path)
            .unwrap()
            .set_modified(std::time::SystemTime::now() + Duration::from_secs(60))
            .unwrap();
        assert_eq!(before, manifest(&root, &config, "key", false).unwrap());
        fs::write(&path, b"tampered").unwrap();
        assert_ne!(before, manifest(&root, &config, "key", false).unwrap());
        assert!(scan(&root, &path, &mut Vec::new(), &mut 0, 1, false).is_err());
        fs::hard_link(&path, root.join("compiler/link")).unwrap();
        assert!(manifest(&root, &config, "key", false).is_err());
        fs::remove_file(root.join("compiler/link")).unwrap();
        std::os::unix::fs::symlink("/etc/passwd", root.join("compiler/link")).unwrap();
        assert!(manifest(&root, &config, "key", false).is_err());
        fs::remove_file(root.join("compiler/link")).unwrap();
        let socket = std::os::unix::net::UnixListener::bind(root.join("compiler/socket")).unwrap();
        assert!(manifest(&root, &config, "key", false).is_err());
        drop(socket);
        fs::remove_file(root.join("compiler/socket")).unwrap();
        assert!(output_budget(&root, 1).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
        fs::write(root.join("compiler/tool"), b"executable").unwrap();
        fs::set_permissions(
            root.join("compiler/tool"),
            fs::Permissions::from_mode(0o555),
        )
        .unwrap();
        fs::create_dir(root.join("copy")).unwrap();
        // Nix's build sandbox rejects setting setgid. Still check permission
        // preservation there, and include setgid where the environment permits it.
        let copy_mode =
            match fs::set_permissions(root.join("copy"), fs::Permissions::from_mode(0o2770)) {
                Ok(()) => 0o2770,
                Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                    fs::set_permissions(root.join("copy"), fs::Permissions::from_mode(0o770))
                        .unwrap();
                    0o770
                }
                Err(error) => panic!("setting cache copy directory permissions: {error}"),
            };
        let status = std::process::Command::new("python3")
            .args(["-c", include_str!("../../../scripts/copy-cache.py")])
            .arg(root.join("compiler"))
            .arg(root.join("copy"))
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(
            fs::metadata(root.join("copy")).unwrap().mode() & 0o7777,
            copy_mode
        );
        assert_eq!(
            fs::metadata(root.join("copy/tool")).unwrap().mode() & 0o777,
            0o755
        );
        fs::write(root.join("copy/object"), b"student mutation").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"tampered");
        fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;
    use grading_core::{
        caching::{Artifact, Mode},
        config::Resources,
        integrity::{Blob, Manifest as SourceManifest},
        protocol::{Grader, Workflow},
    };
    use std::sync::{Arc, Mutex};
    fn mock(root: std::path::PathBuf, fail: bool) -> Client {
        let state = Arc::new(Mutex::new(None::<serde_json::Value>));
        let service = tower::service_fn(move |request: axum::http::Request<kube::client::Body>| {
            let root = root.clone();
            let state = state.clone();
            async move {
                let path = request.uri().path().to_owned();
                let method = request.method().clone();
                let response = if method == axum::http::Method::POST {
                    let bytes = axum::body::to_bytes(
                        axum::body::Body::new(request.into_body()),
                        1024 * 1024,
                    )
                    .await
                    .unwrap();
                    let job: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
                    let id = job["metadata"]["labels"]["grading-lease"].as_str().unwrap();
                    let build = root.join("cache-builds").join(id);
                    assert!(build.join("recipe/prepare.sh").exists());
                    assert!(!build.join("recipe/private.py").exists());
                    fs::create_dir(build.join("output/export")).unwrap();
                    fs::write(build.join("output/export/object"), b"before stop").unwrap();
                    *state.lock().unwrap() = Some(job.clone());
                    job
                } else if method == axum::http::Method::DELETE {
                    let job = state.lock().unwrap().clone().unwrap();
                    let id = job["metadata"]["labels"]["grading-lease"].as_str().unwrap();
                    // A background writer changes the file until deletion. Capture must see this version.
                    fs::write(
                        root.join("cache-builds")
                            .join(id)
                            .join("output/export/object"),
                        b"after stop",
                    )
                    .unwrap();
                    fs::set_permissions(
                        root.join("cache-builds").join(id).join("output/export"),
                        fs::Permissions::from_mode(0o555),
                    )
                    .unwrap();
                    job
                } else if path.ends_with("/pods") {
                    json!({"apiVersion":"v1","kind":"PodList","metadata":{},"items":[]})
                } else if path.ends_with("/jobs") {
                    json!({"apiVersion":"batch/v1","kind":"JobList","metadata":{},"items":[]})
                } else {
                    let mut job = state.lock().unwrap().clone().unwrap();
                    job["status"] = if fail {
                        json!({"failed":1})
                    } else {
                        json!({"succeeded":1})
                    };
                    job
                };
                Ok::<_, std::convert::Infallible>(axum::http::Response::new(
                    axum::body::Body::from(serde_json::to_vec(&response).unwrap()),
                ))
            }
        });
        Client::new(service, "sandbox")
    }
    fn remove(path: &Path) {
        if path.is_dir() {
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
            for entry in fs::read_dir(path).unwrap() {
                remove(&entry.unwrap().path());
            }
            fs::remove_dir(path).unwrap();
        } else {
            fs::remove_file(path).unwrap();
        }
    }
    #[tokio::test]
    async fn prepares_after_stop_reuses_immutable_seed_and_never_publishes_failure() {
        let root = std::env::temp_dir().join(format!("cache-lifecycle-{}", uuid::Uuid::new_v4()));
        let image = format!("registry.example/grading/runner@sha256:{}", "a".repeat(64));
        let mut assignment: grading_core::config::Assignment =
            toml::from_str(include_str!("../../../tests/fixtures/assignment.toml")).unwrap();
        assignment.image = image.clone();
        assignment.execution_profile = "registered-v1".into();
        assignment.public_tests = "tests".into();
        let source = Snapshot {
            sha: assignment.template_revision.clone(),
            files: std::collections::BTreeMap::from([(
                "tests/test".into(),
                Blob {
                    mode: "100644".into(),
                    data: "dGVzdA==".into(),
                },
            )]),
        };
        let mut grader_source = source.clone();
        grader_source.files = std::collections::BTreeMap::from([
            (
                "cache/prepare.sh".into(),
                Blob {
                    mode: "100644".into(),
                    data: "ZWNobw==".into(),
                },
            ),
            (
                "private.py".into(),
                Blob {
                    mode: "100644".into(),
                    data: "c2VjcmV0".into(),
                },
            ),
        ]);
        let revision = Revision {
            course_id: "course".into(),
            assignment_id: "exercise".into(),
            manifest: SourceManifest::generate(&source, vec!["src/".into()]).unwrap(),
            assignment,
            tests: Default::default(),
            grader: Some(Grader {
                caching: None,
                repository: "org/grader".into(),
                revision: "b".repeat(40),
                image: image.clone(),
                source_digest: Some("c".repeat(64)),
                workflow: Some(Workflow {
                    public_command: vec!["/bin/true".into()],
                    private_command: None,
                }),
            }),
        };
        let resources = Resources {
            cpu: 1,
            memory_gib: 1,
            storage_gib: 2,
        };
        let config = Config {
            api_url: "https://unused.invalid".into(),
            token_file: "/unused".into(),
            tls_identity_file: None,
            tls_ca_file: None,
            namespace: "sandbox".into(),
            runtime_class: "gvisor".into(),
            source_pvc: "source".into(),
            staging_root: root.clone(),
            registry: Some(crate::Registry {
                image_prefix: "registry.example/grading".into(),
                runner_images: vec![image],
                resources: resources.clone(),
                timeout_seconds: 60,
            }),
        };
        let mut caching = Caching {
            version: 1,
            recipe_dir: "cache".into(),
            command: vec!["/bin/true".into()],
            timeout_seconds: 30,
            resources,
            architecture: "amd64".into(),
            artifacts: vec![Artifact {
                name: "compiler".into(),
                path: "export".into(),
                mount_path: "/cache/compiler".into(),
                mode: Mode::ReadOnly,
                max_size_gib: 1,
            }],
        };
        let recipe = caching.recipe(&grader_source).unwrap();
        let seed = prepare_with_client(
            &config,
            &revision,
            &caching,
            &source,
            &recipe,
            Some(mock(root.clone(), false)),
        )
        .await
        .unwrap();
        let artifact = root
            .join("caches")
            .join(&seed.digest)
            .join("compiler/object");
        assert_eq!(fs::read(&artifact).unwrap(), b"after stop");
        assert_eq!(fs::metadata(&artifact).unwrap().mode() & 0o222, 0);
        let reused = prepare(&config, &revision, &caching, &source, &recipe)
            .await
            .unwrap();
        assert_eq!(seed.digest, reused.digest);
        caching.version += 1;
        let key = caching
            .key(&revision.assignment.image, &source, &recipe)
            .unwrap();
        assert!(
            prepare_with_client(
                &config,
                &revision,
                &caching,
                &source,
                &recipe,
                Some(mock(root.clone(), true))
            )
            .await
            .is_err()
        );
        assert!(!root.join("cache-refs").join(key).exists());
        verify(&config, &seed).unwrap();
        fs::set_permissions(&artifact, fs::Permissions::from_mode(0o644)).unwrap();
        fs::write(&artifact, b"corrupt").unwrap();
        assert!(verify(&config, &seed).is_err());
        remove(&root);
    }
}
