-- The store: the only stateful thing in a bezel deployment.
-- Two tables carry everything: items (current truth) and changes (the bus).

CREATE TABLE items (
    id         UUID PRIMARY KEY,
    facet      TEXT NOT NULL,
    body       JSONB NOT NULL,
    revision   BIGINT NOT NULL DEFAULT 1,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX items_facet_updated_idx ON items (facet, updated_at);

-- Facet definitions are items in the meta-facet 'facet'; names are unique.
CREATE UNIQUE INDEX items_facet_name_idx ON items ((body ->> 'name')) WHERE facet = 'facet';

-- Every mutation writes a change row in the same transaction: the change
-- feed is the event bus, durable and totally ordered.
CREATE TABLE changes (
    seq     BIGSERIAL PRIMARY KEY,
    item_id UUID,
    facet   TEXT NOT NULL,
    op      TEXT NOT NULL CHECK (op IN ('created', 'updated', 'deleted', 'tick')),
    at      TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX changes_facet_seq_idx ON changes (facet, seq);

-- Bootstrap the meta-facet: the facet that describes facets.
INSERT INTO items (id, facet, body) VALUES (
    '00000000-0000-0000-0000-000000000001',
    'facet',
    '{
        "name": "facet",
        "strict": true,
        "schema": {
            "type": "object",
            "required": ["name", "schema"],
            "properties": {
                "name":   {"type": "string", "minLength": 1},
                "strict": {"type": "boolean"},
                "schema": {"type": "object"}
            },
            "additionalProperties": false
        }
    }'
);
