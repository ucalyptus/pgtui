//! Live round-trip against a real Postgres, exercising the real
//! `pgtui::db` worker thread (DbRequest -> DbResponse over mpsc).
//!
//! Ignored by default; run with:
//!   PGTUI_TEST_URL=postgres://postgres@127.0.0.1:55432/demo cargo test --test live -- --ignored
//!
//! Idempotent: fixture rows are deleted before insert and every assertion is
//! scoped to the fixture emails, so repeated runs against a dirty database
//! still pass. The worker is polled with `recv_timeout` so a dead worker
//! fails the test instead of hanging.

use pgtui::db::{self, DbRequest, DbResponse};
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

const FIXTURE_EMAILS: [&str; 2] = ["zzz@example.com", "aaa@example.com"];
const FIXTURE_FILTER: &str = "email IN ('zzz@example.com','aaa@example.com')";

fn url() -> String {
    std::env::var("PGTUI_TEST_URL").expect("set PGTUI_TEST_URL")
}

/// Send one request and wait for its response (10 s budget; a wedged worker
/// fails the test rather than hanging CI).
fn roundtrip(tx: &Sender<DbRequest>, rx: &Receiver<DbResponse>, req: DbRequest) -> DbResponse {
    tx.send(req).expect("send request");
    rx.recv_timeout(Duration::from_secs(10))
        .expect("worker response within 10s")
}

fn execute(tx: &Sender<DbRequest>, rx: &Receiver<DbResponse>, sql: &str) {
    match roundtrip(tx, rx, DbRequest::Execute(sql.into())) {
        DbResponse::Execute(Ok(_)) => {}
        other => panic!("execute failed: {other:?}"),
    }
}

fn rows(
    tx: &Sender<DbRequest>,
    rx: &Receiver<DbResponse>,
    order: Option<(&str, bool)>,
    filter: Option<&str>,
) -> pgtui::db::RowsResult {
    match roundtrip(
        tx,
        rx,
        DbRequest::Rows {
            schema: "public".into(),
            table: "users".into(),
            page: 1,
            page_size: 50,
            order: order.map(|(c, d)| (c.to_string(), d)),
            filter: filter.map(str::to_string),
        },
    ) {
        DbResponse::Rows { result: Ok(r), .. } => r,
        other => panic!("rows failed: {other:?}"),
    }
}

#[test]
#[ignore = "needs PGTUI_TEST_URL"]
fn worker_connect_tables_rows_sort_filter_execute() {
    let (tx, rx) = db::spawn();

    // Connect through the real worker.
    let meta = match roundtrip(&tx, &rx, DbRequest::Connect(url())) {
        DbResponse::Connect(Ok(m)) => m,
        other => panic!("connect failed: {other:?}"),
    };
    assert!(!meta.short_version.is_empty());
    assert_eq!(meta.database, "demo");

    // Tables listing via the real catalog query.
    let tables = match roundtrip(&tx, &rx, DbRequest::Tables) {
        DbResponse::Tables(Ok(t)) => t,
        other => panic!("tables failed: {other:?}"),
    };
    assert!(tables
        .iter()
        .any(|t| t.schema == "public" && t.name == "users"));

    // Remove any prior fixture rows, then insert exactly two.
    execute(
        &tx,
        &rx,
        "DELETE FROM users WHERE email IN ('zzz@example.com','aaa@example.com')",
    );
    execute(
        &tx,
        &rx,
        "INSERT INTO users (name, email, age) VALUES \
         ('zzz_test','zzz@example.com',99), ('aaa_test','aaa@example.com',NULL)",
    );

    // Rows with ORDER BY name ASC — aaa_test first; grid carries users columns.
    let rows_asc = rows(&tx, &rx, Some(("name", false)), Some(FIXTURE_FILTER));
    assert_eq!(rows_asc.grid.columns[0], "id");
    assert!(rows_asc.grid.columns.iter().any(|c| c == "name"));
    assert_eq!(
        rows_asc
            .grid
            .rows
            .iter()
            .filter(|r| FIXTURE_EMAILS.contains(&r[2].as_deref().unwrap_or("")))
            .find_map(|r| r[1].as_deref())
            .expect("a fixture row appears first"),
        "aaa_test",
        "asc: aaa_test sorts before zzz_test"
    );

    // Rows with ORDER BY name DESC — zzz_test first among fixtures.
    let rows_desc = rows(&tx, &rx, Some(("name", true)), Some(FIXTURE_FILTER));
    let fixture_desc: Vec<&str> = rows_desc
        .grid
        .rows
        .iter()
        .filter(|r| FIXTURE_EMAILS.contains(&r[2].as_deref().unwrap_or("")))
        .map(|r| r[1].as_deref().unwrap_or(""))
        .collect();
    assert_eq!(fixture_desc, vec!["zzz_test", "aaa_test"], "desc ordering");

    // Filter descends all the way to the server: scoping to fixture emails
    // yields exactly the two seeded rows, still sorted.
    let only_fixtures = rows(&tx, &rx, Some(("name", false)), Some(FIXTURE_FILTER));
    assert_eq!(only_fixtures.total, 2, "filtered total");
    let names: Vec<&str> = only_fixtures
        .grid
        .rows
        .iter()
        .map(|r| r[1].as_deref().unwrap_or(""))
        .collect();
    assert_eq!(names, vec!["aaa_test", "zzz_test"], "filtered + sorted");

    // WHERE filter excludes rows with NULL age: aaa_test is seeded with a
    // NULL age, so the IS NOT NULL filter must drop exactly one fixture row
    // while the unfiltered fixture set keeps both.
    let all_fixtures = rows(&tx, &rx, None, Some(FIXTURE_FILTER));
    let non_null = rows(
        &tx,
        &rx,
        None,
        Some(&format!("{FIXTURE_FILTER} AND age IS NOT NULL")),
    );
    assert_eq!(all_fixtures.total, 2, "all fixture rows present");
    assert_eq!(non_null.total, 1, "NULL-age fixture excluded");
    assert!(
        non_null.total < all_fixtures.total,
        "filter strictly shrinks the set"
    );

    // Arbitrary execute returns a result grid through the worker.
    match roundtrip(
        &tx,
        &rx,
        DbRequest::Execute("SELECT 1 AS one, 'x' AS x".into()),
    ) {
        DbResponse::Execute(Ok(qr)) => {
            let g = qr.grid.expect("result set");
            assert_eq!(g.columns, vec!["one".to_string(), "x".to_string()]);
            assert_eq!(g.rows.len(), 1);
            assert_eq!(g.rows[0][0].as_deref(), Some("1"));
        }
        other => panic!("execute failed: {other:?}"),
    }

    // Server info round trip.
    match roundtrip(&tx, &rx, DbRequest::ServerInfo) {
        DbResponse::ServerInfo(Ok(s)) => assert_eq!(s.database, "demo"),
        other => panic!("server info failed: {other:?}"),
    }
}
