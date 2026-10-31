-- Phase 1: System State Mirror
-- Tables filled by the osquery ingest service. Read-only from the agent's
-- perspective; no write path touches these outside the ingest service.

CREATE EXTENSION IF NOT EXISTS pgcrypto; -- for gen_random_uuid()

CREATE TABLE IF NOT EXISTS processes (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    pid INTEGER NOT NULL,
    name TEXT,
    owner TEXT,
    status TEXT,
    cpu_usage FLOAT,
    memory_bytes BIGINT,
    started_at TIMESTAMPTZ,
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_processes_pid_unique ON processes(pid);
CREATE INDEX IF NOT EXISTS idx_processes_last_seen ON processes(last_seen_at);

CREATE TABLE IF NOT EXISTS network_connections (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    process_pid INTEGER,
    local_port INTEGER,
    remote_addr TEXT,
    remote_port INTEGER,
    protocol TEXT,
    state TEXT,
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_network_pid ON network_connections(process_pid);
CREATE INDEX IF NOT EXISTS idx_network_last_seen ON network_connections(last_seen_at);

CREATE TABLE IF NOT EXISTS services (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    name TEXT NOT NULL,
    status TEXT,
    last_seen_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_services_name_unique ON services(name);

CREATE TABLE IF NOT EXISTS resources (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    scope TEXT NOT NULL,          -- cgroup path or 'host'
    cpu_limit FLOAT,
    memory_limit_bytes BIGINT,
    recorded_at TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_resources_scope ON resources(scope, recorded_at);

-- Tracks each ingest poll cycle for the /health/mirror endpoint and the
-- Phase 1 benchmarking requirements (latency, staleness bound).
CREATE TABLE IF NOT EXISTS ingest_polls (
    id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    source TEXT NOT NULL,          -- 'processes' | 'network' | 'services' | 'resources'
    started_at TIMESTAMPTZ NOT NULL,
    finished_at TIMESTAMPTZ,
    row_count INTEGER,
    error TEXT
);

CREATE INDEX IF NOT EXISTS idx_ingest_polls_source_time ON ingest_polls(source, started_at DESC);
