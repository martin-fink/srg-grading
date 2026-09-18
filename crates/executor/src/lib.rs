//! Approved shared runners and Kubernetes sandbox job construction.
use anyhow::{Result, ensure};
use grading_core::{
    config::{Resources, identifier},
    protocol::Lease,
};
use k8s_openapi::api::batch::v1::Job;
use serde::Deserialize;
use serde_json::json;
use std::path::PathBuf;

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
    pub registry: Option<Registry>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Registry {
    pub image_prefix: String,
    #[serde(default)]
    pub runner_images: Vec<String>,
    pub resources: Resources,
    pub timeout_seconds: u32,
}

impl Config {
    pub fn profile_names(&self) -> Vec<String> {
        vec!["registered-v1".into()]
    }

    pub fn approve(&self, lease: &Lease) -> Result<()> {
        ensure!(
            lease.schema_version == 1 && lease.revision.digest()? == lease.revision_digest,
            "lease revision mismatch"
        );
        lease.revision.validate()?;
        if let Some(baseline) = &lease.baseline {
            ensure!(
                chrono::Utc::now() > baseline.deadline,
                "private grading is not open yet"
            );
        }
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
        let registry = self
            .registry
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("registered exercises are disabled"))?;
        registry.resources.validate()?;
        let prefix = format!("{}/", registry.image_prefix.trim_end_matches('/'));
        ensure!(
            prefix.len() > 2
                && registry.image_prefix.contains('/')
                && registry
                    .image_prefix
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"./:_-".contains(&b)),
            "invalid registry prefix"
        );
        let grader = lease
            .revision
            .grader
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("missing grader provenance"))?;
        ensure!(
            assignment.image.starts_with(&prefix) && grader.image.starts_with(&prefix),
            "image outside approved registry namespace"
        );
        ensure!(
            registry.runner_images.contains(&assignment.image),
            "shared runner digest is not approved"
        );
        ensure!(
            assignment.resources.fits(&registry.resources)
                && assignment.timeout_seconds <= registry.timeout_seconds,
            "exercise exceeds worker caps"
        );
        Ok(())
    }
}

fn job(config: &Config, lease: &Lease, test_id: &str, remaining: u32) -> Result<Job> {
    config.approve(lease)?;
    ensure!(identifier(test_id), "invalid test ID");
    let id = lease.lease_token.simple().to_string();
    let name = format!("grade-{}-{}", &id[..16], test_id.to_lowercase());
    ensure!(
        name.len() <= 63 && !name.contains('_'),
        "test IDs used in jobs must be DNS-compatible"
    );
    let resources = &lease.revision.assignment.resources;
    let command = vec![
        "/bin/sh".to_string(),
        "-c".into(),
        "cp -R /source/. /workspace/ && cd /workspace && exec \"$@\" < /input/stdin".into(),
        "grading".into(),
        "/bin/python3".into(),
        "-c".into(),
        include_str!("../../../scripts/capture-execution.py").into(),
    ];
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

/// The private checker sees immutable source and public outcomes in a different Pod.
fn controller_base_job(config: &Config, lease: &Lease, remaining: u32) -> Result<Job> {
    config.approve(lease)?;
    let grader = lease
        .revision
        .grader
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("missing private checker"))?;
    let mut value = serde_json::to_value(job(config, lease, "private-check", remaining)?)?;
    let id = lease.lease_token.simple().to_string();
    value["metadata"]["name"] = json!(format!("check-{}", &id[..16]));
    let container = &mut value["spec"]["template"]["spec"]["containers"][0];
    container["image"] = json!(grader.image);
    container["command"] = json!(["/bin/grade"]);
    container["volumeMounts"] = json!([
        {"name":"source","mountPath":"/submission","subPath":format!("runs/{id}/source"),"readOnly":true},
        {"name":"source","mountPath":"/public","subPath":format!("runs/{id}/public"),"readOnly":true},
        {"name":"tmp","mountPath":"/tmp"}
    ]);
    Ok(serde_json::from_value(value)?)
}

/// The instructor controller can request commands but cannot select images or mount secrets.
pub fn controller_job(config: &Config, lease: &Lease, remaining: u32) -> Result<Job> {
    let workflow = lease
        .revision
        .grader
        .as_ref()
        .and_then(|g| g.workflow.as_ref())
        .ok_or_else(|| anyhow::anyhow!("missing script workflow"))?;
    let command = if lease.baseline.is_some() {
        workflow
            .private_command
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("missing private command"))?
    } else {
        &workflow.public_command
    };
    let mut value = serde_json::to_value(controller_base_job(config, lease, remaining)?)?;
    let id = lease.lease_token.simple().to_string();
    let pod = &mut value["spec"]["template"]["spec"];
    pod["securityContext"]["runAsUser"] = json!(10004);
    pod["securityContext"]["runAsGroup"] = json!(10004);
    pod["securityContext"]["fsGroup"] = json!(10004);
    pod["volumes"][0]["persistentVolumeClaim"]["readOnly"] = json!(false);
    let container = &mut pod["containers"][0];
    // Kubernetes combines stdout/stderr; keep diagnostics out of the score document.
    let mut wrapped = vec![
        "/bin/python3".to_owned(),
        "-c".into(),
        "import os,sys; os.dup2(os.open('/control/grader.stderr',os.O_WRONLY|os.O_CREAT|os.O_TRUNC,0o600),2); os.execv(sys.argv[1],sys.argv[1:])".into(),
    ];
    wrapped.extend(command.iter().cloned());
    container["command"] = json!(wrapped);
    container["resources"] = json!({"requests":{"cpu":"100m","memory":"128Mi","ephemeral-storage":"128Mi"},"limits":{"cpu":"1","memory":"512Mi","ephemeral-storage":"1Gi"}});
    container["volumeMounts"] = json!([
        {"name":"source","mountPath":"/submission","subPath":format!("runs/{id}/source"),"readOnly":true},
        {"name":"source","mountPath":"/grading","subPath":format!("runs/{id}/context"),"readOnly":true},
        {"name":"source","mountPath":"/platform","subPath":format!("runs/{id}/platform"),"readOnly":true},
        {"name":"source","mountPath":"/control","subPath":format!("runs/{id}/control")},
        {"name":"tmp","mountPath":"/tmp"}
    ]);
    if lease
        .revision
        .grader
        .as_ref()
        .is_some_and(|g| g.source_digest.is_some())
    {
        container["volumeMounts"].as_array_mut().unwrap().push(json!({
            "name":"source", "mountPath":"/grader", "subPath":format!("runs/{id}/grader"), "readOnly":true
        }));
        container["workingDir"] = json!("/grader");
    }
    Ok(serde_json::from_value(value)?)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionRequest {
    pub id: uuid::Uuid,
    pub command: Vec<String>,
    pub stdin: String,
}
impl ExecutionRequest {
    pub fn validate(&self) -> Result<()> {
        grading_core::protocol::validate_command(&self.command)?;
        ensure!(self.stdin.len() <= 65536, "execution input exceeds 64 KiB");
        Ok(())
    }
}

pub fn execution_job(
    config: &Config,
    lease: &Lease,
    request: &ExecutionRequest,
    remaining: u32,
) -> Result<Job> {
    request.validate()?;
    let id = request.id.simple().to_string();
    let mut value = serde_json::to_value(job(config, lease, "script", remaining)?)?;
    value["metadata"]["name"] = json!(format!("exec-{id}"));
    let container = &mut value["spec"]["template"]["spec"]["containers"][0];
    let mut command = vec![
        "/bin/python3".to_owned(),
        "-c".into(),
        include_str!("../../../scripts/capture-execution.py").into(),
        "/bin/sh".into(),
        "-c".into(),
        "cp -R /source/. /workspace/ && cd /workspace && exec \"$@\" < /input/stdin".into(),
        "grading".into(),
    ];
    command.extend(request.command.clone());
    container["command"] = json!(command);
    container["volumeMounts"][1]["subPath"] =
        json!(format!("runs/{}/requests/{id}", lease.lease_token.simple()));
    Ok(serde_json::from_value(value)?)
}
