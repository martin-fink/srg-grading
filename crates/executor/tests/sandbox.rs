//! Sandbox isolation and the independent image/profile approval boundary.
use chrono::Utc;
use grading_core::{
    config::CourseConfig,
    integrity::{Manifest, ProtectedFile},
    protocol::{Lease, PublicTest, Revision, TestSuite},
};
use grading_executor::{Config, Profile, job};
use std::collections::BTreeMap;
use uuid::Uuid;

#[test]
fn jobs_are_restricted_and_only_approved_profiles_execute() {
    let course = CourseConfig::parse(include_str!("../../../tests/fixtures/course.toml")).unwrap();
    let assignment = course.assignments["echo"].clone();
    let revision = Revision {
        grader: None,
        course_id: course.course.id.clone(),
        assignment_id: "echo".into(),
        manifest: Manifest {
            schema_version: 1,
            template_revision: assignment.template_revision.clone(),
            editable: vec!["src/".into()],
            files: BTreeMap::from([(
                "tests/cases.toml".into(),
                ProtectedFile {
                    mode: "100644".into(),
                    sha256: "a".repeat(64),
                },
            )]),
        },
        assignment: assignment.clone(),
        tests: TestSuite {
            schema_version: 1,
            tests: vec![PublicTest {
                id: "test".into(),
                points: 20,
                stdin: "hello".into(),
                stdout: "hello".into(),
            }],
        },
    };
    let lease = Lease {
        baseline: None,
        schema_version: 1,
        task_id: Uuid::new_v4(),
        run_id: Uuid::new_v4(),
        lease_token: Uuid::new_v4(),
        expires_at: Utc::now(),
        sha: "a".repeat(40),
        revision_digest: revision.digest().unwrap(),
        source_digest: "a".repeat(64),
        revision,
    };
    let mut config = Config {
        registry: None,
        api_url: "https://worker.example".into(),
        token_file: "/secret".into(),
        tls_identity_file: None,
        tls_ca_file: None,
        namespace: "grading-sandboxes".into(),
        runtime_class: "gvisor".into(),
        source_pvc: "grading-source".into(),
        staging_root: "/source".into(),
        profiles: BTreeMap::from([(
            "functional-v1".into(),
            Profile {
                images: vec![assignment.image],
                command: vec!["/opt/instructor/run".into()],
                resources: assignment.resources,
                timeout_seconds: 60,
            },
        )]),
    };
    let definition = serde_json::to_value(job(&config, &lease, "test", 60).unwrap()).unwrap();
    let pod = &definition["spec"]["template"]["spec"];
    assert_eq!(pod["automountServiceAccountToken"], false);
    assert_eq!(pod["runtimeClassName"], "gvisor");
    assert_eq!(pod["securityContext"]["runAsNonRoot"], true);
    let container = &pod["containers"][0];
    assert_eq!(container["securityContext"]["readOnlyRootFilesystem"], true);
    assert_eq!(
        container["securityContext"]["allowPrivilegeEscalation"],
        false
    );
    assert_eq!(
        container["securityContext"]["capabilities"]["drop"][0],
        "ALL"
    );
    assert!(container.get("env").is_none());
    assert!(pod.get("hostNetwork").is_none());
    assert!(
        pod["volumes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|v| v.get("hostPath").is_none() && v.get("secret").is_none())
    );
    assert_eq!(pod["volumes"][0]["persistentVolumeClaim"]["readOnly"], true);
    assert_eq!(definition["spec"]["backoffLimit"], 0);
    config
        .profiles
        .get_mut("functional-v1")
        .unwrap()
        .images
        .clear();
    assert!(job(&config, &lease, "test", 60).is_err());
    config
        .profiles
        .get_mut("functional-v1")
        .unwrap()
        .images
        .push(lease.revision.assignment.image.clone());
    config
        .profiles
        .get_mut("functional-v1")
        .unwrap()
        .resources
        .cpu = 0;
    assert!(job(&config, &lease, "test", 60).is_err());
}

#[test]
fn registered_exercises_have_separate_private_checker_and_bounded_decisions() {
    use grading_core::protocol::{
        Grader, PrivateDecision, PrivateTest, RunResult, RunStatus, TestResult,
    };
    use grading_executor::{Registry, private_job, private_test_job};
    let course = CourseConfig::parse(include_str!("../../../tests/fixtures/course.toml")).unwrap();
    let mut assignment = course.assignments["echo"].clone();
    assignment.execution_profile = "registered-v1".into();
    assignment.image = format!("registry.example/grading/student@sha256:{}", "a".repeat(64));
    let revision = Revision {
        course_id: course.course.id,
        assignment_id: "echo".into(),
        manifest: Manifest {
            schema_version: 1,
            template_revision: assignment.template_revision.clone(),
            editable: vec!["src/".into()],
            files: BTreeMap::from([(
                "tests/cases.toml".into(),
                ProtectedFile {
                    mode: "100644".into(),
                    sha256: "a".repeat(64),
                },
            )]),
        },
        assignment: assignment.clone(),
        tests: TestSuite {
            schema_version: 1,
            tests: vec![PublicTest {
                id: "one".into(),
                points: 20,
                stdin: "".into(),
                stdout: "".into(),
            }],
        },
        grader: Some(Grader {
            source_digest: None,
            workflow: None,
            tests: vec![PrivateTest {
                id: "one".into(),
                stdin: "private-input-marker".into(),
                stdout: "private-answer-marker".into(),
            }],
            repository: "org/private".into(),
            revision: "b".repeat(40),
            image: format!("registry.example/grading/checker@sha256:{}", "c".repeat(64)),
        }),
    };
    let lease = Lease {
        baseline: Some(grading_core::protocol::PublicBaseline {
            run_id: Uuid::new_v4(),
            points: 20,
            deadline: Utc::now() - chrono::Duration::hours(1),
        }),
        schema_version: 1,
        task_id: Uuid::new_v4(),
        run_id: Uuid::new_v4(),
        lease_token: Uuid::new_v4(),
        expires_at: Utc::now(),
        sha: "d".repeat(40),
        revision_digest: revision.digest().unwrap(),
        source_digest: "e".repeat(64),
        revision,
    };
    let mut config = Config {
        api_url: "https://worker.example".into(),
        token_file: "/secret".into(),
        tls_identity_file: None,
        tls_ca_file: None,
        namespace: "sandboxes".into(),
        runtime_class: "gvisor".into(),
        source_pvc: "source".into(),
        staging_root: "/source".into(),
        profiles: BTreeMap::new(),
        registry: Some(Registry {
            runner_images: vec![],
            image_prefix: "registry.example/grading".into(),
            resources: assignment.resources.clone(),
            timeout_seconds: 86400,
        }),
    };
    assert_eq!(config.profile_names(), vec!["registered-v1"]);
    let student = serde_json::to_value(job(&config, &lease, "one", 60).unwrap()).unwrap();
    let checker = serde_json::to_value(private_job(&config, &lease, 60).unwrap()).unwrap();
    let pod = &checker["spec"]["template"]["spec"];
    assert_eq!(pod["automountServiceAccountToken"], false);
    assert_eq!(
        pod["containers"][0]["command"],
        serde_json::json!(["/bin/grade"])
    );
    assert_ne!(
        pod["containers"][0]["image"],
        student["spec"]["template"]["spec"]["containers"][0]["image"]
    );
    assert_eq!(
        pod["containers"][0]["volumeMounts"][0]["mountPath"],
        "/submission"
    );
    assert_eq!(pod["containers"][0]["volumeMounts"][0]["readOnly"], true);
    assert!(student.to_string().find("/public").is_none());
    assert!(!student.to_string().contains("/bin/grade"));
    let probe =
        serde_json::to_value(private_test_job(&config, &lease, "one", 60).unwrap()).unwrap();
    let probe_pod = &probe["spec"]["template"]["spec"];
    assert_eq!(
        probe_pod["containers"][0]["image"],
        student["spec"]["template"]["spec"]["containers"][0]["image"]
    );
    assert_eq!(probe_pod["automountServiceAccountToken"], false);
    assert_eq!(
        probe_pod["containers"][0]["securityContext"]["readOnlyRootFilesystem"],
        true
    );
    assert_ne!(probe["metadata"]["name"], student["metadata"]["name"]);
    assert!(
        probe_pod["containers"][0]["volumeMounts"][1]["subPath"]
            .as_str()
            .unwrap()
            .ends_with("/private-inputs/one")
    );
    for secret in [
        "private-input-marker",
        "private-answer-marker",
        "/public",
        "/bin/grade",
        "registry.example/grading/checker",
    ] {
        assert!(!probe.to_string().contains(secret));
    }
    assert!(private_test_job(&config, &lease, "unregistered", 60).is_err());

    let mut result = RunResult {
        logs: vec![],
        score: None,
        private_tests: vec![
            lease.revision.grader.as_ref().unwrap().tests[0].outcome(true, "private-answer-marker"),
        ],
        schema_version: 1,
        lease_token: lease.lease_token,
        run_id: lease.run_id,
        sha: lease.sha.clone(),
        revision_digest: lease.revision_digest.clone(),
        image: assignment.image,
        resources: assignment.resources,
        status: RunStatus::Completed,
        tests: vec![TestResult {
            id: "one".into(),
            passed: true,
            log: "".into(),
        }],
        findings: vec![],
        private: Some(PrivateDecision {
            schema_version: 1,
            adjustment: -2,
            invalidated: false,
            reason: "Required source convention missing".into(),
        }),
    };
    assert_eq!(result.validate(&lease).unwrap(), Some(18));
    let mut leaked = result.clone();
    leaked.logs.push(grading_core::protocol::RunLog {
        student_visible: true,
        text: "private output".into(),
    });
    assert!(leaked.validate(&lease).is_err());
    let mut oversized = result.clone();
    oversized.logs.push(grading_core::protocol::RunLog {
        student_visible: false,
        text: "x".repeat(grading_core::protocol::MAX_RUN_LOG_BYTES + 1),
    });
    assert!(oversized.validate(&lease).is_err());
    let mut omitted = result.clone();
    omitted.private_tests.clear();
    assert!(omitted.validate(&lease).is_err());
    let mut unknown = result.clone();
    unknown.private_tests[0].id = "unregistered".into();
    assert!(unknown.validate(&lease).is_err());
    let mut duplicate = result.clone();
    duplicate
        .private_tests
        .push(duplicate.private_tests[0].clone());
    assert!(duplicate.validate(&lease).is_err());
    let report = serde_json::to_string(&result).unwrap();
    assert!(!report.contains("private-input-marker"));
    assert!(!report.contains("private-answer-marker"));

    result.private.as_mut().unwrap().reason.clear();
    assert!(result.validate(&lease).is_err());
    result.private.as_mut().unwrap().reason = "Reviewed check finding".into();
    result.private.as_mut().unwrap().adjustment = i32::MAX;
    assert!(result.validate(&lease).is_err());
    result.private.as_mut().unwrap().adjustment = -21;
    assert!(result.validate(&lease).is_err());
    result.private.as_mut().unwrap().adjustment = 0;
    result.private.as_mut().unwrap().invalidated = true;
    assert!(result.validate(&lease).is_err());
    result.status = RunStatus::Invalidated;
    assert_eq!(result.validate(&lease).unwrap(), None);
    result.private = None;
    assert!(result.validate(&lease).is_err());
    config.registry.as_mut().unwrap().image_prefix = "registry.example/grading-other".into();
    assert!(config.approve(&lease).is_err());
    config.registry.as_mut().unwrap().image_prefix = "registry.example/grading".into();
    config.registry.as_mut().unwrap().resources.cpu = 0;
    assert!(config.approve(&lease).is_err());
}

#[test]
fn script_controller_and_student_commands_have_separate_mounts() {
    use grading_core::protocol::{Grader, Workflow};
    use grading_executor::{ExecutionRequest, Registry, controller_job, execution_job};
    let course = CourseConfig::parse(include_str!("../../../tests/fixtures/course.toml")).unwrap();
    let mut assignment = course.assignments["echo"].clone();
    assignment.execution_profile = "registered-v1".into();
    assignment.public_tests = "tests".into();
    assignment.image = format!("registry.example/grading/student@sha256:{}", "a".repeat(64));
    let revision = Revision {
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
        tests: TestSuite {
            schema_version: 1,
            tests: vec![],
        },
        grader: Some(Grader {
            source_digest: Some("d".repeat(64)),
            repository: "org/private".into(),
            revision: "a".repeat(40),
            image: assignment.image.clone(),
            tests: vec![],
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
        profiles: BTreeMap::new(),
        registry: Some(Registry {
            runner_images: vec![assignment.image.clone()],
            image_prefix: "registry.example/grading".into(),
            resources: assignment.resources,
            timeout_seconds: 86400,
        }),
    };
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
    assert!(pod["containers"][0].get("env").is_none());
    assert!(pod["containers"][0].get("envFrom").is_none());
    config.registry.as_mut().unwrap().runner_images.clear();
    assert!(controller_job(&config, &lease, 60).is_err());
    let mut invalid = request;
    invalid.command.clear();
    assert!(execution_job(&config, &lease, &invalid, 60).is_err());
}
