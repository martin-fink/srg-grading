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
}

impl Revision {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            identifier(&self.course_id) && identifier(&self.assignment_id),
            "invalid revision identity"
        );
        self.assignment.validate()?;
        self.manifest.validate()?;
        ensure!(
            self.manifest.template_revision == self.assignment.template_revision,
            "template/manifest revision mismatch"
        );
        ensure!(
            self.manifest
                .files
                .contains_key(&self.assignment.public_tests),
            "public tests must be protected"
        );
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
        if self.status != RunStatus::Completed {
            ensure!(self.tests.is_empty(), "non-scoring result contains tests");
            ensure!(
                self.status == RunStatus::IntegrityFailed || self.findings.is_empty(),
                "unexpected findings"
            );
            return Ok(None);
        }
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
        Ok(Some(points))
    }
}
