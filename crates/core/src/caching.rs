//! Instructor-owned, immutable cache seeds. Input keys never include file timestamps.
use crate::{
    config::Resources,
    integrity::{Snapshot, safe_path},
    security::{digest, valid_hex},
};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Caching {
    pub version: u32,
    pub recipe_dir: String,
    pub command: Vec<String>,
    pub timeout_seconds: u32,
    pub resources: Resources,
    #[serde(default = "architecture")]
    pub architecture: String,
    pub artifacts: Vec<Artifact>,
}
fn architecture() -> String {
    "amd64".into()
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub name: String,
    pub path: String,
    pub mount_path: String,
    pub mode: Mode,
    pub max_size_gib: u32,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    ReadOnly,
    PrivateCopy,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Seed {
    pub config: Caching,
    pub input_key: String,
    pub digest: String,
    pub namespace: String,
    pub source_pvc: String,
}
impl Seed {
    pub fn validate(&self) -> Result<()> {
        self.config.validate()?;
        ensure!(
            valid_hex(&self.input_key, 64) && valid_hex(&self.digest, 64),
            "invalid cache digest"
        );
        ensure!(
            crate::config::identifier(&self.namespace)
                && crate::config::identifier(&self.source_pvc),
            "invalid cache location"
        );
        Ok(())
    }
}
impl Caching {
    pub fn validate(&self) -> Result<()> {
        ensure!(self.version > 0, "cache version must be positive");
        safe_path(&self.recipe_dir)?;
        crate::protocol::validate_command(&self.command)?;
        ensure!(
            (1..=86400).contains(&self.timeout_seconds),
            "cache timeout must be 1..86400 seconds"
        );
        self.resources.validate()?;
        ensure!(
            ["amd64", "arm64"].contains(&self.architecture.as_str()),
            "unsupported cache architecture"
        );
        ensure!(
            !self.artifacts.is_empty() && self.artifacts.len() <= 16,
            "cache requires 1..16 artifacts"
        );
        let mut names = std::collections::BTreeSet::new();
        let mut mounts = std::collections::BTreeSet::new();
        for a in &self.artifacts {
            ensure!(
                !a.name.is_empty()
                    && a.name.len() <= 40
                    && a.name
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
                "invalid cache artifact name"
            );
            safe_path(&a.path)?;
            // A fixed namespace prevents masking executables, inputs, or grading controls.
            let leaf = a.mount_path.strip_prefix("/cache/").unwrap_or("");
            ensure!(
                !leaf.is_empty()
                    && leaf.len() <= 64
                    && leaf
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"_-".contains(&b)),
                "cache mounts must be /cache/<name>"
            );
            ensure!(
                names.insert(&a.name) && mounts.insert(&a.mount_path),
                "duplicate cache artifact or mount"
            );
            ensure!(
                (1..=512).contains(&a.max_size_gib),
                "cache size must be 1..512 GiB"
            );
            for other in &self.artifacts {
                if !std::ptr::eq(a, other) {
                    ensure!(
                        a.path != other.path && !a.path.starts_with(&format!("{}/", other.path)),
                        "overlapping cache output paths"
                    );
                }
            }
        }
        ensure!(
            self.artifacts
                .iter()
                .map(|a| u64::from(a.max_size_gib))
                .sum::<u64>()
                <= u64::from(self.resources.storage_gib),
            "cache exports exceed preparation storage budget"
        );
        Ok(())
    }
    pub fn recipe(&self, grader: &Snapshot) -> Result<Snapshot> {
        self.validate()?;
        let prefix = format!("{}/", self.recipe_dir);
        let files = grader
            .files
            .iter()
            .filter_map(|(path, blob)| {
                path.strip_prefix(&prefix)
                    .map(|p| (p.to_owned(), blob.clone()))
            })
            .collect();
        // The full grader commit is deliberately excluded: private test edits don't invalidate seeds.
        let recipe = Snapshot {
            sha: "0".repeat(40),
            files,
        };
        recipe.validate()?;
        ensure!(
            !recipe.files.is_empty(),
            "cache recipe directory is empty or missing"
        );
        Ok(recipe)
    }
    pub fn key(&self, image: &str, source: &Snapshot, recipe: &Snapshot) -> Result<String> {
        self.validate()?;
        source.validate()?;
        recipe.validate()?;
        crate::config::validate_image(image)?;
        Ok(digest(serde_json::to_vec(&(
            "cache-preparation-v1",
            image,
            self,
            source,
            recipe,
        ))?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::integrity::Blob;
    fn config() -> Caching {
        Caching {
            version: 1,
            recipe_dir: "cache".into(),
            command: vec!["/bin/sh".into(), "/recipe/prepare.sh".into()],
            timeout_seconds: 60,
            resources: Resources {
                cpu: 1,
                memory_gib: 1,
                storage_gib: 2,
            },
            architecture: "amd64".into(),
            artifacts: vec![Artifact {
                name: "compiler".into(),
                path: "ccache".into(),
                mount_path: "/cache/compiler".into(),
                mode: Mode::ReadOnly,
                max_size_gib: 1,
            }],
        }
    }
    #[test]
    fn keys_pin_inputs_but_exclude_private_tests_and_grader_commit() {
        let mut config = config();
        let image = format!("registry.example/runner@sha256:{}", "a".repeat(64));
        let mut source = Snapshot {
            sha: "a".repeat(40),
            files: Default::default(),
        };
        let mut grader = Snapshot {
            sha: "b".repeat(40),
            files: std::collections::BTreeMap::from([(
                "cache/prepare.sh".into(),
                Blob {
                    mode: "100644".into(),
                    data: "ZWNobw==".into(),
                },
            )]),
        };
        let recipe = config.recipe(&grader).unwrap();
        let key = config.key(&image, &source, &recipe).unwrap();
        grader.sha = "c".repeat(40);
        grader.files.insert(
            "private.py".into(),
            Blob {
                mode: "100644".into(),
                data: "c2VjcmV0".into(),
            },
        );
        assert_eq!(
            key,
            config
                .key(&image, &source, &config.recipe(&grader).unwrap())
                .unwrap()
        );
        grader.files.get_mut("cache/prepare.sh").unwrap().data = "b3RoZXI=".into();
        assert_ne!(
            key,
            config
                .key(&image, &source, &config.recipe(&grader).unwrap())
                .unwrap()
        );
        source.sha = "d".repeat(40);
        assert_ne!(key, config.key(&image, &source, &recipe).unwrap());
        source.sha = "a".repeat(40);
        for change in ["version", "architecture", "image"] {
            config = super::tests::config();
            let mut changed_image = image.clone();
            match change {
                "version" => config.version += 1,
                "architecture" => config.architecture = "arm64".into(),
                _ => changed_image = image.replace(&"a".repeat(64), &"b".repeat(64)),
            };
            assert_ne!(key, config.key(&changed_image, &source, &recipe).unwrap());
        }
    }
    #[test]
    fn reject_path_escape_mount_masking_and_invalid_budgets() {
        for path in ["/etc", "../cache", "a/../../b", "x/.git/y"] {
            let mut c = config();
            c.artifacts[0].path = path.into();
            assert!(c.validate().is_err());
        }
        for mount in ["/grader", "/cache/../source", "/cache/x;id", "/cache/a/b"] {
            let mut c = config();
            c.artifacts[0].mount_path = mount.into();
            assert!(c.validate().is_err());
        }
        let mut c = config();
        c.artifacts.push(c.artifacts[0].clone());
        assert!(c.validate().is_err());
        let mut c = config();
        c.artifacts[0].max_size_gib = 3;
        assert!(c.validate().is_err());
        let mut c = config();
        c.version = 0;
        assert!(c.validate().is_err());
    }
}
