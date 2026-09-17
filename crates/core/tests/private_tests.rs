//! Private execution outcomes must be bounded and cannot contain submission-controlled logs.
use grading_core::protocol::{PrivateSuite, PrivateTest, PrivateTestResult, TestSuite};
use std::{
    io::Write,
    process::{Command, Stdio},
};

fn run(program: &str, input: &str) -> (bool, String) {
    let mut child = Command::new("sh")
        .args(["-c", program])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();
    (
        output.status.success(),
        String::from_utf8(output.stdout).unwrap(),
    )
}

#[test]
fn private_inputs_detect_a_solution_that_passes_every_public_case() {
    let hardcoded = "input=$(cat); if [ -n \"$input\" ]; then printf 'Hello\\n'; fi";
    let public: TestSuite =
        toml::from_str(include_str!("../../../examples/template/tests/cases.toml")).unwrap();
    for test in &public.tests {
        let (success, output) = run(hardcoded, &test.stdin);
        assert!(success);
        assert_eq!(output, test.stdout);
    }
    let suite: PrivateSuite =
        toml::from_str(include_str!("../../../examples/grader/private-tests.toml")).unwrap();
    for test in &suite.tests {
        test.validate().unwrap();
        for (program, expected) in [("cat", true), (hardcoded, false)] {
            let (success, output) = run(program, &test.stdin);
            let outcome = test.outcome(success, &output);
            assert_eq!(outcome.passed, expected);
            let report = serde_json::to_value(outcome).unwrap();
            assert_eq!(report.as_object().unwrap().len(), 2);
            assert!(report.get("log").is_none());
        }
    }
}

#[test]
fn private_case_validation_and_forged_scores() {
    let mut case = PrivateTest {
        id: "extra".into(),
        stdin: "input".into(),
        stdout: "answer".into(),
    };
    assert!(!case.outcome(false, "answer").passed);
    assert!(!case.outcome(true, r#"{"passed":true,"points":20}"#).passed);
    assert!(!case.outcome(true, &"x".repeat(65537)).passed);
    case.id = "../public".into();
    assert!(case.validate().is_err());
    case.id = "extra".into();
    case.stdin = "x".repeat(65537);
    assert!(case.validate().is_err());
    assert!(
        serde_json::from_str::<PrivateTestResult>(
            r#"{"id":"extra","passed":true,"log":"private input"}"#
        )
        .is_err()
    );
}
