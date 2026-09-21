-- One active registration per installation proof. A later human-approved
-- enrollment replaces that installation's permission set, not its identity.
SELECT pg_advisory_xact_lock('x0062657a656c0001'::bit(64)::bigint);

-- If a pre-release registry already contains duplicate enrollments, retain
-- the most recent approval and retire the older credentials with an audit.
WITH ranked AS (
    SELECT id, row_number() OVER (PARTITION BY identity ORDER BY created_at DESC, id DESC) AS ordinal
    FROM clients WHERE revoked_at IS NULL
), retired AS (
    UPDATE clients SET revoked_at = now(), revision = revision + 1
    WHERE id IN (SELECT id FROM ranked WHERE ordinal > 1)
    RETURNING *
)
INSERT INTO changes (facet, op, body, revision, source)
SELECT 'system', 'updated', jsonb_build_object('event', 'client', 'client', to_jsonb(retired)),
    revision, '{"migration":"0007_one_active_registration"}'::jsonb FROM retired;

CREATE UNIQUE INDEX clients_active_identity_idx ON clients (identity) WHERE revoked_at IS NULL;

WITH extended AS (
    UPDATE items SET body = jsonb_set(body, '{schema,properties}',
        (body #> '{schema,properties}') || '{
            "client_id": {"type":"string", "format":"uuid"},
            "approval_anchor": {"type":["object","null"]}
        }'::jsonb),
        revision = revision + 1, updated_at = now(),
        source = '{"migration":"0007_one_active_registration"}'::jsonb
    WHERE facet = 'facet' AND body->>'name' = 'pair'
    RETURNING *
)
INSERT INTO changes (item_id, facet, op, body, revision, source)
SELECT id, facet, 'updated', body, revision, source FROM extended;
-- Old approvals have no revision anchor and must not overwrite newer authority.
WITH invalidated AS (
    UPDATE items SET body = jsonb_set(body, '{status}', '"denied"'),
        revision = revision + 1, updated_at = now(),
        source = '{"migration":"0007_one_active_registration"}'::jsonb
    WHERE facet = 'pair' AND body->>'status' = 'approved' AND NOT body ? 'approval_anchor'
    RETURNING *
)
INSERT INTO changes (item_id, facet, op, body, revision, source)
SELECT id, facet, 'updated', body, revision, source FROM invalidated;
SELECT pg_notify('bezel_changes', '');
