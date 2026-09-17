//! Configuration rejects mutable inputs, unknown keys, and unsupported policies.
use grading_core::config::CourseConfig;

#[test]
fn strict_schema_and_exact_environment() {
    let fixture = include_str!("../../../tests/fixtures/course.toml");
    let config = CourseConfig::parse(fixture).unwrap();
    assert!(CourseConfig::parse(&format!("unknown = true\n{fixture}")).is_err());
    assert!(
        CourseConfig::parse(
            &fixture.replace("latest-eligible-submission", "latest-commit-timestamp")
        )
        .is_err()
    );
    assert!(
        CourseConfig::parse(&fixture.replace("schema_version = 1", "schema_version = 2")).is_err()
    );
    let mut changed = config.clone();
    changed.assignments.get_mut("echo").unwrap().image = "example:latest".into();
    assert!(changed.validate().is_err());
    let mut changed = config.clone();
    changed
        .assignments
        .get_mut("echo")
        .unwrap()
        .template_revision = "main".into();
    assert!(changed.validate().is_err());
    let mut changed = config.clone();
    changed.assignments.get_mut("echo").unwrap().resources.cpu = 100;
    assert!(changed.validate().is_err());
    let mut changed = config;
    changed.assignments.get_mut("echo").unwrap().deadline = changed.assignments["echo"].opens_at;
    assert!(changed.validate().is_err());
}
