-- Phase 2: core permission/capability schema.
--
-- Grants are append-mostly: a grant is revoked (revoked_at set), never
-- deleted or edited in place after creation, so "what could this agent
-- do as of time T" stays reconstructable from history.

CREATE TABLE IF NOT EXISTS agents (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'suspended', 'retired')),
    created_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE IF NOT EXISTS capabilities (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name TEXT UNIQUE NOT NULL,
    description TEXT
);

CREATE TABLE IF NOT EXISTS grants (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    agent_id UUID NOT NULL REFERENCES agents(id),
    capability_id UUID NOT NULL REFERENCES capabilities(id),
    scope TEXT NOT NULL,
    granted_by TEXT NOT NULL,
    granted_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at TIMESTAMPTZ,
    revoked_at TIMESTAMPTZ,
    revoked_by TEXT,
    -- A revocation always records who did it; a non-revoked grant must not
    -- carry a revoked_by left over from some other code path.
    CHECK (
        (revoked_at IS NULL AND revoked_by IS NULL) OR
        (revoked_at IS NOT NULL AND revoked_by IS NOT NULL)
    ),
    -- Defense in depth against a backdated/nonsensical expiry slipping in
    -- through any path other than the API's own future-only validation.
    -- Uses >= rather than > so a grant whose expiry is issued in the same
    -- instant as granted_at (clock-resolution edge case) isn't rejected.
    CHECK (expires_at IS NULL OR expires_at >= granted_at)
);

CREATE INDEX IF NOT EXISTS idx_grants_agent_capability_active
    ON grants(agent_id, capability_id) WHERE revoked_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_grants_agent ON grants(agent_id);

COMMENT ON TABLE grants IS
    'Rows are revoked (revoked_at set), never deleted or mutated after creation — history must stay reconstructable.';
