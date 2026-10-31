-- Fixes a data-growth bug: the original ingest inserted a brand-new row
-- for every observed connection on every poll cycle, with no identity
-- key to upsert against. A single long-lived connection produced roughly
-- one duplicate row every 5 seconds instead of one row that got refreshed
-- — meaning the table represented "every observation ever made" rather
-- than "current connections", which breaks the Phase 1 coverage
-- requirement (the table should reflect current state, not poll history).
--
-- NULLs are common in these columns (e.g. a listening socket may have no
-- remote_addr yet), and Postgres unique constraints/indexes treat NULL as
-- never equal to itself, so a plain UNIQUE(...) over nullable columns
-- would let "duplicate" NULL-containing rows through. This uses an
-- expression index with COALESCE so identity comparison is well-defined
-- even when a field is absent.

CREATE UNIQUE INDEX IF NOT EXISTS idx_network_connection_identity
ON network_connections (
    COALESCE(process_pid, -1),
    COALESCE(local_port, -1),
    COALESCE(remote_addr, ''),
    COALESCE(remote_port, -1),
    COALESCE(protocol, '')
);
