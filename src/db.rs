//! PostgreSQL access layer.
//!
//! One background thread owns the [`postgres::Client`]. The UI thread sends
//! [`DbRequest`]s over an mpsc channel and drains [`DbResponse`]s every tick,
//! so a long-running query shows a spinner instead of freezing input.

use postgres::{Client, NoTls, SimpleQueryMessage};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------- data model

#[derive(Clone, Debug)]
pub struct TableInfo {
    pub schema: String,
    pub name: String,
    pub kind: String,
    pub est_rows: i64,
}

impl TableInfo {
    /// Schema-qualified, safely quoted identifier.
    pub fn qualified(&self) -> String {
        format!("{}.{}", quote_ident(&self.schema), quote_ident(&self.name))
    }

    pub fn label(&self) -> String {
        format!("{}.{}", self.schema, self.name)
    }
}

/// Quote an identifier for safe interpolation into SQL.
pub fn quote_ident(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

#[derive(Clone, Debug)]
pub struct ConnMeta {
    pub full_version: String,
    pub short_version: String,
    pub database: String,
    pub user: String,
    pub host: String,
    pub port: String,
}

#[derive(Clone, Debug, Default)]
pub struct Grid {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Option<String>>>,
}

impl Grid {
    fn from_simple(columns: Vec<String>, msgs: &[SimpleQueryMessage]) -> Grid {
        let n = columns.len();
        Grid {
            columns,
            rows: msgs
                .iter()
                .filter_map(|m| match m {
                    SimpleQueryMessage::Row(r) => Some(
                        (0..n)
                            .map(|i| r.get(i).map(str::to_string))
                            .collect::<Vec<_>>(),
                    ),
                    _ => None,
                })
                .collect(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct RowsResult {
    pub grid: Grid,
    pub total: i64,
    pub elapsed: Duration,
}

#[derive(Clone, Debug)]
pub struct ColInfo {
    pub name: String,
    pub data_type: String,
    pub nullable: bool,
    pub default: String,
    pub comment: String,
    pub pk: bool,
}

#[derive(Clone, Debug)]
pub struct ConInfo {
    pub name: String,
    pub kind: String,
    pub definition: String,
}

#[derive(Clone, Debug)]
pub struct IdxInfo {
    pub name: String,
    pub is_unique: bool,
    pub is_primary: bool,
    pub definition: String,
}

#[derive(Clone, Debug)]
pub struct TableDetail {
    pub columns: Vec<ColInfo>,
    pub constraints: Vec<ConInfo>,
    pub indexes: Vec<IdxInfo>,
    pub elapsed: Duration,
}

#[derive(Clone, Debug)]
pub struct QueryResult {
    pub grid: Option<Grid>,
    /// `Some(n)` for command statements (INSERT/UPDATE/...); `None` for result sets.
    pub affected: Option<u64>,
    pub elapsed: Duration,
}

#[derive(Clone, Debug)]
pub struct ServerStats {
    pub short_version: String,
    pub database: String,
    pub size_pretty: String,
    pub connections: i64,
    pub active: i64,
    pub started: String,
}

// ----------------------------------------------------------- protocol types

#[derive(Debug)]
pub enum DbRequest {
    Connect(String),
    Tables,
    Describe {
        schema: String,
        table: String,
    },
    Rows {
        schema: String,
        table: String,
        page: u32,
        page_size: u32,
        order: Option<(String, bool)>,
        filter: Option<String>,
    },
    Execute(String),
    ServerInfo,
}

#[derive(Debug)]
pub enum DbResponse {
    Connect(Result<ConnMeta, String>),
    Tables(Result<Vec<TableInfo>, String>),
    Describe {
        schema: String,
        table: String,
        result: Result<TableDetail, String>,
    },
    Rows {
        schema: String,
        table: String,
        page: u32,
        result: Result<RowsResult, String>,
    },
    Execute(Result<QueryResult, String>),
    ServerInfo(Result<ServerStats, String>),
}

/// Spawn the worker thread; returns the request sender and response receiver.
pub fn spawn() -> (Sender<DbRequest>, Receiver<DbResponse>) {
    let (req_tx, req_rx) = channel::<DbRequest>();
    let (resp_tx, resp_rx) = channel::<DbResponse>();
    std::thread::Builder::new()
        .name("pgtui-db".into())
        .spawn(move || {
            let mut client: Option<Client> = None;
            while let Ok(req) = req_rx.recv() {
                let resp = match req {
                    DbRequest::Connect(url) => connect(&mut client, url),
                    other => run(&mut client, other),
                };
                let _ = resp_tx.send(resp);
            }
        })
        .expect("spawn pgtui-db thread");
    (req_tx, resp_rx)
}

fn connect(slot: &mut Option<Client>, url: String) -> DbResponse {
    *slot = None;
    match Client::connect(url.as_str(), NoTls) {
        Ok(mut c) => match fetch_meta(&mut c) {
            Ok(meta) => {
                *slot = Some(c);
                DbResponse::Connect(Ok(meta))
            }
            Err(e) => DbResponse::Connect(Err(e.to_string())),
        },
        Err(e) => DbResponse::Connect(Err(e.to_string())),
    }
}

fn not_connected(req: DbRequest) -> DbResponse {
    let msg = "not connected".to_string();
    match req {
        DbRequest::Connect(_) => unreachable!("connect handled by caller"),
        DbRequest::Tables => DbResponse::Tables(Err(msg)),
        DbRequest::Describe { schema, table } => DbResponse::Describe {
            schema,
            table,
            result: Err(msg),
        },
        DbRequest::Rows {
            schema,
            table,
            page,
            ..
        } => DbResponse::Rows {
            schema,
            table,
            page,
            result: Err(msg),
        },
        DbRequest::Execute(_) => DbResponse::Execute(Err(msg)),
        DbRequest::ServerInfo => DbResponse::ServerInfo(Err(msg)),
    }
}

fn run(slot: &mut Option<Client>, req: DbRequest) -> DbResponse {
    if slot.is_none() {
        return not_connected(req);
    }
    let c = slot.as_mut().expect("checked above");
    match req {
        DbRequest::Connect(_) => unreachable!("connect handled by caller"),
        DbRequest::Tables => DbResponse::Tables(list_tables(c).map_err(|e| e.to_string())),
        DbRequest::Describe { schema, table } => {
            let r = describe(c, &schema, &table).map_err(|e| e.to_string());
            DbResponse::Describe {
                schema,
                table,
                result: r,
            }
        }
        DbRequest::Rows {
            schema,
            table,
            page,
            page_size,
            order,
            filter,
        } => {
            let ord = order.as_ref().map(|(s, d)| (s.as_str(), *d));
            let r = rows(c, &schema, &table, page, page_size, ord, filter.as_deref())
                .map_err(|e| e.to_string());
            DbResponse::Rows {
                schema,
                table,
                page,
                result: r,
            }
        }
        DbRequest::Execute(sql) => {
            DbResponse::Execute(execute_sql(c, &sql).map_err(|e| e.to_string()))
        }
        DbRequest::ServerInfo => DbResponse::ServerInfo(server_info(c).map_err(|e| e.to_string())),
    }
}

// ------------------------------------------------------------ SQL builders

/// Build `(data_sql, count_sql)` for a paged table scan.
///
/// `order` is `(column, descending)` and MUST already be whitelisted against the
/// table's real column names by the caller — it gets identifier-quoted here.
/// `filter` is a raw SQL fragment appended after WHERE (operator tool, like pgweb).
pub fn build_rows_sql(
    schema: &str,
    table: &str,
    page: u32,
    page_size: u32,
    order: Option<(&str, bool)>,
    filter: Option<&str>,
) -> (String, String) {
    let qn = format!("{}.{}", quote_ident(schema), quote_ident(table));
    let wf = match filter.map(str::trim).filter(|f| !f.is_empty()) {
        Some(f) => format!(" WHERE {f}"),
        None => String::new(),
    };
    let ob = match order {
        Some((col, desc)) => format!(
            " ORDER BY {} {}",
            quote_ident(col),
            if desc { "DESC" } else { "ASC" }
        ),
        None => String::new(),
    };
    let off = page.saturating_sub(1).saturating_mul(page_size);
    (
        format!("SELECT * FROM {qn}{wf}{ob} LIMIT {page_size} OFFSET {off}"),
        format!("SELECT count(*)::bigint FROM {qn}{wf}"),
    )
}

/// Extract the short server version ("16.4") from `version()` output.
pub fn short_version(full: &str) -> String {
    full.split_whitespace()
        .find(|t| t.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .unwrap_or("unknown")
        .trim_end_matches(',')
        .to_string()
}

// ------------------------------------------------------------- worker impls

fn fetch_meta(c: &mut Client) -> Result<ConnMeta, postgres::Error> {
    let row = c.query_one(
        "SELECT version()::text, current_database()::text, current_user::text, \
         COALESCE(host(inet_server_addr()), 'localhost'), \
         COALESCE(inet_server_port()::text, '5432')",
        &[],
    )?;
    let full: String = row.get(0);
    Ok(ConnMeta {
        short_version: short_version(&full),
        full_version: full,
        database: row.get(1),
        user: row.get(2),
        host: row.get(3),
        port: row.get(4),
    })
}

fn list_tables(c: &mut Client) -> Result<Vec<TableInfo>, postgres::Error> {
    let rows = c.query(
        "SELECT n.nspname AS schema, c.relname AS name, \
         CASE c.relkind WHEN 'r' THEN 'table' WHEN 'v' THEN 'view' \
                        WHEN 'm' THEN 'materialized view' WHEN 'p' THEN 'partitioned table' \
                        WHEN 'f' THEN 'foreign table' ELSE 'other' END AS kind, \
         GREATEST(c.reltuples, 0)::bigint AS est_rows \
         FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
         WHERE c.relkind IN ('r', 'v', 'm', 'p', 'f') \
           AND n.nspname NOT IN ('pg_catalog', 'information_schema') \
           AND n.nspname !~ '^pg_toast' \
         ORDER BY (n.nspname = 'public') DESC, n.nspname, c.relname",
        &[],
    )?;
    Ok(rows
        .into_iter()
        .map(|r| TableInfo {
            schema: r.get(0),
            name: r.get(1),
            kind: r.get(2),
            est_rows: r.get::<_, Option<i64>>(3).unwrap_or(0),
        })
        .collect())
}

fn table_columns(
    c: &mut Client,
    schema: &str,
    table: &str,
) -> Result<Vec<String>, postgres::Error> {
    let rows = c.query(
        "SELECT a.attname FROM pg_attribute a \
         JOIN pg_class t ON t.oid = a.attrelid \
         JOIN pg_namespace n ON n.oid = t.relnamespace \
         WHERE t.relname = $1 AND n.nspname = $2 AND a.attnum > 0 AND NOT a.attisdropped \
         ORDER BY a.attnum",
        &[&table, &schema],
    )?;
    Ok(rows.iter().map(|r| r.get(0)).collect())
}

fn rows(
    c: &mut Client,
    schema: &str,
    table: &str,
    page: u32,
    page_size: u32,
    order: Option<(&str, bool)>,
    filter: Option<&str>,
) -> Result<RowsResult, postgres::Error> {
    let t0 = Instant::now();
    let columns = table_columns(c, schema, table)?;
    // Whitelist the sort column against real column names before interpolation.
    let order = order.filter(|(col, _)| columns.iter().any(|c| c == col));
    let (data_sql, count_sql) =
        build_rows_sql(schema, table, page.max(1), page_size, order, filter);
    let cnt = c.simple_query(&count_sql)?;
    let total = cnt
        .iter()
        .find_map(|m| match m {
            SimpleQueryMessage::Row(r) => r.get(0).and_then(|v| v.parse::<i64>().ok()),
            _ => None,
        })
        .unwrap_or(0);
    let data = c.simple_query(&data_sql)?;
    Ok(RowsResult {
        grid: Grid::from_simple(columns, &data),
        total,
        elapsed: t0.elapsed(),
    })
}

const COLUMNS_SQL: &str = "SELECT a.attname, format_type(a.atttypid, a.atttypmod) AS data_type, \
     NOT a.attnotnull AS nullable, \
     COALESCE(pg_get_expr(ad.adbin, ad.adrelid), '') AS defval, \
     COALESCE(col_description(a.attrelid, a.attnum), '') AS comment, \
     EXISTS (SELECT 1 FROM pg_index ix WHERE ix.indrelid = a.attrelid AND ix.indisprimary \
             AND a.attnum = ANY(ix.indkey::smallint[])) AS pk \
     FROM pg_attribute a \
     LEFT JOIN pg_attrdef ad ON ad.adrelid = a.attrelid AND ad.adnum = a.attnum \
     JOIN pg_class t ON t.oid = a.attrelid \
     JOIN pg_namespace n ON n.oid = t.relnamespace \
     WHERE t.relname = $1 AND n.nspname = $2 AND a.attnum > 0 AND NOT a.attisdropped \
     ORDER BY a.attnum";

fn describe(c: &mut Client, schema: &str, table: &str) -> Result<TableDetail, postgres::Error> {
    let t0 = Instant::now();
    let cols = c.query(COLUMNS_SQL, &[&table, &schema])?;
    let columns = cols
        .iter()
        .map(|r| ColInfo {
            name: r.get(0),
            data_type: r.get(1),
            nullable: r.get(2),
            default: r.get(3),
            comment: r.get(4),
            pk: r.get(5),
        })
        .collect();

    let cons = c.query(
        "SELECT conname, contype::text, pg_get_constraintdef(oid) \
         FROM pg_constraint \
         WHERE conrelid = (SELECT oid FROM pg_class \
                           WHERE relname = $1 AND relnamespace = (SELECT oid FROM pg_namespace WHERE nspname = $2)) \
         ORDER BY conname",
        &[&table, &schema],
    )?;
    let constraints = cons
        .iter()
        .map(|r| {
            let t: String = r.get(1);
            ConInfo {
                name: r.get(0),
                kind: match t.as_str() {
                    "p" => "PRIMARY KEY".into(),
                    "f" => "FOREIGN KEY".into(),
                    "u" => "UNIQUE".into(),
                    "c" => "CHECK".into(),
                    "x" => "EXCLUSION".into(),
                    other => other.to_uppercase(),
                },
                definition: r.get(2),
            }
        })
        .collect();

    let idxs = c.query(
        "SELECT i.relname, ix.indisunique, ix.indisprimary, pg_get_indexdef(ix.indexrelid) \
         FROM pg_index ix \
         JOIN pg_class t ON t.oid = ix.indrelid \
         JOIN pg_namespace n ON n.oid = t.relnamespace \
         JOIN pg_class i ON i.oid = ix.indexrelid \
         WHERE t.relname = $1 AND n.nspname = $2 \
         ORDER BY i.relname",
        &[&table, &schema],
    )?;
    let indexes = idxs
        .iter()
        .map(|r| IdxInfo {
            name: r.get(0),
            is_unique: r.get(1),
            is_primary: r.get(2),
            definition: r.get(3),
        })
        .collect();

    Ok(TableDetail {
        columns,
        constraints,
        indexes,
        elapsed: t0.elapsed(),
    })
}

fn execute_sql(c: &mut Client, sql: &str) -> Result<QueryResult, postgres::Error> {
    let t0 = Instant::now();
    let trimmed = sql.trim();
    match c.prepare(trimmed) {
        // Statement returning no rows: report the command tag row count.
        Ok(stmt) if stmt.columns().is_empty() => {
            let affected = c.execute(&stmt, &[])?;
            Ok(QueryResult {
                grid: None,
                affected: Some(affected),
                elapsed: t0.elapsed(),
            })
        }
        // Result set: re-run through the simple protocol so every value is text.
        Ok(stmt) => {
            let cols = stmt
                .columns()
                .iter()
                .map(|k| k.name().to_string())
                .collect();
            let srows = c.simple_query(trimmed)?;
            Ok(QueryResult {
                grid: Some(Grid::from_simple(cols, &srows)),
                affected: None,
                elapsed: t0.elapsed(),
            })
        }
        // Multi-statement strings cannot be prepared; fall back to batch_execute.
        Err(prepare_err) => match c.batch_execute(trimmed) {
            Ok(()) => Ok(QueryResult {
                grid: None,
                affected: Some(0),
                elapsed: t0.elapsed(),
            }),
            Err(_) => Err(prepare_err),
        },
    }
}

fn server_info(c: &mut Client) -> Result<ServerStats, postgres::Error> {
    let row = c.query_one(
        "SELECT version()::text, current_database()::text, \
         pg_size_pretty(pg_database_size(current_database())), \
         (SELECT count(*) FROM pg_stat_activity)::bigint, \
         (SELECT count(*) FROM pg_stat_activity WHERE state = 'active')::bigint, \
         pg_postmaster_start_time()::text",
        &[],
    )?;
    let full: String = row.get(0);
    Ok(ServerStats {
        short_version: short_version(&full),
        database: row.get(1),
        size_pretty: row.get(2),
        connections: row.get(3),
        active: row.get(4),
        started: row.get(5),
    })
}

// ------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_identifiers() {
        assert_eq!(quote_ident("users"), "\"users\"");
        assert_eq!(quote_ident("weird\"name"), "\"weird\"\"name\"");
    }

    #[test]
    fn builds_paged_rows_sql() {
        let (data, count) = build_rows_sql("public", "users", 3, 50, Some(("name", true)), None);
        assert_eq!(
            data,
            "SELECT * FROM \"public\".\"users\" ORDER BY \"name\" DESC LIMIT 50 OFFSET 100"
        );
        assert_eq!(count, "SELECT count(*)::bigint FROM \"public\".\"users\"");
    }

    #[test]
    fn builds_filtered_rows_sql() {
        let (data, count) = build_rows_sql("s", "t", 1, 10, None, Some("  id > 5  "));
        assert!(data.ends_with("WHERE id > 5 LIMIT 10 OFFSET 0"), "{data}");
        assert!(count.ends_with("WHERE id > 5"), "{count}");
    }

    #[test]
    fn parses_short_versions() {
        assert_eq!(
            short_version("PostgreSQL 16.4 (Homebrew) on aarch64"),
            "16.4"
        );
        assert_eq!(short_version("Postgres-XL 10.2 on x86_64"), "10.2");
        assert_eq!(short_version("nonsense"), "unknown");
    }
}
