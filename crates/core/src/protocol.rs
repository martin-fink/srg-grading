//! Versioned leases and results for the restricted executor API.
use crate::{
    config::{Assignment, Resources, identifier},
    integrity::{Finding, Manifest},
    security::digest,
};
use anyhow::{Result, ensure};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Empty wire marker retained to preserve immutable shared-runner revision digests.
/// Fixed test cases cannot be deserialized into this type.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptTests {
    pub schema_version: u32,
    pub tests: [(); 0],
}
impl Default for ScriptTests {
    fn default() -> Self {
        Self {
            schema_version: 1,
            tests: [],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Revision {
    pub course_id: String,
    pub assignment_id: String,
    pub assignment: Assignment,
    pub manifest: Manifest,
    pub tests: ScriptTests,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grader: Option<Grader>,
}

impl Revision {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            identifier(&self.course_id) && identifier(&self.assignment_id),
            "invalid revision identity"
        );
        self.assignment.validate()?;
        ensure!(
            self.tests.schema_version == 1,
            "unsupported script test marker"
        );
        let grader = self
            .grader
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("missing shared-runner grader"))?;
        grader.validate()?;
        ensure!(
            grader.image == self.assignment.image,
            "shared runner images must match"
        );
        ensure!(
            self.assignment.execution_profile == "registered-v1",
            "only registered-v1 is supported"
        );
        self.manifest.validate()?;
        ensure!(
            self.manifest.template_revision == self.assignment.template_revision,
            "template/manifest revision mismatch"
        );
        ensure!(
            self.manifest
                .files
                .keys()
                .any(|p| p == &self.assignment.public_tests
                    || p.starts_with(&format!("{}/", self.assignment.public_tests))),
            "public tests must be protected"
        );
        ensure!(
            serde_json::to_vec(self)?.len() <= 1_900_000,
            "exercise definition exceeds worker lease size limit"
        );
        Ok(())
    }

    pub fn digest(&self) -> Result<String> {
        Ok(digest(serde_json::to_vec(self)?))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Lease {
    pub schema_version: u32,
    pub task_id: Uuid,
    pub run_id: Uuid,
    pub lease_token: Uuid,
    pub expires_at: DateTime<Utc>,
    pub sha: String,
    pub revision_digest: String,
    pub source_digest: String,
    pub revision: Revision,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline: Option<PublicBaseline>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeaseRequest {
    pub profiles: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Heartbeat {
    pub lease_token: Uuid,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunStatus {
    Completed,
    IntegrityFailed,
    InfrastructureFailed,
    TimedOut,
    Invalidated,
}

pub const MAX_RUN_LOG_BYTES: usize = 256 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunLog {
    pub student_visible: bool,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunResult {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub logs: Vec<RunLog>,
    pub schema_version: u32,
    pub lease_token: Uuid,
    pub run_id: Uuid,
    pub sha: String,
    pub revision_digest: String,
    pub image: String,
    pub resources: Resources,
    pub status: RunStatus,
    pub findings: Vec<Finding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<ScriptScore>,
}

impl RunResult {
    pub fn validate(&self, lease: &Lease) -> Result<Option<i32>> {
        lease.revision.validate()?;
        ensure!(
            self.schema_version == 1
                && self.lease_token == lease.lease_token
                && self.run_id == lease.run_id
                && self.sha == lease.sha
                && self.revision_digest == lease.revision_digest,
            "result provenance mismatch"
        );
        ensure!(
            self.image == lease.revision.assignment.image
                && self.resources == lease.revision.assignment.resources,
            "execution environment mismatch"
        );
        ensure!(
            self.findings.len() <= 10_000
                && self
                    .findings
                    .iter()
                    .all(|f| f.path.len() <= 1024 && f.reason.len() <= 256),
            "findings too large"
        );
        ensure!(
            self.logs.len() <= 128
                && self.logs.iter().map(|log| log.text.len()).sum::<usize>() <= MAX_RUN_LOG_BYTES,
            "run logs exceed limit"
        );
        ensure!(
            lease.baseline.is_none() || self.logs.iter().all(|log| !log.student_visible),
            "private run logs cannot be student visible"
        );
        let scoring = matches!(self.status, RunStatus::Completed | RunStatus::Invalidated);
        if !scoring {
            ensure!(
                self.score.is_none(),
                "failed execution cannot contain a score"
            );
            ensure!(
                self.status == RunStatus::IntegrityFailed || self.findings.is_empty(),
                "unexpected findings"
            );
            return Ok(None);
        }
        ensure!(
            self.findings.is_empty(),
            "unexpected findings for script workflow"
        );
        let score = self
            .score
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("missing script score"))?;
        score.validate(
            lease.revision.assignment.max_points,
            lease.baseline.as_ref(),
        )?;
        ensure!(
            score.invalidated == (self.status == RunStatus::Invalidated),
            "script status mismatch"
        );
        Ok((!score.invalidated).then_some(score.points))
    }
}

/// Immutable provenance for instructor grading code and its runtime.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Grader {
    pub repository: String,
    pub revision: String,
    pub image: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_digest: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<Workflow>,
}
impl Grader {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            crate::config::github_repository(&self.repository),
            "invalid grader repository"
        );
        ensure!(
            crate::security::valid_hex(&self.revision, 40),
            "grader must use a commit SHA"
        );
        crate::config::validate_image(&self.image)?;
        let digest = self
            .source_digest
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("missing grader snapshot reference"))?;
        ensure!(
            crate::security::valid_hex(digest, 64),
            "invalid grader snapshot reference"
        );
        self.workflow
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("missing script workflow"))?
            .validate()?;
        Ok(())
    }
}

/// A manual private run starts from this completed public run, never another adjustment.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicBaseline {
    pub run_id: Uuid,
    pub points: i32,
    pub deadline: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Workflow {
    pub public_command: Vec<String>,
    pub private_command: Option<Vec<String>>,
}
impl Workflow {
    pub fn validate(&self) -> Result<()> {
        validate_command(&self.public_command)?;
        if let Some(command) = &self.private_command {
            validate_command(command)?;
        }
        Ok(())
    }
}
pub fn validate_command(command: &[String]) -> Result<()> {
    ensure!(
        !command.is_empty()
            && command.len() <= 256
            && command[0].starts_with('/')
            && command
                .iter()
                .all(|s| s.len() <= 65536 && !s.contains('\0')),
        "invalid execution command"
    );
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScriptScore {
    pub schema_version: u32,
    pub points: i32,
    #[serde(default)]
    pub invalidated: bool,
    #[serde(default)]
    pub reason: String,
}
impl ScriptScore {
    pub fn validate(&self, maximum: i32, baseline: Option<&PublicBaseline>) -> Result<()> {
        ensure!(
            self.schema_version == 1
                && (0..=maximum).contains(&self.points)
                && self.reason.len() <= 2048,
            "invalid script result"
        );
        ensure!(
            !self.invalidated || baseline.is_some(),
            "only private grading can invalidate a score"
        );
        ensure!(
            !(self.invalidated || baseline.is_some_and(|b| b.points != self.points))
                || !self.reason.trim().is_empty(),
            "private score changes require a reason"
        );
        Ok(())
    }
}
