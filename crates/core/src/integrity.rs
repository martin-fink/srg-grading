//! Exact Git-blob manifests and bounded snapshots; no checkout conversion is used.
use crate::security::{digest, valid_hex};
use anyhow::{Result, ensure};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// A source-policy violation cannot be fixed by retrying the same commit.
#[derive(Debug)]
pub struct InvalidSubmission;
impl std::fmt::Display for InvalidSubmission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("source violates submission limits")
    }
}
impl std::error::Error for InvalidSubmission {}

pub const MAX_SNAPSHOT_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_FILE_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_FILES: usize = 10_000;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Blob {
    pub mode: String,
    pub data: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub sha: String,
    pub files: BTreeMap<String, Blob>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub schema_version: u32,
    pub template_revision: String,
    pub editable: Vec<String>,
    pub files: BTreeMap<String, ProtectedFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtectedFile {
    pub mode: String,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Finding {
    pub path: String,
    pub reason: String,
}

pub fn safe_path(path: &str) -> Result<()> {
    ensure!(
        !path.is_empty()
            && path.len() <= 1024
            && !path.contains(['\\', '\0', ':'])
            && !path.chars().any(char::is_control),
        "unsafe path"
    );
    ensure!(
        path.split('/')
            .all(|p| !p.is_empty() && p != "." && p != ".." && !p.eq_ignore_ascii_case(".git")),
        "unsafe path"
    );
    Ok(())
}

impl Blob {
    pub fn bytes(&self) -> Result<Vec<u8>> {
        ensure!(
            self.data.len() <= MAX_FILE_BYTES.div_ceil(3) * 4,
            "file exceeds size limit"
        );
        let data = STANDARD.decode(&self.data)?;
        ensure!(data.len() <= MAX_FILE_BYTES, "file exceeds size limit");
        Ok(data)
    }
}

impl Snapshot {
    pub fn validate(&self) -> Result<()> {
        self.validate_structure()?;
        for (path, blob) in &self.files {
            ensure!(
                blob.mode == "100644" || blob.mode == "100755",
                "symlinks and submodules are unsupported: {path}"
            );
        }
        Ok(())
    }

    pub fn validate_structure(&self) -> Result<()> {
        ensure!(valid_hex(&self.sha, 40), "invalid snapshot SHA");
        ensure!(self.files.len() <= MAX_FILES, "too many source files");
        let mut size = 0;
        for (path, blob) in &self.files {
            safe_path(path)?;
            ensure!(
                matches!(
                    blob.mode.as_str(),
                    "100644" | "100755" | "120000" | "160000"
                ),
                "unsupported Git mode: {path}"
            );
            size += blob.bytes()?.len();
            ensure!(size <= MAX_SNAPSHOT_BYTES, "snapshot exceeds size limit");
            let mut parent = path.as_str();
            while let Some((prefix, _)) = parent.rsplit_once('/') {
                ensure!(!self.files.contains_key(prefix), "file/directory collision");
                parent = prefix;
            }
        }
        Ok(())
    }
}

impl Manifest {
    pub fn generate(snapshot: &Snapshot, editable: Vec<String>) -> Result<Self> {
        snapshot.validate()?;
        let mut manifest = Self {
            schema_version: 1,
            template_revision: snapshot.sha.clone(),
            editable,
            files: BTreeMap::new(),
        };
        manifest.validate()?;
        for (path, blob) in &snapshot.files {
            if !manifest.is_editable(path) {
                manifest.files.insert(
                    path.clone(),
                    ProtectedFile {
                        mode: blob.mode.clone(),
                        sha256: digest(blob.bytes()?),
                    },
                );
            }
        }
        Ok(manifest)
    }

    pub fn is_editable(&self, path: &str) -> bool {
        self.editable.iter().any(|prefix| path.starts_with(prefix))
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == 1 && valid_hex(&self.template_revision, 40),
            "invalid manifest version or revision"
        );
        for prefix in &self.editable {
            ensure!(
                prefix.ends_with('/'),
                "editable areas must be directory prefixes"
            );
            safe_path(prefix.trim_end_matches('/'))?;
            ensure!(
                !prefix.starts_with(".github/") && !prefix.starts_with("tests/"),
                "workflow and test trees cannot be editable"
            );
        }
        for (path, file) in &self.files {
            safe_path(path)?;
            ensure!(
                !self.is_editable(path) && valid_hex(&file.sha256, 64),
                "invalid protected file"
            );
            ensure!(
                file.mode == "100644" || file.mode == "100755",
                "invalid protected mode"
            );
        }
        Ok(())
    }

    pub fn check(&self, snapshot: &Snapshot) -> Result<Vec<Finding>> {
        self.validate()?;
        snapshot.validate_structure()?;
        let mut findings = Vec::new();
        for (path, expected) in &self.files {
            let reason = match snapshot.files.get(path) {
                None => Some("deleted"),
                Some(blob) if blob.mode != expected.mode => Some("mode changed"),
                Some(blob) if digest(blob.bytes()?) != expected.sha256 => Some("content changed"),
                _ => None,
            };
            if let Some(reason) = reason {
                findings.push(Finding {
                    path: path.clone(),
                    reason: reason.into(),
                });
            }
        }
        for (path, blob) in &snapshot.files {
            if !self.files.contains_key(path) && !self.is_editable(path) {
                findings.push(Finding {
                    path: path.clone(),
                    reason: "unapproved addition".into(),
                });
            } else if self.is_editable(path) && blob.mode != "100644" && blob.mode != "100755" {
                findings.push(Finding {
                    path: path.clone(),
                    reason: "unsupported symlink or submodule".into(),
                });
            }
        }
        Ok(findings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn snapshot() -> Snapshot {
        Snapshot {
            sha: "a".repeat(40),
            files: BTreeMap::from([(
                ".github/workflows/test.yml".into(),
                Blob {
                    mode: "100644".into(),
                    data: STANDARD.encode(b"exact\r\nbytes"),
                },
            )]),
        }
    }
    #[test]
    fn catches_additions_deletions_modes_and_byte_changes() {
        let original = snapshot();
        let manifest = Manifest::generate(&original, vec!["src/".into()]).unwrap();
        for operation in 0..4 {
            let mut changed = original.clone();
            match operation {
                0 => {
                    changed.files.insert(
                        ".github/workflows/new.yml".into(),
                        Blob {
                            mode: "100644".into(),
                            data: String::new(),
                        },
                    );
                }
                1 => changed.files.clear(),
                2 => changed.files.values_mut().next().unwrap().mode = "100755".into(),
                _ => {
                    changed.files.values_mut().next().unwrap().data =
                        STANDARD.encode(b"exact\nbytes")
                }
            }
            assert!(!manifest.check(&changed).unwrap().is_empty());
        }
    }
    #[test]
    fn extraction_rejects_escaping_paths_and_symlinks() {
        for path in [
            "../x",
            "/x",
            "a/../../x",
            ".git/config",
            "x\\y",
            "x//y",
            "x/./y",
        ] {
            assert!(safe_path(path).is_err());
        }
        let mut source = snapshot();
        source.files.values_mut().next().unwrap().mode = "120000".into();
        assert!(source.validate().is_err());
        let manifest = Manifest::generate(&snapshot(), vec!["src/".into()]).unwrap();
        let findings = manifest.check(&source).unwrap();
        assert_eq!(findings[0].path, ".github/workflows/test.yml");
        source.files.insert(
            "src/escape".into(),
            Blob {
                mode: "120000".into(),
                data: STANDARD.encode(b"../../secret"),
            },
        );
        assert!(
            manifest
                .check(&source)
                .unwrap()
                .iter()
                .any(|f| f.path == "src/escape")
        );
    }
}
