-- Write tools, one per Phase 3 action handler. backing_action_type
-- matches actions::HandlerRegistry's action_type() strings exactly —
-- see tool_registry::call_tool, which uses this column to route a call
-- into propose_action unmodified.
INSERT INTO tools (name, description, input_schema, required_capability_id, backing_action_type, is_read_only)
SELECT
    'process.stop',
    'Stop a running process by PID',
    '{"type":"object","properties":{"pid":{"type":"integer"}},"required":["pid"]}'::jsonb,
    id, 'process.stop', false
FROM capabilities WHERE name = 'process.stop'
ON CONFLICT (name) DO NOTHING;

INSERT INTO tools (name, description, input_schema, required_capability_id, backing_action_type, is_read_only)
SELECT
    'process.start',
    'Start a defined process or service',
    '{"type":"object","properties":{"command":{"type":"string"}},"required":["command"]}'::jsonb,
    id, 'process.start', false
FROM capabilities WHERE name = 'process.start'
ON CONFLICT (name) DO NOTHING;

INSERT INTO tools (name, description, input_schema, required_capability_id, backing_action_type, is_read_only)
SELECT
    'cgroup.set_limit',
    'Adjust the memory limit on a cgroup scope',
    '{"type":"object","properties":{"cgroup_path":{"type":"string"},"memory_limit_bytes":{"type":"integer"}},"required":["cgroup_path","memory_limit_bytes"]}'::jsonb,
    id, 'cgroup.set_limit', false
FROM capabilities WHERE name = 'cgroup.set_limit'
ON CONFLICT (name) DO NOTHING;

INSERT INTO tools (name, description, input_schema, required_capability_id, backing_action_type, is_read_only)
SELECT
    'service.restart',
    'Restart a systemd-managed service',
    '{"type":"object","properties":{"service_name":{"type":"string"}},"required":["service_name"]}'::jsonb,
    id, 'service.restart', false
FROM capabilities WHERE name = 'service.restart'
ON CONFLICT (name) DO NOTHING;

-- Read-only tools wrapping Phase 1's mirror queries. All three are
-- gated by the same 'filesystem.read' capability as a practical stand-in
-- for "general read access to system state" — the seeded capability set
-- from migrations/0007 has no finer-grained read capability, and
-- introducing one is out of scope for this phase (see README's Phase 5
-- scope-limits list).
INSERT INTO tools (name, description, input_schema, required_capability_id, backing_action_type, is_read_only)
SELECT
    'query.processes',
    'List running processes, optionally filtered by name and sorted by CPU or memory',
    '{"type":"object","properties":{"name_filter":{"type":"string"},"sort_by":{"type":"string","enum":["cpu","memory"]},"limit":{"type":"integer"}}}'::jsonb,
    id, NULL, true
FROM capabilities WHERE name = 'filesystem.read'
ON CONFLICT (name) DO NOTHING;

INSERT INTO tools (name, description, input_schema, required_capability_id, backing_action_type, is_read_only)
SELECT
    'query.network',
    'List current network connections',
    '{"type":"object","properties":{"limit":{"type":"integer"}}}'::jsonb,
    id, NULL, true
FROM capabilities WHERE name = 'filesystem.read'
ON CONFLICT (name) DO NOTHING;

INSERT INTO tools (name, description, input_schema, required_capability_id, backing_action_type, is_read_only)
SELECT
    'query.services',
    'List known services and their status',
    '{"type":"object","properties":{}}'::jsonb,
    id, NULL, true
FROM capabilities WHERE name = 'filesystem.read'
ON CONFLICT (name) DO NOTHING;
