-- Phase 3: transactional action engine.
--
-- requested_dry_run vs. effective_dry_run is a deliberate split: the
-- caller's preference at proposal time is not necessarily what actually
-- ran, because the server-level ACTIONS_DRY_RUN ceiling can force dry-run
-- regardless of what was requested (see src/actions.rs::DryRunConfig).
-- Recording both means the audit trail never has to guess which mode an
-- already-completed action actually ran in.
CREATE TABLE IF NOT EXISTS actions (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    agent_id UUID NOT NULL REFERENCES agents(id),
    action_type TEXT NOT NULL,
    parameters JSONB NOT NULL DEFAULT '{}'::jsonb,
    -- Set by the permission check at proposal time, never chosen by the
    -- handler itself — this is what makes "this action was authorized by
    -- this specific grant" a join instead of a guess (phase3-action-engine.md §2).
    grant_id UUID REFERENCES grants(id),
    status TEXT NOT NULL DEFAULT 'proposed' CHECK (
        status IN ('proposed', 'approved', 'executing', 'executed', 'failed', 'rolled_back', 'compensated')
    ),
    requested_dry_run BOOLEAN,   -- caller's preference at proposal time; NULL = no preference stated
    effective_dry_run BOOLEAN,   -- the actual mode used at execution time; NULL until executed
    outcome JSONB,               -- the handler's ActionOutcome, once executed
    error TEXT,
    requires_human_approval BOOLEAN NOT NULL DEFAULT true,
    approved_by TEXT,
    approved_at TIMESTAMPTZ,
    created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    completed_at TIMESTAMPTZ,
    -- An action can only be 'approved' with a recorded approver, mirroring
    -- the same invariant style as grants.revoked_at/revoked_by in Phase 2.
    CHECK (status != 'approved' OR (approved_by IS NOT NULL AND approved_at IS NOT NULL)),
    CHECK (status NOT IN ('executed', 'failed', 'rolled_back', 'compensated') OR completed_at IS NOT NULL)
);

CREATE INDEX IF NOT EXISTS idx_actions_agent_time ON actions(agent_id, created_at DESC);
CREATE INDEX IF NOT EXISTS idx_actions_status ON actions(status);
