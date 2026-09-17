//! Local exercise catalogs, complete previews, and confirmed atomic application.
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use clap::Args;
use grading_core::{config::identifier, security::digest};
use grading_store::{
    artifacts::Artifacts,
    exercises::{self, Publication},
};
use serde::Deserialize;
use sqlx::PgPool;
use std::{
    collections::BTreeMap,
    io::{IsTerminal, Write},
    path::PathBuf,
};

#[derive(Args)]
pub struct Apply {
    #[arg(required = true, num_args = 1..)]
    files: Vec<PathBuf>,
    #[arg(long)]
    reason: String,
    #[arg(long)]
    dry_run: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    schema_version: u32,
    course: String,
    runner_image: Option<String>,
    #[serde(default)]
    exercises: BTreeMap<String, Entry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Entry {
    template: String,
    #[serde(default = "main_ref")]
    template_ref: String,
    grader: String,
    #[serde(default = "main_ref")]
    grader_ref: String,
    runner_image: Option<String>,
    #[serde(deserialize_with = "grading_core::config::datetime")]
    opens_at: DateTime<Utc>,
    #[serde(deserialize_with = "grading_core::config::datetime")]
    deadline: DateTime<Utc>,
    #[serde(default)]
    existing: bool,
}

fn main_ref() -> String {
    "main".into()
}

fn merge(files: Vec<File>) -> Result<BTreeMap<String, BTreeMap<String, Entry>>> {
    let mut courses: BTreeMap<String, BTreeMap<String, Entry>> = BTreeMap::new();
    for file in files {
        ensure!(
            file.schema_version == 1 && identifier(&file.course),
            "invalid exercise catalog schema/course"
        );
        let entries = courses.entry(file.course.clone()).or_default();
        for (name, mut entry) in file.exercises {
            ensure!(identifier(&name), "invalid exercise name {name}");
            ensure!(
                entry.opens_at < entry.deadline,
                "invalid times for {}/{name}",
                file.course
            );
            if entry.runner_image.is_none() {
                entry.runner_image = file.runner_image.clone();
            }
            ensure!(
                entries.insert(name.clone(), entry).is_none(),
                "duplicate exercise {}/{name} across input files",
                file.course
            );
        }
    }
    Ok(courses)
}

pub async fn apply(pool: &PgPool, args: &super::Args, options: &Apply) -> Result<()> {
    ensure!(
        !options.reason.trim().is_empty(),
        "--reason must not be empty"
    );
    let mut files = Vec::new();
    let mut hashes = Vec::new();
    for path in &options.files {
        let bytes = tokio::fs::read(path)
            .await
            .with_context(|| format!("read {}", path.display()))?;
        hashes.push(digest(&bytes));
        files.push(
            toml::from_str::<File>(std::str::from_utf8(&bytes)?)
                .with_context(|| format!("parse {}", path.display()))?,
        );
    }
    let desired = merge(files)?;
    let mut before = BTreeMap::new();
    for course in desired.keys() {
        let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM courses WHERE id=$1)")
            .bind(course)
            .fetch_one(pool)
            .await?;
        ensure!(exists, "import course {course} before applying exercises");
        before.insert(course.clone(), exercises::catalog(pool, course).await?);
    }
    let github = if desired.values().any(|entries| !entries.is_empty()) {
        Some(super::github(args).await?)
    } else {
        None
    };
    let mut prepared = Vec::new();
    for (course, entries) in &desired {
        for (name, entry) in entries {
            let previous = before[course].iter().find(|e| e.slug == *name);
            let register = super::exercises::Register {
                course: course.clone(),
                name: name.clone(),
                template: Some(entry.template.clone()),
                grader: Some(entry.grader.clone()),
                template_ref: Some(entry.template_ref.clone()),
                grader_ref: Some(entry.grader_ref.clone()),
                opens_at: Some(entry.opens_at),
                deadline: Some(entry.deadline),
                existing: entry.existing,
                reason: options.reason.clone(),
                dry_run: true,
                build_config: None,
                runner_image: entry.runner_image.clone(),
            };
            let item = super::exercises::prepare(
                pool,
                github.as_ref().context("missing GitHub adapter")?,
                &register,
                previous.is_some(),
            )
            .await?;
            ensure!(
                item.source.is_some(),
                "file imports require grader exercise.toml schema 3 (shared runner)"
            );
            let revision = item.revision.digest()?;
            let action = match previous {
                None => "ADD",
                Some(old) if old.archived => "RESTORE",
                Some(old) if old.current_revision.as_deref() == Some(&revision) => "UNCHANGED",
                Some(_) => "UPDATE",
            };
            println!(
                "{action} {course}/{name}: revision {revision}; existing={}",
                entry.existing
            );
            prepared.push((item, entry.existing));
        }
    }
    let removals: Vec<_> = before
        .iter()
        .flat_map(|(course, entries)| {
            entries
                .iter()
                .filter(|entry| !entry.archived && !desired[course].contains_key(&entry.slug))
                .map(move |entry| format!("{course}/{}", entry.slug))
        })
        .collect();
    if !removals.is_empty() {
        eprintln!(
            "\n!!! WARNING: REMOVING {} EXERCISE(S) FROM THE ACTIVE CATALOG !!!",
            removals.len()
        );
        for name in &removals {
            eprintln!("  REMOVE {name}");
        }
        eprintln!(
            "New repository creation will stop. Existing repositories, grading and history are preserved."
        );
    }
    if options.dry_run {
        println!(
            "DRY RUN: no artifacts, course state or grades changed; removals require confirmation on apply."
        );
        return Ok(());
    }
    if !removals.is_empty() {
        confirm_removals()?;
    }
    let reason = format!("{}; manifest_sha256={}", options.reason, hashes.join(","));
    let operator = super::operator();
    if !prepared.is_empty() {
        let artifacts = Artifacts::new(&args.artifact_dir).await?;
        for (item, _) in &prepared {
            artifacts
                .put(
                    pool,
                    "source",
                    item.source.as_ref().context("missing grader source")?,
                )
                .await?;
        }
    }
    let publications: Vec<_> = prepared
        .iter()
        .map(|(item, existing)| Publication {
            revision: &item.revision,
            expected: item.expected.as_deref(),
            existing: *existing,
            dry_run: false,
            operator: &operator,
            reason: &reason,
        })
        .collect();
    exercises::apply_set(pool, &before, &publications, &operator, &reason, false).await?;
    println!(
        "Applied {} exercise(s), retired {} exercise(s).",
        prepared.len(),
        removals.len()
    );
    Ok(())
}

fn confirm_removals() -> Result<()> {
    ensure!(
        std::io::stdin().is_terminal(),
        "removals require interactive confirmation; rerun in a terminal (or use --dry-run)"
    );
    eprint!("Type REMOVE EXERCISES to confirm the listed removals: ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    ensure!(
        answer.trim() == "REMOVE EXERCISES",
        "cancelled; no exercises changed"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn catalogs_merge_and_reject_duplicates_or_unknown_fields() {
        let text = include_str!("../../../examples/exercises.toml");
        let courses = merge(vec![toml::from_str(text).unwrap()]).unwrap();
        assert_eq!(courses["systems-2026"]["echo"].template_ref, "main");
        let second = text.replace("[exercises.echo]", "[exercises.second]");
        let merged = merge(vec![
            toml::from_str(text).unwrap(),
            toml::from_str(&second).unwrap(),
        ])
        .unwrap();
        assert_eq!(merged["systems-2026"].len(), 2);
        assert_eq!(merged["systems-2026"]["second"].grader_ref, "b".repeat(40));
        assert!(
            merge(vec![
                toml::from_str(text).unwrap(),
                toml::from_str(text).unwrap()
            ])
            .is_err()
        );
        assert!(toml::from_str::<File>(&format!("unknown=true\n{text}")).is_err());
        let empty: File = toml::from_str("schema_version=1\ncourse='systems-2026'").unwrap();
        assert!(merge(vec![empty]).unwrap()["systems-2026"].is_empty());
    }

    #[test]
    fn command_accepts_multiple_files_and_dry_run() {
        use clap::Parser;
        let parsed = crate::Args::try_parse_from([
            "gradingctl",
            "exercise",
            "apply",
            "first.toml",
            "second.toml",
            "--reason",
            "Course update",
            "--dry-run",
        ])
        .unwrap();
        let crate::Command::Exercise {
            command: super::super::exercises::ExerciseCommand::Apply(options),
        } = parsed.command
        else {
            panic!("wrong command");
        };
        assert_eq!(options.files.len(), 2);
        assert!(options.dry_run);
    }
}
