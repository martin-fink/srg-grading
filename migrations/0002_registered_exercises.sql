-- Keep repository provisioning pinned while allowing explicit grader rollouts.
ALTER TABLE student_repositories ADD COLUMN grading_revision text;
UPDATE student_repositories SET grading_revision=revision_digest;
ALTER TABLE student_repositories ALTER COLUMN grading_revision SET NOT NULL;
ALTER TABLE student_repositories ADD FOREIGN KEY(assignment_id,grading_revision)
    REFERENCES assignment_revisions(assignment_id,digest);
ALTER TABLE grading_runs ADD COLUMN public_points integer CHECK(public_points >= 0);
ALTER TABLE grading_runs DROP CONSTRAINT grading_runs_status_check;
ALTER TABLE grading_runs ADD CHECK(status IN ('pending','running','completed','integrity_failed','infrastructure_failed','timed_out','superseded','invalidated'));
