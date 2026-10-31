-- Phase 5: MCP-native tool registry.
--
-- A tool is either read-only (wraps a Phase 1 mirror query, no backing
-- action type) or a write tool (backed by one of Phase 3's action
-- types). The CHECK keeps these two kinds from drifting out of sync
-- with each other — a write tool must name the action type it proposes,
-- a read-only tool must not pretend to have one.
CREATE TABLE IF NOT EXISTS tools (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name TEXT UNIQUE NOT NULL,
    description TEXT NOT NULL,
    input_schema JSONB NOT NULL DEFAULT '{}'::jsonb,
    required_capability_id UUID NOT NULL REFERENCES capabilities(id),
    backing_action_type TEXT,
    is_read_only BOOLEAN NOT NULL DEFAULT false,
    CHECK (is_read_only = (backing_action_type IS NULL))
);

CREATE INDEX IF NOT EXISTS idx_tools_capability ON tools(required_capability_id);
