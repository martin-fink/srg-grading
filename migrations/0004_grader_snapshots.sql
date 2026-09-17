ALTER TABLE assignment_revisions ADD COLUMN grader_source_digest text REFERENCES artifacts(digest);
ALTER TABLE assignment_revisions ADD CONSTRAINT grader_snapshot_matches_definition
    CHECK (grader_source_digest IS NOT DISTINCT FROM (definition #>> '{grader,source_digest}'));
