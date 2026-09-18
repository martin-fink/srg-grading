//! Configuration rejects mutable inputs, unknown keys, and unsupported policies.
use grading_core::config::CourseConfig;

#[test]
fn strict_schema_and_exact_environment() {
    let fixture = include_str!("../../../tests/fixtures/course.toml");
    CourseConfig::parse(fixture).unwrap();
    assert!(
        CourseConfig::parse(&format!("{fixture}\n[assignments.echo]\ntitle = 'old'\n")).is_err()
    );
    assert!(CourseConfig::parse(&format!("unknown = true\n{fixture}")).is_err());
    assert!(
        CourseConfig::parse(&fixture.replace("schema_version = 1", "schema_version = 2")).is_err()
    );
    let config: grading_core::config::Assignment =
        toml::from_str(include_str!("../../../tests/fixtures/assignment.toml")).unwrap();
    let mut changed = config.clone();
    changed.image = "example:latest".into();
    assert!(changed.validate().is_err());
    changed = config.clone();
    changed.template_revision = "main".into();
    assert!(changed.validate().is_err());
    changed = config.clone();
    changed.resources.cpu = 100;
    assert!(changed.validate().is_err());
    changed = config.clone();
    changed.deadline = changed.opens_at;
    assert!(changed.validate().is_err());
    changed = config;
    changed.execution_profile = "functional-v1".into();
    assert!(changed.validate().is_err());
}
