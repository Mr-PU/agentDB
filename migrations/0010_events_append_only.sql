-- Closes a real gap left open in the initial Phase 4 build: `events`
-- was "append-only by convention" (no application code path issues
-- UPDATE/DELETE against it) but nothing enforced that at the database
-- level, which is what phase4-event-store.md §8's exit criterion
-- actually asks for ("fails at the database permission level, not just
-- application logic").
--
-- A plain `REVOKE UPDATE, DELETE ON events FROM <app_role>` would NOT
-- achieve this: the role that runs migrations — and therefore owns this
-- table — is `agentdb` by default (see docker-compose.yml), which is
-- the Postgres instance superuser in local dev and, in any deployment,
-- the table's owner regardless. Postgres table owners (and superusers
-- always) bypass REVOKE-based restrictions on their own tables
-- entirely — a REVOKE here would compile, run, and silently do nothing.
--
-- A trigger is the mechanism that actually works regardless of who's
-- connected or what they own: it fires on every UPDATE/DELETE attempt
-- unconditionally, including from the table's owner. The only way
-- around it is a deliberate, auditable schema-level action (disabling
-- the trigger), not an accidental or malicious application-level query
-- — which is the threat model that actually matters here.
CREATE OR REPLACE FUNCTION reject_events_mutation() RETURNS TRIGGER AS $$
BEGIN
    RAISE EXCEPTION 'events is append-only: % is not permitted on this table (see src/events.rs)', TG_OP;
END;
$$ LANGUAGE plpgsql;

DROP TRIGGER IF EXISTS events_append_only ON events;

CREATE TRIGGER events_append_only
    BEFORE UPDATE OR DELETE ON events
    FOR EACH ROW
    EXECUTE FUNCTION reject_events_mutation();

-- Note: this is a row-level UPDATE/DELETE trigger, which does NOT fire
-- for TRUNCATE (Postgres requires a separate statement-level TRUNCATE
-- trigger for that, which this deliberately isn't). That's why
-- tests/common/mod.rs's `reset_tables()` — which TRUNCATEs `events`
-- between tests — keeps working unaffected by this trigger. A stray
-- TRUNCATE from application code is a much smaller realistic risk than
-- a stray UPDATE/DELETE, and isn't the gap this migration closes.
