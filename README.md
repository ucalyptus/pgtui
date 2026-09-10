# pgtui

A terminal-UI browser for PostgreSQL databases, built with [ratatui](https://github.com/ratatui/ratatui).
It is the TUI sibling of [pgweb](https://github.com/sosedoff/pgweb): connect, browse relations,
page through rows, inspect structure/indexes, run SQL — without leaving the terminal.

## Features
- Connect via URL (`postgres://...`), `DATABASE_URL`, CLI flags, PostgreSQL's `~/.pgpass` or `~/.netrc`, or an interactive form
- Sidebar with schemas/tables/views/materialized views + live filter (public schema first)
- **Rows** tab: paginated data grid, cell cursor, column sort, raw-SQL `WHERE` filter, CSV export
- **Structure** tab: columns (type, nullability, default, PK, comment) and constraints
- **Indexes** tab: definitions, uniqueness, primary flags
- **Query** tab: multi-line SQL editor, history, result grid, command tags, CSV export
- **Info** tab: server version, database size, connection counts, start time
- Mouse support: click tables/tabs/cells/headers to sort, scroll everything
- Kitty/Ghostty keyboard protocol (disambiguated keys) when the terminal supports it
- Single background DB thread: long queries never freeze the UI

## Install

```sh
cargo install --path .
# or
cargo build --release   # binary at target/release/pgtui
```

Requires Rust 1.81+. No external dependencies; talks to Postgres over the wire protocol.

## Usage

```sh
pgtui                                                # auto-connects from ~/.pgpass when present
pgtui postgres://user:pass@localhost:5432/mydb       # full URL
pgtui --url "$SOME_URL"                              # same, explicit
DATABASE_URL=postgres://... pgtui                    # from environment

pgtui -H localhost -p 5432 -U alice -d shop          # keyword pieces; ~/.pgpass supplies the password
```

For automatic credential loading, pgtui accepts PostgreSQL's `~/.pgpass` or
a common `~/.netrc` file. Both files must have mode `0600`. A complete
`.pgpass` entry auto-connects; `.netrc` entries prefill the form because netrc
does not include a database name.

Preferred PostgreSQL format:

```text
hostname:port:database:username:password
localhost:5432:shop:alice:secret
```

Common netrc format:

```text
machine localhost login alice password secret
```

Use `*` as a `.pgpass` wildcard, or `default` in `.netrc`. Set `PGPASSFILE`
or `NETRC` to use another file. Explicit URLs and `PGPASSWORD` take
precedence over credential files.

## Keys

Global:

| Key        | Action                                    |
|------------|-------------------------------------------|
| `Tab`      | toggle focus sidebar / content            |
| `1`..`5`   | switch Rows / Structure / Indexes / Query / Info |
| `?`        | help overlay                              |
| `r`        | refresh current view                      |
| `q` / Ctrl+C | quit (Ctrl+C always works)              |

Sidebar: `j/k` or arrows move, `Enter` opens relation, `/` filters,
`Esc` clears filter.

Rows tab: arrows/hjkl move the cell cursor, `PgUp/PgDn` or `n/p` change page,
`g/G` first/last page, `s` sorts by the cursor column (asc → desc → off),
`/` edits a raw `WHERE` clause, `e` exports the loaded page to CSV.
Click a column header to sort; click cells to move the cursor.

Query tab: type SQL, `Alt+Enter` or `F5` executes, `Ctrl+H` toggles history,
`Esc` returns from results to the editor, `e` exports the last result grid.
In the editor `Tab` indents, `Ctrl+K` kills to end of line.

Mouse: wheel scrolls lists/grids/results; left-click selects relations,
switches tabs, focuses form fields and positions cursors.

## Architecture

`src/db.rs` owns one `postgres::Client` on a background thread. The UI loop sends
`DbRequest`s over an mpsc channel and drains `DbResponse`s every tick, so a slow
query only shows a spinner instead of freezing input. All identifiers are quoted;
sort columns are whitelisted against catalog-fetched names before interpolation.
Row data uses the simple query protocol so every value type renders as text
(`NULL` shown dim).

## Limitations

- No TLS yet (`sslmode=require` URLs will fail). Use an SSH tunnel for remote DBs.
- CSV export writes what is loaded (current page / last result), not the whole table.

## Development

```sh
cargo test                       # unit tests
PGTUI_TEST_URL=postgres://... cargo test -- --ignored   # live round-trip test
```

## License

MIT. Inspired by [pgweb](https://github.com/sosedoff/pgweb) by Semyon Sosedoff.
