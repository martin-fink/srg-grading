CREATE TABLE submission_admission (
    github_id bigint PRIMARY KEY REFERENCES users,
    next_allowed_at timestamptz NOT NULL
);
CREATE INDEX submissions_repository_sha ON submissions(repository_id, sha);
CREATE INDEX login_states_expiry ON login_states(expires_at);
