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
