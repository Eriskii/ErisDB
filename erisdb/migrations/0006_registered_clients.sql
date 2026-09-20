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

-- Old pending ceremonies must be restarted: they have no collector binding.
UPDATE items SET body = jsonb_set(body, '{status}', '"denied"')
WHERE facet = 'pair' AND body->>'status' IN ('pending', 'requested', 'approved');

UPDATE items SET body = jsonb_set(body, '{schema,properties}',
    (body #> '{schema,properties}') || '{
        "identity": {"type": "object"},
        "fingerprint": {"type": "string"},
        "access_ttl_secs": {"type": "integer", "minimum": 1, "maximum": 604800}
    }'::jsonb)
WHERE facet = 'facet' AND body->>'name' = 'pair';
