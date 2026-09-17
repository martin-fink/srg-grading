//! Local course administration and synchronization commands.
mod exercises;
mod lifecycle;
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use clap::{Parser, Subcommand};
use grading_core::{
    config::{CourseConfig, Resources},
    integrity::Manifest,
    protocol::{Revision, TestSuite},
    security,
};
use grading_github::{AccountDirectory, GitHub};
use grading_store::{
    artifacts::Artifacts,
    courses::{self, ResolvedStudent, RosterRow},
    grading, identity, submissions,
};
use serde::Deserialize;
use sqlx::PgPool;
use std::path::{Path, PathBuf};
use uuid::Uuid;

#[derive(Parser)]
#[command(name = "gradingctl")]
struct Args {
    #[arg(long, env = "GRADING_DATABASE_URL_FILE", global = true)]
    database_url_file: Option<PathBuf>,
    #[arg(long, env = "GRADING_GITHUB_CONFIG", global = true)]
    github_config: Option<PathBuf>,
    #[arg(long, default_value = "/var/lib/grading/artifacts", global = true)]
    artifact_dir: PathBuf,
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    Migrate,
    Exercise {
        #[command(subcommand)]
        command: exercises::ExerciseCommand,
    },
    Admin {
        #[command(subcommand)]
        command: Admin,
    },
    Course {
        #[command(subcommand)]
        command: Course,
    },
    Roster {
        #[command(subcommand)]
        command: Roster,
    },
    Manifest {
        #[command(subcommand)]
        command: ManifestCommand,
    },
    Worker {
        #[command(subcommand)]
        command: WorkerCommand,
    },
    Grades {
        #[command(subcommand)]
        command: Grades,
    },
    Extension {
        #[arg(long)]
        repository: Uuid,
        #[arg(long)]
        deadline: DateTime<Utc>,
        #[arg(long)]
        reason: String,
    },
    Regrade {
        #[arg(long)]
        submission: Uuid,
        #[arg(long)]
        reason: String,
    },
    SelectSubmission {
        #[arg(long)]
        event: Uuid,
        #[arg(long)]
        reason: String,
    },
    Work {
        #[arg(long)]
        once: bool,
    },
    /// Sync repository state, close expired assignments, and retry incomplete work.
    Sync,
}
#[derive(Subcommand)]
enum Admin {
    Grant {
        #[arg(long)]
        github_username: String,
        #[arg(long)]
        reason: String,
    },
    Revoke {
        #[arg(long)]
        github_username: String,
        #[arg(long)]
        reason: String,
        #[arg(long)]
        recovery_override: bool,
    },
    List,
}
#[derive(Subcommand)]
enum Course {
    Apply {
        file: PathBuf,
        #[arg(long)]
        dry_run: bool,
    },
}
#[derive(Subcommand)]
enum Roster {
    Import {
        #[arg(long)]
        course: String,
        file: PathBuf,
        #[arg(long)]
        dry_run: bool,
    },
}
#[derive(Subcommand)]
enum ManifestCommand {
    Generate {
        #[arg(long)]
        template: String,
        #[arg(long)]
        revision: String,
        #[arg(long, required = true)]
        editable: Vec<String>,
        #[arg(long)]
        output: PathBuf,
    },
}
#[derive(Subcommand)]
enum WorkerCommand {
    Register {
        #[arg(long)]
        id: String,
        #[arg(long)]
        token_file: PathBuf,
        #[arg(long, required = true)]
        profile: Vec<String>,
        #[arg(long, default_value_t = 10)]
        cpu: u32,
        #[arg(long, default_value_t = 32)]
        memory_gib: u32,
        #[arg(long, default_value_t = 64)]
        storage_gib: u32,
    },
    Revoke {
        #[arg(long)]
        id: String,
    },
}
#[derive(Subcommand)]
enum Grades {
    Export {
        #[arg(long)]
        course: String,
        #[arg(long)]
        output: PathBuf,
    },
    Override {
        #[arg(long)]
        repository: Uuid,
        #[arg(long)]
        points: i32,
        #[arg(long)]
        reason: String,
    },
}

async fn github(args: &Args) -> Result<GitHub> {
    GitHub::from_file(
        args.github_config
            .as_deref()
            .context("--github-config is required")?,
    )
    .await
}
async fn pool(args: &Args) -> Result<PgPool> {
    grading_store::connect(
        args.database_url_file
            .as_deref()
            .context("--database-url-file is required")?,
    )
    .await
}
fn operator() -> String {
    std::env::var("SUDO_USER")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "host-root".into())
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();
    let args = Args::parse();
    match &args.command {
        Command::Exercise { command } => {
            let pool = pool(&args).await?;
            match command {
                exercises::ExerciseCommand::PrivateGrade {
                    course,
                    name,
                    reason,
                } => {
                    for (repository, run, message) in
                        grading::enqueue_private(&pool, course, name, &operator(), reason).await?
                    {
                        println!(
                            "{repository}: {message}{}",
                            run.map(|id| format!(" ({id})")).unwrap_or_default()
                        );
                    }
                }
                exercises::ExerciseCommand::Show { course, name } => {
                    let revision = grading_store::exercises::current(&pool, course, name)
                        .await?
                        .context("exercise not found")?;
                    println!("{}", serde_json::to_string_pretty(&revision)?);
                }
                exercises::ExerciseCommand::Add(options)
                | exercises::ExerciseCommand::Update(options) => {
                    exercises::register(
                        &pool,
                        &github(&args).await?,
                        options,
                        matches!(command, exercises::ExerciseCommand::Update(_)),
                        &operator(),
                        &args.artifact_dir,
                    )
                    .await?;
                }
            }
        }
        Command::Migrate => {
            let pool = pool(&args).await?;
            grading_store::MIGRATOR.run(&pool).await?;
            sqlx::raw_sql(include_str!("../../../nix/database-grants.sql"))
                .execute(&pool)
                .await?;
        }
        Command::Admin { command } => {
            let directory = AccountDirectory::new()?;
            let pool = pool(&args).await?;
            match command {
                Admin::Grant {
                    github_username,
                    reason,
                } => {
                    let account = directory.resolve(github_username).await?;
                    identity::admin_change(&pool, account.id, true, false, &operator(), reason)
                        .await?;
                    println!("Granted administrator access to @{}", account.login);
                }
                Admin::Revoke {
                    github_username,
                    reason,
                    recovery_override,
                } => {
                    let account = directory.resolve(github_username).await?;
                    identity::admin_change(
                        &pool,
                        account.id,
                        false,
                        *recovery_override,
                        &operator(),
                        reason,
                    )
                    .await?;
                    println!("Revoked administrator access for @{}", account.login);
                }
                Admin::List => {
                    let ids: Vec<i64> =
                        sqlx::query_scalar("SELECT github_id FROM admins ORDER BY github_id")
                            .fetch_all(&pool)
                            .await?;
                    for id in ids {
                        println!("{}", directory.account(id).await?.login);
                    }
                }
            }
        }
        Command::Course {
            command: Course::Apply { file, dry_run },
        } => {
            let github = github(&args).await?;
            let (root, revision, relative) = git_location(file).await?;
            let config = CourseConfig::parse(&git_file(&root, &revision, &relative).await?)?;
            let mut revisions = Vec::new();
            for (id, assignment) in &config.assignments {
                let manifest_path = relative
                    .parent()
                    .unwrap_or(Path::new(""))
                    .join(&assignment.integrity_manifest);
                let manifest: Manifest =
                    toml::from_str(&git_file(&root, &revision, &manifest_path).await?)?;
                let source = github
                    .snapshot(&assignment.template, &assignment.template_revision)
                    .await?;
                ensure!(
                    manifest.check(&source)?.is_empty(),
                    "template does not match manifest for {id}"
                );
                let tests: TestSuite = toml::from_str(&String::from_utf8(
                    source
                        .files
                        .get(&assignment.public_tests)
                        .context("public test file is missing")?
                        .bytes()?,
                )?)?;
                let revision = Revision {
                    grader: None,
                    course_id: config.course.id.clone(),
                    assignment_id: id.clone(),
                    assignment: assignment.clone(),
                    manifest,
                    tests,
                };
                revision.validate()?;
                println!(
                    "assignment {id}: approved revision {} (existing repositories retain their revision)",
                    revision.digest()?
                );
                revisions.push((id.clone(), revision));
            }
            courses::apply_course(
                &pool(&args).await?,
                &config,
                &revision,
                &revisions,
                *dry_run,
                &operator(),
            )
            .await?;
            println!(
                "{} course {} from {revision}",
                if *dry_run {
                    "Validated (rolled back)"
                } else {
                    "Applied"
                },
                config.course.id
            );
        }
        Command::Roster {
            command:
                Roster::Import {
                    course,
                    file,
                    dry_run,
                },
        } => {
            let github = github(&args).await?;
            let input = tokio::fs::read_to_string(file).await?;
            let rows = parse_roster(&input, file.extension().is_some_and(|ext| ext == "toml"))?;
            let mut resolved = Vec::new();
            for row in rows {
                let account = github.resolve(&row.github_username).await?;
                println!(
                    "{} → GitHub ID {} ({})",
                    row.github_username, account.id, account.login
                );
                resolved.push(ResolvedStudent {
                    row,
                    github_id: account.id,
                    login: account.login,
                });
            }
            courses::import_roster(
                &pool(&args).await?,
                course,
                &resolved,
                *dry_run,
                &operator(),
            )
            .await?;
            println!(
                "{} {} records; omitted enrollments retained",
                if *dry_run {
                    "Validated (rolled back)"
                } else {
                    "Imported"
                },
                resolved.len()
            );
        }
        Command::Manifest {
            command:
                ManifestCommand::Generate {
                    template,
                    revision,
                    editable,
                    output,
                },
        } => {
            let snapshot = github(&args).await?.snapshot(template, revision).await?;
            let manifest = Manifest::generate(&snapshot, editable.clone())?;
            write_new(output, toml::to_string_pretty(&manifest)?.as_bytes()).await?;
            println!(
                "Wrote manifest for {} protected files",
                manifest.files.len()
            );
        }
        Command::Worker { command } => {
            let pool = pool(&args).await?;
            match command {
                WorkerCommand::Register {
                    id,
                    token_file,
                    profile,
                    cpu,
                    memory_gib,
                    storage_gib,
                } => {
                    ensure!(
                        grading_core::config::identifier(id)
                            && profile.iter().all(|p| grading_core::config::identifier(p)),
                        "invalid worker or profile ID"
                    );
                    let caps = Resources {
                        cpu: *cpu,
                        memory_gib: *memory_gib,
                        storage_gib: *storage_gib,
                    };
                    caps.validate()?;
                    let raw = tokio::fs::read_to_string(token_file).await?;
                    ensure!(
                        raw.trim().len() >= 43,
                        "provide a random worker token of at least 32 bytes encoded as base64url"
                    );
                    sqlx::query("INSERT INTO workers(id,token_hash,profiles,resource_caps) VALUES($1,$2,$3,$4) ON CONFLICT(id) DO UPDATE SET token_hash=$2,profiles=$3,resource_caps=$4,enabled=true")
                        .bind(id).bind(security::digest(raw.trim())).bind(profile).bind(serde_json::to_value(caps)?).execute(&pool).await?;
                }
                WorkerCommand::Revoke { id } => {
                    sqlx::query("UPDATE workers SET enabled=false WHERE id=$1")
                        .bind(id)
                        .execute(&pool)
                        .await?;
                }
            }
        }
        Command::Grades {
            command: Grades::Export { course, output },
        } => export(&pool(&args).await?, course, output).await?,
        Command::Grades {
            command:
                Grades::Override {
                    repository,
                    points,
                    reason,
                },
        } => {
            submissions::override_grade(
                &pool(&args).await?,
                *repository,
                *points,
                &operator(),
                reason,
            )
            .await?
        }
        Command::Extension {
            repository,
            deadline,
            reason,
        } => {
            submissions::extend(
                &pool(&args).await?,
                *repository,
                *deadline,
                &operator(),
                reason,
            )
            .await?
        }
        Command::Regrade { submission, reason } => {
            ensure!(!reason.trim().is_empty(), "reason required");
            let pool = pool(&args).await?;
            let mut tx = pool.begin().await?;
            submissions::audit(&mut tx, &operator(), "regrade.request", *submission, reason)
                .await?;
            tx.commit().await?;
            println!("{}", grading::enqueue_run(&pool, *submission, true).await?);
        }
        Command::SelectSubmission { event, reason } => {
            select_submission(&pool(&args).await?, *event, reason).await?
        }
        Command::Work { once } => {
            lifecycle::work(
                &lifecycle::ContextData {
                    pool: pool(&args).await?,
                    github: github(&args).await?,
                    artifacts: Artifacts::new(&args.artifact_dir).await?,
                },
                *once,
            )
            .await?
        }
        Command::Sync => {
            lifecycle::sync(&lifecycle::ContextData {
                pool: pool(&args).await?,
                github: github(&args).await?,
                artifacts: Artifacts::new(&args.artifact_dir).await?,
            })
            .await?
        }
    }
    Ok(())
}

fn parse_roster(input: &str, toml: bool) -> Result<Vec<RosterRow>> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct TomlRoster {
        students: Vec<RosterRow>,
    }
    if toml {
        Ok(toml::from_str::<TomlRoster>(input)?.students)
    } else {
        Ok(csv::Reader::from_reader(input.as_bytes())
            .deserialize()
            .collect::<Result<_, _>>()?)
    }
}

async fn write_new(path: &Path, bytes: &[u8]) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options.open(path).await?;
    file.write_all(bytes).await?;
    file.sync_all().await?;
    Ok(())
}

async fn git_location(file: &Path) -> Result<(PathBuf, String, PathBuf)> {
    let absolute = tokio::fs::canonicalize(file).await?;
    let parent = absolute.parent().context("course path has no parent")?;
    let root = tokio::process::Command::new("git")
        .arg("-C")
        .arg(parent)
        .args(["rev-parse", "--show-toplevel"])
        .output()
        .await?;
    ensure!(
        root.status.success(),
        "course config must be in a Git repository"
    );
    let root = PathBuf::from(String::from_utf8(root.stdout)?.trim());
    let revision = tokio::process::Command::new("git")
        .arg("-C")
        .arg(&root)
        .args(["rev-parse", "HEAD"])
        .output()
        .await?;
    ensure!(
        revision.status.success(),
        "course repository has no revision"
    );
    let relative = absolute.strip_prefix(&root)?.to_path_buf();
    Ok((
        root,
        String::from_utf8(revision.stdout)?.trim().to_owned(),
        relative,
    ))
}
async fn git_file(root: &Path, revision: &str, path: &Path) -> Result<String> {
    let output = tokio::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("show")
        .arg(format!("{revision}:{}", path.display()))
        .output()
        .await?;
    ensure!(
        output.status.success(),
        "configuration and manifests must be committed at {revision}"
    );
    Ok(String::from_utf8(output.stdout)?)
}

async fn export(pool: &PgPool, course: &str, output: &Path) -> Result<()> {
    let students: Vec<(i64, String, String)> = sqlx::query_as(
        "SELECT github_id,student_id,name FROM enrollments WHERE course_id=$1 ORDER BY student_id",
    )
    .bind(course)
    .fetch_all(pool)
    .await?;
    let assignment_ids: Vec<Uuid> =
        sqlx::query_scalar("SELECT id FROM assignments WHERE course_id=$1")
            .bind(course)
            .fetch_all(pool)
            .await?;
    let mut writer = csv::Writer::from_writer(Vec::new());
    writer.write_record([
        "student_id",
        "name",
        "github_id",
        "assignment_id",
        "sha",
        "points",
        "status",
        "override",
    ])?;
    for (id, student_id, name) in students {
        for row in courses::dashboard(pool, id)
            .await?
            .into_iter()
            .filter(|row| assignment_ids.contains(&row.assignment_id))
        {
            writer.write_record([
                spreadsheet_safe(&student_id),
                spreadsheet_safe(&name),
                id.to_string(),
                row.assignment_id.to_string(),
                row.sha.unwrap_or_default(),
                row.override_points
                    .or(row.points)
                    .map(|n| n.to_string())
                    .unwrap_or_default(),
                row.status.unwrap_or_else(|| "not_submitted".into()),
                row.override_points.is_some().to_string(),
            ])?;
        }
    }
    write_new(output, &writer.into_inner()?).await
}
fn spreadsheet_safe(value: &str) -> String {
    if value.starts_with(['=', '+', '-', '@', '\t', '\r']) {
        format!("'{value}")
    } else {
        value.to_owned()
    }
}

async fn select_submission(pool: &PgPool, event: Uuid, reason: &str) -> Result<()> {
    ensure!(!reason.trim().is_empty(), "reason required");
    let mut tx = pool.begin().await?;
    let (repository, sha, received): (Uuid, String, DateTime<Utc>) =
        sqlx::query_as("SELECT repository_id,sha,received_at FROM submission_events WHERE id=$1")
            .bind(event)
            .fetch_one(&mut *tx)
            .await?;
    let closed: bool =
        sqlx::query_scalar("SELECT closure_due FROM student_repositories WHERE id=$1 FOR UPDATE")
            .bind(repository)
            .fetch_one(&mut *tx)
            .await?;
    ensure!(closed, "selection overrides require a closed assignment");
    let id:Uuid=sqlx::query_scalar("INSERT INTO submissions(id,repository_id,event_id,sha,received_at) VALUES($1,$2,$3,$4,$5) ON CONFLICT(event_id) DO UPDATE SET event_id=$3 RETURNING id")
        .bind(Uuid::new_v4()).bind(repository).bind(event).bind(sha).bind(received).fetch_one(&mut *tx).await?;
    sqlx::query(
        "UPDATE student_repositories SET final_submission_id=$2,needs_review=false WHERE id=$1",
    )
    .bind(repository)
    .bind(id)
    .execute(&mut *tx)
    .await?;
    submissions::audit(&mut tx, &operator(), "submission.select", event, reason).await?;
    grading_store::queue::enqueue(
        &mut tx,
        "snapshot",
        serde_json::json!({"submission_id":id}),
        &format!("snapshot:{id}"),
        100,
    )
    .await?;
    tx.commit().await?;
    let source: Option<String> =
        sqlx::query_scalar("SELECT source_digest FROM submissions WHERE id=$1")
            .bind(id)
            .fetch_one(pool)
            .await?;
    if source.is_some() {
        grading::enqueue_run(pool, id, true).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn administrator_commands_require_handles_and_reject_id_flags() {
        for command in ["grant", "revoke"] {
            let args = Args::try_parse_from([
                "gradingctl",
                "admin",
                command,
                "--github-username",
                "martin-fink",
                "--reason",
                "Test",
            ])
            .unwrap();
            match args.command {
                Command::Admin {
                    command:
                        Admin::Grant {
                            github_username, ..
                        }
                        | Admin::Revoke {
                            github_username, ..
                        },
                } => assert_eq!(github_username, "martin-fink"),
                _ => panic!("unexpected command"),
            }
            assert!(
                Args::try_parse_from([
                    "gradingctl",
                    "admin",
                    command,
                    "--github-id",
                    "17706737",
                    "--reason",
                    "Test"
                ])
                .is_err()
            );
        }
    }

    #[test]
    fn student_rosters_accept_handles_but_never_supplied_github_ids() {
        let csv = "student_id,name,github_username\nfixture,Student,martin-fink\n";
        let toml = "[[students]]\nstudent_id = 'fixture'\nname = 'Student'\ngithub_username = 'martin-fink'\n";
        assert_eq!(
            parse_roster(csv, false).unwrap()[0].github_username,
            "martin-fink"
        );
        assert_eq!(
            parse_roster(toml, true).unwrap()[0].github_username,
            "martin-fink"
        );
        assert!(
            parse_roster(
                "student_id,name,github_id\nfixture,Student,17706737\n",
                false
            )
            .is_err()
        );
        assert!(
            parse_roster(
                "student_id,name,github_username,github_id\nfixture,Student,martin-fink,17706737\n",
                false
            )
            .is_err()
        );
        assert!(parse_roster(&format!("{toml}github_id = 17706737\n"), true).is_err());
        assert!(
            parse_roster(
                &toml.replace("github_username = 'martin-fink'", "github_id = 17706737"),
                true
            )
            .is_err()
        );
    }
}
