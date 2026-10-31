-- Creates a read-only role for the /query raw-SQL endpoint. The API's own
-- connection pool for that endpoint should ultimately use this role rather
-- than the migration-owning role, so "SELECT-only" is enforced by Postgres
-- itself, not just by the application-level prefix check in api.rs.

DO $$
BEGIN
    IF NOT EXISTS (SELECT FROM pg_catalog.pg_roles WHERE rolname = 'agentdb_reader') THEN
        CREATE ROLE agentdb_reader LOGIN PASSWORD 'agentdb_reader_change_me';
    END IF;
END
$$;

GRANT CONNECT ON DATABASE agentdb TO agentdb_reader;
GRANT USAGE ON SCHEMA public TO agentdb_reader;
GRANT SELECT ON processes, network_connections, services, resources, ingest_polls TO agentdb_reader;

-- NOTE: deliberately no `ALTER DEFAULT PRIVILEGES` here. That clause only
-- grants on tables subsequently created *by the same role that runs the
-- ALTER* — if a later migration runs under a different role (a common
-- setup: a separate migration-owner role vs. the app's runtime role), new
-- tables silently would not get this grant, and the gap wouldn't be
-- visible until someone tried to query a newer table through the reader
-- role and got a confusing permission error.
--
-- Instead, `db::ensure_reader_grants()` re-runs `GRANT SELECT ON ALL
-- TABLES IN SCHEMA public` every application startup, after migrations.
-- It's idempotent and always covers every table that currently exists,
-- regardless of which role created it or when.
