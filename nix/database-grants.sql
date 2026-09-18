-- Apply as grading_owner after migrations. No runtime role inherits another role.
REVOKE ALL ON ALL TABLES IN SCHEMA public FROM grading_web, grading_operator, grading_admin;
GRANT USAGE ON SCHEMA public TO grading_web, grading_operator, grading_admin;
GRANT SELECT ON ALL TABLES IN SCHEMA public TO grading_operator;
GRANT SELECT ON users, admins, courses, enrollments, assignments, assignment_revisions,
    student_repositories, extensions, submissions, submission_events, grading_runs,
    integrity_findings, grade_overrides, artifacts, workers, outbox,
    tasks, webhook_deliveries, sessions, login_states TO grading_web;
GRANT INSERT, UPDATE ON users, student_repositories, submissions, tasks,
    artifacts, outbox TO grading_web;
GRANT INSERT(id,submission_id,revision_digest,attempt,status),
    UPDATE(status,points,public_points,report_digest,result_digest,completed_at) ON grading_runs TO grading_web;
GRANT INSERT ON submission_events, webhook_deliveries, integrity_findings TO grading_web;
GRANT INSERT, UPDATE, DELETE ON sessions, login_states TO grading_web;
GRANT DELETE ON sessions, login_states TO grading_operator;
GRANT INSERT, UPDATE ON users, courses, enrollments, assignments, student_repositories,
    submissions, grading_runs, tasks, artifacts, outbox, extensions, workers TO grading_operator;
GRANT INSERT ON assignment_revisions, submission_events, grade_overrides,
    reconciliation_observations, audit_events, integrity_findings TO grading_operator;
GRANT SELECT, INSERT, DELETE ON admins TO grading_admin;
GRANT INSERT ON audit_events TO grading_admin;
GRANT USAGE ON SEQUENCE audit_events_id_seq TO grading_admin, grading_operator;
GRANT USAGE ON SEQUENCE reconciliation_observations_id_seq TO grading_operator;
REVOKE EXECUTE ON ALL FUNCTIONS IN SCHEMA public FROM PUBLIC;

GRANT SELECT, INSERT, UPDATE ON submission_admission TO grading_web;

GRANT SELECT ON admin_operations, admin_worker_status TO grading_web;
GRANT INSERT(id,actor,session_hash,input) ON admin_operations TO grading_web;
GRANT SELECT, INSERT, UPDATE ON admin_operations, admin_worker_status TO grading_operator;
GRANT EXECUTE ON FUNCTION confirm_admin_operation(uuid,bigint,text,text) TO grading_web;
