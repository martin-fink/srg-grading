//! Allowlisted portal actions; never accept shell commands or server file paths.
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const MAX_INPUT: usize = 1024 * 1024;
pub struct Action {
    pub id: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    pub fields: &'static [(&'static str, &'static str)],
    pub content: bool,
    pub example: &'static str,
}
pub const ACTIONS: &[Action] = &[
    Action {
        id: "course",
        title: "Create or update course",
        description: "Validate course TOML and preview the course changes.",
        fields: &[],
        content: true,
        example: include_str!("../../../examples/course.toml"),
    },
    Action {
        id: "roster",
        title: "Import students",
        description: "Resolve GitHub accounts and check enrollment conflicts. Students omitted from the import remain enrolled.",
        fields: &[("course", "Course ID"), ("format", "Format: csv or toml")],
        content: true,
        example: "student_id,name,github_username\nstudent-01,Example Student,github-handle\n",
    },
    Action {
        id: "exercises",
        title: "Apply exercise catalog",
        description: "Validate and pin all exercises in a TOML catalog. Exercises omitted from the selected course are archived. Cache preparation happens after confirmation.",
        fields: &[],
        content: true,
        example: include_str!("../../../examples/exercises.toml"),
    },
    Action {
        id: "exercise_add",
        title: "Add exercise",
        description: "Pin template and grader commits, validate schema 3, then prepare caches before publication.",
        fields: &[
            ("course", "Course ID"),
            ("name", "Exercise name"),
            ("template", "Template owner/repository"),
            ("grader", "Grader owner/repository"),
            ("template_ref", "Template ref (default main)"),
            ("grader_ref", "Grader ref (default main)"),
            ("runner_image", "Approved runner image digest"),
            ("opens_at", "Opens at (RFC 3339, including timezone)"),
            ("deadline", "Deadline (RFC 3339, including timezone)"),
        ],
        content: false,
        example: "",
    },
    Action {
        id: "exercise_update",
        title: "Update exercise",
        description: "Omitted settings retain the current revision. Set existing=true to update grading for existing repositories.",
        fields: &[
            ("course", "Course ID"),
            ("name", "Exercise name"),
            ("template", "Template owner/repository (optional)"),
            ("grader", "Grader owner/repository (optional)"),
            ("template_ref", "Template ref (optional)"),
            ("grader_ref", "Grader ref (optional)"),
            ("runner_image", "Approved runner image digest (optional)"),
            ("opens_at", "Opens at (optional RFC 3339)"),
            ("deadline", "Deadline (optional RFC 3339)"),
            ("existing", "Apply to existing repositories: true or false"),
        ],
        content: false,
        example: "",
    },
    Action {
        id: "exercise_show",
        title: "Show exercise and cache",
        description: "View the immutable current definition, source pins and cache digests.",
        fields: &[("course", "Course ID"), ("name", "Exercise name")],
        content: false,
        example: "",
    },
    Action {
        id: "private_grade",
        title: "Run private grading",
        description: "Preview final submissions and effective deadlines before scheduling private grading.",
        fields: &[("course", "Course ID"), ("name", "Exercise name")],
        content: false,
        example: "",
    },
    Action {
        id: "export",
        title: "Export grades",
        description: "Preview course scope, then download a spreadsheet-safe CSV with final and provisional grades.",
        fields: &[("course", "Course ID")],
        content: false,
        example: "",
    },
    Action {
        id: "override",
        title: "Override grade",
        description: "Record an audited points override for a repository.",
        fields: &[("repository", "Repository UUID"), ("points", "Points")],
        content: false,
        example: "",
    },
    Action {
        id: "extension",
        title: "Extend deadline",
        description: "Preview the previous and proposed deadline. Closed assignments cannot be extended.",
        fields: &[
            ("repository", "Repository UUID"),
            ("deadline", "New deadline (RFC 3339, including timezone)"),
        ],
        content: false,
        example: "",
    },
    Action {
        id: "regrade",
        title: "Regrade submission",
        description: "Queue a new public grading attempt for retained source.",
        fields: &[("submission", "Submission UUID")],
        content: false,
        example: "",
    },
    Action {
        id: "select_submission",
        title: "Select final submission",
        description: "Override the selected event for a closed assignment, with an audit reason.",
        fields: &[("event", "Submission event UUID")],
        content: false,
        example: "",
    },
    Action {
        id: "retry_private",
        title: "Retry private grading",
        description: "Retry the latest failed private run without changing its pinned baseline.",
        fields: &[("run", "Failed run UUID")],
        content: false,
        example: "",
    },
    Action {
        id: "retry_task",
        title: "Retry failed task",
        description: "Retry a failed provisioning, snapshot, publication or locking task.",
        fields: &[("task", "Failed task UUID")],
        content: false,
        example: "",
    },
    Action {
        id: "worker_register",
        title: "Register or rotate worker",
        description: "Paste or upload a random worker token (at least 43 characters). Only its hash is retained; existing credentials for this worker are replaced.",
        fields: &[
            ("id", "Worker ID"),
            ("cpu", "CPU limit"),
            ("memory_gib", "Memory limit (GiB)"),
            ("storage_gib", "Storage limit (GiB)"),
        ],
        content: true,
        example: "",
    },
    Action {
        id: "worker_revoke",
        title: "Revoke worker",
        description: "Disable this worker's API access.",
        fields: &[("id", "Worker ID")],
        content: false,
        example: "",
    },
    Action {
        id: "admin_list",
        title: "List administrators",
        description: "List administrator GitHub identities.",
        fields: &[],
        content: false,
        example: "",
    },
    Action {
        id: "admin_grant",
        title: "Grant administrator",
        description: "Resolve the GitHub handle and preview the exact account receiving access.",
        fields: &[("github_username", "GitHub username")],
        content: false,
        example: "",
    },
    Action {
        id: "admin_revoke",
        title: "Revoke administrator",
        description: "Preview the exact account losing access. Recovery override can remove the last administrator and lock everyone out of this portal.",
        fields: &[
            ("github_username", "GitHub username"),
            (
                "recovery_override",
                "Recovery override: true or false (normally false)",
            ),
        ],
        content: false,
        example: "",
    },
    Action {
        id: "sync",
        title: "Synchronize repositories",
        description: "Inspect repository scope before reconciling GitHub state and enforcing expired deadlines.",
        fields: &[],
        content: false,
        example: "",
    },
    Action {
        id: "work",
        title: "Process queued work once",
        description: "Process one queued control operation. The background service normally handles this automatically.",
        fields: &[],
        content: false,
        example: "",
    },
    Action {
        id: "migrate",
        title: "Apply database migrations",
        description: "Inspect applied and pending schema versions. Requires an explicitly configured migration connection on the admin worker.",
        fields: &[],
        content: false,
        example: "",
    },
];
pub fn action(id: &str) -> Option<&'static Action> {
    ACTIONS.iter().find(|a| a.id == id)
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Input {
    pub action: String,
    pub fields: BTreeMap<String, String>,
    pub content: String,
    pub reason: String,
}
impl Input {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.content.contains('\0')
                && !self.reason.contains('\0')
                && self.fields.values().all(|v| !v.contains('\0')),
            "Text fields and uploaded files must not contain NUL characters."
        );
        let action = action(&self.action)
            .ok_or_else(|| anyhow::anyhow!("Unknown administrative action."))?;
        ensure!(
            self.content.len() <= MAX_INPUT,
            "Input exceeds 1 MiB. Split the import into smaller files."
        );
        ensure!(
            !self.reason.trim().is_empty() && self.reason.len() <= 2048,
            "Provide an audit reason (at most 2,048 characters)."
        );
        ensure!(
            self.fields
                .iter()
                .all(|(k, v)| action.fields.iter().any(|(name, _)| name == k) && v.len() <= 2048),
            "Unknown field or field longer than 2,048 characters."
        );
        ensure!(
            action.content || self.content.is_empty(),
            "This action does not accept file contents."
        );
        ensure!(
            !action.content || !self.content.trim().is_empty(),
            "Upload a file or paste its contents."
        );
        Ok(())
    }
    pub fn field(&self, name: &str) -> &str {
        self.fields.get(name).map_or("", |s| s.trim())
    }
}
