//! Pin source snapshots and publish shared-runner exercises.
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use clap::{Args, Subcommand};
use grading_core::{
    config::{Assignment, Resources, ScorePolicy, identifier},
    integrity::{Manifest, Snapshot},
    protocol::{Grader, Revision, Workflow},
    security::valid_hex,
};
use grading_github::GitHub;
use grading_store::exercises::{self, Publication};
use serde::Deserialize;
use sqlx::PgPool;
use std::path::{Path, PathBuf};

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

#[derive(Args, Clone, serde::Serialize, Deserialize)]
pub struct Register {
    /// Executor configuration and shared staging PVC, required to build cache seeds.
    #[arg(long, env = "GRADING_CACHE_CONFIG")]
    pub cache_config: Option<PathBuf>,
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
    /// Prebuilt shared runner, pinned by digest; retained on update if omitted.
    #[arg(long, env = "GRADING_RUNNER_IMAGE")]
    pub runner_image: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Definition {
    caching: Option<grading_core::caching::Caching>,
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

impl Definition {
    fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == 3,
            "only exercise schema 3 is supported"
        );
        if let Some(caching) = &self.caching {
            caching.validate()?;
            let copies: u32 = caching
                .artifacts
                .iter()
                .filter(|a| a.mode == grading_core::caching::Mode::PrivateCopy)
                .map(|a| a.max_size_gib)
                .sum();
            ensure!(
                copies < self.resources.storage_gib,
                "private cache copies need additional grading storage"
            );
        }
        self.workflow
            .as_ref()
            .context("schema 3 requires a workflow")?
            .validate()
    }
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
    pub cache: Option<CacheInput>,
    pub revision: Revision,
    pub expected: Option<String>,
    pub source: Option<Vec<u8>>,
}

pub struct CacheInput {
    config: grading_core::caching::Caching,
    source: Snapshot,
    recipe: Snapshot,
}
impl Prepared {
    pub fn cache_key(&self) -> Result<Option<String>> {
        self.cache
            .as_ref()
            .map(|input| {
                input.config.key(
                    &self.revision.assignment.image,
                    &input.source,
                    &input.recipe,
                )
            })
            .transpose()
    }
    pub async fn prepare_cache(&mut self, path: Option<&Path>, dry_run: bool) -> Result<()> {
        let Some(input) = &self.cache else {
            return Ok(());
        };
        let key = self.cache_key()?.context("missing cache input")?;
        if dry_run && path.is_none() {
            println!(
                "Cache {key}: preparation/reuse requires --cache-config (not checked in this dry run)"
            );
            return Ok(());
        }
        let path = path.context("exercise caching requires --cache-config or GRADING_CACHE_CONFIG, with access to the executor staging PVC and Kubernetes")?;
        let config: grading_executor::Config =
            toml::from_str(&tokio::fs::read_to_string(path).await?)?;
        grading_executor::caching::preparation_job(
            &config,
            &self.revision,
            &input.config,
            uuid::Uuid::new_v4(),
        )?;
        if dry_run {
            let reference = config.staging_root.join("cache-refs").join(&key);
            if reference.exists() {
                let seed: grading_core::caching::Seed =
                    serde_json::from_slice(&tokio::fs::read(reference).await?)?;
                ensure!(seed.input_key == key, "cache reference mismatch");
                grading_executor::caching::verify_async(&config, &seed).await?;
                println!("Cache {key}: reuse {}", seed.digest);
                self.revision
                    .grader
                    .as_mut()
                    .context("missing grader")?
                    .caching = Some(seed);
            } else {
                println!("Cache {key}: preparation required");
            }
        } else {
            let seed = grading_executor::caching::prepare(
                &config,
                &self.revision,
                &input.config,
                &input.source,
                &input.recipe,
            )
            .await?;
            self.revision
                .grader
                .as_mut()
                .context("missing grader")?
                .caching = Some(seed);
        }
        self.revision.validate()?;
        Ok(())
    }
}

pub async fn register(
    pool: &PgPool,
    github: &GitHub,
    args: &Register,
    update: bool,
    operator: &str,
    artifact_dir: &Path,
) -> Result<()> {
    let mut prepared = prepare(pool, github, args, update).await?;
    prepared
        .prepare_cache(args.cache_config.as_deref(), args.dry_run)
        .await?;
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
    definition.validate()?;
    if let Some(path) = &definition.private_tests {
        grading_core::integrity::safe_path(path)?;
        ensure!(
            grader_source
                .files
                .keys()
                .any(|p| p == path || p.starts_with(&format!("{path}/"))),
            "private test path is missing"
        );
    }
    let cache = definition
        .caching
        .as_ref()
        .map(|config| {
            Ok::<_, anyhow::Error>(CacheInput {
                config: config.clone(),
                source: template_source.clone(),
                recipe: config.recipe(&grader_source)?,
            })
        })
        .transpose()?;
    let manifest = Manifest::generate(&template_source, definition.editable)?;
    let opens_at = args
        .opens_at
        .or(previous.as_ref().map(|r| r.assignment.opens_at))
        .context("--opens-at is required")?;
    let deadline = args
        .deadline
        .or(previous.as_ref().map(|r| r.assignment.deadline))
        .context("--deadline is required")?;
    let runner = args
        .runner_image
        .as_deref()
        .or(old_grader.map(|g| g.image.as_str()))
        .context("--runner-image is required for the first shared-runner registration")?;
    let grader_bytes = serde_json::to_vec(&grader_source)?;
    let revision = Revision {
        course_id: args.course.clone(),
        assignment_id: args.name.clone(),
        manifest,
        tests: Default::default(),
        assignment: Assignment {
            title: definition.title,
            opens_at,
            deadline,
            branch: definition.branch,
            template,
            template_revision: template_sha.clone(),
            image: runner.to_owned(),
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
            caching: None,
            source_digest: Some(grading_core::security::digest(&grader_bytes)),
            workflow: definition.workflow,
            repository: grader,
            revision: grader_sha.clone(),
            image: runner.to_owned(),
        }),
    };
    revision.validate()?;
    println!(
        "template {template_sha}; grader {grader_sha}; existing repositories use new grader: {}",
        args.existing
    );
    let expected = previous.as_ref().map(Revision::digest).transpose()?;
    Ok(Prepared {
        cache,
        revision,
        expected,
        source: Some(grader_bytes),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn optional_cache_keeps_schema_three_and_validates_the_recipe() {
        let example = include_str!("../../../examples/shared-grader/exercise.toml");
        let cached: Definition = toml::from_str(example).unwrap();
        cached.validate().unwrap();
        assert_eq!(
            cached.caching.as_ref().unwrap().artifacts[0].mount_path,
            "/cache/launcher"
        );
        let example = example.split("\n[caching]").next().unwrap();
        let uncached: Definition = toml::from_str(example).unwrap();
        uncached.validate().unwrap();
        assert!(uncached.caching.is_none());
        let cache = r#"
[caching]
version=1
recipe_dir="cache"
command=["/bin/bash","/recipe/prepare.sh"]
timeout_seconds=14400
[caching.resources]
cpu=8
memory_gib=32
storage_gib=100
[[caching.artifacts]]
name="compiler-cache"
path="ccache"
mount_path="/cache/compiler"
mode="read_only"
max_size_gib=30
"#;
        let definition: Definition = toml::from_str(&format!("{example}\n{cache}")).unwrap();
        definition.validate().unwrap();
        assert_eq!(definition.schema_version, 3);
        assert_eq!(definition.caching.unwrap().architecture, "amd64");
        let invalid = format!("{example}\n{cache}").replace("/cache/compiler", "/grader");
        assert!(
            toml::from_str::<Definition>(&invalid)
                .unwrap()
                .validate()
                .is_err()
        );
    }

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
    fn old_grader_schemas_and_builder_flags_are_rejected() {
        let example = include_str!("../../../examples/shared-grader/exercise.toml");
        for version in [1, 2] {
            let definition: Definition = toml::from_str(
                &example.replace("schema_version = 3", &format!("schema_version = {version}")),
            )
            .unwrap();
            assert!(definition.validate().is_err());
        }
        assert!(
            crate::Args::try_parse_from([
                "gradingctl",
                "exercise",
                "add",
                "--course",
                "course",
                "--name",
                "echo",
                "--reason",
                "setup",
                "--build-config",
                "/tmp/build.toml"
            ])
            .is_err()
        );
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
