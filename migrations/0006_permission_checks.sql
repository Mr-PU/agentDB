-- Phase 2: append-only audit log of every permission decision, allow or
-- deny. This is what makes "why was this denied" a query instead of a
-- debugging session once later phases build on top of check_permission.
--
-- agent_id is deliberately NOT a foreign key to agents(id): a denial with
-- reason='agent_not_found' is, by definition, for an agent_id that never
-- existed in that table — and that attempt still needs to be logged.
-- It's exactly the kind of thing worth being able to query later
-- ("someone tried to use an id that was never issued").
CREATE TABLE IF NOT EXISTS permission_checks (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    agent_id UUID NOT NULL,
    capability_name TEXT NOT NULL,
    requested_scope TEXT NOT NULL,
    decision TEXT NOT NULL CHECK (decision IN ('allowed', 'denied')),
    matched_grant_id UUID REFERENCES grants(id),
    reason TEXT,
    checked_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- Mirrors the invariant in permissions::finalize() at the database
    -- level: an allowed decision always carries the grant that allowed
    -- it and no reason; a denied decision always carries a reason and no
    -- grant. Catches a future bug in that function before it can produce
    -- a row that doesn't make sense.
    CHECK (
        (decision = 'allowed' AND matched_grant_id IS NOT NULL AND reason IS NULL) OR
        (decision = 'denied' AND matched_grant_id IS NULL AND reason IS NOT NULL)
    )
);

CREATE INDEX IF NOT EXISTS idx_permission_checks_agent_time
    ON permission_checks(agent_id, checked_at DESC);
CREATE INDEX IF NOT EXISTS idx_permission_checks_decision_time
    ON permission_checks(decision, checked_at DESC);

-- No explicit REVOKE UPDATE/DELETE needed here for agentdb_reader: Phase 1's
-- db::ensure_reader_grants() only ever grants SELECT, never write access,
-- so append-only-ness for that role is already guaranteed by what it's
-- granted, not by what's taken away from it.
