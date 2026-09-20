-- safe_ts is STABLE, not IMMUTABLE.
--
-- `t::timestamptz` reads the clock for inputs like 'now', 'today' and
-- 'yesterday', so the result is not a pure function of the argument. Declaring
-- it IMMUTABLE lets the planner constant-fold a call, and would silently
-- corrupt any expression index built on it.

CREATE OR REPLACE FUNCTION safe_ts(t TEXT) RETURNS TIMESTAMPTZ AS $$
BEGIN
    RETURN t::timestamptz;
EXCEPTION WHEN OTHERS THEN
    RETURN NULL;
END;
$$ LANGUAGE plpgsql STABLE;
