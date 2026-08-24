//! Terminal UI browser for PostgreSQL databases, inspired by pgweb.

mod app;
mod ui;

use pgtui::db;

use anyhow::{bail, Context, Result};
use crossterm::event::{
    poll, read, DisableMouseCapture, EnableMouseCapture, Event, KeyEventKind,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use std::io::{self, Stdout};
use std::time::Duration;

const VERSION: &str = env!("CARGO_PKG_VERSION");

const USAGE: &str = "\
pgtui — PostgreSQL browser for the terminal

USAGE:
    pgtui [OPTIONS] [CONNECTION_STRING]

ARGS:
    CONNECTION_STRING    postgres://user:pass@host:port/database

OPTIONS:
    -H, --host <HOST>    server host (default localhost)
    -p, --port <PORT>    server port (default 5432)
    -U, --user <USER>    login user
    -d, --db <NAME>      database name
    --url <STRING>       connection URL (same as positional arg)
    -h, --help           print help
    -V, --version        print version

ENVIRONMENT:
    DATABASE_URL         used when no connection string is given
    PGPASSWORD           prefills the password field";

struct Opts {
    url: Option<String>,
    host: Option<String>,
    port: Option<String>,
    user: Option<String>,
    db: Option<String>,
}

fn parse_args() -> Result<(Opts, bool)> {
    let mut o = Opts {
        url: None,
        host: None,
        port: None,
        user: None,
        db: None,
    };
    let mut args = std::env::args().skip(1);
    let mut positional: Option<String> = None;
    while let Some(a) = args.next() {
        fn need(name: &str, it: &mut impl Iterator<Item = String>) -> Result<String> {
            it.next()
                .ok_or_else(|| anyhow::anyhow!("{name} requires a value"))
        }
        match a.as_str() {
            "-h" | "--help" => {
                println!("{USAGE}");
                return Ok((o, false));
            }
            "-V" | "--version" => {
                println!("pgtui {VERSION}");
                return Ok((o, false));
            }
            "--url" => o.url = Some(need("--url", &mut args)?),
            "-H" | "--host" => o.host = Some(need("--host", &mut args)?),
            "-p" | "--port" => o.port = Some(need("--port", &mut args)?),
            "-U" | "--user" => o.user = Some(need("--user", &mut args)?),
            "-d" | "--db" | "--dbname" | "-D" => o.db = Some(need("--db", &mut args)?),
            _ if a.starts_with('-') && a != "-" => bail!("unknown flag: {a}\n\n{USAGE}"),
            _ => {
                if positional.replace(a).is_some() {
                    bail!("only one CONNECTION_STRING allowed\n\n{USAGE}");
                }
            }
        }
    }
    if positional.is_some() && o.url.is_some() {
        bail!("pass either a positional CONNECTION_STRING or --url, not both");
    }
    let url = positional
        .or(o.url.take())
        .or_else(|| std::env::var("DATABASE_URL").ok());
    o.url = url;
    Ok((o, true))
}

type Tui = Terminal<CrosstermBackend<Stdout>>;

fn setup() -> io::Result<Tui> {
    enable_raw_mode()?;
    let mut out = io::stdout();
    crossterm::execute!(
        out,
        EnterAlternateScreen,
        EnableMouseCapture,
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_EVENT_TYPES
        ),
    )?;
    let backend = CrosstermBackend::new(out);
    Terminal::new(backend)
}

fn teardown() {
    let _ = crossterm::execute!(
        io::stdout(),
        PopKeyboardEnhancementFlags,
        DisableMouseCapture,
        LeaveAlternateScreen
    );
    let _ = disable_raw_mode();
}

fn run() -> Result<()> {
    let (opts, proceed) = parse_args()?;
    if !proceed {
        return Ok(());
    }
    // Auto-connect when the user supplied a URL or any explicit keyword piece
    // (-H/-p/-U/-d). Bare `pgtui` has none of these set (only defaults) and
    // must land on the interactive form.
    let has_url = opts.url.is_some();
    let has_flags =
        opts.host.is_some() || opts.port.is_some() || opts.user.is_some() || opts.db.is_some();

    let (tx, rx) = db::spawn();
    let mut a = app::App::new(
        tx,
        app::StartupOpts {
            url: opts.url,
            host: opts.host,
            port: opts.port,
            user: opts.user,
            db: opts.db,
        },
    );
    if has_url || has_flags {
        a.begin_connect();
    }

    let mut terminal = setup().context("initializing terminal")?;
    loop {
        terminal.draw(|f| ui::draw(f, &mut a))?;
        while let Ok(resp) = rx.try_recv() {
            a.on_response(resp);
        }
        if poll(Duration::from_millis(50))? {
            match read()? {
                Event::Key(k) => {
                    if k.kind == KeyEventKind::Press {
                        a.on_key(k);
                    }
                }
                Event::Mouse(m) => a.on_mouse(m),
                _ => {}
            }
        }
        if a.quit {
            break;
        }
    }
    drop(terminal);
    teardown();
    Ok(())
}

fn main() {
    if let Err(e) = run() {
        teardown();
        eprintln!("pgtui: {e:#}");
        std::process::exit(1);
    }
}
