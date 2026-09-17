ALTER TABLE grading_runs ADD COLUMN public_run_id uuid;
ALTER TABLE grading_runs ADD UNIQUE(id,submission_id);
ALTER TABLE grading_runs ADD FOREIGN KEY(public_run_id,submission_id)
    REFERENCES grading_runs(id,submission_id);
CREATE INDEX grading_public_baseline ON grading_runs(public_run_id);

CREATE FUNCTION validate_private_run() RETURNS trigger LANGUAGE plpgsql AS $$
BEGIN
    IF TG_OP='UPDATE' THEN
        IF NEW.public_run_id IS DISTINCT FROM OLD.public_run_id THEN
            RAISE EXCEPTION 'grading baseline is immutable';
        END IF;
        RETURN NEW;
    END IF;
    IF NEW.public_run_id IS NOT NULL AND NOT EXISTS (
        SELECT 1 FROM grading_runs b JOIN submissions s ON s.id=b.submission_id
        JOIN student_repositories r ON r.id=s.repository_id
        JOIN assignment_revisions v ON v.digest=r.revision_digest
        LEFT JOIN extensions x ON x.repository_id=r.id
        WHERE b.id=NEW.public_run_id AND b.submission_id=NEW.submission_id
          AND b.public_run_id IS NULL AND b.status='completed' AND b.points IS NOT NULL
          AND COALESCE(x.deadline,v.deadline)<now()
    ) THEN
        RAISE EXCEPTION 'private grading requires a completed public run after the effective deadline';
    END IF;
    RETURN NEW;
END $$;
CREATE TRIGGER private_run_guard BEFORE INSERT OR UPDATE ON grading_runs
    FOR EACH ROW EXECUTE FUNCTION validate_private_run();
