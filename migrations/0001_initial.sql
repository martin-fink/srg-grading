CREATE TABLE users (
    github_id bigint PRIMARY KEY CHECK (github_id > 0),
    login text NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE admins (
    github_id bigint PRIMARY KEY CHECK (github_id > 0),
    granted_at timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE audit_events (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    operator text NOT NULL,
    action text NOT NULL,
    target text NOT NULL,
    reason text NOT NULL CHECK (length(reason) > 0),
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE login_states (
    state_hash text PRIMARY KEY,
    browser_hash text NOT NULL,
    verifier text NOT NULL,
    expires_at timestamptz NOT NULL
);
CREATE TABLE sessions (
    token_hash text PRIMARY KEY,
    github_id bigint NOT NULL REFERENCES users,
    csrf text NOT NULL,
    expires_at timestamptz NOT NULL
);
CREATE INDEX sessions_expiry ON sessions(expires_at);
CREATE TABLE courses (
    id text PRIMARY KEY,
    title text NOT NULL,
    organization text NOT NULL,
    timezone text NOT NULL,
    config_revision text NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE enrollments (
    id uuid PRIMARY KEY,
    course_id text NOT NULL REFERENCES courses,
    github_id bigint NOT NULL REFERENCES users,
    student_id text NOT NULL,
    name text NOT NULL,
    UNIQUE(course_id, github_id),
    UNIQUE(course_id, student_id)
);
CREATE TABLE assignments (
    id uuid PRIMARY KEY,
    course_id text NOT NULL REFERENCES courses,
    slug text NOT NULL,
    current_revision text,
    UNIQUE(course_id, slug)
);
CREATE TABLE assignment_revisions (
    digest text PRIMARY KEY CHECK (digest ~ '^[0-9a-f]{64}$'),
    assignment_id uuid NOT NULL REFERENCES assignments,
    config_revision text NOT NULL,
    definition jsonb NOT NULL,
    opens_at timestamptz NOT NULL,
    deadline timestamptz NOT NULL,
    max_points integer NOT NULL CHECK(max_points > 0),
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE(assignment_id, digest),
    CHECK(opens_at < deadline)
);
ALTER TABLE assignments ADD CONSTRAINT current_revision_fk FOREIGN KEY(id, current_revision) REFERENCES assignment_revisions(assignment_id, digest);
CREATE TABLE student_repositories (
    id uuid PRIMARY KEY,
    enrollment_id uuid NOT NULL REFERENCES enrollments,
    assignment_id uuid NOT NULL REFERENCES assignments,
    revision_digest text NOT NULL,
    github_repo_id bigint UNIQUE,
    name text NOT NULL UNIQUE,
    provisioning_nonce uuid NOT NULL,
    state text NOT NULL DEFAULT 'creating' CHECK(state IN ('creating','invitation_pending','ready')),
    invitation_url text,
    observed_sha text,
    observed_at timestamptz,
    closure_due boolean NOT NULL DEFAULT false,
    final_submission_id uuid,
    locked_at timestamptz,
    needs_review boolean NOT NULL DEFAULT false,
    last_error text,
    UNIQUE(enrollment_id, assignment_id),
    FOREIGN KEY(assignment_id, revision_digest) REFERENCES assignment_revisions(assignment_id, digest)
);
CREATE TABLE extensions (
    repository_id uuid PRIMARY KEY REFERENCES student_repositories,
    deadline timestamptz NOT NULL,
    reason text NOT NULL,
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE webhook_deliveries (
    delivery_id uuid PRIMARY KEY,
    body_digest text NOT NULL,
    received_at timestamptz NOT NULL DEFAULT clock_timestamp()
);
CREATE TABLE submission_events (
    id uuid PRIMARY KEY,
    repository_id uuid NOT NULL REFERENCES student_repositories,
    sha text NOT NULL CHECK(sha ~ '^[0-9a-f]{40}$'),
    received_at timestamptz NOT NULL,
    source text NOT NULL CHECK(source IN ('webhook','registration','reconciliation')),
    eligible boolean NOT NULL,
    delivery_id uuid REFERENCES webhook_deliveries,
    UNIQUE(repository_id, delivery_id)
);
CREATE TABLE artifacts (
    digest text PRIMARY KEY CHECK(digest ~ '^[0-9a-f]{64}$'),
    bytes bigint NOT NULL CHECK(bytes >= 0),
    kind text NOT NULL CHECK(kind IN ('source','report')),
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE submissions (
    id uuid PRIMARY KEY,
    repository_id uuid NOT NULL REFERENCES student_repositories,
    event_id uuid NOT NULL UNIQUE REFERENCES submission_events,
    sha text NOT NULL,
    received_at timestamptz NOT NULL,
    source_digest text REFERENCES artifacts,
    created_at timestamptz NOT NULL DEFAULT now(),
    UNIQUE(id, repository_id)
);
ALTER TABLE student_repositories ADD CONSTRAINT final_submission_fk FOREIGN KEY(final_submission_id, id) REFERENCES submissions(id, repository_id);
CREATE TABLE grading_runs (
    id uuid PRIMARY KEY,
    submission_id uuid NOT NULL REFERENCES submissions,
    revision_digest text NOT NULL REFERENCES assignment_revisions,
    attempt integer NOT NULL DEFAULT 1,
    status text NOT NULL DEFAULT 'pending' CHECK(status IN ('pending','running','completed','integrity_failed','infrastructure_failed','timed_out','superseded')),
    points integer CHECK(points >= 0),
    report_digest text REFERENCES artifacts,
    result_digest text,
    completed_at timestamptz,
    UNIQUE(submission_id, revision_digest, attempt)
);
CREATE TABLE test_results (
    run_id uuid NOT NULL REFERENCES grading_runs,
    test_id text NOT NULL,
    passed boolean NOT NULL,
    PRIMARY KEY(run_id, test_id)
);
CREATE TABLE integrity_findings (
    run_id uuid NOT NULL REFERENCES grading_runs,
    path text NOT NULL,
    reason text NOT NULL,
    PRIMARY KEY(run_id, path)
);
CREATE TABLE grade_overrides (
    id uuid PRIMARY KEY,
    repository_id uuid NOT NULL REFERENCES student_repositories,
    points integer NOT NULL CHECK(points >= 0),
    reason text NOT NULL,
    operator text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now()
);
CREATE TABLE tasks (
    id uuid PRIMARY KEY,
    kind text NOT NULL CHECK(kind IN ('provision','snapshot','grade','publish','lock')),
    payload jsonb NOT NULL,
    dedup_key text NOT NULL UNIQUE,
    priority integer NOT NULL DEFAULT 0,
    status text NOT NULL DEFAULT 'pending' CHECK(status IN ('pending','leased','done','failed','cancelled')),
    attempts integer NOT NULL DEFAULT 0,
    available_at timestamptz NOT NULL DEFAULT now(),
    lease_owner text,
    lease_token uuid,
    lease_until timestamptz,
    last_error text,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX tasks_ready ON tasks(priority DESC, available_at) WHERE status IN ('pending','leased');
CREATE TABLE workers (
    id text PRIMARY KEY,
    token_hash text NOT NULL UNIQUE,
    profiles text[] NOT NULL,
    resource_caps jsonb NOT NULL,
    enabled boolean NOT NULL DEFAULT true
);
CREATE TABLE outbox (
    run_id uuid PRIMARY KEY REFERENCES grading_runs,
    github_check_id bigint,
    published_at timestamptz
);
CREATE TABLE reconciliation_observations (
    id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    repository_id uuid NOT NULL REFERENCES student_repositories,
    success boolean NOT NULL,
    detail text NOT NULL,
    observed_at timestamptz NOT NULL DEFAULT now()
);

CREATE FUNCTION reject_history_changes() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    RAISE EXCEPTION 'historical record is immutable';
END
$$;
CREATE TRIGGER immutable_revision BEFORE UPDATE OR DELETE ON assignment_revisions FOR EACH ROW EXECUTE FUNCTION reject_history_changes();
CREATE TRIGGER immutable_event BEFORE UPDATE OR DELETE ON submission_events FOR EACH ROW EXECUTE FUNCTION reject_history_changes();
CREATE TRIGGER immutable_audit BEFORE UPDATE OR DELETE ON audit_events FOR EACH ROW EXECUTE FUNCTION reject_history_changes();
CREATE TRIGGER immutable_override BEFORE UPDATE OR DELETE ON grade_overrides FOR EACH ROW EXECUTE FUNCTION reject_history_changes();

CREATE INDEX submission_selection ON submissions(repository_id, received_at DESC, id DESC);
CREATE INDEX submission_observations ON submission_events(repository_id, sha);
CREATE INDEX run_history ON grading_runs(submission_id, attempt DESC);
