//! Privileged administration worker. The public service only queues typed requests.
use super::*;
use anyhow::Context as _;
use grading_core::admin::Input;
use grading_store::{admin as operations, exercises as stored_exercises};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::Duration;

#[derive(clap::Args)]
pub struct Options {
    #[arg(long)]
    pub once: bool,
    #[arg(long, env = "GRADING_ADMIN_DATABASE_URL_FILE")]
    pub admin_database_url_file: Option<PathBuf>,
    #[arg(long, env = "GRADING_MIGRATION_DATABASE_URL_FILE")]
    pub migration_database_url_file: Option<PathBuf>,
    #[arg(long, env = "GRADING_CACHE_CONFIG")]
    pub cache_config: Option<PathBuf>,
}
#[derive(Serialize, Deserialize)]
struct Plan {
    input: Input,
    before: Value,
    resolved: Value,
    registrations: Vec<exercises::Register>,
    digests: Vec<String>,
    cache_seeds: Vec<Option<grading_core::caching::Seed>>,
    catalogs: BTreeMap<String, Vec<stored_exercises::CatalogEntry>>,
}
struct Context<'a> {
    args: &'a Args,
    options: &'a Options,
    pool: PgPool,
    actor: String,
}
fn required<'a>(input: &'a Input, name: &str) -> Result<&'a str> {
    let v = input.field(name);
    ensure!(!v.is_empty(), "Field '{name}' is required.");
    Ok(v)
}
fn parse<T: std::str::FromStr>(input: &Input, name: &str) -> Result<T> {
    required(input, name)?
        .parse()
        .map_err(|_| anyhow::anyhow!("Field '{name}' has an invalid value."))
}
fn flag(input: &Input, name: &str) -> Result<bool> {
    match input.field(name) {
        "" | "false" => Ok(false),
        "true" => Ok(true),
        _ => anyhow::bail!("Field '{name}' must be true or false."),
    }
}
fn optional(input: &Input, name: &str) -> Option<String> {
    let s = input.field(name);
    (!s.is_empty()).then(|| s.into())
}
fn decode<T: DeserializeOwned>(value: &Value) -> Result<T> {
    Ok(serde_json::from_value(value.clone())?)
}
async fn rows(pool: &PgPool, sql: &'static str, key: &str) -> Result<Value> {
    Ok(json!(
        sqlx::query_scalar::<_, Value>(sql)
            .bind(key)
            .fetch_all(pool)
            .await?
    ))
}
async fn snapshot(pool: &PgPool, input: &Input) -> Result<Value> {
    let (sql, key) = match input.action.as_str() {
        "course" => {
            let c = CourseConfig::parse(&input.content)?;
            return rows(
                pool,
                "SELECT to_jsonb(c) FROM courses c WHERE id=$1",
                &c.course.id,
            )
            .await;
        }
        "roster" => (
            "SELECT to_jsonb(t) FROM (SELECT c.id,c.config_revision,(SELECT jsonb_agg(to_jsonb(e) ORDER BY e.student_id) FROM enrollments e WHERE e.course_id=c.id) AS students FROM courses c WHERE c.id=$1) t",
            required(input, "course")?,
        ),
        "extension" | "override" => (
            "SELECT to_jsonb(t) FROM (SELECT r.id,r.closure_due,r.grading_revision,v.max_points,COALESCE(x.deadline,d.deadline) AS deadline,(SELECT jsonb_agg(to_jsonb(o) ORDER BY o.created_at,o.id) FROM grade_overrides o WHERE o.repository_id=r.id) AS overrides FROM student_repositories r JOIN assignment_revisions v ON v.digest=r.grading_revision JOIN assignment_revisions d ON d.digest=r.revision_digest LEFT JOIN extensions x ON x.repository_id=r.id WHERE r.id::text=$1) t",
            required(input, "repository")?,
        ),
        "regrade" => (
            "SELECT to_jsonb(t) FROM (SELECT s.id,s.sha,s.source_digest,r.grading_revision FROM submissions s JOIN student_repositories r ON r.id=s.repository_id WHERE s.id::text=$1) t",
            required(input, "submission")?,
        ),
        "select_submission" => (
            "SELECT to_jsonb(t) FROM (SELECT e.id,e.sha,e.repository_id,r.closure_due,r.final_submission_id FROM submission_events e JOIN student_repositories r ON r.id=e.repository_id WHERE e.id::text=$1) t",
            required(input, "event")?,
        ),
        "retry_private" => (
            "SELECT to_jsonb(t) FROM (SELECT g.id,g.status,g.public_run_id,(SELECT id FROM grading_runs other WHERE other.submission_id=g.submission_id ORDER BY attempt DESC LIMIT 1) AS latest FROM grading_runs g WHERE g.id::text=$1) t",
            required(input, "run")?,
        ),
        "retry_task" => (
            "SELECT to_jsonb(t) FROM (SELECT id,kind,status,attempts,last_error FROM tasks WHERE id::text=$1) t",
            required(input, "task")?,
        ),
        "worker_register" | "worker_revoke" => (
            "SELECT to_jsonb(t) FROM (SELECT id,enabled,profiles,resource_caps,token_hash FROM workers WHERE id=$1) t",
            required(input, "id")?,
        ),
        "exercise_show" | "private_grade" => {
            return rows(pool,"SELECT to_jsonb(t) FROM (SELECT a.id,a.current_revision,v.definition,(SELECT jsonb_agg(jsonb_build_object('id',r.id,'grading_revision',r.grading_revision) ORDER BY r.id) FROM student_repositories r WHERE r.assignment_id=a.id) AS repositories FROM assignments a JOIN assignment_revisions v ON v.digest=a.current_revision WHERE a.course_id||'/'||a.slug=$1) t",&format!("{}/{}",required(input,"course")?,required(input,"name")?)).await;
        }
        "export" => (
            "SELECT to_jsonb(t) FROM (SELECT id,title,config_revision FROM courses WHERE id=$1) t",
            required(input, "course")?,
        ),
        "admin_grant" | "admin_revoke" | "admin_list" => (
            "SELECT to_jsonb(t) FROM (SELECT a.github_id,u.login FROM admins a LEFT JOIN users u ON u.github_id=a.github_id WHERE $1='' ORDER BY a.github_id) t",
            "",
        ),
        "sync" | "work" => (
            "SELECT to_jsonb(t) FROM (SELECT id,title,organization FROM courses WHERE $1='' ORDER BY id) t",
            "",
        ),
        "migrate" => (
            "SELECT to_jsonb(t) FROM (SELECT version,description,success,encode(checksum,'hex') AS checksum FROM _sqlx_migrations WHERE $1='' ORDER BY version) t",
            "",
        ),
        _ => return Ok(Value::Null),
    };
    rows(pool, sql, key).await
}
fn first(plan: &Value) -> Result<&Value> {
    plan.as_array()
        .and_then(|r| r.first())
        .context("The selected record does not exist. Check its ID in the admin records pages.")
}
fn register(input: &Input, options: &Options) -> Result<exercises::Register> {
    Ok(exercises::Register {
        cache_config: options.cache_config.clone(),
        course: required(input, "course")?.into(),
        name: required(input, "name")?.into(),
        template: optional(input, "template"),
        grader: optional(input, "grader"),
        template_ref: optional(input, "template_ref"),
        grader_ref: optional(input, "grader_ref"),
        opens_at: optional(input, "opens_at")
            .map(|_| parse(input, "opens_at"))
            .transpose()?,
        deadline: optional(input, "deadline")
            .map(|_| parse(input, "deadline"))
            .transpose()?,
        existing: flag(input, "existing")?,
        reason: input.reason.clone(),
        dry_run: true,
        runner_image: optional(input, "runner_image"),
    })
}
async fn prepare(context: &Context<'_>, input: Input) -> Result<(Plan, String)> {
    input.validate()?;
    let before = snapshot(&context.pool, &input).await?;
    let mut plan = Plan {
        input,
        before,
        resolved: Value::Null,
        registrations: Vec::new(),
        digests: Vec::new(),
        cache_seeds: Vec::new(),
        catalogs: BTreeMap::new(),
    };
    let input = &plan.input;
    let mut report = format!(
        "Action: {}\nReason: {}\n\n",
        grading_core::admin::action(&input.action)
            .context("unknown action")?
            .title,
        input.reason
    );
    match input.action.as_str() {
        "course" => {
            let config = CourseConfig::parse(&input.content)?;
            if plan.before.as_array().is_some_and(|rows| !rows.is_empty()) {
                report.push_str(&format!(
                    "Previous course settings:\n{}\n\n",
                    serde_json::to_string_pretty(&plan.before)?
                ));
            }
            let hash = format!("sha256:{}", security::digest(&input.content));
            courses::apply_course(&context.pool, &config, &hash, true, &context.actor).await?;
            report.push_str(&format!("{} course {}\nTitle: {}\nOrganization: {}\nTimezone: {}\nInput: {hash}\nDatabase dry run rolled back.\n",if plan.before.as_array().is_some_and(Vec::is_empty){"CREATE"}else{"UPDATE"},config.course.id,config.course.title,config.course.github_organization,config.course.timezone));
        }
        "roster" => {
            first(&plan.before)?;
            let format = required(input, "format")?;
            ensure!(
                ["csv", "toml"].contains(&format),
                "Roster format must be csv or toml."
            );
            let rows = parse_roster(&input.content, format == "toml")?;
            ensure!(
                !rows.is_empty() && rows.len() <= 1000,
                "Import 1..1000 students at a time."
            );
            let github = github(context.args).await?;
            let mut resolved = Vec::new();
            for row in rows {
                let account = github.resolve(&row.github_username).await?;
                report.push_str(&format!(
                    "{} / {} → @{} (GitHub ID {})\n",
                    row.student_id, row.name, account.login, account.id
                ));
                resolved.push(ResolvedStudent {
                    row,
                    github_id: account.id,
                    login: account.login,
                });
            }
            courses::import_roster(
                &context.pool,
                required(input, "course")?,
                &resolved,
                true,
                &context.actor,
            )
            .await?;
            report.push_str("Database dry run rolled back. Omitted students remain enrolled.\n");
            plan.resolved = serde_json::to_value(resolved)?;
        }
        "exercises" | "exercise_add" | "exercise_update" => {
            let mut registrations = Vec::new();
            if input.action == "exercises" {
                let file: exercise_files::File = toml::from_str(&input.content)?;
                let desired = exercise_files::merge(vec![file])?;
                for (course, entries) in desired {
                    let exists: bool =
                        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM courses WHERE id=$1)")
                            .bind(&course)
                            .fetch_one(&context.pool)
                            .await?;
                    ensure!(
                        exists,
                        "Course {course} does not exist. Create it before applying exercises."
                    );
                    plan.catalogs.insert(
                        course.clone(),
                        stored_exercises::catalog(&context.pool, &course).await?,
                    );
                    for (name, e) in entries {
                        registrations.push(exercises::Register {
                            cache_config: context.options.cache_config.clone(),
                            course: course.clone(),
                            name,
                            template: Some(e.template),
                            grader: Some(e.grader),
                            template_ref: Some(e.template_ref),
                            grader_ref: Some(e.grader_ref),
                            opens_at: Some(e.opens_at),
                            deadline: Some(e.deadline),
                            existing: e.existing,
                            reason: input.reason.clone(),
                            dry_run: true,
                            runner_image: e.runner_image,
                        });
                    }
                }
            } else {
                registrations.push(register(input, context.options)?);
            }
            ensure!(
                registrations.len() <= 50,
                "Validate at most 50 exercises per operation."
            );
            let github = if registrations.is_empty() {
                None
            } else {
                Some(github(context.args).await?)
            };
            for mut registration in registrations {
                let old = stored_exercises::current(
                    &context.pool,
                    &registration.course,
                    &registration.name,
                )
                .await?;
                ensure!(
                    input.action != "exercise_add" || old.is_none(),
                    "Exercise exists; use Update exercise."
                );
                ensure!(
                    input.action != "exercise_update" || old.is_some(),
                    "Exercise not found; use Add exercise."
                );
                let mut prepared = exercises::prepare(
                    &context.pool,
                    github
                        .as_ref()
                        .context("GitHub connection not configured.")?,
                    &registration,
                    old.is_some(),
                )
                .await?;
                plan.digests.push(prepared.revision.digest()?);
                registration.template = Some(prepared.revision.assignment.template.clone());
                registration.template_ref =
                    Some(prepared.revision.assignment.template_revision.clone());
                registration.grader = Some(
                    prepared
                        .revision
                        .grader
                        .as_ref()
                        .context("missing grader")?
                        .repository
                        .clone(),
                );
                registration.grader_ref = Some(
                    prepared
                        .revision
                        .grader
                        .as_ref()
                        .context("missing grader")?
                        .revision
                        .clone(),
                );
                registration.runner_image = Some(prepared.revision.assignment.image.clone());
                registration.opens_at = Some(prepared.revision.assignment.opens_at);
                registration.deadline = Some(prepared.revision.assignment.deadline);
                if prepared.cache.is_some() {
                    ensure!(
                        context.options.cache_config.is_some(),
                        "Caching requires GRADING_CACHE_CONFIG on the admin worker."
                    );
                }
                prepared
                    .prepare_cache(context.options.cache_config.as_deref(), true)
                    .await?;
                plan.cache_seeds.push(
                    prepared
                        .revision
                        .grader
                        .as_ref()
                        .and_then(|g| g.caching.clone()),
                );
                report.push_str(&format!("{} {}/{}\nTemplate: {}\nGrader: {}\nRevision: {}\nExisting repositories updated: {}\nCache: {}\n\n",if old.is_some(){"UPDATE"}else{"ADD"},registration.course,registration.name,registration.template_ref.as_deref().unwrap_or(""),registration.grader_ref.as_deref().unwrap_or(""),prepared.revision.digest()?,registration.existing,prepared.revision.grader.as_ref().and_then(|g|g.caching.as_ref()).map(|s|format!("reuse {}",s.digest)).unwrap_or_else(||if prepared.cache.is_some(){"preparation required after confirmation".into()}else{"none".into()})));
                if !plan.catalogs.contains_key(&registration.course) {
                    plan.catalogs.insert(
                        registration.course.clone(),
                        stored_exercises::catalog(&context.pool, &registration.course).await?,
                    );
                }
                plan.registrations.push(registration);
            }
            for (course, entries) in &plan.catalogs {
                for old in entries {
                    if input.action == "exercises"
                        && !old.archived
                        && !plan
                            .registrations
                            .iter()
                            .any(|r| &r.course == course && r.name == old.slug)
                    {
                        report.push_str(&format!("ARCHIVE {course}/{}: new repository creation stops; existing history is retained.\n",old.slug));
                    }
                }
            }
            report.push_str("No artifacts, cache Jobs or catalog changes have been made. All refs above are pinned for confirmation.\n");
        }
        "extension" => {
            let current = first(&plan.before)?;
            let deadline: DateTime<Utc> = parse(input, "deadline")?;
            let previous: DateTime<Utc> = serde_json::from_value(current["deadline"].clone())?;
            ensure!(
                current["closure_due"] == false && deadline > previous && deadline > Utc::now(),
                "Extension must advance a deadline that has not been closed."
            );
            report.push_str(&format!(
                "Repository {}\nPrevious deadline: {previous}\nNew deadline: {deadline}\n",
                required(input, "repository")?
            ));
        }
        "override" => {
            let current = first(&plan.before)?;
            let points: i32 = parse(input, "points")?;
            let max = current["max_points"].as_i64().context("missing maximum")?;
            ensure!(
                (0..=max).contains(&i64::from(points)),
                "Points must be between 0 and {max}."
            );
            report.push_str(&format!(
                "Repository {}: set override to {points} / {max}.\n",
                required(input, "repository")?
            ));
        }
        "regrade" => {
            let current = first(&plan.before)?;
            ensure!(
                current["source_digest"].is_string(),
                "Source has not been retained yet. Wait for snapshot processing."
            );
            report.push_str(&format!(
                "Queue a new public grading attempt for {} using revision {}.\n",
                current["sha"], current["grading_revision"]
            ));
        }
        "select_submission" => {
            ensure!(
                first(&plan.before)?["closure_due"] == true,
                "Selection overrides require a closed assignment."
            );
            report.push_str(&format!(
                "Select this event as the final submission:\n{}\n",
                serde_json::to_string_pretty(&plan.before)?
            ));
        }
        "retry_private" => {
            let r = first(&plan.before)?;
            ensure!(
                r["latest"] == r["id"]
                    && r["public_run_id"].is_string()
                    && ["infrastructure_failed", "timed_out"]
                        .contains(&r["status"].as_str().unwrap_or("")),
                "Only the latest failed private run can be retried."
            );
            report.push_str(&format!(
                "Retry private run {} with its existing baseline and grader.\n",
                required(input, "run")?
            ));
        }
        "retry_task" => {
            let r = first(&plan.before)?;
            ensure!(
                r["status"] == "failed"
                    && ["snapshot", "provision", "publish", "lock"]
                        .contains(&r["kind"].as_str().unwrap_or("")),
                "This is not a failed control task."
            );
            report.push_str(&format!(
                "Reset retry attempts and queue:\n{}\n",
                serde_json::to_string_pretty(&plan.before)?
            ));
        }
        "worker_register" => {
            ensure!(
                grading_core::config::identifier(required(input, "id")?),
                "Invalid worker ID."
            );
            ensure!(
                security::valid_hex(&input.content, 64),
                "Invalid stored token hash."
            );
            let caps = Resources {
                cpu: parse(input, "cpu")?,
                memory_gib: parse(input, "memory_gib")?,
                storage_gib: parse(input, "storage_gib")?,
            };
            caps.validate()?;
            plan.resolved = serde_json::to_value(&caps)?;
            report.push_str(&format!("Enable worker {} with registered-v1, {} CPU, {} GiB memory, {} GiB storage. Replace its token; the token is not shown or retained as plaintext.\n",required(input,"id")?,caps.cpu,caps.memory_gib,caps.storage_gib));
        }
        "worker_revoke" => {
            first(&plan.before)?;
            report.push_str(&format!("Disable worker {}.\n", required(input, "id")?));
        }
        "admin_grant" | "admin_revoke" => {
            ensure!(
                context.options.admin_database_url_file.is_some(),
                "Administrator membership requires GRADING_ADMIN_DATABASE_URL_FILE on the admin worker."
            );
            check_connection(
                context
                    .options
                    .admin_database_url_file
                    .as_deref()
                    .context("Administrator connection not configured.")?,
                "admin",
            )
            .await?;
            let account = AccountDirectory::new()?
                .resolve(required(input, "github_username")?)
                .await?;
            let recovery = flag(input, "recovery_override")?;
            if input.action == "admin_revoke" {
                ensure!(
                    recovery
                        || plan
                            .before
                            .as_array()
                            .context("admin list")?
                            .iter()
                            .any(|r| r["github_id"] != account.id),
                    "Cannot revoke the last administrator without recovery override."
                );
            }
            plan.resolved = json!({"id":account.id,"login":account.login});
            report.push_str(&format!(
                "{} @{} (immutable GitHub ID {})\nRecovery override: {recovery}\n",
                if input.action == "admin_grant" {
                    "Grant administrator to"
                } else {
                    "Revoke administrator from"
                },
                account.login,
                account.id
            ));
        }
        "admin_list" => {
            let directory = AccountDirectory::new()?;
            let mut accounts = Vec::new();
            for row in plan.before.as_array().context("administrator list")? {
                let account = directory
                    .account(row["github_id"].as_i64().context("administrator ID")?)
                    .await?;
                accounts.push(json!({"github_id":account.id,"login":account.login}));
            }
            plan.resolved = json!(accounts);
            report.push_str(&serde_json::to_string_pretty(&plan.resolved)?);
        }
        "exercise_show" => {
            let revision = stored_exercises::current(
                &context.pool,
                required(input, "course")?,
                required(input, "name")?,
            )
            .await?
            .context("Exercise not found.")?;
            plan.resolved = serde_json::to_value(revision)?;
            report.push_str(&serde_json::to_string_pretty(&plan.resolved)?);
        }
        "private_grade" => {
            let current = first(&plan.before)?;
            ensure!(
                current["definition"]["grader"]["workflow"]["private_command"].is_array(),
                "Exercise has no private grading command."
            );
            report.push_str(&format!(
                "Current exercise revision: {}\n",
                current["current_revision"]
            ));
            let records:Vec<Value>=sqlx::query_scalar("SELECT to_jsonb(t) FROM (SELECT r.id,r.name,r.final_submission_id,r.grading_revision,jsonb_typeof(g.definition#>'{grader,workflow,private_command}')='array' AS has_private_command,COALESCE(x.deadline,v.deadline) AS effective_deadline,COALESCE(x.deadline,v.deadline)<now() AS deadline_passed FROM student_repositories r JOIN assignments a ON a.id=r.assignment_id JOIN assignment_revisions v ON v.digest=r.revision_digest JOIN assignment_revisions g ON g.digest=r.grading_revision LEFT JOIN extensions x ON x.repository_id=r.id WHERE a.course_id=$1 AND a.slug=$2 ORDER BY r.id) t").bind(required(input,"course")?).bind(required(input,"name")?).fetch_all(&context.pool).await?;
            ensure!(!records.is_empty(), "Exercise has no student repositories.");
            ensure!(
                records
                    .iter()
                    .all(|r| r["deadline_passed"] != true || r["has_private_command"] == true),
                "Some closed repositories use a grader without private tests. Explicitly update their grading revision before scheduling private grading."
            );
            report.push_str("Schedule private grading only for eligible closed final submissions with completed public grading. Others will be reported as skipped.\n");
            report.push_str(&serde_json::to_string_pretty(&records)?);
        }
        "export" => {
            first(&plan.before)?;
            report.push_str(&format!("Export current final and provisional grades for course {}. Spreadsheet formula prefixes are escaped.\n",required(input,"course")?));
        }
        "sync" | "work" => {
            let _ = github(context.args).await?;
            report.push_str(if input.action=="sync"{"Reconcile all repositories and enforce deadlines in the following courses. This contacts GitHub and may close expired assignments.\n"}else{"Process one queued provisioning/snapshot/publication/locking task. The exact task is selected from the queue when confirmed.\n"});
            report.push_str(&serde_json::to_string_pretty(&plan.before)?);
        }
        "migrate" => {
            ensure!(
                context.options.migration_database_url_file.is_some(),
                "Migrations require GRADING_MIGRATION_DATABASE_URL_FILE on the admin worker."
            );
            check_connection(
                context
                    .options
                    .migration_database_url_file
                    .as_deref()
                    .context("Migration connection not configured.")?,
                "migration",
            )
            .await?;
            for row in plan.before.as_array().context("migration history")? {
                ensure!(
                    row["success"] == true,
                    "A previous migration is incomplete; investigate before continuing."
                );
            }
            for migration in grading_store::MIGRATOR.iter() {
                let applied = plan
                    .before
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|r| r["version"] == migration.version);
                if let Some(applied) = applied {
                    ensure!(
                        applied["checksum"] == hex_digest(&migration.checksum),
                        "Applied migration checksum differs from this build."
                    );
                }
                report.push_str(&format!(
                    "{} {} {}\n",
                    if applied.is_some() {
                        "APPLIED"
                    } else {
                        "PENDING"
                    },
                    migration.version,
                    migration.description
                ));
            }
            report.push_str("Apply pending migrations and refresh runtime role grants.\n");
        }
        _ => anyhow::bail!("Unsupported administrative action."),
    }
    report.push_str("\nValidation succeeded. Review this output before confirming; no requested changes have been applied.");
    Ok((plan, report))
}
async fn check_connection(path: &Path, kind: &str) -> Result<()> {
    let pool = grading_store::connect(path).await?;
    let allowed: bool = if kind == "admin" {
        sqlx::query_scalar("SELECT has_table_privilege(current_user,'admins','INSERT') AND has_table_privilege(current_user,'admins','DELETE') AND has_table_privilege(current_user,'audit_events','INSERT')").fetch_one(&pool).await?
    } else {
        sqlx::query_scalar("SELECT current_user=pg_get_userbyid(nspowner) FROM pg_namespace WHERE nspname='public'").fetch_one(&pool).await?
    };
    pool.close().await;
    ensure!(
        allowed,
        "Configured {kind} connection lacks required privileges."
    );
    Ok(())
}
fn hex_digest(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

async fn apply(
    context: &Context<'_>,
    plan: Plan,
    operation: Uuid,
) -> Result<(String, Option<String>)> {
    let input = &plan.input;
    input.validate()?;
    ensure!(
        snapshot(&context.pool, input).await? == plan.before,
        "Relevant records changed since validation. Validate again before applying."
    );
    let mut output = String::new();
    match input.action.as_str() {
        "course" => {
            let config = CourseConfig::parse(&input.content)?;
            courses::apply_course(
                &context.pool,
                &config,
                &format!("sha256:{}", security::digest(&input.content)),
                false,
                &context.actor,
            )
            .await?;
            output = format!("Applied course {}.", config.course.id);
        }
        "roster" => {
            let resolved: Vec<ResolvedStudent> = decode(&plan.resolved)?;
            courses::import_roster(
                &context.pool,
                required(input, "course")?,
                &resolved,
                false,
                &context.actor,
            )
            .await?;
            output = format!(
                "Imported {} students; omitted enrollments retained.",
                resolved.len()
            );
        }
        "exercises" | "exercise_add" | "exercise_update" => {
            for (course, before) in &plan.catalogs {
                ensure!(
                    &stored_exercises::catalog(&context.pool, course).await? == before,
                    "Course catalog changed since validation; validate again."
                );
            }
            let github = if plan.registrations.is_empty() {
                None
            } else {
                Some(github(context.args).await?)
            };
            let mut prepared = Vec::new();
            ensure!(
                plan.registrations.len() == plan.digests.len()
                    && plan.registrations.len() == plan.cache_seeds.len(),
                "Invalid validated exercise plan."
            );
            for ((registration, digest), expected_seed) in plan
                .registrations
                .iter()
                .zip(&plan.digests)
                .zip(&plan.cache_seeds)
            {
                let update = stored_exercises::current(
                    &context.pool,
                    &registration.course,
                    &registration.name,
                )
                .await?
                .is_some();
                let mut item = exercises::prepare(
                    &context.pool,
                    github
                        .as_ref()
                        .context("GitHub connection not configured.")?,
                    registration,
                    update,
                )
                .await?;
                ensure!(
                    &item.revision.digest()? == digest,
                    "Pinned exercise definition differs from validation; validate again."
                );
                operations::progress(&context.pool,operation,&format!("Confirmed. Preparing {}/{} and its optional cache. This may take up to the recipe timeout. No catalog changes are published until all preparations succeed.",registration.course,registration.name)).await?;
                let cache_key = item.cache_key()?;
                let started = std::time::SystemTime::now();
                let cache_result = item
                    .prepare_cache(context.options.cache_config.as_deref(), false)
                    .await;
                if let (Some(path), Some(key)) = (
                    context.options.cache_config.as_deref(),
                    cache_key.as_deref(),
                ) {
                    let details=cache_diagnostics(path,key,started).await.unwrap_or_else(|_|"Cache diagnostics could not be read; inspect the admin worker and sandbox Job logs.".into());
                    operations::progress(&context.pool, operation, &details).await?;
                }
                cache_result?;
                if let Some(expected_seed) = expected_seed {
                    ensure!(
                        serde_json::to_value(
                            item.revision
                                .grader
                                .as_ref()
                                .and_then(|g| g.caching.as_ref())
                        )? == serde_json::to_value(expected_seed)?,
                        "Prepared cache changed since validation. Validate again."
                    );
                }
                if let Some(seed) = item
                    .revision
                    .grader
                    .as_ref()
                    .and_then(|g| g.caching.as_ref())
                {
                    output.push_str(&format!(
                        "Cache ready: {} (input key {})\n",
                        seed.digest, seed.input_key
                    ));
                }
                if let Some(source) = &item.source {
                    Artifacts::new(&context.args.artifact_dir)
                        .await?
                        .put(&context.pool, "source", source)
                        .await?;
                }
                output.push_str(&format!(
                    "Published {}/{} revision {}\n",
                    registration.course,
                    registration.name,
                    item.revision.digest()?
                ));
                prepared.push((item, registration.existing));
            }
            let publications: Vec<_> = prepared
                .iter()
                .map(|(item, existing)| stored_exercises::Publication {
                    revision: &item.revision,
                    expected: item.expected.as_deref(),
                    existing: *existing,
                    dry_run: false,
                    operator: &context.actor,
                    reason: &input.reason,
                })
                .collect();
            if input.action == "exercises" {
                stored_exercises::apply_set(
                    &context.pool,
                    &plan.catalogs,
                    &publications,
                    &context.actor,
                    &input.reason,
                    false,
                )
                .await?;
            } else {
                stored_exercises::publish(
                    &context.pool,
                    *publications.first().context("missing exercise")?,
                )
                .await?;
            }
            if output.is_empty() {
                output = "Applied empty catalog; previous active exercises archived.".into();
            }
        }
        "extension" => {
            submissions::extend(
                &context.pool,
                parse(input, "repository")?,
                parse(input, "deadline")?,
                &context.actor,
                &input.reason,
            )
            .await?
        }
        "override" => {
            submissions::override_grade(
                &context.pool,
                parse(input, "repository")?,
                parse(input, "points")?,
                &context.actor,
                &input.reason,
            )
            .await?
        }
        "regrade" => {
            let submission = parse(input, "submission")?;
            let mut tx = context.pool.begin().await?;
            submissions::audit(
                &mut tx,
                &context.actor,
                "regrade.request",
                submission,
                &input.reason,
            )
            .await?;
            tx.commit().await?;
            output = format!(
                "Queued grading run {}.",
                grading::enqueue_run(&context.pool, submission, true).await?
            );
        }
        "retry_private" => {
            output = format!(
                "Queued private run {}.",
                grading::retry_private(
                    &context.pool,
                    parse(input, "run")?,
                    &context.actor,
                    &input.reason
                )
                .await?
            );
        }
        "retry_task" => {
            grading_store::queue::retry(
                &context.pool,
                parse(input, "task")?,
                &context.actor,
                &input.reason,
            )
            .await?
        }
        "select_submission" => {
            select_submission(&context.pool, parse(input, "event")?, &input.reason).await?
        }
        "worker_register" => {
            let caps: Resources = decode(&plan.resolved)?;
            caps.validate()?;
            sqlx::query("INSERT INTO workers(id,token_hash,profiles,resource_caps) VALUES($1,$2,ARRAY['registered-v1'],$3) ON CONFLICT(id) DO UPDATE SET token_hash=$2,profiles=ARRAY['registered-v1'],resource_caps=$3,enabled=true").bind(required(input,"id")?).bind(&input.content).bind(serde_json::to_value(caps)?).execute(&context.pool).await?;
        }
        "worker_revoke" => {
            sqlx::query("UPDATE workers SET enabled=false WHERE id=$1")
                .bind(required(input, "id")?)
                .execute(&context.pool)
                .await?;
        }
        "admin_grant" | "admin_revoke" => {
            let pool = grading_store::connect(
                context
                    .options
                    .admin_database_url_file
                    .as_deref()
                    .context("Administrator connection not configured.")?,
            )
            .await?;
            identity::admin_change(
                &pool,
                plan.resolved["id"]
                    .as_i64()
                    .context("missing resolved account")?,
                input.action == "admin_grant",
                flag(input, "recovery_override")?,
                &context.actor,
                &input.reason,
            )
            .await?;
            output = format!(
                "Updated administrator access for @{}.",
                plan.resolved["login"].as_str().unwrap_or("")
            );
            pool.close().await;
        }
        "admin_list" => {
            output = serde_json::to_string_pretty(&plan.resolved)?;
        }
        "exercise_show" => {
            output = serde_json::to_string_pretty(&plan.resolved)?;
        }
        "private_grade" => {
            for (repo, run, message) in grading::enqueue_private(
                &context.pool,
                required(input, "course")?,
                required(input, "name")?,
                &context.actor,
                &input.reason,
            )
            .await?
            {
                output.push_str(&format!(
                    "{repo}: {message} {}\n",
                    run.map(|id| id.to_string()).unwrap_or_default()
                ));
            }
        }
        "export" => {
            let csv = export_csv(&context.pool, required(input, "course")?).await?;
            ensure!(csv.len() <= 8 * 1024 * 1024, "Export exceeds 8 MiB.");
            return Ok((
                "Grade export ready to download.".into(),
                Some(String::from_utf8(csv)?),
            ));
        }
        "work" | "sync" => {
            let data = lifecycle::ContextData {
                pool: context.pool.clone(),
                github: github(context.args).await?,
                artifacts: Artifacts::new(&context.args.artifact_dir).await?,
            };
            if input.action == "work" {
                lifecycle::work(&data, true).await?;
            } else {
                lifecycle::sync(&data).await?;
            }
            output="Control operation finished. Inspect the task and repository pages for per-repository diagnostics and retry state.".into();
        }
        "migrate" => {
            let pool = grading_store::connect(
                context
                    .options
                    .migration_database_url_file
                    .as_deref()
                    .context("Migration connection not configured.")?,
            )
            .await?;
            grading_store::MIGRATOR.run(&pool).await?;
            sqlx::raw_sql(include_str!("../../../nix/database-grants.sql"))
                .execute(&pool)
                .await?;
            pool.close().await;
            output = "Database migrations and grants applied.".into();
        }
        _ => anyhow::bail!("Unsupported operation."),
    }
    Ok((
        if output.is_empty() {
            "Operation applied successfully.".into()
        } else {
            output
        },
        None,
    ))
}
async fn cache_diagnostics(
    path: &Path,
    key: &str,
    started: std::time::SystemTime,
) -> Result<String> {
    let config: grading_executor::Config = toml::from_str(&tokio::fs::read_to_string(path).await?)?;
    let mut output = format!("Cache input key: {key}\n");
    let root = config.staging_root.join("cache-builds");
    let Ok(mut entries) = tokio::fs::read_dir(root).await else {
        return Ok(output + "No new preparation attempt; cache may have been reused.");
    };
    let mut found = 0;
    use tokio::io::AsyncReadExt;
    while let Some(entry) = entries.next_entry().await? {
        if !entry.file_type().await?.is_dir()
            || !grading_core::security::valid_hex(&entry.file_name().to_string_lossy(), 32)
        {
            continue;
        }
        let metadata = entry.path().join("metadata.json");
        if !tokio::fs::metadata(&metadata)
            .await
            .is_ok_and(|m| m.modified().is_ok_and(|time| time >= started))
        {
            continue;
        }
        let mut bytes = Vec::new();
        tokio::fs::File::open(metadata)
            .await?
            .take(8192)
            .read_to_end(&mut bytes)
            .await?;
        let data: Value = serde_json::from_slice(&bytes)?;
        if data["input_key"] != key {
            continue;
        }
        output.push_str(&format!(
            "Job {} in namespace {}\n",
            data["job"], data["namespace"]
        ));
        if let Ok(file) = tokio::fs::File::open(entry.path().join("status.json")).await {
            let mut bytes = Vec::new();
            file.take(16384).read_to_end(&mut bytes).await?;
            output.push_str("Pod status:\n");
            output.push_str(&String::from_utf8_lossy(&bytes));
            output.push('\n');
        }
        if let Ok(file) = tokio::fs::File::open(entry.path().join("preparation.log")).await {
            let mut bytes = Vec::new();
            file.take(65536).read_to_end(&mut bytes).await?;
            output.push_str(&String::from_utf8_lossy(&bytes));
            output.push('\n');
        } else {
            output.push_str("No preparation log was available. Check the Job's events (image pull, scheduling, quota or timeout).\n");
        }
        found += 1;
        if found >= 4 {
            break;
        }
    }
    if found == 0 {
        output.push_str(
            "No new preparation Job; cache reused or preparation stopped before Job creation.\n",
        );
    }
    Ok(output)
}
fn diagnostic(error: &anyhow::Error) -> String {
    let details = grading_store::diagnostics(error);
    let message = if error.downcast_ref::<sqlx::Error>().is_some() {
        match details.reason{"record_not_found"=>"The selected record no longer exists. Validate again.","database_permission_denied"=>"The administration worker database role lacks a required permission. Apply current database grants.","database_unique_conflict"=>"A conflicting record already exists. Review the current course or roster and validate again.","database_foreign_key"=>"A referenced course, student or record does not exist.",_=>"The database operation failed. Check the administration worker logs using this operation ID."}.to_owned()
    } else if error.downcast_ref::<reqwest::Error>().is_some() {
        "GitHub or another upstream service could not be reached. Check connectivity, credentials and rate limits, then validate again.".into()
    } else {
        format!("{error:#}")
    };
    format!(
        "{}\n\nDiagnostic stage: {}\nCategory: {}\nUpstream status: {}",
        operations::truncate(&message, 16_000),
        details.stage,
        details.reason,
        details
            .upstream_status
            .map(|s| s.to_string())
            .unwrap_or_else(|| "none".into())
    )
}
async fn process(
    context: &Context<'_>,
    id: Uuid,
    actor: i64,
    input: Value,
    phase: &str,
    plan: Option<Value>,
) -> Result<()> {
    let authorized: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM admins WHERE github_id=$1)")
            .bind(actor)
            .fetch_one(&context.pool)
            .await?;
    ensure!(
        authorized,
        "Administrator access was revoked; the operation was not executed."
    );
    if phase == "validating" {
        let input: Input = decode(&input)?;
        let (plan, report) = prepare(context, input)
            .await
            .context(grading_core::diagnostics::Stage("admin_validation"))?;
        let token = security::token();
        sqlx::query("UPDATE admin_operations SET state='ready',plan=$2,output=$3,confirmation_token=$4,confirmation_hash=$5,validated_until=now()+interval '15 minutes',updated_at=now() WHERE id=$1 AND state='validating'").bind(id).bind(serde_json::to_value(plan)?).bind(operations::truncate(&report,900_000)).bind(&token).bind(security::digest(&token)).execute(&context.pool).await?;
    } else {
        let plan: Plan = decode(&plan.context("Missing validated plan.")?)?;
        ensure!(
            serde_json::to_value(&plan.input)? == input,
            "Validated input changed."
        );
        let (result, download) = apply(context, plan, id)
            .await
            .context(grading_core::diagnostics::Stage("admin_application"))?;
        // A crash after application but before this commit is deliberately not retried automatically.
        let mut tx = context.pool.begin().await?;
        submissions::audit(
            &mut tx,
            &context.actor,
            "admin_portal.applied",
            id,
            "Confirmed through administrator portal",
        )
        .await?;
        sqlx::query("UPDATE admin_operations SET state='succeeded',output=left(output,100000)||E'\\n\\nRESULT\\n'||$2,download=$3,updated_at=now() WHERE id=$1 AND state='applying'").bind(id).bind(operations::truncate(&result,400_000)).bind(download).execute(&mut *tx).await?;
        tx.commit().await?;
    }
    Ok(())
}
pub async fn work(args: &Args, options: &Options) -> Result<()> {
    let pool = pool(args).await?;
    // One privileged dispatcher at a time; the dedicated connection releases its
    // session lock on process death instead of returning the lock to a pool.
    let mut guard = pool.acquire().await?.detach();
    let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock(704321)")
        .fetch_one(&mut guard)
        .await?;
    ensure!(locked, "Another administration worker is already running.");
    sqlx::query("UPDATE admin_operations SET state=CASE WHEN state='validating' THEN 'pending_validation' ELSE 'uncertain' END,output=output||E'\\nWorker restarted. An interrupted apply is not replayed automatically; inspect current state before validating again.',updated_at=now() WHERE state IN ('validating','applying')").execute(&pool).await?;
    loop {
        // Losing this lock connection terminates the dispatcher before another operation.
        sqlx::query("SELECT 1").execute(&mut guard).await?;
        sqlx::query("INSERT INTO admin_worker_status(id,heartbeat) VALUES(true,now()) ON CONFLICT(id) DO UPDATE SET heartbeat=now()").execute(&pool).await?;
        sqlx::query("UPDATE admin_operations SET state='expired',confirmation_token=NULL,updated_at=now() WHERE state='ready' AND validated_until<=now()").execute(&pool).await?;
        let mut tx = pool.begin().await?;
        let selected:Option<(Uuid,i64,Value,String,Option<Value>)>=sqlx::query_as("SELECT id,actor,input,state,plan FROM admin_operations WHERE state IN ('pending_validation','queued') ORDER BY created_at,id LIMIT 1 FOR UPDATE SKIP LOCKED").fetch_optional(&mut *tx).await?;
        if let Some((id, actor, input, state, plan)) = selected {
            let phase = if state == "queued" {
                "applying"
            } else {
                "validating"
            };
            sqlx::query("UPDATE admin_operations SET state=$2,output=CASE WHEN $2='validating' THEN 'Validation started. Resolving inputs and checking current state…' ELSE output||E'\\n\\nConfirmed. Applying the validated operation…' END,updated_at=now() WHERE id=$1").bind(id).bind(phase).execute(&mut *tx).await?;
            tx.commit().await?;
            let context = Context {
                args,
                options,
                pool: pool.clone(),
                actor: format!("github:{actor}; admin-operation:{id}"),
            };
            let execution = PORTAL_OPERATOR.scope(
                context.actor.clone(),
                process(&context, id, actor, input, phase, plan),
            );
            tokio::pin!(execution);
            let mut heartbeat = tokio::time::interval(Duration::from_secs(5));
            let result = loop {
                tokio::select! {result=&mut execution=>break result,_=heartbeat.tick()=>{
                    sqlx::query("SELECT 1").execute(&mut guard).await?;
                    sqlx::query("UPDATE admin_worker_status SET heartbeat=now() WHERE id=true").execute(&pool).await?;
                let authorized:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM admins WHERE github_id=$1)").bind(actor).fetch_one(&pool).await?;
                if !authorized { break Err(anyhow::anyhow!("Administrator access was revoked while this operation was running.")); }
                }}
            };
            if let Err(error) = result {
                let message = diagnostic(&error);
                let prefix = if phase == "validating" {
                    "Validation failed. Nothing was applied.\n"
                } else {
                    "Application failed. Some external or multi-step work may have completed; inspect diagnostics and current state before validating again.\n"
                };
                tracing::warn!(operation_id=%id,phase,reason=grading_store::diagnostics(&error).reason,"Admin operation failed");
                sqlx::query("UPDATE admin_operations SET state='failed',confirmation_token=NULL,output=left(output,100000)||E'\\n\\n'||$2,updated_at=now() WHERE id=$1").bind(id).bind(format!("{prefix}{message}")).execute(&pool).await?;
            }
        } else {
            tx.rollback().await?;
            if !options.once {
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
        if options.once {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn portal_dry_run_confirmation_and_execution_are_separate() -> Result<()> {
        let Ok(url) = std::env::var("TEST_OPERATOR_DATABASE_URL") else {
            eprintln!("Portal DB test skipped; use just test");
            return Ok(());
        };
        let pool = PgPool::connect(&url).await?;
        let web = PgPool::connect(&std::env::var("TEST_WEB_DATABASE_URL")?).await?;
        let admin = PgPool::connect(&std::env::var("TEST_ADMIN_DATABASE_URL")?).await?;
        let owner = PgPool::connect(&std::env::var("TEST_DATABASE_URL")?).await?;
        let actor = 902001;
        let raw = identity::new_session(&web, actor, "portal-instructor", None).await?;
        let session = identity::session(&web, &raw).await?.unwrap();
        identity::admin_change(&admin, actor, true, false, "fixture", "portal test").await?;
        let root = std::env::temp_dir().join(format!("portal-test-{}", Uuid::new_v4()));
        tokio::fs::create_dir(&root).await?;
        let db_file = root.join("operator-url");
        write_new(&db_file, url.as_bytes()).await?;
        let args = Args {
            database_url_file: Some(db_file),
            github_config: None,
            artifact_dir: root.join("artifacts"),
            command: Command::Sync,
        };
        let options = Options {
            once: true,
            admin_database_url_file: None,
            migration_database_url_file: None,
            cache_config: None,
        };
        let course = "portal-fixture";
        let input = Input {
            action: "course".into(),
            fields: BTreeMap::new(),
            content: format!(
                "schema_version=1\n[course]\nid='{course}'\ntitle='Portal course'\ngithub_organization='fixture-org'\ntimezone='UTC'\n"
            ),
            reason: "Test validation boundary".into(),
        };
        let id = operations::submit(&web, actor, &session.csrf, &input).await?;
        assert!(!operations::confirm(&web, id, actor, &session.csrf, "invented").await?);
        work(&args, &options).await?;
        let ready = operations::get(&web, id, actor).await?;
        assert!(ready.confirmable, "{}", ready.output);
        assert!(ready.output.contains("Database dry run rolled back"));
        let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM courses WHERE id=$1)")
            .bind(course)
            .fetch_one(&pool)
            .await?;
        assert!(!exists);
        let token = ready.confirmation_token.as_deref().unwrap();
        assert!(!operations::confirm(&web, id, actor, "other-session", token).await?);
        assert!(!operations::confirm(&web, id, actor + 1, &session.csrf, token).await?);
        assert!(
            sqlx::query("UPDATE admin_operations SET state='succeeded' WHERE id=$1")
                .bind(id)
                .execute(&web)
                .await
                .is_err()
        );
        assert!(operations::confirm(&web, id, actor, &session.csrf, token).await?);
        assert!(!operations::confirm(&web, id, actor, &session.csrf, token).await?);
        work(&args, &options).await?;
        let done = operations::get(&web, id, actor).await?;
        assert_eq!(done.state, "succeeded", "{}", done.output);
        let title: String = sqlx::query_scalar("SELECT title FROM courses WHERE id=$1")
            .bind(course)
            .fetch_one(&pool)
            .await?;
        assert_eq!(title, "Portal course");
        let audited:bool=sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM audit_events WHERE action='course.apply' AND operator LIKE 'github:902001;%')").fetch_one(&pool).await?;
        assert!(audited);

        let catalog = Input {
            action: "exercises".into(),
            fields: BTreeMap::new(),
            content: format!("schema_version=1\ncourse='{course}'\n"),
            reason: "Empty course catalog".into(),
        };
        let empty = operations::submit(&web, actor, &session.csrf, &catalog).await?;
        work(&args, &options).await?;
        let ready = operations::get(&web, empty, actor).await?;
        assert!(ready.confirmable, "{}", ready.output);
        operations::confirm(
            &web,
            empty,
            actor,
            &session.csrf,
            ready.confirmation_token.as_deref().unwrap(),
        )
        .await?;
        work(&args, &options).await?;
        assert_eq!(
            operations::get(&web, empty, actor).await?.state,
            "succeeded"
        );

        let stale = operations::submit(&web, actor, &session.csrf, &input).await?;
        work(&args, &options).await?;
        let ready = operations::get(&web, stale, actor).await?;
        sqlx::query("UPDATE courses SET title='Concurrent change' WHERE id=$1")
            .bind(course)
            .execute(&pool)
            .await?;
        assert!(
            operations::confirm(
                &web,
                stale,
                actor,
                &session.csrf,
                ready.confirmation_token.as_deref().unwrap()
            )
            .await?
        );
        work(&args, &options).await?;
        let failed = operations::get(&web, stale, actor).await?;
        assert_eq!(failed.state, "failed");
        assert!(failed.output.contains("changed since validation"));

        let mut invalid = input.clone();
        invalid.content = "schema_version = invalid".into();
        let id = operations::submit(&web, actor, &session.csrf, &invalid).await?;
        work(&args, &options).await?;
        let failed = operations::get(&web, id, actor).await?;
        assert_eq!(failed.state, "failed");
        assert!(!failed.confirmable);
        assert!(failed.output.contains("Validation failed"));

        let expired = operations::submit(&web, actor, &session.csrf, &input).await?;
        work(&args, &options).await?;
        let ready = operations::get(&web, expired, actor).await?;
        sqlx::query(
            "UPDATE admin_operations SET validated_until=now()-interval '1 second' WHERE id=$1",
        )
        .bind(expired)
        .execute(&pool)
        .await?;
        assert!(
            !operations::confirm(
                &web,
                expired,
                actor,
                &session.csrf,
                ready.confirmation_token.as_deref().unwrap()
            )
            .await?
        );

        let token_hash = security::digest("a-test-worker-token-with-at-least-43-characters");
        let worker = Input {
            action: "worker_register".into(),
            fields: BTreeMap::from([
                ("id".into(), "portal-worker".into()),
                ("cpu".into(), "1".into()),
                ("memory_gib".into(), "1".into()),
                ("storage_gib".into(), "1".into()),
            ]),
            content: token_hash.clone(),
            reason: "Register worker".into(),
        };
        let id = operations::submit(&web, actor, &session.csrf, &worker).await?;
        work(&args, &options).await?;
        let ready = operations::get(&web, id, actor).await?;
        assert!(ready.confirmable, "{}", ready.output);
        assert!(!ready.output.contains(&token_hash));
        assert!(
            operations::confirm(
                &web,
                id,
                actor,
                &session.csrf,
                ready.confirmation_token.as_deref().unwrap()
            )
            .await?
        );
        work(&args, &options).await?;
        let hash: String =
            sqlx::query_scalar("SELECT token_hash FROM workers WHERE id='portal-worker'")
                .fetch_one(&pool)
                .await?;
        assert_eq!(hash, token_hash);
        sqlx::query("DELETE FROM workers WHERE id='portal-worker'")
            .execute(&owner)
            .await?;

        let export = Input {
            action: "export".into(),
            fields: BTreeMap::from([("course".into(), course.into())]),
            content: String::new(),
            reason: "Download".into(),
        };
        let id = operations::submit(&web, actor, &session.csrf, &export).await?;
        work(&args, &options).await?;
        let ready = operations::get(&web, id, actor).await?;
        assert!(ready.confirmable, "{}", ready.output);
        operations::confirm(
            &web,
            id,
            actor,
            &session.csrf,
            ready.confirmation_token.as_deref().unwrap(),
        )
        .await?;
        work(&args, &options).await?;
        assert!(
            operations::get(&web, id, actor)
                .await?
                .download
                .unwrap()
                .contains("provisional_points")
        );

        let interrupted = operations::submit(&web, actor, &session.csrf, &input).await?;
        sqlx::query("UPDATE admin_operations SET state='applying' WHERE id=$1")
            .bind(interrupted)
            .execute(&pool)
            .await?;
        work(&args, &options).await?;
        assert_eq!(
            operations::get(&web, interrupted, actor).await?.state,
            "uncertain"
        );
        let revoked = operations::submit(&web, actor, &session.csrf, &input).await?;
        identity::admin_change(
            &admin,
            actor,
            false,
            true,
            "fixture",
            "Revoke before worker executes",
        )
        .await?;
        work(&args, &options).await?;
        assert_eq!(operations::get(&web, revoked, actor).await?.state, "failed");
        sqlx::query("DELETE FROM courses WHERE id=$1")
            .bind(course)
            .execute(&owner)
            .await?;
        tokio::fs::remove_dir_all(root).await?;
        Ok(())
    }
}
