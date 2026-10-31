# Contributing

This is Phase 1 (system state mirror) of a multi-phase project — see the
architecture doc and per-phase docs for the full design before making
structural changes. Phase boundaries matter: Phase 1 has no write path to
the host at all, and that's intentional, not incomplete.

## Before opening a PR

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo build --release
```

CI runs the same checks; a PR that fails any of them won't merge.

## Conventions

- **Migrations are append-only.** Never edit a migration that's already
  been merged — add a new one. `sqlx::migrate!` tracks applied migrations
  by filename+checksum; editing an old one breaks it for anyone who
  already ran it.
- **Every new table needs a decision about `agentdb_reader` access.** If
  it should be queryable via `/query`, no action needed —
  `db::ensure_reader_grants` grants `SELECT` on all tables automatically
  on every startup. If it should *not* be reader-visible (e.g. anything
  with secrets), say so explicitly in the migration's comments, since the
  default is "visible."
- **Errors returned to API clients go through `api_error`/`internal_error`
  in `src/api.rs`**, not raw `.to_string()` on a `sqlx::Error` — those can
  leak schema details. `/query` is the one deliberate exception, since the
  caller wrote that SQL themselves.
- **Use `extract::ValidatedJson`/`extract::ValidatedQuery`, not
  `axum::Json`/`axum::Query`, for any new handler.** The bare Axum
  extractors return a plain-text body on failure (missing field,
  malformed value, wrong content-type) instead of this project's
  `{"error": "..."}` envelope — easy to miss in review since the handler
  looks identical either way, and the gap only shows up when someone
  actually sends bad input. Every existing endpoint was fixed to use the
  wrappers; a new one should start with them rather than needing the same
  fix later.
- **Don't bypass `validate_single_select`** for `/query`-like endpoints.
  If you need a new raw-SQL-ish endpoint, reuse it or extend it rather
  than writing a second, less-tested check.
- **`permissions::check_permission` is the only enforcement choke point.**
  No handler, action, or tool should query `grants` directly to decide
  whether something is allowed — always go through `check_permission`,
  even if it feels like a shortcut for a simple case. This is what
  guarantees every allow/deny decision is logged to `permission_checks`
  and keeps the audit trail complete. The `/permissions/check` HTTP route
  is for manual inspection only — Phase 3's action engine (`actions.rs`)
  calls `check_permission` directly in-process, not over HTTP.
- **A new action handler must implement both the real path and the
  dry-run path in the same `execute()`/`compensate()` method, not as
  separate functions the engine switches between.** See the trait doc
  comment on `ActionHandler` in `src/actions.rs` for why: colocating them
  is what makes it structurally hard for a handler to accidentally take
  its real path while the engine thinks it's simulating. Every real
  system call belongs in the `actions::real` submodule — if you're
  calling `tokio::process::Command`, `nix::sys::signal`, or
  `tokio::fs::write` from anywhere else in `actions.rs`, it almost
  certainly belongs in `real` instead, since that module is what makes
  "grep for every place this codebase can touch the real host" a
  one-line answer.
- **A new LLM provider implements `diagnosis::LlmBackend` and is wired
  into `LlmProvider`/`DiagnosisConfig::from_env()`** — see
  `OtariClient` alongside `AnthropicClient` for the pattern. Nothing
  else in the codebase should need to change; `diagnose_api.rs` and the
  top-level `diagnose()` function only ever see `Arc<dyn LlmBackend>`.
- **Both diagnosis modes must only ever see `DiagnosticContext`, gathered
  once before either runs.** Don't let either mode make its own
  additional queries — that's what keeps "cite only retrieved data" a
  structural property of `diagnosis.rs` instead of an instruction a mode
  could ignore. If a mode needs more data, add it to
  `gather_context`/`DiagnosticContext`, not as a side query inside the
  mode itself.
- **If two call sites need the same query, extract a shared pool-level
  function rather than duplicating SQL.** See `api::query_processes`/
  `query_network`/`query_services`, written once and used by both the
  Phase 1 HTTP handlers and Phase 5's `tool_registry::execute_read_tool`
  — duplicated SQL is exactly the kind of thing that quietly drifts
  apart when one copy gets a bug fix and the other doesn't.
- **A new write path needs its state change and its event in the same
  transaction, committed together — never two separate writes.** Follow
  the pattern in `propose_action`/`create_grant`/`revoke_grant`: open a
  transaction with `pool.begin()`, do the state-change query and the
  `events::emit_event()` call against `&mut *tx`/`&mut tx`, then
  `tx.commit()`. The one exception is a write path that wraps a slow or
  unbounded real-world operation (see `execute_action`'s doc comment) —
  in that case, keep the transaction short and bracket the untransacted
  work, rather than holding a connection open across it.
- **A new action handler needs a test proving its dry-run path never
  reaches the real one.** The existing pattern (see
  `dry_run_execute_never_touches_the_real_path_for_process_stop` in
  `actions.rs`, and the HTTP-level versions in `tests/actions_tests.rs`)
  is to call the real path with an input that would visibly fail or do
  something detectable if actually executed (a pid that can't exist, a
  filesystem path that can't exist), then assert the dry-run result
  doesn't show that failure. A test that only checks `dry_run: true` in
  the response proves the flag was *set*, not that the real call was
  *skipped* — prefer the stronger version.
- **Ingest loops must remain cancellable.** If you add a new poll loop,
  follow the `tokio::select!` + `CancellationToken` pattern in
  `spawn_ingest_loops` — don't add a bare infinite loop that
  `main.rs`'s shutdown sequence can't wait for.

## Adding a new osquery-backed table

1. Add the query constant and a raw deserialization struct in
   `models.rs` (see `OsqueryProcessRow` for the pattern — osquery returns
   almost everything as strings, so the raw struct should reflect that
   and parsing happens explicitly in `ingest.rs`).
2. Add the destination table in a new migration.
3. Add a `poll_x` function in `ingest.rs`, following the existing
   transaction-per-cycle + `ingest_polls` bookkeeping pattern.
4. Wire it into `spawn_ingest_loops` at whichever interval fits (see
   Phase 1 doc §3.1 for the reasoning behind 5s/30s defaults).
5. Add a typed read endpoint in `api.rs` if it should be queryable
   beyond raw `/query`.

## Testing

Three tiers, from fastest/most isolated to slowest/most realistic:

1. **Unit tests**, alongside the code they test (`cargo test` picks these
   up automatically). See `src/query_guard.rs` and the `compute_process_sample`
   tests in `src/ingest.rs` for the pattern: pure functions with no I/O,
   tested directly, covering both the normal case and the specific bug
   each one was written to prevent recurring.
2. **Integration tests**, in `tests/`. These need a real (disposable)
   Postgres — set `DATABASE_URL` before running them, or use `make up`
   first. `tests/api_tests.rs` exercises the real router via
   `tower::ServiceExt::oneshot` (no port binding needed) and needs no
   osquery. `tests/ingest_tests.rs` needs `osqueryi` on `PATH`; tests
   there check for it themselves via `common::osquery_available()` and
   skip (not fail) if it's missing, so the suite still passes without
   osquery installed locally.
3. **Scripts**, for things that need a fully running server process:
   `scripts/benchmark.sh` (Phase 1 exit-criteria: latency, coverage,
   staleness) and `scripts/resilience-test.sh` (failed-poll recovery).
   Run these after any change to `ingest.rs` or `api.rs` that could
   affect those behaviors.

The project exposes a library crate (`src/lib.rs`, `agentdb_runtime`)
specifically so integration tests can reach `api::router`, `ingest::*`,
etc. directly — `src/main.rs` is a thin binary wrapper around it. If you
add a new module, add it to `src/lib.rs`'s `pub mod` list, not just as a
`mod` declaration in `main.rs`, or it won't be reachable from `tests/`.

```bash
make up                                    # start disposable Postgres
export DATABASE_URL=postgres://agentdb:agentdb@localhost:5432/agentdb
cargo test                                 # unit + integration tests
make bench                                 # needs a running `serve` instance
make resilience                            # needs DATABASE_URL + psql
```

## Reporting a bug vs. a design gap

If something is wrong for the phase it's in (e.g. a Phase 1 endpoint
writes to the host), that's a bug — open an issue or PR against this
phase. If something is *missing* because a later phase hasn't been built
yet (e.g. "there's no permission check"), check the phase docs first —
it's very likely already scoped for Phase 2+ rather than an oversight
here.
