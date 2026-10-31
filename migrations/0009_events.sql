-- Phase 4: event-sourced audit trail.
--
-- agent_id is NOT a foreign key, for the same reason as
-- permission_checks.agent_id (migrations/0006): an event tied to an
-- agent_id that was never issued (or a denied proposal) must still be
-- loggable.
--
-- `sequence` (not `occurred_at` alone) is what guarantees deterministic
-- replay order. Two events written in the same transaction, or by
-- concurrent transactions committing in close succession, can land
-- within the same clock tick — ORDER BY occurred_at alone is not a
-- reliable total order at that resolution. BIGSERIAL is.
CREATE TABLE IF NOT EXISTS events (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    agent_id UUID NOT NULL,
    action_id UUID REFERENCES actions(id),
    grant_id UUID REFERENCES grants(id),
    event_type TEXT NOT NULL,
    payload JSONB NOT NULL DEFAULT '{}'::jsonb,
    occurred_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    sequence BIGSERIAL
);

CREATE INDEX IF NOT EXISTS idx_events_agent_seq ON events(agent_id, sequence);
CREATE INDEX IF NOT EXISTS idx_events_action ON events(action_id);
CREATE INDEX IF NOT EXISTS idx_events_type_time ON events(event_type, occurred_at);

-- Append-only, like permission_checks: db::ensure_reader_grants() only
-- ever grants SELECT to agentdb_reader, so no explicit REVOKE is needed
-- for that role to be read-only here. No application code path should
-- ever UPDATE or DELETE a row in this table — see src/events.rs.
