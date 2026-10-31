-- Seeds the capability set that matches the Phase 3 action handlers
-- already scoped in phase3-action-engine.md, plus the read capabilities
-- the mirror's typed endpoints will eventually be gated by.
--
-- Deliberately a small, fixed list rather than something the API grows
-- freely — see CONTRIBUTING.md on why the capability surface should stay
-- matched to what Phase 3 can actually enforce.
INSERT INTO capabilities (name, description) VALUES
    ('process.stop',     'Terminate a running process by PID'),
    ('process.start',    'Start a defined process or service'),
    ('cgroup.set_limit', 'Modify CPU/memory limits on a cgroup scope'),
    ('service.restart',  'Restart a systemd-managed service'),
    ('filesystem.read',  'Read files within a scoped path'),
    ('filesystem.write', 'Write files within a scoped path'),
    ('network.request',  'Make an outbound request to a scoped host')
ON CONFLICT (name) DO NOTHING;
