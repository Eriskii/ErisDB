-- Declarative lapse rules: a facet may declare {"lapse": {"due": f, "done": g}}
-- and the tick sweep emits a 'lapsed' change for overdue, un-done items.

ALTER TABLE changes DROP CONSTRAINT changes_op_check;
ALTER TABLE changes ADD CONSTRAINT changes_op_check
    CHECK (op IN ('created', 'updated', 'deleted', 'tick', 'lapsed'));

-- Timestamp parse that returns NULL instead of erroring on garbage, so one
-- malformed item can never wedge the tick.
CREATE FUNCTION safe_ts(t TEXT) RETURNS TIMESTAMPTZ AS $$
BEGIN
    RETURN t::timestamptz;
EXCEPTION WHEN OTHERS THEN
    RETURN NULL;
END;
$$ LANGUAGE plpgsql IMMUTABLE;

-- The meta-facet grows the optional lapse rule.
UPDATE items SET body = '{
    "name": "facet",
    "strict": true,
    "schema": {
        "type": "object",
        "required": ["name", "schema"],
        "properties": {
            "name":   {"type": "string", "minLength": 1},
            "strict": {"type": "boolean"},
            "schema": {"type": "object"},
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
