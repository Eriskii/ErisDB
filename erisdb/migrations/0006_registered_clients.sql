-- Installation authority belongs to the control plane, outside generic item CRUD.
-- Browser keys are S256 commitments, never refresh secrets. Iroh keys are
-- public endpoint IDs authenticated by the transport.
CREATE TABLE clients (
    id UUID PRIMARY KEY,
    name TEXT NOT NULL CHECK (length(name) BETWEEN 1 AND 128),
    identity JSONB NOT NULL,
    requested TEXT[] NOT NULL,
    grants TEXT[] NOT NULL,
    user_name TEXT,
    access_ttl_secs BIGINT NOT NULL CHECK (access_ttl_secs BETWEEN 1 AND 604800),
    expires BIGINT,
    revoked_at TIMESTAMPTZ,
    revision BIGINT NOT NULL DEFAULT 1,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX clients_identity_idx ON clients (identity);

-- Share the ordinary feed append lock while revising existing control state.
SELECT pg_advisory_xact_lock('x0062657a656c0001'::bit(64)::bigint);

-- Old pending ceremonies must be restarted: they have no collector binding.
WITH invalidated AS (
    UPDATE items
    SET body = jsonb_set(body, '{status}', '"denied"'),
        revision = revision + 1, updated_at = now(),
        source = '{"migration":"0006_registered_clients"}'::jsonb
    WHERE facet = 'pair' AND body->>'status' IN ('pending', 'requested', 'approved')
    RETURNING *
)
INSERT INTO changes (item_id, facet, op, body, revision, source)
SELECT id, facet, 'updated', body, revision, source FROM invalidated;

WITH extended AS (
    UPDATE items SET body = jsonb_set(body, '{schema,properties}',
        (body #> '{schema,properties}') || '{
            "identity": {"type": "object"},
            "fingerprint": {"type": "string"},
            "access_ttl_secs": {"type": "integer", "minimum": 1, "maximum": 604800}
        }'::jsonb),
        revision = revision + 1, updated_at = now(),
        source = '{"migration":"0006_registered_clients"}'::jsonb
    WHERE facet = 'facet' AND body->>'name' = 'pair'
    RETURNING *
)
INSERT INTO changes (item_id, facet, op, body, revision, source)
SELECT id, facet, 'updated', body, revision, source FROM extended;

SELECT pg_notify('bezel_changes', '');
