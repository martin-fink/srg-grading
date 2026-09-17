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
    if workflow && let Ok(JobOutcome::Output(_, output)) = &outcome {
        #[derive(serde::Deserialize)]
        struct Output {
            stdout: String,
            stderr: String,
            exit_code: i32,
        }
        let output: Output =
            serde_json::from_str(output).context(Stage("student_output_decode"))?;
        capture.push(
            student_visible,
            &format!(
                "Job {name}\nstdout:\n{}\nstderr:\n{}\nExit code: {}\n",
                output.stdout, output.stderr, output.exit_code
            ),
        );
        return Ok(JobOutcome::Output(output.exit_code, output.stdout));
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
