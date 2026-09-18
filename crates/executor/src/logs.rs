//! Bounded execution transcripts collected before sandbox deletion.
use crate::{JobOutcome, wait_job};
use anyhow::{Context, Result};
use grading_core::{
    diagnostics::Stage,
    protocol::{MAX_RUN_LOG_BYTES, RunLog},
};
use k8s_openapi::api::{batch::v1::Job, core::v1::Pod};
use kube::{
    Api,
    api::{ListParams, LogParams},
};
use std::{
    sync::Mutex,
    time::{Duration, Instant},
};

#[derive(Default)]
pub struct Capture(Mutex<Vec<RunLog>>);

impl Capture {
    pub fn push(&self, student_visible: bool, text: &str) {
        let mut logs = self.0.lock().unwrap();
        if logs.len() >= 128 {
            return;
        }
        let used: usize = logs.iter().map(|log| log.text.len()).sum();
        let mut end = text.len().min(MAX_RUN_LOG_BYTES.saturating_sub(used));
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        if end > 0 {
            logs.push(RunLog {
                student_visible,
                text: text[..end].to_owned(),
            });
        }
    }

    pub fn finish(&self) -> Vec<RunLog> {
        std::mem::take(&mut *self.0.lock().unwrap())
    }
}

#[derive(serde::Deserialize)]
struct Output {
    stdout: String,
    stderr: String,
    exit_code: i32,
    #[serde(default)]
    failure: Option<String>,
}

fn decode_output(code: i32, output: &str) -> Result<Output> {
    anyhow::ensure!(code == 0, "execution supervisor failed");
    serde_json::from_str(output).context(Stage("student_output_decode"))
}

#[allow(clippy::too_many_arguments)]
pub async fn wait(
    jobs: &Api<Job>,
    pods: &Api<Pod>,
    name: &str,
    started: Instant,
    deadline: Duration,
    capture: &Capture,
    student_visible: bool,
    workflow: bool,
) -> Result<JobOutcome> {
    let outcome = wait_job(jobs, pods, name, started, deadline).await;
    if workflow && let Ok(JobOutcome::Output(code, output)) = &outcome {
        if *code != 0 {
            capture.push(
                student_visible,
                "Execution supervisor terminated abnormally.\n",
            );
            return Ok(JobOutcome::StudentFailure("execution_failed"));
        }
        let output = decode_output(*code, output)?;
        capture.push(
            student_visible,
            &format!(
                "Job {name}\nstdout:\n{}\nstderr:\n{}\nExit code: {}\n",
                output.stdout, output.stderr, output.exit_code
            ),
        );
        return Ok(match output.failure.as_deref() {
            Some("timeout") => JobOutcome::StudentFailure("timeout"),
            Some("output_limit") => JobOutcome::StudentFailure("output_limit"),
            Some(_) => anyhow::bail!("unknown execution failure"),
            None => JobOutcome::Output(output.exit_code, output.stdout),
        });
    }
    // Fetch even after timeouts or errors, while the Pod still exists.
    match pods
        .list(&ListParams::default().labels(&format!("job-name={name}")))
        .await
    {
        Ok(list) => {
            for pod in list.items {
                if let Some(pod_name) = pod.metadata.name {
                    match pods
                        .logs(
                            &pod_name,
                            &LogParams {
                                limit_bytes: Some(65536),
                                ..Default::default()
                            },
                        )
                        .await
                    {
                        Ok(output) => {
                            capture.push(student_visible, &format!("Job {name}\n{output}\n"))
                        }
                        Err(_) => capture.push(student_visible, "Sandbox logs were unavailable.\n"),
                    }
                }
            }
        }
        Err(_) => capture.push(student_visible, "Sandbox logs were unavailable.\n"),
    }
    match &outcome {
        Ok(JobOutcome::Output(code, _)) => {
            capture.push(student_visible, &format!("Exit code: {code}\n"))
        }
        Ok(JobOutcome::StudentFailure(reason)) => {
            capture.push(student_visible, &format!("Execution failed: {reason}\n"))
        }
        Ok(JobOutcome::Failed(status)) => {
            capture.push(student_visible, &format!("Execution status: {status:?}\n"))
        }
        Err(_) => capture.push(student_visible, "Execution could not complete.\n"),
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn capture_keeps_scoring_stdout_and_failure_stderr_separate() {
        let output = std::process::Command::new("python3")
            .args(["-c", include_str!("../../../scripts/capture-execution.py"),
                "python3", "-c", "import sys; print('answer'); print('compiler failure', file=sys.stderr); sys.exit(2)"])
            .output().unwrap();
        assert!(output.status.success());
        let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(result["stdout"], "answer\n");
        assert_eq!(result["stderr"], "compiler failure\n");
        assert_eq!(result["exit_code"], 2);
    }
    #[test]
    fn execution_timeout_kills_children_even_with_closed_output() {
        for child in [
            "import time; time.sleep(30)",
            "import os,time; os.close(1); os.close(2); time.sleep(30)",
        ] {
            let started = std::time::Instant::now();
            let output = std::process::Command::new("python3")
                .env("GRADING_EXECUTION_TIMEOUT", "0.1")
                .args([
                    "-c",
                    include_str!("../../../scripts/capture-execution.py"),
                    "python3",
                    "-c",
                    child,
                ])
                .output()
                .unwrap();
            assert!(started.elapsed() < Duration::from_secs(5));
            let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(result["failure"], "timeout");
            assert_eq!(result["exit_code"], 124);
        }
    }

    #[test]
    fn output_floods_are_stopped_and_both_streams_are_drained() {
        for stream in [1, 2] {
            let child = format!(
                "import os; os.write(1,b'answer'); os.write(2,b'diagnostic');\nwhile True: os.write({stream}, b'x'*8192)"
            );
            let output = std::process::Command::new("python3")
                .args([
                    "-c",
                    include_str!("../../../scripts/capture-execution.py"),
                    "python3",
                    "-c",
                    &child,
                ])
                .output()
                .unwrap();
            assert!(output.status.success());
            let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
            assert_eq!(result["failure"], "output_limit");
            assert_eq!(result["exit_code"], 125);
            assert!(result["stdout"].as_str().unwrap().len() <= 65536);
            assert!(result["stderr"].as_str().unwrap().len() <= 65536);
        }
    }

    #[test]
    fn killed_supervisor_cannot_claim_success() {
        assert!(decode_output(137, r#"{"stdout":"forged","stderr":"","exit_code":0}"#).is_err());
    }

    #[test]
    fn student_cannot_open_supervisor_output() {
        let output = std::process::Command::new("python3")
            .args([
                "-c",
                include_str!("../../../scripts/capture-execution.py"),
                "python3",
                "-c",
                "import os; open('/proc/%d/fd/1' % os.getppid(), 'w').write('forged')",
            ])
            .output()
            .unwrap();
        assert!(output.status.success());
        let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_ne!(result["exit_code"], 0);
        assert_eq!(result["stdout"], "");
        assert!(
            result["stderr"]
                .as_str()
                .unwrap()
                .contains("PermissionError")
        );
    }

    #[test]
    fn capture_bounds_utf8_and_preserves_visibility() {
        let capture = Capture::default();
        capture.push(false, &"ü".repeat(MAX_RUN_LOG_BYTES));
        capture.push(true, "overflow");
        let logs = capture.finish();
        assert_eq!(logs.len(), 1);
        assert!(!logs[0].student_visible);
        assert_eq!(logs[0].text.len(), MAX_RUN_LOG_BYTES);
    }
}
