//! Escaped view models for the minimal TUM-blue website.
use askama::Template;
use chrono::Utc;
use grading_store::courses::DashboardRow;

#[derive(Template)]
#[template(path = "dashboard.html")]
pub struct Dashboard {
    pub login: String,
    pub csrf: String,
    pub authenticated: bool,
    pub admin: bool,
    pub preview: bool,
    pub rows: Vec<AssignmentRow>,
}

impl Dashboard {
    pub fn has_creating_repository(&self) -> bool {
        self.rows.iter().any(|row| row.creating)
    }
}

pub struct AssignmentRow {
    pub id: String,
    pub course: String,
    pub title: String,
    pub deadline: String,
    pub repository_id: String,
    pub repository_url: String,
    pub invitation_url: String,
    pub state: String,
    pub status: String,
    pub sha: String,
    pub points: String,
    pub public_points: String,
    pub run_id: String,
    pub creating: bool,
    pub can_create: bool,
    pub can_submit: bool,
    pub notice: String,
}

impl From<DashboardRow> for AssignmentRow {
    fn from(row: DashboardRow) -> Self {
        let timezone = row
            .timezone
            .parse::<chrono_tz::Tz>()
            .unwrap_or(chrono_tz::UTC);
        let now = Utc::now();
        let open = now >= row.opens_at && now <= row.deadline && !row.closure_due.unwrap_or(false);
        let state = if row.locked_at.is_some() {
            "Read-only".into()
        } else if row.closure_due.unwrap_or(false) {
            "Closing — lock pending".into()
        } else {
            match row.state.as_deref() {
                Some("ready") => "Ready",
                Some("creating") => "Creating repository",
                Some("invitation_pending") => "Invitation pending",
                _ if now < row.opens_at => "Not yet open",
                _ if now > row.deadline => "Closed",
                _ => "Available",
            }
            .into()
        };
        let points = match row.override_points.or(row.points) {
            Some(points) => format!(
                "{points} / {}{}",
                row.max_points,
                if row.override_points.is_some() {
                    " (override)"
                } else {
                    ""
                }
            ),
            None => format!("— / {}", row.max_points),
        };
        Self {
            id: row.assignment_id.to_string(),
            course: row.course,
            title: row.title,
            deadline: row
                .deadline
                .with_timezone(&timezone)
                .format("%d %b %Y, %H:%M %Z")
                .to_string(),
            repository_id: row
                .repository_id
                .map(|id| id.to_string())
                .unwrap_or_default(),
            repository_url: row
                .repository_name
                .map(|name| format!("https://github.com/{}/{name}", row.organization))
                .unwrap_or_default(),
            invitation_url: row.invitation_url.unwrap_or_default(),
            state,
            status: format!(
                "{}{}",
                row.status
                    .unwrap_or_else(|| "No official result".into())
                    .replace('_', " "),
                if row.private_grading {
                    " · private grading"
                } else {
                    ""
                }
            ),
            sha: row.sha.unwrap_or_default(),
            points,
            public_points: row
                .public_points
                .map(|p| p.to_string())
                .unwrap_or_else(|| "—".into()),
            run_id: row.run_id.map(|id| id.to_string()).unwrap_or_default(),
            creating: row.state.as_deref() == Some("creating") && !row.closure_due.unwrap_or(false),
            can_create: open && row.repository_id.is_none(),
            can_submit: open && row.state.as_deref() == Some("ready"),
            notice: if row.needs_review.unwrap_or(false) {
                "Submission evidence needs instructor review.".into()
            } else if row.last_error.is_some() {
                "An operation needs attention; it will be retried.".into()
            } else {
                String::new()
            },
        }
    }
}

pub fn preview() -> Dashboard {
    Dashboard {
        login: "student-preview".into(),
        csrf: String::new(),
        authenticated: true,
        admin: false,
        preview: true,
        rows: vec![
            AssignmentRow {
                id: String::new(),
                course: "Practical Systems".into(),
                title: "C echo exercise".into(),
                deadline: "26 Oct 2026, 23:59 CET".into(),
                repository_id: String::new(),
                repository_url: String::new(),
                invitation_url: String::new(),
                state: "Ready".into(),
                status: "completed".into(),
                sha: "79bf205b56a2e5042e8efdf261e2a76ce713cd46".into(),
                points: "16 / 20".into(),
                public_points: "16".into(),
                run_id: String::new(),
                creating: false,
                can_create: false,
                can_submit: false,
                notice: String::new(),
            },
            AssignmentRow {
                id: String::new(),
                course: "Practical Systems".into(),
                title: "Simulation exercise".into(),
                deadline: "09 Nov 2026, 23:59 CET".into(),
                repository_id: String::new(),
                repository_url: String::new(),
                invitation_url: String::new(),
                state: "Not yet open".into(),
                status: "No official result".into(),
                sha: String::new(),
                points: "— / 20".into(),
                public_points: "—".into(),
                run_id: String::new(),
                creating: false,
                can_create: false,
                can_submit: false,
                notice: String::new(),
            },
        ],
    }
}

#[derive(Template)]
#[template(path = "admin.html")]
pub struct AdminPage {
    pub actions: &'static [grading_core::admin::Action],
    pub operations: Vec<(uuid::Uuid, String, String)>,
    pub login: String,
    pub rows: Vec<AdminRow>,
}

#[derive(Template)]
#[template(path = "logs.html")]
pub struct LogsPage {
    pub status: String,
    pub sha: String,
    pub text: String,
    pub public_run_id: String,
}

pub struct AdminRow {
    pub course: String,
    pub enrolled: i64,
    pub repositories: i64,
    pub needs_review: i64,
    pub failed_tasks: i64,
}
