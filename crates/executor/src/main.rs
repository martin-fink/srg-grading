//! Restricted Kubernetes grading worker.
mod cleanup;
mod logs;
mod workflow;
use anyhow::{Context, Result, ensure};
use clap::Parser;
use grading_core::diagnostics::{HttpStatus, Stage};
use grading_core::{
    integrity::{MAX_SNAPSHOT_BYTES, Snapshot},
    protocol::{Heartbeat, Lease, LeaseRequest, RunResult, RunStatus},
    security::digest,
};
use grading_executor::Config;
use k8s_openapi::api::{batch::v1::Job, core::v1::Pod};
use kube::{
    Api, Client,
    api::{DeleteParams, ListParams, LogParams},
};
use reqwest::Client as HttpClient;
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tokio::io::AsyncWriteExt;
use tracing::Instrument;

#[derive(Parser)]
struct Args {
    #[arg(long)]
    config: PathBuf,
    #[arg(long)]
    once: bool,
}

fn log_failure(stage: &'static str, error: &anyhow::Error) {
    let mut details = grading_core::diagnostics::describe(error);
    if let Some(kube::Error::Api(response)) = error.downcast_ref::<kube::Error>() {
        details.reason = "kubernetes_api";
        details.upstream_status = Some(response.code);
    } else if error.downcast_ref::<kube::Error>().is_some() {
        details.reason = "kubernetes_client";
    } else if let Some(http) = error.downcast_ref::<reqwest::Error>() {
        details.reason = if http.is_timeout() {
            "http_timeout"
        } else {
            "http_transport"
        };
    }
    let stage = if details.stage == "unspecified" {
        stage
    } else {
        details.stage
    };
    tracing::warn!(
        stage,
        reason = details.reason,
        upstream_status = details.upstream_status,
        "executor operation failed"
    );
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("install Rustls crypto provider");
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into())
                .add_directive("reqwest=off".parse().unwrap())
                .add_directive("hyper=off".parse().unwrap())
                .add_directive("hyper_util=off".parse().unwrap())
                .add_directive("kube_client=off".parse().unwrap()),
        )
        .init();
    match run(Args::parse()).await {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            log_failure("executor_startup_or_poll", &error);
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run(args: Args) -> Result<()> {
    let config: Config = toml::from_str(
        &tokio::fs::read_to_string(args.config)
            .await
            .context(Stage("executor_config_read"))?,
    )
    .context(Stage("executor_config_parse"))?;
    let url = reqwest::Url::parse(&config.api_url).context(Stage("executor_api_url"))?;
    ensure!(
        url.scheme() == "https" && url.query().is_none() && url.fragment().is_none(),
        "worker API requires HTTPS"
    );
    let token = tokio::fs::read_to_string(&config.token_file)
        .await
        .context(Stage("worker_credentials_read"))?;
    let mut http = HttpClient::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none());
    if let Some(path) = &config.tls_identity_file {
        http = http.identity(reqwest::Identity::from_pem(&tokio::fs::read(path).await?)?);
    }
    if let Some(path) = &config.tls_ca_file {
        http = http.add_root_certificate(reqwest::Certificate::from_pem(
            &tokio::fs::read(path).await?,
        )?);
    }
    let http = http.build()?;
    let client = Client::try_default()
        .await
        .context(Stage("kubernetes_client_init"))?;
    let jobs: Api<Job> = Api::namespaced(client.clone(), &config.namespace);
    let pods: Api<Pod> = Api::namespaced(client, &config.namespace);
    tokio::fs::create_dir_all(&config.staging_root)
        .await
        .context(Stage("staging_root_create"))?;
    tracing::info!("executor polling started");
    let mut last_cleanup = None;
    loop {
        if last_cleanup.is_none_or(|time: Instant| time.elapsed() >= Duration::from_secs(60)) {
            if let Err(error) = cleanup::reconcile(&config, &pods).await {
                log_failure("staging_reconcile", &error);
            }
            last_cleanup = Some(Instant::now());
        }
        let response = http
            .post(format!(
                "{}/internal/lease",
                config.api_url.trim_end_matches('/')
            ))
            .bearer_auth(token.trim())
            .json(&LeaseRequest {
                profiles: config.profile_names(),
            })
            .send()
            .await
            .context(Stage("lease_request"))?;
        let lease: Option<Lease> = serde_json::from_slice(
            &bounded(response, 2 * 1024 * 1024)
                .await
                .context(Stage("lease_response"))?,
        )
        .context(Stage("lease_decode"))?;
        if let Some(lease) = lease {
            let base = format!(
                "{}/internal/tasks/{}",
                config.api_url.trim_end_matches('/'),
                lease.task_id
            );
            tracing::info!(task_id=%lease.task_id, run_id=%lease.run_id, private=lease.baseline.is_some(), "grading lease acquired");
            let execution = execute(&config, &http, token.trim(), &jobs, &pods, &lease, &base).instrument(tracing::info_span!("grading_run", task_id=%lease.task_id, run_id=%lease.run_id));
            tokio::pin!(execution);
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            let outcome = loop {
                tokio::select! {
                    result=&mut execution=>break result,
                    _=interval.tick()=>{
                        let heartbeat=http.post(format!("{base}/heartbeat")).bearer_auth(token.trim()).json(&Heartbeat{lease_token:lease.lease_token}).send().await;
                        match heartbeat {
                            Ok(response) if response.status().is_success() => {},
                            Ok(response) => { tracing::warn!(task_id=%lease.task_id, stage="lease_heartbeat", upstream_status=response.status().as_u16(), "heartbeat rejected"); break Err(anyhow::anyhow!("lease heartbeat lost")); },
                            Err(error) => { log_failure("lease_heartbeat", &error.into()); break Err(anyhow::anyhow!("lease heartbeat lost")); }
                        }
                    }
                }
            };
            let selector = ListParams::default()
                .labels(&format!("grading-lease={}", lease.lease_token.simple()));
            if let Err(error) = jobs
                .delete_collection(&DeleteParams::default(), &selector)
                .await
            {
                log_failure("kubernetes_cleanup", &error.into());
            }
            match outcome {
                Ok(result) => {
                    tracing::info!(task_id=%lease.task_id, run_id=%lease.run_id, status=?result.status, "grading execution finished");
                    let mut accepted = false;
                    for attempt in 1..=3 {
                        let response = http
                            .post(format!("{base}/result"))
                            .bearer_auth(token.trim())
                            .json(&result)
                            .send()
                            .await;
                        match response {
                            Ok(response) if response.status().is_success() => {
                                accepted = true;
                                break;
                            }
                            Ok(response) => {
                                tracing::warn!(task_id=%lease.task_id, stage="result_publish", attempt, upstream_status=response.status().as_u16(), "result rejected")
                            }
                            Err(error) => log_failure("result_publish", &error.into()),
                        }
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                    if !accepted {
                        tracing::warn!(task_id=%lease.task_id,"result not accepted; lease will expire");
                    }
                }
                Err(error) => {
                    let span = tracing::info_span!("grading_run", task_id=%lease.task_id, run_id=%lease.run_id);
                    let _entered = span.enter();
                    log_failure("grading_execute", &error);
                }
            }
            let directory = config
                .staging_root
                .join("runs")
                .join(lease.lease_token.simple().to_string());
            if directory.exists() {
                if let Err(error) = tokio::fs::write(directory.join(".finished"), b"").await {
                    log_failure("staging_mark_finished", &error.into());
                }
                // Deletion is asynchronous; reconciliation keeps retrying after this turn.
                if let Err(error) = cleanup::reconcile(&config, &pods).await {
                    log_failure("staging_reconcile", &error);
                }
            }
        } else if !args.once {
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
        if args.once {
            break;
        }
    }
    Ok(())
}

fn initial_result(lease: &Lease) -> RunResult {
    RunResult {
        logs: vec![],
        score: None,
        schema_version: 1,
        lease_token: lease.lease_token,
        run_id: lease.run_id,
        sha: lease.sha.clone(),
        revision_digest: lease.revision_digest.clone(),
        image: lease.revision.assignment.image.clone(),
        resources: lease.revision.assignment.resources.clone(),
        status: RunStatus::InfrastructureFailed,
        findings: vec![],
    }
}

async fn execute(
    config: &Config,
    http: &HttpClient,
    token: &str,
    jobs: &Api<Job>,
    pods: &Api<Pod>,
    lease: &Lease,
    base: &str,
) -> Result<RunResult> {
    let logs = logs::Capture::default();
    let mut result = match execute_inner(config, http, token, jobs, pods, lease, base, &logs).await
    {
        Ok(result) => result,
        Err(error) => {
            log_failure("grading_execute", &error);
            let details = grading_core::diagnostics::describe(&error);
            logs.push(lease.baseline.is_none(), &format!("Execution stopped because of an infrastructure or grader error (stage: {}, reason: {}). Contact your instructor.\n", details.stage, details.reason));
            initial_result(lease)
        }
    };
    result.logs = logs.finish();
    Ok(result)
}

#[allow(clippy::too_many_arguments)]
async fn execute_inner(
    config: &Config,
    http: &HttpClient,
    token: &str,
    jobs: &Api<Job>,
    pods: &Api<Pod>,
    lease: &Lease,
    base: &str,
    logs: &logs::Capture,
) -> Result<RunResult> {
    let mut result = initial_result(lease);
    if let Err(error) = config.approve(lease) {
        log_failure("lease_approval", &error);
        return Ok(result);
    }
    if let Some(seed) = lease
        .revision
        .grader
        .as_ref()
        .and_then(|g| g.caching.as_ref())
    {
        let config = config.clone();
        let seed = seed.clone();
        tokio::task::spawn_blocking(move || grading_executor::caching::verify(&config, &seed))
            .await??;
    }
    let response = http
        .get(format!("{base}/source"))
        .query(&[("lease_token", lease.lease_token.to_string())])
        .bearer_auth(token)
        .send()
        .await?;
    let bytes = bounded(response, MAX_SNAPSHOT_BYTES * 2).await?;
    ensure!(
        digest(&bytes) == lease.source_digest,
        Stage("submission_digest_verify")
    );
    let snapshot: Snapshot =
        serde_json::from_slice(&bytes).context(Stage("submission_snapshot_decode"))?;
    ensure!(snapshot.sha == lease.sha, Stage("submission_commit_verify"));
    let findings = match lease.revision.manifest.check(&snapshot) {
        Ok(findings) => findings,
        Err(error) => {
            log_failure("submission_integrity", &error);
            result.status = RunStatus::IntegrityFailed;
            result.findings.push(grading_core::integrity::Finding {
                path: "(snapshot)".into(),
                reason: "Unsafe or unsupported source tree".into(),
            });
            return Ok(result);
        }
    };
    if !findings.is_empty() {
        tracing::info!(
            stage = "submission_integrity",
            findings = findings.len(),
            "integrity check failed"
        );
        result.status = RunStatus::IntegrityFailed;
        result.findings = findings;
        return Ok(result);
    }
    let directory = config
        .staging_root
        .join("runs")
        .join(lease.lease_token.simple().to_string());
    tokio::fs::create_dir_all(directory.parent().context("staging path")?).await?;
    tokio::fs::create_dir(&directory)
        .await
        .context(Stage("staging_directory_create"))?;
    if let Some(grader) = &lease.revision.grader
        && let Some(expected) = &grader.source_digest
    {
        let response = http
            .get(format!("{base}/grader"))
            .query(&[("lease_token", lease.lease_token.to_string())])
            .bearer_auth(token)
            .send()
            .await?;
        let bytes = bounded(response, MAX_SNAPSHOT_BYTES * 2).await?;
        ensure!(&digest(&bytes) == expected, Stage("grader_digest_verify"));
        let snapshot: Snapshot = serde_json::from_slice(&bytes)?;
        ensure!(
            snapshot.sha == grader.revision,
            Stage("grader_commit_verify")
        );
        snapshot
            .validate()
            .context(Stage("grader_snapshot_validate"))?;
        let root = directory.join("grader");
        tokio::fs::create_dir(&root).await?;
        #[cfg(unix)]
        tokio::fs::set_permissions(&root, std::os::unix::fs::PermissionsExt::from_mode(0o700))
            .await?;
        for (path, blob) in &snapshot.files {
            let destination = root.join(path);
            tokio::fs::create_dir_all(destination.parent().context("grader source path")?).await?;
            write(&destination, &blob.bytes()?, blob.mode == "100755").await?;
        }
    }
    let source = directory.join("source");
    tokio::fs::create_dir(&source).await?;
    for (path, blob) in &snapshot.files {
        let destination = source.join(path);
        tokio::fs::create_dir_all(destination.parent().context("source path")?).await?;
        write(&destination, &blob.bytes()?, blob.mode == "100755").await?;
    }
    if let Some(baseline) = &lease.baseline {
        ensure!(
            chrono::Utc::now() > baseline.deadline,
            "private grading is not open yet"
        );
    }
    match workflow::execute(config, jobs, pods, lease, &directory, logs).await? {
        workflow::Outcome::Scored(score) => {
            result.status = if score.invalidated {
                RunStatus::Invalidated
            } else {
                RunStatus::Completed
            };
            result.score = Some(score);
        }
        workflow::Outcome::Failed(status) => result.status = status,
    }
    Ok(result)
}

async fn write(path: &Path, bytes: &[u8], executable: bool) -> Result<()> {
    let mut options = tokio::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    options.mode(if executable { 0o755 } else { 0o644 });
    let mut file = options.open(path).await?;
    file.write_all(bytes).await?;
    Ok(())
}

async fn bounded(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>> {
    if !response.status().is_success() {
        return Err(HttpStatus(response.status().as_u16()).into());
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        ensure!(
            bytes.len() + chunk.len() <= limit,
            "worker response exceeds limit"
        );
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

enum JobOutcome {
    Output(i32, String),
    StudentFailure(&'static str),
    Failed(RunStatus),
}
async fn wait_job(
    jobs: &Api<Job>,
    pods: &Api<Pod>,
    name: &str,
    started: Instant,
    deadline: Duration,
) -> Result<JobOutcome> {
    loop {
        if started.elapsed() >= deadline {
            tracing::warn!(stage = "execution_deadline", "grading time limit exceeded");
            return Ok(JobOutcome::Failed(RunStatus::TimedOut));
        }
        let current = jobs
            .get(name)
            .await
            .context(Stage("kubernetes_job_status"))?;
        if let Some(status) = current.status {
            if status.conditions.as_ref().is_some_and(|conditions| {
                conditions
                    .iter()
                    .any(|c| c.reason.as_deref() == Some("DeadlineExceeded") && c.status == "True")
            }) {
                tracing::warn!(stage = "execution_deadline", "grading time limit exceeded");
                return Ok(JobOutcome::Failed(RunStatus::TimedOut));
            }
            if status.succeeded.unwrap_or(0) > 0 || status.failed.unwrap_or(0) > 0 {
                let list = pods
                    .list(&ListParams::default().labels(&format!("job-name={name}")))
                    .await?;
                let pod = list.items.first().context(Stage("sandbox_pod_lookup"))?;
                let terminated = pod
                    .status
                    .as_ref()
                    .and_then(|s| s.container_statuses.as_ref())
                    .and_then(|s| s.first())
                    .and_then(|s| s.state.as_ref())
                    .and_then(|s| s.terminated.as_ref())
                    .context(Stage("sandbox_termination_status"))?;
                if terminated.reason.as_deref() == Some("OOMKilled") {
                    tracing::warn!(
                        stage = "sandbox_execution",
                        reason = "oom_killed",
                        "sandbox exceeded memory limit"
                    );
                    return Ok(JobOutcome::StudentFailure("memory_limit"));
                }
                let output = pods
                    .logs(
                        pod.metadata.name.as_deref().context("pod name")?,
                        &LogParams {
                            limit_bytes: Some(1024 * 1024),
                            ..Default::default()
                        },
                    )
                    .await?;
                tracing::debug!(
                    stage = "sandbox_execution",
                    exit_code = terminated.exit_code,
                    "sandbox process completed"
                );
                return Ok(JobOutcome::Output(terminated.exit_code, output));
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}
