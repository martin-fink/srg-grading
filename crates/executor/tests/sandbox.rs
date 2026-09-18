//! Shared-runner controller and student sandbox boundaries.
use chrono::Utc;
use grading_core::{
    config::CourseConfig,
    integrity::{Manifest, ProtectedFile},
    protocol::{Lease, Revision},
};
use grading_executor::Config;
use std::collections::BTreeMap;
use uuid::Uuid;

#[test]
fn script_controller_and_student_commands_have_separate_mounts() {
    use grading_core::protocol::{Grader, Workflow};
    use grading_executor::{ExecutionRequest, Registry, controller_job, execution_job};
    let course = CourseConfig::parse(include_str!("../../../tests/fixtures/course.toml")).unwrap();
    let mut assignment: grading_core::config::Assignment =
        toml::from_str(include_str!("../../../tests/fixtures/assignment.toml")).unwrap();
    assignment.execution_profile = "registered-v1".into();
    assignment.public_tests = "tests".into();
    assignment.image = format!("registry.example/grading/student@sha256:{}", "a".repeat(64));
    let revision = Revision {
        tests: Default::default(),
        course_id: course.course.id,
        assignment_id: "echo".into(),
        assignment: assignment.clone(),
        manifest: Manifest {
            schema_version: 1,
            template_revision: assignment.template_revision.clone(),
            editable: vec!["src/".into()],
            files: BTreeMap::from([(
                "tests/arbitrary.py".into(),
                ProtectedFile {
                    mode: "100644".into(),
                    sha256: "a".repeat(64),
                },
            )]),
        },
        grader: Some(Grader {
            caching: None,
            source_digest: Some("d".repeat(64)),
            repository: "org/private".into(),
            revision: "a".repeat(40),
            image: assignment.image.clone(),
            workflow: Some(Workflow {
                public_command: vec!["/bin/grade-public".into()],
                private_command: Some(vec!["/bin/grade-private".into()]),
            }),
        }),
    };
    let mut lease = Lease {
        schema_version: 1,
        task_id: Uuid::new_v4(),
        run_id: Uuid::new_v4(),
        lease_token: Uuid::new_v4(),
        expires_at: Utc::now(),
        sha: "a".repeat(40),
        revision_digest: revision.digest().unwrap(),
        source_digest: "b".repeat(64),
        revision,
        baseline: None,
    };
    let mut config = Config {
        api_url: "https://worker.example".into(),
        token_file: "/token".into(),
        tls_identity_file: None,
        tls_ca_file: None,
        namespace: "sandbox".into(),
        runtime_class: "gvisor".into(),
        source_pvc: "source".into(),
        staging_root: "/source".into(),
        registry: Some(Registry {
            runner_images: vec![assignment.image.clone()],
            image_prefix: "registry.example/grading".into(),
            resources: assignment.resources,
            timeout_seconds: 86400,
        }),
    };
    let example = include_str!("../../../docs/executor.toml.example");
    assert!(toml::from_str::<Config>(example).is_ok());
    assert!(
        toml::from_str::<Config>(&format!(
            "{example}\n[profiles.functional-v1]\ncommand = ['/bin/old']\n"
        ))
        .is_err()
    );
    let mut old = lease.clone();
    old.revision.grader.as_mut().unwrap().source_digest = None;
    old.revision_digest = old.revision.digest().unwrap();
    assert!(config.approve(&old).is_err());
    let mut old = serde_json::to_value(&lease.revision).unwrap();
    old["tests"] = serde_json::json!({"schema_version":1,"tests":[{"id":"old","points":20,"stdin":"","stdout":""}]});
    assert!(serde_json::from_value::<Revision>(old).is_err());
    let public = serde_json::to_value(controller_job(&config, &lease, 60).unwrap()).unwrap();
    assert_eq!(
        public["spec"]["template"]["spec"]["containers"][0]["command"][3],
        "/bin/grade-public"
    );
    lease.baseline = Some(grading_core::protocol::PublicBaseline {
        run_id: Uuid::new_v4(),
        points: 18,
        deadline: Utc::now() - chrono::Duration::hours(1),
    });
    let private = serde_json::to_value(controller_job(&config, &lease, 60).unwrap()).unwrap();
    assert_eq!(
        private["spec"]["template"]["spec"]["containers"][0]["command"][3],
        "/bin/grade-private"
    );
    let request = ExecutionRequest {
        id: Uuid::new_v4(),
        command: vec![
            "/bin/sh".into(),
            "-c".into(),
            "cc src/main.c -o program && ./program".into(),
        ],
        stdin: "extra input".into(),
        timeout_seconds: 10,
    };
    let student =
        serde_json::to_value(execution_job(&config, &lease, &request, 60).unwrap()).unwrap();
    let pod = &student["spec"]["template"]["spec"];
    assert_eq!(pod["automountServiceAccountToken"], false);
    assert_eq!(pod["securityContext"]["runAsUser"], 10003);
    assert!(
        pod["containers"][0]["volumeMounts"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["mountPath"] != "/grading")
    );
    for hidden in ["/control", "/platform", "extra input", "grading/grader"] {
        assert!(!student.to_string().contains(hidden));
    }
    assert!(student.to_string().contains("cc src/main.c"));
    assert_eq!(pod["volumes"][0]["persistentVolumeClaim"]["readOnly"], true);
    let controller = &private["spec"]["template"]["spec"]["containers"][0];
    assert_eq!(controller["image"], pod["containers"][0]["image"]);
    assert_eq!(controller["workingDir"], "/grader");
    assert!(
        controller["volumeMounts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["mountPath"] == "/grader" && m["readOnly"] == true)
    );
    assert!(!student.to_string().contains("/grader"));
    assert_eq!(
        pod["containers"][0]["env"][0]["name"],
        "GRADING_EXECUTION_TIMEOUT"
    );
    assert_eq!(pod["containers"][0]["env"][0]["value"], "10");
    assert_eq!(student["spec"]["activeDeadlineSeconds"], 15);
    assert_eq!(
        pod["containers"][0]["env"][1]["name"],
        "GRADING_SANDBOX_LIMITS"
    );
    assert_eq!(pod["containers"][0]["env"][1]["value"], "1");
    assert!(pod["containers"][0].get("envFrom").is_none());
    let cache = grading_core::caching::Caching {
        version: 1,
        recipe_dir: "cache".into(),
        command: vec!["/bin/sh".into(), "/recipe/prepare.sh".into()],
        timeout_seconds: 60,
        resources: config.registry.as_ref().unwrap().resources.clone(),
        architecture: "arm64".into(),
        artifacts: vec![grading_core::caching::Artifact {
            name: "compiler".into(),
            path: "ccache".into(),
            mount_path: "/cache/compiler".into(),
            mode: grading_core::caching::Mode::ReadOnly,
            max_size_gib: 1,
        }],
    };
    let prep = serde_json::to_value(
        grading_executor::caching::preparation_job(
            &config,
            &lease.revision,
            &cache,
            Uuid::new_v4(),
        )
        .unwrap(),
    )
    .unwrap();
    let prep_pod = &prep["spec"]["template"]["spec"];
    assert_eq!(prep_pod["automountServiceAccountToken"], false);
    assert_eq!(prep_pod["runtimeClassName"], "gvisor");
    assert_eq!(prep_pod["securityContext"]["runAsUser"], 10004);
    assert_eq!(prep_pod["nodeSelector"]["kubernetes.io/arch"], "arm64");
    assert!(!prep.to_string().contains("/grader"));
    let mounts = prep_pod["containers"][0]["volumeMounts"]
        .as_array()
        .unwrap();
    assert!(
        mounts
            .iter()
            .any(|m| m["mountPath"] == "/recipe" && m["readOnly"] == true)
    );
    assert!(
        mounts
            .iter()
            .any(|m| m["mountPath"] == "/source" && m["readOnly"] == true)
    );
    lease.revision.grader.as_mut().unwrap().caching = Some(grading_core::caching::Seed {
        config: cache,
        input_key: "e".repeat(64),
        digest: "f".repeat(64),
        namespace: config.namespace.clone(),
        source_pvc: config.source_pvc.clone(),
    });
    lease.revision_digest = lease.revision.digest().unwrap();
    let cached =
        serde_json::to_value(execution_job(&config, &lease, &request, 60).unwrap()).unwrap();
    let mounts = cached["spec"]["template"]["spec"]["containers"][0]["volumeMounts"]
        .as_array()
        .unwrap();
    assert!(
        mounts
            .iter()
            .any(|m| m["mountPath"] == "/cache/compiler" && m["readOnly"] == true)
    );
    assert!(
        !serde_json::to_string(&controller_job(&config, &lease, 60).unwrap())
            .unwrap()
            .contains("/cache/compiler")
    );
    lease
        .revision
        .grader
        .as_mut()
        .unwrap()
        .caching
        .as_mut()
        .unwrap()
        .config
        .artifacts[0]
        .mode = grading_core::caching::Mode::PrivateCopy;
    lease.revision_digest = lease.revision.digest().unwrap();
    assert!(execution_job(&config, &lease, &request, 60).is_err()); // No budget left for compilation.
    lease.revision.assignment.resources.storage_gib = 2;
    config.registry.as_mut().unwrap().resources.storage_gib = 2;
    lease.revision_digest = lease.revision.digest().unwrap();
    let private_copy =
        serde_json::to_value(execution_job(&config, &lease, &request, 60).unwrap()).unwrap();
    let pod = &private_copy["spec"]["template"]["spec"];
    assert!(
        pod["volumes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["name"] == "cache-0" && v["emptyDir"]["sizeLimit"] == "1Gi")
    );
    assert!(
        pod["containers"][0]["command"][5]
            .as_str()
            .unwrap()
            .contains("/cache-seeds/compiler /cache/compiler && ")
    );
    lease
        .revision
        .grader
        .as_mut()
        .unwrap()
        .caching
        .as_mut()
        .unwrap()
        .source_pvc = "other".into();
    lease.revision_digest = lease.revision.digest().unwrap();
    assert!(execution_job(&config, &lease, &request, 60).is_err());
    config.registry.as_mut().unwrap().runner_images.clear();
    assert!(controller_job(&config, &lease, 60).is_err());
    let mut invalid = request;
    invalid.command.clear();
    assert!(execution_job(&config, &lease, &invalid, 60).is_err());
}
