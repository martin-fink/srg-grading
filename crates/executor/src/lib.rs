//! Independently approved Kubernetes execution profiles and sandbox job construction.
use anyhow::{Result, ensure};
use grading_core::{
    config::{Resources, identifier},
    protocol::Lease,
};
use k8s_openapi::api::batch::v1::Job;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::{collections::BTreeMap, path::PathBuf};

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub api_url: String,
    pub token_file: PathBuf,
    pub tls_identity_file: Option<PathBuf>,
    pub tls_ca_file: Option<PathBuf>,
    pub namespace: String,
    pub runtime_class: String,
    pub source_pvc: String,
    pub staging_root: PathBuf,
    pub profiles: BTreeMap<String, Profile>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    pub images: Vec<String>,
    pub command: Vec<String>,
    pub resources: Resources,
    pub timeout_seconds: u32,
}

impl Config {
    pub fn approve(&self, lease: &Lease) -> Result<&Profile> {
        ensure!(
            lease.schema_version == 1 && lease.revision.digest()? == lease.revision_digest,
            "lease revision mismatch"
        );
        lease.revision.validate()?;
        ensure!(
            identifier(&self.namespace)
                && identifier(&self.runtime_class)
                && identifier(&self.source_pvc),
            "invalid cluster configuration"
        );
        ensure!(
            self.runtime_class == "gvisor",
            "prototype requires a gvisor RuntimeClass"
        );
        let assignment = &lease.revision.assignment;
        let profile = self
            .profiles
            .get(&assignment.execution_profile)
            .ok_or_else(|| anyhow::anyhow!("unapproved execution profile"))?;
        ensure!(
            profile.images.contains(&assignment.image)
                && assignment.resources.fits(&profile.resources)
                && assignment.timeout_seconds <= profile.timeout_seconds,
            "unapproved image or resource budget"
        );
        ensure!(
            !profile.command.is_empty()
                && profile.command[0].starts_with('/')
                && profile.command.iter().all(|s| !s.contains('\0')),
            "invalid instructor command"
        );
        Ok(profile)
    }
}

pub fn job(config: &Config, lease: &Lease, test_id: &str, remaining: u32) -> Result<Job> {
    let profile = config.approve(lease)?;
    ensure!(identifier(test_id), "invalid test ID");
    let id = lease.lease_token.simple().to_string();
    let name = format!("grade-{}-{}", &id[..16], test_id.to_lowercase());
    ensure!(
        name.len() <= 63 && !name.contains('_'),
        "test IDs used in jobs must be DNS-compatible"
    );
    let resources = &lease.revision.assignment.resources;
    let mut command = vec![
        "/bin/sh".to_string(),
        "-c".into(),
        "cp -R /source/. /workspace/ && cd /workspace && exec \"$@\" < /input/stdin 2>/tmp/stderr"
            .into(),
        "grading".into(),
    ];
    command.extend(profile.command.clone());
    Ok(serde_json::from_value(json!({
        "apiVersion":"batch/v1","kind":"Job",
        "metadata":{"name":name,"namespace":config.namespace,"labels":{"app":"grading-sandbox","grading-lease":id}},
        "spec":{"backoffLimit":0,"activeDeadlineSeconds":remaining.max(1).min(lease.revision.assignment.timeout_seconds),"ttlSecondsAfterFinished":300,
            "template":{"metadata":{"labels":{"app":"grading-sandbox","grading-lease":id}},"spec":{
                "restartPolicy":"Never","automountServiceAccountToken":false,"runtimeClassName":config.runtime_class,
                "enableServiceLinks":false,"terminationGracePeriodSeconds":5,
                "securityContext":{"runAsNonRoot":true,"runAsUser":10003,"runAsGroup":10003,"fsGroup":10003,"seccompProfile":{"type":"RuntimeDefault"}},
                "containers":[{"name":"submission","image":lease.revision.assignment.image,"imagePullPolicy":"IfNotPresent","command":command,
                    "securityContext":{"allowPrivilegeEscalation":false,"readOnlyRootFilesystem":true,"capabilities":{"drop":["ALL"]}},
                    "resources":{"requests":{"cpu":resources.cpu.to_string(),"memory":format!("{}Gi",resources.memory_gib),"ephemeral-storage":format!("{}Gi",resources.storage_gib)},"limits":{"cpu":resources.cpu.to_string(),"memory":format!("{}Gi",resources.memory_gib),"ephemeral-storage":format!("{}Gi",resources.storage_gib)}},
                    "volumeMounts":[{"name":"source","mountPath":"/source","subPath":format!("runs/{id}/source"),"readOnly":true},
                        {"name":"source","mountPath":"/input","subPath":format!("runs/{id}/inputs/{test_id}"),"readOnly":true},
                        {"name":"workspace","mountPath":"/workspace"},{"name":"tmp","mountPath":"/tmp"}]}],
                "volumes":[{"name":"source","persistentVolumeClaim":{"claimName":config.source_pvc,"readOnly":true}},
                    {"name":"workspace","emptyDir":{"sizeLimit":format!("{}Gi",resources.storage_gib)}},
                    {"name":"tmp","emptyDir":{"sizeLimit":"1Gi"}}]
            }}}
    }))?)
}
