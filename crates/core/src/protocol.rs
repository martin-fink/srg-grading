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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestSuite {
    pub schema_version: u32,
    pub tests: Vec<PublicTest>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicTest {
    pub id: String,
    pub points: i32,
    pub stdin: String,
    pub stdout: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Revision {
    pub course_id: String,
    pub assignment_id: String,
    pub assignment: Assignment,
    pub manifest: Manifest,
    pub tests: TestSuite,
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
        if let Some(grader) = &self.grader {
            grader.validate()?;
            if grader.source_digest.is_some() {
                ensure!(
                    grader.image == self.assignment.image,
                    "shared runner images must match"
                );
            }
            ensure!(
                self.assignment.execution_profile == "registered-v1",
                "registered grader requires registered-v1"
            );
        } else {
            ensure!(
                self.assignment.execution_profile != "registered-v1",
                "missing registered grader"
            );
        }
        self.manifest.validate()?;
        ensure!(
            self.manifest.template_revision == self.assignment.template_revision,
            "template/manifest revision mismatch"
        );
        let scripted = self.grader.as_ref().is_some_and(|g| g.workflow.is_some());
        ensure!(
            self.manifest
                .files
                .keys()
                .any(|p| p == &self.assignment.public_tests
                    || (scripted && p.starts_with(&format!("{}/", self.assignment.public_tests)))),
            "public tests must be protected"
        );
        if scripted {
            ensure!(
                self.tests.tests.is_empty(),
                "script workflows define their own public tests"
            );
            ensure!(
                serde_json::to_vec(self)?.len() <= 1_900_000,
                "exercise definition exceeds worker lease size limit"
            );
            return Ok(());
        }
        ensure!(
            self.tests.schema_version == 1
                && !self.tests.tests.is_empty()
                && self.tests.tests.len() <= 100,
            "invalid test suite"
        );
        let mut ids = std::collections::BTreeSet::new();
        let mut total = 0_i64;
        for test in &self.tests.tests {
            ensure!(
                identifier(&test.id) && ids.insert(&test.id),
                "duplicate or invalid test ID"
            );
            ensure!(
                test.id.len() <= 30
                    && test
                        .id
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                    && !test.id.ends_with('-'),
                "test IDs must be lowercase DNS-compatible names of at most 30 characters"
            );
            ensure!(
                test.points > 0 && test.stdin.len() <= 65536 && test.stdout.len() <= 65536,
                "invalid test limits"
            );
            total += i64::from(test.points);
        }
        ensure!(
            total == i64::from(self.assignment.max_points),
            "test points do not total max_points"
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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestResult {
    pub id: String,
    pub passed: bool,
    pub log: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunResult {
    pub schema_version: u32,
    pub lease_token: Uuid,
    pub run_id: Uuid,
    pub sha: String,
    pub revision_digest: String,
    pub image: String,
    pub resources: Resources,
    pub status: RunStatus,
    pub tests: Vec<TestResult>,
    pub findings: Vec<Finding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub private: Option<PrivateDecision>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub private_tests: Vec<PrivateTestResult>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score: Option<ScriptScore>,
}

impl RunResult {
    pub fn validate(&self, lease: &Lease) -> Result<Option<i32>> {
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
        let scoring = matches!(self.status, RunStatus::Completed | RunStatus::Invalidated);
        if !scoring {
            ensure!(
                self.private.is_none() && self.private_tests.is_empty() && self.score.is_none(),
                "failed execution cannot contain a private decision"
            );
            ensure!(self.tests.is_empty(), "non-scoring result contains tests");
            ensure!(
                self.status == RunStatus::IntegrityFailed || self.findings.is_empty(),
                "unexpected findings"
            );
            return Ok(None);
        }
        if lease
            .revision
            .grader
            .as_ref()
            .is_some_and(|g| g.workflow.is_some())
        {
            ensure!(
                self.tests.is_empty()
                    && self.private.is_none()
                    && self.private_tests.is_empty()
                    && self.findings.is_empty(),
                "unexpected fixed test results for script workflow"
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
            return Ok((!score.invalidated).then_some(score.points));
        }
        ensure!(self.score.is_none(), "unexpected script score");
        ensure!(
            self.findings.is_empty() && self.tests.len() == lease.revision.tests.tests.len(),
            "incomplete or inconsistent results"
        );
        let mut seen = std::collections::BTreeSet::new();
        let mut points = 0;
        for result in &self.tests {
            ensure!(
                result.log.len() <= 65536 && seen.insert(&result.id),
                "duplicate or oversized test result"
            );
            let test = lease
                .revision
                .tests
                .tests
                .iter()
                .find(|t| t.id == result.id)
                .ok_or_else(|| anyhow::anyhow!("unknown test"))?;
            if result.passed {
                points += test.points;
            }
        }
        ensure!(
            points <= lease.revision.assignment.max_points,
            "points out of bounds"
        );
        if lease.baseline.is_none() {
            ensure!(
                self.private.is_none()
                    && self.private_tests.is_empty()
                    && self.status == RunStatus::Completed,
                "private grading requires an instructor-scheduled post-deadline run"
            );
            return Ok(Some(points));
        }
        if let Some(grader) = &lease.revision.grader {
            points = lease.baseline.as_ref().unwrap().points;
            ensure!(
                self.private_tests.len() == grader.tests.len(),
                "incomplete private tests"
            );
            let mut ids = std::collections::BTreeSet::new();
            for outcome in &self.private_tests {
                ensure!(
                    ids.insert(&outcome.id) && grader.tests.iter().any(|t| t.id == outcome.id),
                    "duplicate or unknown private test"
                );
            }
            let decision = self
                .private
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("missing private check decision"))?;
            decision.validate(points, lease.revision.assignment.max_points)?;
            ensure!(
                decision.invalidated == (self.status == RunStatus::Invalidated),
                "invalidation status mismatch"
            );
            if decision.invalidated {
                return Ok(None);
            }
            points += decision.adjustment;
        } else {
            ensure!(
                self.private.is_none()
                    && self.private_tests.is_empty()
                    && self.status == RunStatus::Completed,
                "unexpected private decision"
            );
        }
        Ok(Some(points))
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tests: Vec<PrivateTest>,
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
        if let Some(digest) = &self.source_digest {
            ensure!(
                crate::security::valid_hex(digest, 64) && self.workflow.is_some(),
                "invalid grader snapshot reference"
            );
        }
        if let Some(workflow) = &self.workflow {
            workflow.validate()?;
            ensure!(
                self.tests.is_empty(),
                "script workflows do not use the fixed private suite"
            );
        }
        ensure!(self.tests.len() <= 100, "too many private tests");
        let mut ids = std::collections::BTreeSet::new();
        for test in &self.tests {
            test.validate()?;
            ensure!(ids.insert(&test.id), "duplicate private test ID");
        }
        Ok(())
    }
}

/// Private checks report an explicit adjustment or invalidation, never a replacement public score.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrivateDecision {
    pub schema_version: u32,
    pub adjustment: i32,
    pub invalidated: bool,
    pub reason: String,
}
impl PrivateDecision {
    pub fn validate(&self, public_points: i32, max_points: i32) -> Result<()> {
        ensure!(
            self.schema_version == 1 && self.reason.len() <= 2048,
            "invalid private decision"
        );
        ensure!(
            !(self.invalidated || self.adjustment != 0) || !self.reason.trim().is_empty(),
            "private changes require a reason"
        );
        ensure!(
            !self.invalidated || self.adjustment == 0,
            "invalidation cannot also adjust points"
        );
        let total = i64::from(public_points) + i64::from(self.adjustment);
        ensure!(
            (0..=i64::from(max_points)).contains(&total),
            "adjusted points out of bounds"
        );
        Ok(())
    }
}

/// Private inputs execute in student sandboxes; expected output stays with the executor.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrivateTest {
    pub id: String,
    pub stdin: String,
    pub stdout: String,
}
impl PrivateTest {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.id.is_empty()
                && self.id.len() <= 30
                && self
                    .id
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
                && !self.id.ends_with('-'),
            "invalid private test ID"
        );
        ensure!(
            self.stdin.len() <= 65536 && self.stdout.len() <= 65536,
            "private test exceeds size limit"
        );
        Ok(())
    }
    pub fn outcome(&self, success: bool, output: &str) -> PrivateTestResult {
        PrivateTestResult {
            id: self.id.clone(),
            passed: success && output.len() <= 65536 && output == self.stdout,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrivateTestResult {
    pub id: String,
    pub passed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrivateSuite {
    pub schema_version: u32,
    pub tests: Vec<PrivateTest>,
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
