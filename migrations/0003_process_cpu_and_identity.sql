-- Fixes two correctness bugs from the initial Phase 1 scaffold:
--
-- 1. `cpu_usage` stored raw cumulative CPU ticks (ever-increasing since
--    process start), not a rate. Sorting by it surfaced the oldest
--    long-running processes, not the ones currently busy — which breaks
--    the Phase 6 "why is my machine slow" demo, whose whole premise is
--    identifying current CPU pressure.
-- 2. `ON CONFLICT (pid) DO UPDATE` never touched `started_at`, so if the
--    OS recycles a PID for a brand-new process, the row silently kept the
--    *previous* process's start time — corrupting any "running longer
--    than expected" reasoning.
--
-- This migration renames the column to make its (new) meaning explicit
-- and adds the bookkeeping columns the ingest service needs to compute a
-- real rate between polls.

ALTER TABLE processes RENAME COLUMN cpu_usage TO cpu_percent;

ALTER TABLE processes
    ADD COLUMN IF NOT EXISTS cpu_ticks_total BIGINT,
    ADD COLUMN IF NOT EXISTS prev_cpu_ticks_total BIGINT,
    ADD COLUMN IF NOT EXISTS prev_sampled_at TIMESTAMPTZ;

COMMENT ON COLUMN processes.cpu_percent IS
    'CPU usage as a percentage of one core, computed as the delta in cpu_ticks_total since the previous poll divided by elapsed wall-clock time. NULL on the first observation of a given process identity (no prior sample to diff against).';
COMMENT ON COLUMN processes.cpu_ticks_total IS
    'Raw cumulative CPU ticks (user+system) as reported by osquery. Monotonically increasing for a given process; reset to a fresh baseline whenever started_at changes for the same pid (PID reuse).';
