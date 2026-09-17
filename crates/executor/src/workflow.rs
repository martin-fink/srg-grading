//! Private per-run file channel between instructor scripts and isolated student Jobs.
use crate::{JobOutcome, logs, write};
use anyhow::{Context, Result, ensure};
use grading_core::diagnostics::Stage;
use grading_core::protocol::{Lease, RunStatus, ScriptScore};
use grading_executor::{Config, ExecutionRequest, controller_job, execution_job};
use k8s_openapi::api::{batch::v1::Job, core::v1::Pod};
use kube::{
    Api,
    api::{DeleteParams, PostParams},
};
use serde_json::json;
use std::{
    collections::HashMap,
    io::Read,
    path::Path,
    time::{Duration, Instant},
};

pub enum Outcome {
    Scored(ScriptScore),
    Failed(RunStatus),
}

pub async fn execute(
    config: &Config,
    jobs: &Api<Job>,
    pods: &Api<Pod>,
    lease: &Lease,
    directory: &Path,
    logs: &logs::Capture,
) -> Result<Outcome> {
    for name in ["control", "context", "platform", "requests"] {
        tokio::fs::create_dir(directory.join(name)).await?;
    }
    #[cfg(unix)]
    tokio::fs::set_permissions(
        directory.join("control"),
        std::os::unix::fs::PermissionsExt::from_mode(0o700),
    )
    .await?;
    let baseline = lease.baseline.as_ref();
    let context = json!({
        "schema_version": 1,
        "phase": if baseline.is_some() { "private" } else { "public" },
        "public_points": baseline.map(|b| b.points),
        "public_run_id": baseline.map(|b| b.run_id),
        "max_points": lease.revision.assignment.max_points,
        "sha": lease.sha,
    });
    write(
        &directory.join("context/input.json"),
        &serde_json::to_vec(&context)?,
        false,
    )
    .await?;
    write(
        &directory.join("platform/grading-run"),
        include_bytes!("../../../scripts/grading-run.py"),
        true,
    )
    .await?;
    let started = Instant::now();
    let deadline = Duration::from_secs(u64::from(lease.revision.assignment.timeout_seconds));
    let job = controller_job(config, lease, deadline.as_secs() as u32)?;
    let name = job
        .metadata
        .name
        .as_deref()
        .context("controller job name")?;
    jobs.create(&PostParams::default(), &job)
        .await
        .context(Stage("controller_job_create"))?;
    let outcome = tokio::select! {
        outcome=logs::wait(jobs,pods,name,started,deadline,logs,false,false)=>outcome,
        result=serve(config,jobs,pods,lease,directory,started,deadline,logs)=>result.map(JobOutcome::Failed),
    };
    if let Ok(file) = std::fs::File::open(directory.join("control/grader.stderr")) {
        let mut output = String::new();
        let _ = file.take(65536).read_to_string(&mut output);
        logs.push(false, &output);
    }
    let outcome = outcome?;
    jobs.delete(name, &DeleteParams::default()).await?;
    let output = match outcome {
        JobOutcome::Output(0, output) => output,
        JobOutcome::Failed(status) => return Ok(Outcome::Failed(status)),
        _ => return Ok(Outcome::Failed(RunStatus::InfrastructureFailed)),
    };
    ensure!(output.len() <= 65536, "script result exceeds limit");
    let result: ScriptScore =
        serde_json::from_str(&output).context(Stage("grader_result_decode"))?;
    result
        .validate(lease.revision.assignment.max_points, baseline)
        .context(Stage("grader_result_validate"))?;
    Ok(Outcome::Scored(result))
}

#[allow(clippy::too_many_arguments)]
async fn serve(
    config: &Config,
    jobs: &Api<Job>,
    pods: &Api<Pod>,
    lease: &Lease,
    directory: &Path,
    started: Instant,
    deadline: Duration,
    logs: &logs::Capture,
) -> Result<RunStatus> {
    let mut handled = HashMap::new();
    loop {
        if let Some(bytes) = read_request(&directory.join("control/request.json"))? {
            let request: ExecutionRequest =
                serde_json::from_slice(&bytes).context(Stage("workflow_request_decode"))?;
            request
                .validate()
                .context(Stage("workflow_request_validate"))?;
            let digest = grading_core::security::digest(&bytes);
            if let Some(previous) = handled.get(&request.id) {
                ensure!(previous == &digest, "conflicting execution request replay");
            } else {
                ensure!(
                    handled.len() < 1000,
                    "workflow exceeds 1000 isolated executions"
                );
                let remaining = deadline.saturating_sub(started.elapsed());
                if remaining.is_zero() {
                    return Ok(RunStatus::TimedOut);
                }
                let input = directory
                    .join("requests")
                    .join(request.id.simple().to_string());
                tokio::fs::create_dir(&input).await?;
                write(&input.join("stdin"), request.stdin.as_bytes(), false).await?;
                let definition =
                    execution_job(config, lease, &request, remaining.as_secs().max(1) as u32)?;
                let name = definition
                    .metadata
                    .name
                    .as_deref()
                    .context("execution job name")?;
                jobs.create(&PostParams::default(), &definition)
                    .await
                    .context(Stage("student_job_create"))?;
                let outcome = logs::wait(
                    jobs,
                    pods,
                    name,
                    started,
                    deadline,
                    logs,
                    lease.baseline.is_none(),
                    true,
                )
                .await?;
                jobs.delete(name, &DeleteParams::default()).await?;
                let (exit_code, mut stdout) = match outcome {
                    JobOutcome::Output(code, stdout) => (code, stdout),
                    JobOutcome::Failed(status) => return Ok(status),
                };
                let truncated = stdout.len() > 65536;
                if truncated {
                    let mut end = 65536;
                    while !stdout.is_char_boundary(end) {
                        end -= 1;
                    }
                    stdout.truncate(end);
                }
                let response = serde_json::to_vec(
                    &json!({"id":request.id,"exit_code":exit_code,"stdout":stdout,"truncated":truncated}),
                )?;
                let temporary = directory
                    .join("control")
                    .join(format!("{}.response", uuid::Uuid::new_v4()));
                write(&temporary, &response, false).await?;
                tokio::fs::rename(temporary, directory.join("control/response.json")).await?;
                handled.insert(request.id, digest);
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn read_request(path: &Path) -> Result<Option<Vec<u8>>> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    ensure!(
        file.metadata()?.is_file(),
        "execution request must be a regular file"
    );
    let mut bytes = Vec::new();
    file.take(1_048_577).read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= 1_048_576, "execution request exceeds limit");
    Ok(Some(bytes))
}

#[cfg(all(test, unix))]
mod tests {
    use super::read_request;

    #[test]
    fn requests_cannot_follow_symlinks_or_read_unbounded_files() {
        let root = std::env::temp_dir().join(format!("grading-channel-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&root).unwrap();
        let request = root.join("request.json");
        assert!(read_request(&request).unwrap().is_none());
        let target = root.join("target");
        std::fs::write(&target, b"private data").unwrap();
        std::os::unix::fs::symlink(&target, &request).unwrap();
        assert!(read_request(&request).is_err());
        std::fs::remove_file(&request).unwrap();
        std::fs::write(&request, vec![b'x'; 1_048_577]).unwrap();
        assert!(read_request(&request).is_err());
        std::fs::write(&request, b"{}").unwrap();
        assert_eq!(read_request(&request).unwrap().unwrap(), b"{}");
        assert!(read_request(&root).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
}
