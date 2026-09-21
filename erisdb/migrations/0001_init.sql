-- ErisDB's initial schema. Startup checks this exact schema's migration checksum.
CREATE TABLE items (
    id UUID PRIMARY KEY,
    facet TEXT NOT NULL,
    body JSONB NOT NULL,
    revision BIGINT NOT NULL DEFAULT 1,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    source JSONB
);
CREATE INDEX items_facet_updated_idx ON items (facet, updated_at);
CREATE UNIQUE INDEX items_facet_name_idx ON items ((body ->> 'name')) WHERE facet = 'facet';

CREATE TABLE changes (
    seq BIGSERIAL PRIMARY KEY,
    item_id UUID,
    facet TEXT NOT NULL,
    op TEXT NOT NULL CHECK (op IN ('created', 'updated', 'deleted', 'tick', 'lapsed')),
    at TIMESTAMPTZ NOT NULL DEFAULT now(),
    body JSONB,
    source JSONB,
    revision BIGINT
);
CREATE INDEX changes_facet_seq_idx ON changes (facet, seq);
CREATE INDEX changes_item_seq_idx ON changes (item_id, seq);

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
CREATE UNIQUE INDEX clients_active_identity_idx ON clients (identity) WHERE revoked_at IS NULL;

-- Relative timestamps depend on the transaction clock, so this is STABLE.
CREATE FUNCTION safe_ts(t TEXT) RETURNS TIMESTAMPTZ AS $$
BEGIN
    RETURN t::timestamptz;
EXCEPTION WHEN OTHERS THEN
    RETURN NULL;
END;
$$ LANGUAGE plpgsql STABLE;

-- Definitions are data; initialization creates the two built-in schemas.
INSERT INTO items (id, facet, body) VALUES (
    '00000000-0000-0000-0000-000000000001', 'facet',
    '{
    "name": "facet",
    "strict": true,
    "schema": {
        "type": "object",
        "required": [
            "name",
            "schema"
        ],
        "properties": {
            "name": {
                "type": "string",
                "minLength": 1
            },
            "version": {
                "type": "integer",
                "minimum": 1
            },
            "strict": {
                "type": "boolean"
            },
            "schema": {
                "type": "object"
            },
            "permissions": {
                "type": "object",
                "additionalProperties": {
                    "type": "string"
                }
            },
            "lapse": {
                "type": "object",
                "required": [
                    "due"
                ],
                "properties": {
                    "due": {
                        "type": "string"
                    },
                    "done": {
                        "type": "string"
                    }
                },
                "additionalProperties": false
            }
        },
        "additionalProperties": false
    }
}'
);
INSERT INTO items (id, facet, body) VALUES (
    '00000000-0000-0000-0000-000000000002', 'facet',
    '{
    "name": "pair",
    "strict": true,
    "schema": {
        "type": "object",
        "required": [
            "status"
        ],
        "properties": {
            "status": {
                "enum": [
                    "pending",
                    "requested",
                    "approved",
                    "denied",
                    "collected"
                ]
            },
            "client": {
                "type": "string"
            },
            "requested": {
                "type": "array",
                "items": {
                    "type": "string"
                }
            },
            "granted": {
                "type": "array",
                "items": {
                    "type": "string"
                }
            },
            "user": {
                "type": "string"
            },
            "exp": {
                "type": "integer"
            },
            "max_exp": {
                "type": "integer"
            },
            "expires": {
                "type": "integer"
            },
            "identity": {
                "type": "object"
            },
            "fingerprint": {
                "type": "string"
            },
            "access_ttl_secs": {
                "type": "integer",
                "minimum": 1,
                "maximum": 604800
            },
            "client_id": {
                "type": "string",
                "format": "uuid"
            },
            "approval_anchor": {
                "type": [
                    "object",
                    "null"
                ]
            }
        },
        "additionalProperties": false
    }
}'
);
