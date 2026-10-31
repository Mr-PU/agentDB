mod common;

use agentdb_runtime::ingest;

/// Allowed drift between two consecutive polls of a roughly-stable
/// system, before we treat a row-count change as a duplication bug
/// rather than genuine process/connection churn.
fn is_within_normal_churn(before: i64, after: i64) -> bool {
    let allowed = (before / 2).max(5);
    (after - before).abs() <= allowed
}

#[tokio::test]
async fn poll_processes_populates_the_mirror_from_real_osquery() {
    if !common::osquery_available() {
        eprintln!("skipping: osqueryi not found on PATH");
        return;
    }

    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;

    let reported = ingest::poll_processes(&pool)
        .await
        .expect("poll_processes failed against real osquery");
    assert!(
        reported > 0,
        "expected osquery to report at least one running process"
    );

    let db_count: i64 = sqlx::query_scalar("SELECT count(*) FROM processes")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert!(
        db_count > 0,
        "osquery reported {reported} processes but the mirror has {db_count} rows"
    );
}

#[tokio::test]
async fn poll_processes_upserts_by_pid_instead_of_duplicating() {
    if !common::osquery_available() {
        eprintln!("skipping: osqueryi not found on PATH");
        return;
    }

    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;

    ingest::poll_processes(&pool).await.unwrap();
    let first: i64 = sqlx::query_scalar("SELECT count(*) FROM processes")
        .fetch_one(&pool)
        .await
        .unwrap();

    ingest::poll_processes(&pool).await.unwrap();
    let second: i64 = sqlx::query_scalar("SELECT count(*) FROM processes")
        .fetch_one(&pool)
        .await
        .unwrap();

    assert!(
        is_within_normal_churn(first, second),
        "process row count changed drastically between two consecutive \
         polls ({first} -> {second}) — this is the shape of the \
         duplicate-row bug described earlier, now expected to be fixed \
         by the ON CONFLICT (pid) upsert"
    );
}

#[tokio::test]
async fn poll_network_upserts_instead_of_accumulating_duplicate_rows() {
    // This is the direct regression test for the network-dedup bug: the
    // original ingest inserted a brand-new row every cycle with no
    // identity key, so a single long-lived connection would roughly
    // double the table on each repeated poll within the test's short
    // timeframe. The fix (migrations/0004 + the ON CONFLICT upsert in
    // poll_network) should keep two consecutive polls close in count.
    if !common::osquery_available() {
        eprintln!("skipping: osqueryi not found on PATH");
        return;
    }

    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;

    ingest::poll_network(&pool).await.unwrap();
    let first: i64 = sqlx::query_scalar("SELECT count(*) FROM network_connections")
        .fetch_one(&pool)
        .await
        .unwrap();

    ingest::poll_network(&pool).await.unwrap();
    let second: i64 = sqlx::query_scalar("SELECT count(*) FROM network_connections")
        .fetch_one(&pool)
        .await
        .unwrap();

    assert!(
        is_within_normal_churn(first, second),
        "network_connections row count changed drastically between two \
         consecutive polls ({first} -> {second}) — this is exactly the \
         duplication bug that migrations/0004 + the upsert were meant to fix"
    );
}

#[tokio::test]
async fn poll_services_upserts_by_name() {
    if !common::osquery_available() {
        eprintln!("skipping: osqueryi not found on PATH");
        return;
    }

    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;

    ingest::poll_services(&pool).await.unwrap();
    let first: i64 = sqlx::query_scalar("SELECT count(*) FROM services")
        .fetch_one(&pool)
        .await
        .unwrap();

    ingest::poll_services(&pool).await.unwrap();
    let second: i64 = sqlx::query_scalar("SELECT count(*) FROM services")
        .fetch_one(&pool)
        .await
        .unwrap();

    assert!(
        is_within_normal_churn(first, second),
        "services row count changed drastically between two consecutive \
         polls ({first} -> {second})"
    );
}

#[tokio::test]
async fn sweep_functions_do_not_remove_freshly_seen_rows() {
    if !common::osquery_available() {
        eprintln!("skipping: osqueryi not found on PATH");
        return;
    }

    let pool = common::test_pool().await;
    common::reset_tables(&pool).await;

    ingest::poll_processes(&pool).await.unwrap();
    let before: i64 = sqlx::query_scalar("SELECT count(*) FROM processes")
        .fetch_one(&pool)
        .await
        .unwrap();

    // A generous stale threshold — rows just written should never be
    // considered stale under any threshold larger than "zero".
    ingest::sweep_stale_processes(&pool, std::time::Duration::from_secs(300))
        .await
        .unwrap();

    let after: i64 = sqlx::query_scalar("SELECT count(*) FROM processes")
        .fetch_one(&pool)
        .await
        .unwrap();

    assert_eq!(
        before, after,
        "sweep removed rows that were just written — stale threshold logic is wrong"
    );
}
