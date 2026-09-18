-- Browser requests contain data, never executable commands or database credentials.
CREATE TABLE admin_operations (
    id uuid PRIMARY KEY,
    actor bigint NOT NULL REFERENCES users(github_id),
    session_hash text NOT NULL,
    input jsonb NOT NULL CHECK (octet_length(input::text) <= 2200000),
    state text NOT NULL DEFAULT 'pending_validation' CHECK (state IN ('pending_validation','validating','ready','queued','applying','succeeded','failed','uncertain','expired')),
    plan jsonb,
    output text NOT NULL DEFAULT '' CHECK (octet_length(output)<=1048576),
    download text CHECK (octet_length(download)<=8388608),
    confirmation_hash text,
    confirmation_token text,
    validated_until timestamptz,
    created_at timestamptz NOT NULL DEFAULT now(),
    updated_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX admin_operations_pending ON admin_operations(created_at) WHERE state IN ('pending_validation','queued');
CREATE INDEX admin_operations_actor ON admin_operations(actor,created_at DESC);
CREATE TABLE admin_worker_status (id boolean PRIMARY KEY DEFAULT true CHECK(id), heartbeat timestamptz NOT NULL DEFAULT now());

-- The web role can only confirm a worker-validated, unexpired operation from the
-- same authenticated session. It cannot write plans, reports, or mark work ready.
CREATE FUNCTION confirm_admin_operation(operation uuid, administrator bigint, session_digest text, confirmation_digest text)
RETURNS boolean LANGUAGE plpgsql SECURITY DEFINER SET search_path=pg_catalog,public AS $$
DECLARE changed integer;
BEGIN
    UPDATE public.admin_operations SET state='queued',updated_at=now(),confirmation_token=NULL
    WHERE id=operation AND actor=administrator AND session_hash=session_digest
      AND state='ready' AND validated_until>now() AND confirmation_hash=confirmation_digest
      AND EXISTS(SELECT 1 FROM public.admins WHERE github_id=administrator);
    GET DIAGNOSTICS changed = ROW_COUNT;
    IF changed=1 THEN
        INSERT INTO public.audit_events(operator,action,target,reason)
        SELECT 'github:'||administrator::text,'admin_portal.confirmed',operation::text,input->>'reason'
        FROM public.admin_operations WHERE id=operation;
    END IF;
    RETURN changed=1;
END;
$$;
