//! Restricted Kubernetes grading worker.
mod workflow;
use anyhow::{Context, Result, ensure};
use clap::Parser;
use grading_core::{
    integrity::{MAX_SNAPSHOT_BYTES, Snapshot},
    protocol::{Heartbeat, Lease, LeaseRequest, RunResult, RunStatus, TestResult},
    security::digest,
};
use grading_executor::{Config, job, private_job, private_test_job};
use k8s_openapi::api::{batch::v1::Job, core::v1::Pod};
use kube::{
    Api, Client,
    api::{DeleteParams, ListParams, LogParams, PostParams},
};
use reqwest::Client as HttpClient;
use std::{
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tokio::io::AsyncWriteExt;

#[derive(Parser)]
struct Args {
    #[arg(long)]
    config: PathBuf,
    #[arg(long)]
    once: bool,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let args = Args::parse();
    let config: Config = toml::from_str(&tokio::fs::read_to_string(args.config).await?)?;
    let url = reqwest::Url::parse(&config.api_url)?;
    ensure!(
        url.scheme() == "https" && url.query().is_none() && url.fragment().is_none(),
        "worker API requires HTTPS"
    );
    let token = tokio::fs::read_to_string(&config.token_file).await?;
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
    let client = Client::try_default().await?;
    let jobs: Api<Job> = Api::namespaced(client.clone(), &config.namespace);
    let pods: Api<Pod> = Api::namespaced(client, &config.namespace);
    tokio::fs::create_dir_all(&config.staging_root).await?;
    loop {
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
            .await?;
        let lease: Option<Lease> =
            serde_json::from_slice(&bounded(response, 2 * 1024 * 1024).await?)?;
        if let Some(lease) = lease {
            let base = format!(
                "{}/internal/tasks/{}",
                config.api_url.trim_end_matches('/'),
                lease.task_id
            );
            let execution = execute(&config, &http, token.trim(), &jobs, &pods, &lease, &base);
            tokio::pin!(execution);
            let mut interval = tokio::time::interval(Duration::from_secs(30));
            let outcome = loop {
                tokio::select! {
                    result=&mut execution=>break result,
                    _=interval.tick()=>{
                        let heartbeat=http.post(format!("{base}/heartbeat")).bearer_auth(token.trim()).json(&Heartbeat{lease_token:lease.lease_token}).send().await;
                        if !heartbeat.is_ok_and(|r|r.status().is_success()){break Err(anyhow::anyhow!("lease heartbeat lost"));}
                    }
                }
            };
            let selector = ListParams::default()
                .labels(&format!("grading-lease={}", lease.lease_token.simple()));
            let _ = jobs
                .delete_collection(&DeleteParams::default(), &selector)
                .await;
            match outcome {
                Ok(result) => {
                    let mut accepted = false;
                    for _ in 0..3 {
                        let response = http
                            .post(format!("{base}/result"))
                            .bearer_auth(token.trim())
                            .json(&result)
                            .send()
                            .await;
                        if response.is_ok_and(|r| r.status().is_success()) {
                            accepted = true;
                            break;
                        }
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                    if !accepted {
                        tracing::warn!(task_id=%lease.task_id,"result not accepted; lease will expire");
                    }
                }
                Err(_) => {
                    tracing::warn!(task_id=%lease.task_id,"execution interrupted; lease will expire");
                }
            }
            if let Ok(remaining) = pods.list(&selector).await
                && remaining.items.is_empty()
            {
                let directory = config
                    .staging_root
                    .join("runs")
                    .join(lease.lease_token.simple().to_string());
                if directory.exists() {
                    tokio::fs::remove_dir_all(directory).await?;
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

async fn execute(
    config: &Config,
    http: &HttpClient,
    token: &str,
    jobs: &Api<Job>,
    pods: &Api<Pod>,
    lease: &Lease,
    base: &str,
) -> Result<RunResult> {
    let mut result = RunResult {
        score: None,
        private_tests: vec![],
        private: None,
        schema_version: 1,
        lease_token: lease.lease_token,
        run_id: lease.run_id,
        sha: lease.sha.clone(),
        revision_digest: lease.revision_digest.clone(),
        image: lease.revision.assignment.image.clone(),
        resources: lease.revision.assignment.resources.clone(),
        status: RunStatus::InfrastructureFailed,
        tests: vec![],
        findings: vec![],
    };
    if config.approve(lease).is_err() {
        return Ok(result);
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
        "source digest mismatch"
    );
    let snapshot: Snapshot = serde_json::from_slice(&bytes)?;
    ensure!(snapshot.sha == lease.sha, "snapshot SHA mismatch");
    let findings = match lease.revision.manifest.check(&snapshot) {
        Ok(findings) => findings,
        Err(_) => {
            result.status = RunStatus::IntegrityFailed;
            result.findings.push(grading_core::integrity::Finding {
                path: "(snapshot)".into(),
                reason: "Unsafe or unsupported source tree".into(),
            });
            return Ok(result);
        }
    };
    if !findings.is_empty() {
        result.status = RunStatus::IntegrityFailed;
        result.findings = findings;
        return Ok(result);
    }
    let directory = config
        .staging_root
        .join("runs")
        .join(lease.lease_token.simple().to_string());
    tokio::fs::create_dir_all(directory.parent().context("staging path")?).await?;
    tokio::fs::create_dir(&directory).await?;
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
        ensure!(
            &digest(&bytes) == expected,
            "grader snapshot digest mismatch"
        );
        let snapshot: Snapshot = serde_json::from_slice(&bytes)?;
        ensure!(snapshot.sha == grader.revision, "grader commit mismatch");
        snapshot.validate()?;
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
    if lease
        .revision
        .grader
        .as_ref()
        .is_some_and(|g| g.workflow.is_some())
    {
        match workflow::execute(config, jobs, pods, lease, &directory).await? {
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
        return Ok(result);
    }
    let started = Instant::now();
    let deadline = Duration::from_secs(u64::from(lease.revision.assignment.timeout_seconds));
    for test in &lease.revision.tests.tests {
        let input = directory.join("inputs").join(&test.id);
        tokio::fs::create_dir_all(&input).await?;
        write(&input.join("stdin"), test.stdin.as_bytes(), false).await?;
        let remaining = deadline.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            result.status = RunStatus::TimedOut;
            result.tests.clear();
            result.private_tests.clear();
            return Ok(result);
        }
        let definition = job(config, lease, &test.id, remaining.as_secs().max(1) as u32)?;
        let name = definition.metadata.name.as_deref().context("job name")?;
        jobs.create(&PostParams::default(), &definition).await?;
        let (success, output) = match wait_job(jobs, pods, name, started, deadline).await? {
            JobOutcome::Output(success, output) => (success == 0, output),
            JobOutcome::Failed(status) => {
                result.status = status;
                result.tests.clear();
                result.private_tests.clear();
                return Ok(result);
            }
        };
        let passed = success && output.len() <= 65536 && output == test.stdout;
        let log = if output.len() > 65536 {
            "Output exceeded 64 KiB".into()
        } else {
            output
        };
        result.tests.push(TestResult {
            id: test.id.clone(),
            passed,
            log,
        });
        jobs.delete(name, &DeleteParams::default()).await?;
    }
    if let Some(grader) = &lease.revision.grader
        && lease.baseline.is_some()
    {
        for test in &grader.tests {
            let input = directory.join("private-inputs").join(&test.id);
            tokio::fs::create_dir_all(&input).await?;
            write(&input.join("stdin"), test.stdin.as_bytes(), false).await?;
            let remaining = deadline.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                result.status = RunStatus::TimedOut;
                result.tests.clear();
                result.private_tests.clear();
                return Ok(result);
            }
            let definition =
                private_test_job(config, lease, &test.id, remaining.as_secs().max(1) as u32)?;
            let name = definition
                .metadata
                .name
                .as_deref()
                .context("private test job name")?;
            jobs.create(&PostParams::default(), &definition).await?;
            match wait_job(jobs, pods, name, started, deadline).await? {
                JobOutcome::Output(success, output) => result
                    .private_tests
                    .push(test.outcome(success == 0, &output)),
                JobOutcome::Failed(status) => {
                    result.status = status;
                    result.tests.clear();
                    result.private_tests.clear();
                    return Ok(result);
                }
            }
            jobs.delete(name, &DeleteParams::default()).await?;
        }
        let public_points = lease.baseline.as_ref().unwrap().points;
        let public = directory.join("public");
        tokio::fs::create_dir(&public).await?;
        write(&public.join("results.json"), &serde_json::to_vec(&serde_json::json!({"schema_version":1,"public_points":public_points,"max_points":lease.revision.assignment.max_points,"tests":result.tests,"private_tests":result.private_tests}))?,false).await?;
        let remaining = deadline.saturating_sub(started.elapsed());
        if remaining.is_zero() {
            result.status = RunStatus::TimedOut;
            result.tests.clear();
            result.private_tests.clear();
            return Ok(result);
        }
        let definition = private_job(config, lease, remaining.as_secs().max(1) as u32)?;
        let name = definition
            .metadata
            .name
            .as_deref()
            .context("checker job name")?;
        jobs.create(&PostParams::default(), &definition).await?;
        let outcome = wait_job(jobs, pods, name, started, deadline).await?;
        jobs.delete(name, &DeleteParams::default()).await?;
        let output = match outcome {
            JobOutcome::Output(0, output) if output.len() <= 65536 => output,
            JobOutcome::Failed(status) => {
                result.status = status;
                result.tests.clear();
                result.private_tests.clear();
                return Ok(result);
            }
            _ => {
                result.tests.clear();
                result.private_tests.clear();
                return Ok(result);
            }
        };
        let decision: grading_core::protocol::PrivateDecision = serde_json::from_str(&output)?;
        decision.validate(public_points, lease.revision.assignment.max_points)?;
        let invalidated = decision.invalidated;
        result.private = Some(decision);
        result.status = if invalidated {
            RunStatus::Invalidated
        } else {
            RunStatus::Completed
        };
        return Ok(result);
    }
    result.status = RunStatus::Completed;
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
    ensure!(
        response.status().is_success(),
        "worker API rejected request"
    );
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
            return Ok(JobOutcome::Failed(RunStatus::TimedOut));
        }
        let current = jobs.get(name).await?;
        if let Some(status) = current.status {
            if status.conditions.as_ref().is_some_and(|conditions| {
                conditions
                    .iter()
                    .any(|c| c.reason.as_deref() == Some("DeadlineExceeded") && c.status == "True")
            }) {
                return Ok(JobOutcome::Failed(RunStatus::TimedOut));
            }
            if status.succeeded.unwrap_or(0) > 0 || status.failed.unwrap_or(0) > 0 {
                let list = pods
                    .list(&ListParams::default().labels(&format!("job-name={name}")))
                    .await?;
                let pod = list.items.first().context("finished job has no pod")?;
                let terminated = pod
                    .status
                    .as_ref()
                    .and_then(|s| s.container_statuses.as_ref())
                    .and_then(|s| s.first())
                    .and_then(|s| s.state.as_ref())
                    .and_then(|s| s.terminated.as_ref())
                    .context("missing terminated process")?;
                if terminated.reason.as_deref() == Some("OOMKilled") {
                    return Ok(JobOutcome::Failed(RunStatus::InfrastructureFailed));
                }
                let output = pods
                    .logs(
                        pod.metadata.name.as_deref().context("pod name")?,
                        &LogParams {
                            limit_bytes: Some(65537),
                            ..Default::default()
                        },
                    )
                    .await?;
                return Ok(JobOutcome::Output(terminated.exit_code, output));
            }
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}
