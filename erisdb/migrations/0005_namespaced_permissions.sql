-- Facet names become permission namespaces, so the schema version moves out
-- of the name and into the body.
--
-- A grant is `<facet>:<action>`. With the version in the name, a grant reads
-- `tasks/v1:read` and dies the day the schema moves to v2 — every paired
-- client silently loses access until someone re-grants. With the version in
-- the body, `tasks:read` survives the upgrade, which is what a permission
-- should do.

-- Version first: it is read out of the name the name still has.
UPDATE items
SET body = jsonb_set(body, '{version}', to_jsonb((substring(body ->> 'name' from '/v([0-9]+)$'))::int))
WHERE facet = 'facet' AND body ->> 'name' ~ '/v[0-9]+$';

UPDATE items
SET body = jsonb_set(body, '{name}', to_jsonb(regexp_replace(body ->> 'name', '/v[0-9]+$', '')))
WHERE facet = 'facet' AND body ->> 'name' ~ '/v[0-9]+$';

-- Then the items and their history follow the rename. History keeps its
-- bodies untouched; only the facet a row is filed under moves, because that
-- name is now what authorization is computed from — leaving it behind would
-- make an item's own past unreadable to the client that wrote it.
UPDATE items   SET facet = regexp_replace(facet, '/v[0-9]+$', '') WHERE facet ~ '/v[0-9]+$';
UPDATE changes SET facet = regexp_replace(facet, '/v[0-9]+$', '') WHERE facet ~ '/v[0-9]+$';

-- The meta-facet grows `version` and the optional human descriptions a
-- pairing prompt reads out, so a person approving a request sees "add tasks"
-- rather than "tasks:create".
UPDATE items SET body = '{
    "name": "facet",
    "strict": true,
    "schema": {
        "type": "object",
        "required": ["name", "schema"],
        "properties": {
            "name":    {"type": "string", "minLength": 1},
            "version": {"type": "integer", "minimum": 1},
            "strict":  {"type": "boolean"},
            "schema":  {"type": "object"},
            "permissions": {
                "type": "object",
                "additionalProperties": {"type": "string"}
            },
            "lapse": {
                "type": "object",
                "required": ["due"],
                "properties": {
                    "due":  {"type": "string"},
                    "done": {"type": "string"}
                },
                "additionalProperties": false
            }
        },
        "additionalProperties": false
    }
}'
WHERE facet = 'facet' AND body ->> 'name' = 'facet';

-- Pairing sessions are ordinary items, so the core stays stateless: they
-- survive a restart, replicate across cores, and land on the change feed
-- where a dashboard can watch a request arrive.
--
-- A session records the shape of the token it will issue -- the grants, the
-- two deadlines -- and never the token. These rows are permanent; a
-- credential written here would outlive the session and stay readable to
-- anyone holding meta:feed:read. The token is minted when it is collected.
INSERT INTO items (id, facet, body) VALUES (
    '00000000-0000-0000-0000-000000000002',
    'facet',
    '{
        "name": "pair",
        "strict": true,
        "schema": {
            "type": "object",
            "required": ["status"],
            "properties": {
                "status":    {"enum": ["pending", "requested", "approved", "denied", "collected"]},
                "client":    {"type": "string"},
                "requested": {"type": "array", "items": {"type": "string"}},
                "granted":   {"type": "array", "items": {"type": "string"}},
                "user":      {"type": "string"},
                "exp":       {"type": "integer"},
                "max_exp":   {"type": "integer"},
                "expires":   {"type": "integer"}
            },
            "additionalProperties": false
        }
    }'
) ON CONFLICT DO NOTHING;
