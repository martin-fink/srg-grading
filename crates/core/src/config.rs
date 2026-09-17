//! Strict versioned course definitions and instructor-approved execution limits.
use crate::{integrity::safe_path, security::valid_hex};
use anyhow::{Result, ensure};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CourseConfig {
    pub schema_version: u32,
    pub course: Course,
    pub assignments: BTreeMap<String, Assignment>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Course {
    pub id: String,
    pub title: String,
    pub github_organization: String,
    pub timezone: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Assignment {
    pub title: String,
    #[serde(deserialize_with = "datetime")]
    pub opens_at: DateTime<Utc>,
    #[serde(deserialize_with = "datetime")]
    pub deadline: DateTime<Utc>,
    pub branch: String,
    pub template: String,
    pub template_revision: String,
    pub image: String,
    pub integrity_manifest: String,
    pub public_tests: String,
    pub execution_profile: String,
    pub max_points: i32,
    pub timeout_seconds: u32,
    pub resources: Resources,
    pub score: ScorePolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Resources {
    pub cpu: u32,
    pub memory_gib: u32,
    pub storage_gib: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ScorePolicy {
    pub source: String,
    pub selection: String,
}

fn datetime<'de, D: Deserializer<'de>>(d: D) -> Result<DateTime<Utc>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Timestamp {
        Text(String),
        Toml(toml::value::Datetime),
    }
    let value = match Timestamp::deserialize(d)? {
        Timestamp::Text(s) => s,
        Timestamp::Toml(t) => t.to_string(),
    };
    DateTime::parse_from_rfc3339(&value)
        .map(|t| t.with_timezone(&Utc))
        .map_err(serde::de::Error::custom)
}

pub fn identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 80
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

impl CourseConfig {
    pub fn parse(input: &str) -> Result<Self> {
        let value: Self = toml::from_str(input)?;
        value.validate()?;
        Ok(value)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(self.schema_version == 1, "unsupported course schema");
        ensure!(
            identifier(&self.course.id) && identifier(&self.course.github_organization),
            "invalid course or organization ID"
        );
        ensure!(
            !self.course.title.is_empty() && self.course.title.len() <= 200,
            "invalid course title"
        );
        self.course.timezone.parse::<chrono_tz::Tz>()?;
        ensure!(!self.assignments.is_empty(), "course has no assignments");
        for (id, assignment) in &self.assignments {
            ensure!(identifier(id), "invalid assignment ID");
            assignment.validate()?;
        }
        Ok(())
    }
}

impl Assignment {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.opens_at < self.deadline,
            "opening must precede deadline"
        );
        ensure!(
            !self.title.is_empty() && self.title.len() <= 200,
            "invalid title"
        );
        ensure!(
            identifier(&self.branch),
            "prototype requires a simple branch name"
        );
        let parts: Vec<_> = self.template.split('/').collect();
        ensure!(
            parts.len() == 2 && parts.iter().all(|p| identifier(p)),
            "invalid template repository"
        );
        ensure!(
            valid_hex(&self.template_revision, 40),
            "template must use a full lowercase commit SHA"
        );
        let (image, hash) = self
            .image
            .split_once("@sha256:")
            .ok_or_else(|| anyhow::anyhow!("image must be digest-pinned"))?;
        ensure!(
            !image.is_empty()
                && image.len() < 240
                && !image.contains(char::is_whitespace)
                && valid_hex(hash, 64),
            "invalid image digest"
        );
        ensure!(
            identifier(&self.execution_profile),
            "invalid execution profile"
        );
        safe_path(&self.integrity_manifest)?;
        safe_path(&self.public_tests)?;
        ensure!(
            self.max_points > 0 && self.max_points <= 100_000,
            "invalid maximum points"
        );
        ensure!(
            (1..=86400).contains(&self.timeout_seconds),
            "timeout exceeds platform limit"
        );
        self.resources.validate()?;
        ensure!(
            self.score.source == "public-tests"
                && self.score.selection == "latest-eligible-submission",
            "unsupported score policy"
        );
        Ok(())
    }
}

impl Resources {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            (1..=64).contains(&self.cpu)
                && (1..=256).contains(&self.memory_gib)
                && (1..=512).contains(&self.storage_gib),
            "resources outside platform caps"
        );
        Ok(())
    }
    pub fn fits(&self, caps: &Self) -> bool {
        self.cpu <= caps.cpu
            && self.memory_gib <= caps.memory_gib
            && self.storage_gib <= caps.storage_gib
    }
}
