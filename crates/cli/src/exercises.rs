//! Pin source snapshots and publish exercises; legacy schemas retain image builds.
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use clap::{Args, Subcommand};
use grading_core::{
    config::{Assignment, Resources, ScorePolicy, identifier},
    integrity::{Manifest, Snapshot},
    protocol::{Grader, PrivateSuite, Revision, TestSuite, Workflow},
    security::valid_hex,
};
use grading_github::GitHub;
use grading_store::exercises::{self, Publication};
use serde::Deserialize;
use sqlx::PgPool;
use std::path::{Path, PathBuf};
use tokio::process::Command;

#[derive(Subcommand)]
pub enum ExerciseCommand {
    /// Apply the complete exercise list from one or more local TOML files.
    Apply(super::exercise_files::Apply),
    /// Register a new exercise from pinned GitHub sources.
    Add(Register),
    /// Run private grading on final submissions after their effective deadlines.
    PrivateGrade {
        #[arg(long)]
        course: String,
        #[arg(long)]
        name: String,
        #[arg(long)]
        reason: String,
    },
    /// Publish a new revision; existing student Git trees are never modified.
    Update(Register),
    /// Show the current immutable definition and repository hashes.
    Show {
        #[arg(long)]
        course: String,
        #[arg(long)]
        name: String,
    },
}

#[derive(Args)]
pub struct Register {
    #[arg(long)]
    pub course: String,
    #[arg(long)]
    pub name: String,
    #[arg(long)]
    pub template: Option<String>,
    #[arg(long)]
    pub grader: Option<String>,
    /// Branch or full commit SHA. On update, omitted refs retain their previous SHA.
    #[arg(long)]
    pub template_ref: Option<String>,
    #[arg(long)]
    pub grader_ref: Option<String>,
    #[arg(long)]
    pub opens_at: Option<DateTime<Utc>>,
    #[arg(long)]
    pub deadline: Option<DateTime<Utc>>,
    /// Apply the new grader to future runs for existing repositories too.
    #[arg(long)]
    pub existing: bool,
    #[arg(long)]
    pub reason: String,
    #[arg(long)]
    pub dry_run: bool,
    #[arg(long, env = "GRADING_BUILD_CONFIG")]
    pub build_config: Option<PathBuf>,
    /// Prebuilt shared runner, pinned by digest; retained on update if omitted.
    #[arg(long, env = "GRADING_RUNNER_IMAGE")]
    pub runner_image: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BuildConfig {
    registry_prefix: String,
    work_dir: PathBuf,
    registry_auth_file: Option<PathBuf>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Definition {
    schema_version: u32,
    title: String,
    branch: String,
    public_tests: String,
    private_tests: Option<String>,
    workflow: Option<Workflow>,
    editable: Vec<String>,
    max_points: i32,
    timeout_seconds: u32,
    resources: Resources,
}

pub fn repository(input: &str) -> Result<String> {
    let value = input
        .strip_prefix("https://github.com/")
        .unwrap_or(input)
        .trim_end_matches('/');
    let value = value.strip_suffix(".git").unwrap_or(value);
    ensure!(
        grading_core::config::github_repository(value),
        "expected a GitHub HTTPS URL or owner/repository"
    );
    Ok(value.into())
}

async fn resolve(github: &GitHub, repo: &str, reference: &str) -> Result<String> {
    if valid_hex(reference, 40) {
        Ok(reference.into())
    } else {
        github.branch_sha(repo, reference).await
    }
}
fn text_file(snapshot: &Snapshot, path: &str) -> Result<String> {
    String::from_utf8(
        snapshot
            .files
            .get(path)
            .with_context(|| format!("missing {path}"))?
            .bytes()?,
    )
    .map_err(Into::into)
}

pub struct Prepared {
    pub revision: Revision,
    pub expected: Option<String>,
    pub source: Option<Vec<u8>>,
}

pub async fn register(
    pool: &PgPool,
    github: &GitHub,
    args: &Register,
    update: bool,
    operator: &str,
    artifact_dir: &Path,
) -> Result<()> {
    let prepared = prepare(pool, github, args, update).await?;
    if args.dry_run {
        println!("Validated exercise; no artifacts or database records changed");
        return Ok(());
    }
    if let Some(source) = &prepared.source {
        grading_store::artifacts::Artifacts::new(artifact_dir)
            .await?
            .put(pool, "source", source)
            .await?;
    }
    exercises::publish(
        pool,
        Publication {
            revision: &prepared.revision,
            expected: prepared.expected.as_deref(),
            existing: args.existing,
            dry_run: false,
            operator,
            reason: &args.reason,
        },
    )
    .await?;
    println!(
        "Published {}/{} revision {}",
        args.course,
        args.name,
        prepared.revision.digest()?
    );
    Ok(())
}

pub async fn prepare(
    pool: &PgPool,
    github: &GitHub,
    args: &Register,
    update: bool,
) -> Result<Prepared> {
    ensure!(
        identifier(&args.course) && identifier(&args.name),
        "invalid course/exercise name"
    );
    ensure!(!args.reason.trim().is_empty(), "--reason must not be empty");
    let course_exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM courses WHERE id=$1)")
            .bind(&args.course)
            .fetch_one(pool)
            .await?;
    ensure!(course_exists, "import the course before adding exercises");
    let previous = exercises::current(pool, &args.course, &args.name).await?;
    ensure!(
        previous.is_some() == update,
        "use add for a new exercise, update for an existing one"
    );
    let old_grader = previous.as_ref().and_then(|r| r.grader.as_ref());
    let template = repository(
        args.template
            .as_deref()
            .or(previous.as_ref().map(|r| r.assignment.template.as_str()))
            .context("--template is required")?,
    )?;
    let grader = repository(
        args.grader
            .as_deref()
            .or(old_grader.map(|g| g.repository.as_str()))
            .context("--grader is required")?,
    )?;
    if let Some(old) = &previous {
        ensure!(
            args.template.is_none()
                || template == old.assignment.template
                || args.template_ref.is_some(),
            "changing template repository requires --template-ref"
        );
        ensure!(
            args.grader.is_none()
                || old_grader.is_some_and(|g| grader == g.repository)
                || args.grader_ref.is_some(),
            "changing grader repository requires --grader-ref"
        );
    }
    let template_ref = args
        .template_ref
        .as_deref()
        .or(previous
            .as_ref()
            .map(|r| r.assignment.template_revision.as_str()))
        .unwrap_or("main");
    let grader_ref = args
        .grader_ref
        .as_deref()
        .or(old_grader.map(|g| g.revision.as_str()))
        .unwrap_or("main");
    let template_sha = resolve(github, &template, template_ref).await?;
    let grader_sha = resolve(github, &grader, grader_ref).await?;
    let template_source = github.snapshot(&template, &template_sha).await?;
    let grader_source = github.snapshot(&grader, &grader_sha).await?;
    template_source.validate()?;
    grader_source.validate()?;
    let definition: Definition = toml::from_str(&text_file(&grader_source, "exercise.toml")?)?;
    ensure!(
        matches!(definition.schema_version, 1..=3),
        "unsupported exercise schema"
    );
    let shared = definition.schema_version == 3;
    if !shared {
        ensure!(
            args.runner_image.is_none(),
            "{grader}@{grader_sha}: exercise.toml declares schema {}; --runner-image requires schema 3 in the GRADER repository (not the local exercise catalog). Use examples/shared-grader, including its scripts and /grader workflow commands, then commit/push it and select that commit with grader_ref. Changing only the schema number is insufficient for legacy image commands.",
            definition.schema_version
        );
        text_file(&grader_source, "flake.nix")?;
        text_file(&grader_source, "flake.lock")?;
    }
    let scripted = definition.schema_version >= 2;
    ensure!(
        scripted == definition.workflow.is_some(),
        "schemas 2 and 3 require a workflow; schema 1 uses fixed cases"
    );
    let private_tests = if let Some(path) = &definition.private_tests {
        if scripted {
            grading_core::integrity::safe_path(path)?;
            ensure!(
                grader_source
                    .files
                    .keys()
                    .any(|p| p == path || p.starts_with(&format!("{path}/"))),
                "private test path is missing"
            );
            vec![]
        } else {
            grading_core::integrity::safe_path(path)?;
            let suite: PrivateSuite = toml::from_str(&text_file(&grader_source, path)?)?;
            ensure!(suite.schema_version == 1, "unsupported private test suite");
            suite.tests
        }
    } else {
        vec![]
    };
    let manifest = Manifest::generate(&template_source, definition.editable)?;
    let tests: TestSuite = if scripted {
        TestSuite {
            schema_version: 1,
            tests: vec![],
        }
    } else {
        toml::from_str(&text_file(&template_source, &definition.public_tests)?)?
    };
    let opens_at = args
        .opens_at
        .or(previous.as_ref().map(|r| r.assignment.opens_at))
        .context("--opens-at is required")?;
    let deadline = args
        .deadline
        .or(previous.as_ref().map(|r| r.assignment.deadline))
        .context("--deadline is required")?;
    let config: Option<BuildConfig> = if shared {
        None
    } else {
        Some(toml::from_str(
            &tokio::fs::read_to_string(
                args.build_config
                    .as_ref()
                    .context("legacy exercise requires --build-config")?,
            )
            .await?,
        )?)
    };
    let prefix = config
        .as_ref()
        .map(|c| c.registry_prefix.trim_end_matches('/'))
        .unwrap_or("");
    if !shared {
        ensure!(
            prefix.contains('/')
                && prefix
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"./:_-".contains(&b))
                && !prefix.contains('@'),
            "invalid registry prefix"
        );
    }
    let runner = if shared {
        Some(
            args.runner_image
                .as_deref()
                .or(old_grader
                    .filter(|g| g.source_digest.is_some())
                    .map(|g| g.image.as_str()))
                .context("--runner-image is required for the first shared-runner registration")?,
        )
    } else {
        None
    };
    let grader_bytes = serde_json::to_vec(&grader_source)?;
    let mut revision = Revision {
        course_id: args.course.clone(),
        assignment_id: args.name.clone(),
        manifest,
        tests,
        assignment: Assignment {
            title: definition.title,
            opens_at,
            deadline,
            branch: definition.branch,
            template,
            template_revision: template_sha.clone(),
            image: runner
                .map(str::to_owned)
                .unwrap_or_else(|| format!("{prefix}/student@sha256:{}", "0".repeat(64))),
            integrity_manifest: "integrity.toml".into(),
            public_tests: definition.public_tests,
            execution_profile: "registered-v1".into(),
            max_points: definition.max_points,
            timeout_seconds: definition.timeout_seconds,
            resources: definition.resources,
            score: ScorePolicy {
                source: "public-tests".into(),
                selection: "latest-eligible-submission".into(),
            },
        },
        grader: Some(Grader {
            source_digest: shared.then(|| grading_core::security::digest(&grader_bytes)),
            workflow: definition.workflow,
            tests: private_tests,
            repository: grader,
            revision: grader_sha.clone(),
            image: runner
                .map(str::to_owned)
                .unwrap_or_else(|| format!("{prefix}/grader@sha256:{}", "0".repeat(64))),
        }),
    };
    revision.validate()?;
    println!(
        "template {template_sha}; grader {grader_sha}; existing repositories use new grader: {}",
        args.existing
    );
    let expected = previous.as_ref().map(Revision::digest).transpose()?;
    if shared || args.dry_run {
        return Ok(Prepared {
            revision,
            expected,
            source: shared.then_some(grader_bytes),
        });
    }
    let config = config
        .as_ref()
        .context("missing legacy builder configuration")?;
    tokio::fs::create_dir_all(&config.work_dir).await?;
    let root = config.work_dir.join(uuid::Uuid::new_v4().to_string());
    tokio::fs::create_dir(&root).await?;
    #[cfg(unix)]
    tokio::fs::set_permissions(&root, std::os::unix::fs::PermissionsExt::from_mode(0o700)).await?;
    let source = root.join("source");
    tokio::fs::create_dir(&source).await?;
    for (path, blob) in &grader_source.files {
        let dest = source.join(path);
        tokio::fs::create_dir_all(dest.parent().context("source parent")?).await?;
        super::write_new(&dest, &blob.bytes()?).await?;
        #[cfg(unix)]
        tokio::fs::set_permissions(
            &dest,
            std::os::unix::fs::PermissionsExt::from_mode(if blob.mode == "100755" {
                0o755
            } else {
                0o644
            }),
        )
        .await?;
    }
    let source = tokio::fs::canonicalize(source).await?;
    let tag = uuid::Uuid::new_v4().simple().to_string();
    revision.assignment.image = build_image(
        config,
        &source,
        &root,
        "studentImage",
        &format!(
            "{prefix}/{}/{}/student:{tag}",
            args.course.to_ascii_lowercase(),
            args.name.to_ascii_lowercase()
        ),
    )
    .await?;
    revision.grader.as_mut().unwrap().image = build_image(
        config,
        &source,
        &root,
        "graderImage",
        &format!(
            "{prefix}/{}/{}/grader:{tag}",
            args.course.to_ascii_lowercase(),
            args.name.to_ascii_lowercase()
        ),
    )
    .await?;
    println!("Build records retained in {}", root.display());
    Ok(Prepared {
        revision,
        expected,
        source: None,
    })
}

async fn build_image(
    config: &BuildConfig,
    source: &Path,
    root: &Path,
    output: &str,
    destination: &str,
) -> Result<String> {
    let result = Command::new("nix")
        .args([
            "build",
            "--no-link",
            "--json",
            "--no-update-lock-file",
            "--no-write-lock-file",
            "--no-accept-flake-config",
            "--option",
            "sandbox",
            "true",
        ])
        .arg(format!("path:{}#{output}", source.display()))
        .kill_on_drop(true)
        .output()
        .await
        .context("run this command on the dedicated Nix builder")?;
    super::write_new(&root.join(format!("{output}.log")), &result.stderr).await?;
    ensure!(
        result.status.success(),
        "Nix build failed; see private build log"
    );
    let values: serde_json::Value = serde_json::from_slice(&result.stdout)?;
    let archive = values[0]["outputs"]["out"]
        .as_str()
        .context("Nix did not return an image archive")?;
    ensure!(archive.starts_with("/nix/store/"), "invalid build output");
    let digest_file = root.join(format!("{output}.digest"));
    let mut copy = Command::new("skopeo");
    copy.args(["copy", "--digestfile"]).arg(&digest_file);
    if let Some(auth) = &config.registry_auth_file {
        copy.arg("--authfile").arg(auth);
    }
    let result = copy
        .arg(format!("docker-archive:{archive}"))
        .arg(format!("docker://{destination}"))
        .kill_on_drop(true)
        .output()
        .await
        .context("skopeo is required to publish images")?;
    super::write_new(&root.join(format!("{output}-publish.log")), &result.stderr).await?;
    ensure!(
        result.status.success(),
        "image publish failed; see private build log"
    );
    let digest = tokio::fs::read_to_string(digest_file).await?;
    let digest = digest.trim();
    ensure!(
        digest
            .strip_prefix("sha256:")
            .is_some_and(|s| valid_hex(s, 64)),
        "registry returned invalid digest"
    );
    let repository = destination.rsplit_once(':').context("image tag")?.0;
    Ok(format!("{repository}@{digest}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn github_sources_and_explicit_rollouts() {
        assert_eq!(
            repository("https://github.com/course/template.git").unwrap(),
            "course/template"
        );
        assert_eq!(repository("course/private").unwrap(), "course/private");
        assert_eq!(
            repository("https://github.com/course/exercise.v2").unwrap(),
            "course/exercise.v2"
        );
        for invalid in [
            "https://elsewhere.example/course/template",
            "file:///tmp/source",
            "https://github.com/course/template?ref=main",
            "course/../private",
            "-bad",
        ] {
            assert!(repository(invalid).is_err());
        }
        let parsed = crate::Args::try_parse_from([
            "gradingctl",
            "exercise",
            "update",
            "--course",
            "course",
            "--name",
            "echo",
            "--grader-ref",
            "main",
            "--existing",
            "--reason",
            "Fix tests",
            "--build-config",
            "/run/build.toml",
        ])
        .unwrap();
        let crate::Command::Exercise {
            command: ExerciseCommand::Update(options),
        } = parsed.command
        else {
            panic!("wrong command")
        };
        assert!(options.existing);
        assert_eq!(options.grader_ref.as_deref(), Some("main"));
        assert!(options.template_ref.is_none());
    }

    #[test]
    fn shared_runner_registration_needs_no_builder_configuration() {
        let definition: Definition = toml::from_str(include_str!(
            "../../../examples/shared-grader/exercise.toml"
        ))
        .unwrap();
        assert_eq!(definition.schema_version, 3);
        assert_eq!(
            definition.workflow.unwrap().public_command,
            ["/bin/python3", "/grader/public.py"]
        );
        let image = format!("registry.example/grading/runner@sha256:{}", "a".repeat(64));
        let parsed = crate::Args::try_parse_from([
            "gradingctl",
            "exercise",
            "add",
            "--course",
            "systems",
            "--name",
            "echo",
            "--template",
            "org/template",
            "--grader",
            "org/private",
            "--runner-image",
            &image,
            "--reason",
            "Initial registration",
            "--dry-run",
        ])
        .unwrap();
        let crate::Command::Exercise {
            command: ExerciseCommand::Add(options),
        } = parsed.command
        else {
            panic!("wrong command");
        };
        assert_eq!(options.runner_image.as_deref(), Some(image.as_str()));
    }
}
