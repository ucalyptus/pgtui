//! Application state: screens, key/mouse handling, request orchestration.

use crate::db::{
    ConnMeta, DbRequest, DbResponse, Grid, QueryResult, RowsResult, ServerStats, TableDetail,
    TableInfo,
};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use std::path::Path;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const PAGE_SIZE: u32 = 50;
const TOAST_TTL: Duration = Duration::from_secs(4);

// ------------------------------------------------------------------- enums

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Form,
    Browser,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Rows,
    Structure,
    Indexes,
    Query,
    Info,
}

impl Tab {
    pub const ALL: [Tab; 5] = [
        Tab::Rows,
        Tab::Structure,
        Tab::Indexes,
        Tab::Query,
        Tab::Info,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Tab::Rows => "rows",
            Tab::Structure => "structure",
            Tab::Indexes => "indexes",
            Tab::Query => "query",
            Tab::Info => "info",
        }
    }

    pub fn hotkey(self) -> char {
        match self {
            Tab::Rows => '1',
            Tab::Structure => '2',
            Tab::Indexes => '3',
            Tab::Query => '4',
            Tab::Info => '5',
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Focus {
    Sidebar,
    Content,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Error,
}

pub struct Toast {
    pub kind: ToastKind,
    pub msg: String,
    pub at: Instant,
}

// ------------------------------------------------------------ connect form

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CfField {
    Url,
    Host,
    Port,
    User,
    Password,
    Database,
}

pub const CF_FIELDS: [CfField; 6] = [
    CfField::Url,
    CfField::Host,
    CfField::Port,
    CfField::User,
    CfField::Password,
    CfField::Database,
];

impl CfField {
    pub fn label(self) -> &'static str {
        match self {
            CfField::Url => "url",
            CfField::Host => "host",
            CfField::Port => "port",
            CfField::User => "user",
            CfField::Password => "password",
            CfField::Database => "database",
        }
    }
}

pub struct ConnectForm {
    pub url: String,
    pub host: String,
    pub port: String,
    pub user: String,
    pub password: String,
    pub database: String,
    pub focus_idx: usize,
    pub connecting: bool,
    pub error: Option<String>,
}

impl ConnectForm {
    fn new() -> Self {
        ConnectForm {
            url: String::new(),
            host: "localhost".into(),
            port: "5432".into(),
            user: std::env::var("USER").unwrap_or_else(|_| "postgres".into()),
            password: std::env::var("PGPASSWORD").unwrap_or_default(),
            database: String::new(),
            focus_idx: 3, // user
            connecting: false,
            error: None,
        }
    }

    pub fn value(&self, f: CfField) -> &str {
        match f {
            CfField::Url => &self.url,
            CfField::Host => &self.host,
            CfField::Port => &self.port,
            CfField::User => &self.user,
            CfField::Password => &self.password,
            CfField::Database => &self.database,
        }
    }

    fn value_mut(&mut self, f: CfField) -> &mut String {
        match f {
            CfField::Url => &mut self.url,
            CfField::Host => &mut self.host,
            CfField::Port => &mut self.port,
            CfField::User => &mut self.user,
            CfField::Password => &mut self.password,
            CfField::Database => &mut self.database,
        }
    }

    /// Consume an editing key; returns true when handled.
    fn key(&mut self, k: KeyEvent) -> bool {
        let field = CF_FIELDS[self.focus_idx];
        let v = self.value_mut(field);
        match k.code {
            KeyCode::Up => {
                self.focus_idx = (self.focus_idx + CF_FIELDS.len() - 1) % CF_FIELDS.len();
            }
            KeyCode::Down | KeyCode::Tab => {
                self.focus_idx = (self.focus_idx + 1) % CF_FIELDS.len();
            }
            KeyCode::Backspace => {
                v.pop();
            }
            KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => v.clear(),
            KeyCode::Char(c) => v.push(c),
            _ => return false,
        }
        true
    }

    /// Build the connection URL from the form fields.
    pub fn build_url(&self) -> Result<String, String> {
        let raw = self.url.trim();
        if !raw.is_empty() {
            return Ok(raw.to_string());
        }
        let host = self.host.trim();
        if host.is_empty() {
            return Err("host is empty".into());
        }
        let port: u16 = self
            .port
            .trim()
            .parse()
            .map_err(|_| format!("invalid port {:?}", self.port.trim()))?;
        let user = pct_encode(self.user.trim());
        let pass = pct_encode(&self.password);
        let db = self.database.trim();
        let auth = if user.is_empty() {
            String::new()
        } else if pass.is_empty() {
            format!("{user}@")
        } else {
            format!("{user}:{pass}@")
        };
        let path = if db.is_empty() {
            String::new()
        } else {
            format!("/{}", pct_encode(db))
        };
        Ok(format!("postgres://{auth}{host}:{port}{path}"))
    }
}

/// Percent-encode a URL component (RFC 3986 unreserved characters stay bare).
pub fn pct_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

// ------------------------------------------------------------- query editor

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum QMode {
    #[default]
    Editor,
    History,
    Result,
}

#[derive(Default)]
pub struct QueryState {
    pub lines: Vec<String>,
    pub cr: usize,
    pub cc: usize,
    pub ed_off: usize,
    pub mode: QMode,
    pub history: Vec<String>,
    pub hist_pick: usize,
    pub executing: bool,
    pub last: Option<QueryResult>,
    pub error: Option<String>,
    pub res_cell: (usize, usize),
    pub res_row_off: usize,
    pub res_col_off: usize,
}

/// Byte offset of char column `col` within `line`.
fn byte_col(line: &str, col: usize) -> usize {
    line.char_indices()
        .nth(col)
        .map(|(i, _)| i)
        .unwrap_or(line.len())
}

impl QueryState {
    fn new() -> Self {
        QueryState {
            lines: vec![String::new()],
            ..Default::default()
        }
    }

    pub fn content(&self) -> String {
        self.lines.join("\n")
    }

    pub fn set_content(&mut self, sql: &str) {
        self.lines = sql.lines().map(str::to_string).collect();
        if self.lines.is_empty() {
            self.lines.push(String::new());
        }
        self.cr = 0;
        self.cc = 0;
        self.ed_off = 0;
    }

    pub fn cur_len(&self) -> usize {
        self.lines[self.cr].chars().count()
    }

    fn clamp_cursor(&mut self) {
        self.cr = self.cr.min(self.lines.len() - 1);
        self.cc = self.cc.min(self.cur_len());
    }

    pub fn insert_char(&mut self, ch: char) {
        let line = &mut self.lines[self.cr];
        let i = byte_col(line, self.cc);
        line.insert(i, ch);
        self.cc += 1;
    }

    pub fn newline(&mut self) {
        let i = byte_col(&self.lines[self.cr], self.cc);
        let tail = self.lines[self.cr].split_off(i);
        self.cr += 1;
        self.cc = 0;
        self.lines.insert(self.cr, tail);
    }

    pub fn backspace(&mut self) {
        if self.cc > 0 {
            let i = byte_col(&self.lines[self.cr], self.cc - 1);
            self.lines[self.cr].remove(i);
            self.cc -= 1;
        } else if self.cr > 0 {
            let cur = self.lines.remove(self.cr);
            self.cr -= 1;
            self.cc = self.cur_len();
            self.lines[self.cr].push_str(&cur);
        }
    }

    pub fn delete(&mut self) {
        let len = self.cur_len();
        if self.cc < len {
            let i = byte_col(&self.lines[self.cr], self.cc);
            self.lines[self.cr].remove(i);
        } else if self.cr + 1 < self.lines.len() {
            let next = self.lines.remove(self.cr + 1);
            self.lines[self.cr].push_str(&next);
        }
    }

    pub fn left(&mut self) {
        if self.cc > 0 {
            self.cc -= 1;
        } else if self.cr > 0 {
            self.cr -= 1;
            self.cc = self.cur_len();
        }
    }

    pub fn right(&mut self) {
        if self.cc < self.cur_len() {
            self.cc += 1;
        } else if self.cr + 1 < self.lines.len() {
            self.cr += 1;
            self.cc = 0;
        }
    }

    pub fn up(&mut self) {
        if self.cr > 0 {
            self.cr -= 1;
            self.clamp_cursor();
        }
    }

    pub fn down(&mut self) {
        if self.cr + 1 < self.lines.len() {
            self.cr += 1;
            self.clamp_cursor();
        }
    }

    pub fn home(&mut self) {
        self.cc = 0;
    }

    pub fn end(&mut self) {
        self.cc = self.cur_len();
    }

    pub fn kill_to_end(&mut self) {
        let i = byte_col(&self.lines[self.cr], self.cc);
        self.lines[self.cr].truncate(i);
    }

    pub fn clear_line_head(&mut self) {
        let line = std::mem::take(&mut self.lines[self.cr]);
        self.lines[self.cr] = line.chars().skip(self.cc).collect();
        self.cc = 0;
    }

    pub fn indent(&mut self) {
        self.insert_char(' ');
        self.insert_char(' ');
    }
}

// -------------------------------------------------------- browser UI state

/// Rects captured during draw for mouse hit-testing.
#[derive(Default)]
pub struct Rects {
    pub tab_spans: Vec<(u16, u16, Tab)>,
    pub sidebar_filter: Option<Rect>,
    pub sidebar_list: Rect,
    pub rows_grid: Option<Rect>,
    pub col_ranges: Vec<(u16, u16)>,
    pub col_window: usize,
    pub row_window: usize,
    pub editor: Option<Rect>,
    pub result: Option<Rect>,
    pub pane: Rect,
}

pub struct Browser {
    pub meta: ConnMeta,
    pub focus: Focus,
    pub tab: Tab,

    // sidebar
    pub tables: Vec<TableInfo>,
    pub tables_loading: bool,
    pub tables_error: Option<String>,
    pub filter: String,
    pub filtering: bool,
    pub sel: usize,
    pub list_off: usize,

    // selected relation
    pub cur: Option<TableInfo>,
    pub detail_loading: bool,
    pub detail: Option<TableDetail>,
    pub st_off: usize,
    pub ix_off: usize,

    // rows tab
    pub rows_filter: String,
    pub rows_filtering: bool,
    pub rows: Option<RowsResult>,
    pub rows_loading: bool,
    pub rows_error: Option<String>,
    pub page: u32,
    pub page_size: u32,
    pub cell: (usize, usize),
    pub row_off: usize,
    pub col_off: usize,
    pub order: Option<(String, bool)>,

    // query tab
    pub q: QueryState,

    // info tab
    pub stats: Option<ServerStats>,
    pub info_loading: bool,

    pub rects: Rects,
}

pub fn filter_tables<'a>(tables: &'a [TableInfo], filter: &str) -> Vec<&'a TableInfo> {
    let f = filter.trim().to_lowercase();
    tables
        .iter()
        .filter(|t| f.is_empty() || t.label().to_lowercase().contains(&f) || t.kind.contains(&f))
        .collect()
}

impl Browser {
    fn new(meta: ConnMeta) -> Self {
        Browser {
            meta,
            focus: Focus::Sidebar,
            tab: Tab::Rows,
            tables: Vec::new(),
            tables_loading: true,
            tables_error: None,
            filter: String::new(),
            filtering: false,
            sel: 0,
            list_off: 0,
            cur: None,
            detail_loading: false,
            detail: None,
            st_off: 0,
            ix_off: 0,
            rows_filter: String::new(),
            rows_filtering: false,
            rows: None,
            rows_loading: false,
            rows_error: None,
            page: 1,
            page_size: PAGE_SIZE,
            cell: (0, 0),
            row_off: 0,
            col_off: 0,
            order: None,
            q: QueryState::new(),
            stats: None,
            info_loading: false,
            rects: Rects::default(),
        }
    }

    pub fn filtered(&self) -> Vec<&TableInfo> {
        filter_tables(&self.tables, &self.filter)
    }

    pub fn editing(&self) -> bool {
        self.filtering
            || self.rows_filtering
            || (self.tab == Tab::Query && self.q.mode != QMode::Result)
    }

    pub fn total_pages(&self) -> u32 {
        let total = self.rows.as_ref().map(|r| r.total).unwrap_or(0);
        (((total + self.page_size as i64 - 1) / self.page_size as i64).max(1)) as u32
    }

    pub fn sort_label(&self, col: &str) -> Option<&'static str> {
        match &self.order {
            Some((c, false)) if c == col => Some("▲"),
            Some((c, true)) if c == col => Some("▼"),
            _ => None,
        }
    }

    pub fn toggle_sort(&mut self, col: &str) {
        self.order = match &self.order {
            Some((c, false)) if c == col => Some((col.to_string(), true)),
            Some((c, true)) if c == col => None,
            _ => Some((col.to_string(), false)),
        };
    }
}

// -------------------------------------------------------------------- App

pub struct StartupOpts {
    pub url: Option<String>,
    pub host: Option<String>,
    pub port: Option<String>,
    pub user: Option<String>,
    pub db: Option<String>,
}

pub struct App {
    pub screen: Screen,
    pub form: ConnectForm,
    /// Connect-form field rects, populated each frame by the renderer.
    pub form_rects: Vec<(Rect, CfField)>,
    pub br: Option<Browser>,
    pub quit: bool,
    pub help: bool,
    pub toast: Option<Toast>,
    pub tick: u64,
    tx: Sender<DbRequest>,
}

fn hit(r: Rect, x: u16, y: u16) -> bool {
    x >= r.x && x < r.x + r.width && y >= r.y && y < r.y + r.height
}

enum Region {
    Sidebar,
    RowsGrid,
    Pane(Tab),
    Outside,
}

impl App {
    pub fn new(tx: Sender<DbRequest>, opts: StartupOpts) -> Self {
        let mut form = ConnectForm::new();
        if let Some(h) = opts.host {
            form.host = h;
        }
        if let Some(p) = opts.port {
            form.port = p;
        }
        if let Some(u) = opts.user {
            form.user = u;
        }
        if let Some(d) = opts.db {
            form.database = d;
        }
        if let Some(u) = opts.url {
            form.url = u;
        }
        App {
            screen: Screen::Form,
            form,
            form_rects: Vec::new(),
            br: None,
            quit: false,
            help: false,
            toast: None,
            tick: 0,
            tx,
        }
    }

    fn send(&self, req: DbRequest) {
        let _ = self.tx.send(req);
    }

    pub fn toast(&mut self, kind: ToastKind, msg: impl Into<String>) {
        self.toast = Some(Toast {
            kind,
            msg: msg.into(),
            at: Instant::now(),
        });
    }

    pub(crate) fn toast_expired(&mut self) {
        if let Some(t) = &self.toast {
            if t.at.elapsed() > TOAST_TTL {
                self.toast = None;
            }
        }
    }

    /// Kick off a connection attempt from the form.
    pub fn begin_connect(&mut self) {
        match self.form.build_url() {
            Ok(url) => {
                self.form.connecting = true;
                self.form.error = None;
                self.send(DbRequest::Connect(url));
            }
            Err(e) => self.form.error = Some(e),
        }
    }

    // ------------------------------------------------------------ requests

    fn request_tables(&mut self) {
        if let Some(br) = self.br.as_mut() {
            br.tables_loading = true;
            br.tables_error = None;
        }
        self.send(DbRequest::Tables);
    }

    fn request_describe(&mut self) {
        let (schema, table) = {
            let Some(br) = self.br.as_ref() else { return };
            let Some(cur) = br.cur.as_ref() else { return };
            (cur.schema.clone(), cur.name.clone())
        };
        if let Some(br) = self.br.as_mut() {
            br.detail_loading = true;
        }
        self.send(DbRequest::Describe { schema, table });
    }

    fn request_rows(&mut self) {
        let req = {
            let Some(br) = self.br.as_ref() else { return };
            let Some(cur) = br.cur.as_ref() else { return };
            DbRequest::Rows {
                schema: cur.schema.clone(),
                table: cur.name.clone(),
                page: br.page,
                page_size: br.page_size,
                order: br.order.clone(),
                filter: Some(br.rows_filter.trim().to_string()).filter(|f| !f.is_empty()),
            }
        };
        if let Some(br) = self.br.as_mut() {
            br.rows_loading = true;
            br.rows_error = None;
        }
        self.send(req);
    }

    fn request_info(&mut self) {
        if let Some(br) = self.br.as_mut() {
            br.info_loading = true;
        }
        self.send(DbRequest::ServerInfo);
    }

    fn switch_tab(&mut self, tab: Tab) {
        let need_info = {
            let Some(br) = self.br.as_mut() else { return };
            br.tab = tab;
            br.focus = Focus::Content;
            tab == Tab::Info && br.stats.is_none()
        };
        if need_info {
            self.request_info();
        }
    }

    pub fn select_table(&mut self, t: TableInfo) {
        {
            let Some(br) = self.br.as_mut() else { return };
            br.cur = Some(t.clone());
            br.detail = None;
            br.detail_loading = true;
            br.page = 1;
            br.cell = (0, 0);
            br.row_off = 0;
            br.col_off = 0;
            br.order = None;
            br.rows = None;
            br.rows_error = None;
            br.rows_loading = true;
            br.st_off = 0;
            br.ix_off = 0;
        }
        self.send(DbRequest::Describe {
            schema: t.schema.clone(),
            table: t.name.clone(),
        });
        self.request_rows();
    }

    fn set_page(&mut self, p: u32) {
        let max = self.br.as_ref().map(Browser::total_pages).unwrap_or(1);
        {
            let Some(br) = self.br.as_mut() else { return };
            br.page = p.clamp(1, max);
            br.cell = (0, 0);
            br.row_off = 0;
        }
        self.request_rows();
    }

    fn refresh_current(&mut self) {
        let (tab, focus) = match self.br.as_ref() {
            Some(br) => (br.tab, br.focus),
            None => return,
        };
        if focus == Focus::Sidebar {
            self.request_tables();
            return;
        }
        match tab {
            Tab::Rows => self.request_rows(),
            Tab::Structure | Tab::Indexes => self.request_describe(),
            Tab::Info => self.request_info(),
            Tab::Query => {}
        }
    }

    fn run_query(&mut self) {
        let sql = match self.br.as_ref() {
            Some(br) => br.q.content(),
            None => return,
        };
        if sql.trim().is_empty() {
            self.toast(ToastKind::Info, "query is empty");
            return;
        }
        if let Some(br) = self.br.as_mut() {
            br.q.executing = true;
            br.q.error = None;
            br.q.history.push(sql.clone());
        }
        self.send(DbRequest::Execute(sql));
    }

    // ----------------------------------------------------------- responses

    pub fn on_response(&mut self, resp: DbResponse) {
        match resp {
            DbResponse::Connect(Ok(meta)) => {
                let db_name = meta.database.clone();
                let user = meta.user.clone();
                self.screen = Screen::Browser;
                self.form.connecting = false;
                self.br = Some(Browser::new(meta));
                self.toast(ToastKind::Info, format!("connected to {db_name} as {user}"));
                self.request_tables();
            }
            DbResponse::Connect(Err(e)) => {
                self.form.connecting = false;
                self.form.error = Some(e);
            }
            DbResponse::Tables(Ok(tables)) => {
                let auto_select = {
                    let Some(br) = self.br.as_mut() else { return };
                    br.tables_loading = false;
                    br.tables_error = None;
                    br.tables = tables;
                    br.sel = br.sel.min(br.tables.len().saturating_sub(1));
                    if br.cur.is_none() && !br.tables.is_empty() {
                        let f = br.filtered();
                        Some(f[br.sel.min(f.len() - 1)].clone())
                    } else {
                        None
                    }
                };
                if let Some(t) = auto_select {
                    self.select_table(t);
                }
            }
            DbResponse::Tables(Err(e)) => {
                if let Some(br) = self.br.as_mut() {
                    br.tables_loading = false;
                    br.tables_error = Some(e);
                }
            }
            DbResponse::Describe {
                schema,
                table,
                result,
            } => {
                let failure = {
                    let Some(br) = self.br.as_mut() else { return };
                    let matches = br
                        .cur
                        .as_ref()
                        .is_some_and(|c| c.schema == schema && c.name == table);
                    if !matches {
                        return;
                    }
                    br.detail_loading = false;
                    match result {
                        Ok(d) => {
                            br.detail = Some(d);
                            None
                        }
                        Err(e) => Some(e),
                    }
                };
                if let Some(e) = failure {
                    self.toast(ToastKind::Error, format!("describe failed: {e}"));
                }
            }
            DbResponse::Rows {
                schema,
                table,
                page,
                result,
            } => {
                {
                    let Some(br) = self.br.as_mut() else { return };
                    let matches = br
                        .cur
                        .as_ref()
                        .is_some_and(|c| c.schema == schema && c.name == table)
                        && page == br.page;
                    if !matches {
                        return;
                    }
                    br.rows_loading = false;
                    match result {
                        Ok(r) => {
                            br.cell.0 = br.cell.0.min(r.grid.rows.len().saturating_sub(1));
                            br.cell.1 = br.cell.1.min(r.grid.columns.len().saturating_sub(1));
                            br.rows = Some(r);
                        }
                        Err(e) => {
                            br.rows = None;
                            br.rows_error = Some(e); // surfaced in-pane
                        }
                    }
                }
            }
            DbResponse::Execute(result) => {
                let Some(br) = self.br.as_mut() else { return };
                br.q.executing = false;
                match result {
                    Ok(qr) => {
                        br.q.res_cell = (0, 0);
                        br.q.res_row_off = 0;
                        br.q.res_col_off = 0;
                        br.q.last = Some(qr);
                        br.q.error = None;
                        br.q.mode = QMode::Result;
                    }
                    Err(e) => br.q.error = Some(e),
                }
            }
            DbResponse::ServerInfo(result) => {
                let failure = {
                    let Some(br) = self.br.as_mut() else { return };
                    br.info_loading = false;
                    match result {
                        Ok(s) => {
                            br.stats = Some(s);
                            None
                        }
                        Err(e) => Some(e),
                    }
                };
                if let Some(e) = failure {
                    self.toast(ToastKind::Error, format!("info failed: {e}"));
                }
            }
        }
    }

    // -------------------------------------------------------------- exports

    pub fn export_grid_csv(grid: &Grid, path: &Path) -> anyhow::Result<usize> {
        let mut w = csv::Writer::from_path(path)?;
        w.write_record(&grid.columns)?;
        for row in &grid.rows {
            let rec: Vec<&str> = row.iter().map(|c| c.as_deref().unwrap_or("")).collect();
            w.write_record(&rec)?;
        }
        w.flush()?;
        Ok(grid.rows.len())
    }

    fn slug(s: &str) -> String {
        s.chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect()
    }

    fn do_export(&mut self, grid: &Grid, kind: &str, name: &str) {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let fname = format!("pgtui_{}_{}_{}.csv", Self::slug(kind), Self::slug(name), ts);
        match Self::export_grid_csv(grid, Path::new(&fname)) {
            Ok(n) => {
                let full = std::env::current_dir().unwrap_or_default().join(&fname);
                self.toast(
                    ToastKind::Info,
                    format!("wrote {} rows to {}", n, full.display()),
                );
            }
            Err(e) => self.toast(ToastKind::Error, format!("export failed: {e}")),
        }
    }

    fn export_loaded_rows(&mut self) {
        let payload = {
            let Some(br) = self.br.as_ref() else { return };
            match (&br.rows, &br.cur) {
                (Some(r), Some(c)) => Some((r.grid.clone(), c.name.clone())),
                _ => None,
            }
        };
        if let Some((grid, name)) = payload {
            self.do_export(&grid, "rows", &name);
        }
    }

    fn export_query_result(&mut self) {
        let grid = {
            let Some(br) = self.br.as_ref() else { return };
            br.q.last.as_ref().and_then(|qr| qr.grid.clone())
        };
        match grid {
            Some(g) => self.do_export(&g, "query", "result"),
            None => self.toast(ToastKind::Info, "last statement returned no result set"),
        }
    }

    // ------------------------------------------------------------- keyboard

    pub fn on_key(&mut self, k: KeyEvent) {
        if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
            self.quit = true;
            return;
        }
        if self.help {
            if matches!(
                k.code,
                KeyCode::Esc | KeyCode::Char('?') | KeyCode::Char('q') | KeyCode::Enter
            ) {
                self.help = false;
            }
            return;
        }
        if self.screen == Screen::Browser
            && !self.br.as_ref().map(Browser::editing).unwrap_or(false)
        {
            if let KeyCode::Char(c @ '1'..='5') = k.code {
                let t = Tab::ALL[(c as u8 - b'1') as usize];
                self.switch_tab(t);
                return;
            }
            if k.code == KeyCode::Char('?') {
                self.help = true;
                return;
            }
            if k.code == KeyCode::Char('q') {
                self.quit = true;
                return;
            }
            if k.code == KeyCode::Esc {
                // drop any stale (non-editing) sidebar filter text
                if let Some(br) = self.br.as_mut() {
                    br.filter.clear();
                }
            }
        }
        match self.screen {
            Screen::Form => self.on_form_key(k),
            Screen::Browser => self.on_browser_key(k),
        }
    }

    fn on_form_key(&mut self, k: KeyEvent) {
        if self.form.connecting {
            return;
        }
        match k.code {
            KeyCode::Enter if !k.modifiers.contains(KeyModifiers::ALT) => self.begin_connect(),
            KeyCode::Esc => self.form.error = None,
            _ => {
                self.form.key(k);
            }
        }
    }

    fn on_browser_key(&mut self, k: KeyEvent) {
        let (filtering, rows_filtering, tab, focus) = match self.br.as_ref() {
            Some(br) => (br.filtering, br.rows_filtering, br.tab, br.focus),
            None => return,
        };

        // 1. sidebar filter box
        if filtering {
            let Some(br) = self.br.as_mut() else { return };
            match k.code {
                KeyCode::Enter | KeyCode::Esc => {
                    if k.code == KeyCode::Esc {
                        br.filter.clear();
                    }
                    br.filtering = false;
                    let n = br.filtered().len();
                    br.sel = br.sel.min(n.saturating_sub(1));
                }
                KeyCode::Backspace => {
                    br.filter.pop();
                    br.sel = 0;
                }
                KeyCode::Char(c) => {
                    br.filter.push(c);
                    br.sel = 0;
                }
                _ => {}
            }
            return;
        }

        // 2. rows WHERE filter input
        if rows_filtering {
            let apply = {
                let Some(br) = self.br.as_mut() else { return };
                match k.code {
                    KeyCode::Enter => {
                        br.rows_filtering = false;
                        br.page = 1;
                        br.cell = (0, 0);
                        br.row_off = 0;
                        true
                    }
                    KeyCode::Esc => {
                        br.rows_filtering = false;
                        br.rows_filter.clear();
                        false
                    }
                    KeyCode::Backspace => {
                        br.rows_filter.pop();
                        false
                    }
                    KeyCode::Char(c) => {
                        br.rows_filter.push(c);
                        false
                    }
                    _ => false,
                }
            };
            if apply {
                self.request_rows();
            }
            return;
        }

        // 3. query tab modes
        if tab == Tab::Query {
            self.on_query_key(k);
            return;
        }

        // 4. global tab hotkeys (work regardless of pane focus)
        if let KeyCode::Char(c @ '1'..='5') = k.code {
            let t = Tab::ALL[(c as u8 - b'1') as usize];
            self.switch_tab(t);
            return;
        }

        // 5. navigation
        match focus {
            Focus::Sidebar => match k.code {
                KeyCode::Tab => {
                    if let Some(br) = self.br.as_mut() {
                        br.focus = Focus::Content;
                    }
                }
                KeyCode::Char('j') | KeyCode::Down => self.move_sel(1),
                KeyCode::Char('k') | KeyCode::Up => self.move_sel(-1),
                KeyCode::PageDown => self.move_sel(10),
                KeyCode::PageUp => self.move_sel(-10),
                KeyCode::Char('G') | KeyCode::End => self.jump_sel_last(),
                KeyCode::Char('g') | KeyCode::Home => self.jump_sel_first(),
                KeyCode::Enter => self.open_selected(),
                KeyCode::Char('r') => self.refresh_current(),
                KeyCode::Char('/') => {
                    if let Some(br) = self.br.as_mut() {
                        br.filtering = true;
                    }
                }
                _ => {}
            },
            Focus::Content => match k.code {
                KeyCode::Char('r') => self.refresh_current(),
                KeyCode::Tab => {
                    if let Some(br) = self.br.as_mut() {
                        br.focus = Focus::Sidebar;
                    }
                }
                KeyCode::Char('/') if tab == Tab::Rows => {
                    if let Some(br) = self.br.as_mut() {
                        br.rows_filtering = true;
                    }
                }
                KeyCode::Char('e') if tab == Tab::Rows => self.export_loaded_rows(),
                KeyCode::Char('s') if tab == Tab::Rows => self.sort_by_cursor_col(),
                KeyCode::Char('n') | KeyCode::PageDown if tab == Tab::Rows => {
                    let p = self.br.as_ref().map(|b| b.page).unwrap_or(1);
                    self.set_page(p + 1);
                }
                KeyCode::Char('p') | KeyCode::PageUp if tab == Tab::Rows => {
                    let p = self.br.as_ref().map(|b| b.page).unwrap_or(1);
                    self.set_page(p.saturating_sub(1).max(1));
                }
                KeyCode::Char('G') | KeyCode::End if tab == Tab::Rows => {
                    let max = self.br.as_ref().map(Browser::total_pages).unwrap_or(1);
                    self.set_page(max);
                }
                KeyCode::Char('g') | KeyCode::Home if tab == Tab::Rows => self.set_page(1),
                KeyCode::Left | KeyCode::Char('h') if tab == Tab::Rows => self.move_cell(0, -1),
                KeyCode::Right | KeyCode::Char('l') if tab == Tab::Rows => self.move_cell(0, 1),
                KeyCode::Down | KeyCode::Char('j') if tab == Tab::Rows => self.move_cell(1, 0),
                KeyCode::Up | KeyCode::Char('k') if tab == Tab::Rows => self.move_cell(-1, 0),
                KeyCode::Down | KeyCode::Char('j') if tab == Tab::Structure => {
                    if let Some(br) = self.br.as_mut() {
                        br.st_off += 1;
                    }
                }
                KeyCode::Up | KeyCode::Char('k') if tab == Tab::Structure => {
                    if let Some(br) = self.br.as_mut() {
                        br.st_off = br.st_off.saturating_sub(1);
                    }
                }
                KeyCode::Down | KeyCode::Char('j') if tab == Tab::Indexes => {
                    if let Some(br) = self.br.as_mut() {
                        br.ix_off += 1;
                    }
                }
                KeyCode::Up | KeyCode::Char('k') if tab == Tab::Indexes => {
                    if let Some(br) = self.br.as_mut() {
                        br.ix_off = br.ix_off.saturating_sub(1);
                    }
                }
                _ => {}
            },
        }
    }

    fn on_query_key(&mut self, k: KeyEvent) {
        let mode = match self.br.as_ref() {
            Some(br) => br.q.mode,
            None => return,
        };
        let alt_enter = k.code == KeyCode::Enter && k.modifiers.contains(KeyModifiers::ALT);
        match mode {
            QMode::Editor => {
                if alt_enter || k.code == KeyCode::F(5) {
                    self.run_query();
                    return;
                }
                if k.code == KeyCode::Char('h') && k.modifiers.contains(KeyModifiers::CONTROL) {
                    if let Some(br) = self.br.as_mut() {
                        if !br.q.history.is_empty() {
                            br.q.mode = QMode::History;
                            br.q.hist_pick = 0;
                        }
                    }
                    return;
                }
                let Some(br) = self.br.as_mut() else { return };
                match k.code {
                    KeyCode::Enter => br.q.newline(),
                    KeyCode::Tab => br.q.indent(),
                    KeyCode::Backspace => br.q.backspace(),
                    KeyCode::Delete => br.q.delete(),
                    KeyCode::Left => br.q.left(),
                    KeyCode::Right => br.q.right(),
                    KeyCode::Up => br.q.up(),
                    KeyCode::Down => br.q.down(),
                    KeyCode::Home => br.q.home(),
                    KeyCode::End => br.q.end(),
                    KeyCode::Char('k') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                        br.q.kill_to_end()
                    }
                    KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                        br.q.clear_line_head()
                    }
                    KeyCode::Char('a') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                        br.q.home()
                    }
                    KeyCode::Char('e') if k.modifiers.contains(KeyModifiers::CONTROL) => br.q.end(),
                    KeyCode::Char(c) => br.q.insert_char(c),
                    _ => {}
                }
            }
            QMode::History => {
                let adopt = {
                    let Some(br) = self.br.as_mut() else { return };
                    let n = br.q.history.len();
                    if n == 0 {
                        br.q.mode = QMode::Editor;
                        return;
                    }
                    match k.code {
                        KeyCode::Up | KeyCode::Char('k') => {
                            br.q.hist_pick = (br.q.hist_pick + n - 1) % n;
                            false
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            br.q.hist_pick = (br.q.hist_pick + 1) % n;
                            false
                        }
                        KeyCode::Enter => true,
                        KeyCode::Esc => {
                            br.q.mode = QMode::Editor;
                            false
                        }
                        KeyCode::Char('h') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                            br.q.mode = QMode::Editor;
                            false
                        }
                        _ => false,
                    }
                };
                if adopt {
                    let entry = {
                        let br = self.br.as_ref().expect("browser");
                        br.q.history[br.q.hist_pick.min(br.q.history.len() - 1)].clone()
                    };
                    let br = self.br.as_mut().expect("browser");
                    br.q.set_content(&entry);
                    br.q.mode = QMode::Editor;
                }
            }
            QMode::Result => {
                if k.code == KeyCode::Char('e') {
                    self.export_query_result();
                    return;
                }
                let Some(br) = self.br.as_mut() else { return };
                match k.code {
                    KeyCode::Esc => br.q.mode = QMode::Editor,
                    KeyCode::Char('h') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                        if !br.q.history.is_empty() {
                            br.q.mode = QMode::History;
                            br.q.hist_pick = 0;
                        }
                    }
                    KeyCode::Left | KeyCode::Char('h') => {
                        br.q.res_cell.1 = br.q.res_cell.1.saturating_sub(1)
                    }
                    KeyCode::Right | KeyCode::Char('l') => br.q.res_cell.1 += 1,
                    KeyCode::Up | KeyCode::Char('k') => {
                        br.q.res_cell.0 = br.q.res_cell.0.saturating_sub(1)
                    }
                    KeyCode::Down | KeyCode::Char('j') => br.q.res_cell.0 += 1,
                    KeyCode::Char('g') | KeyCode::Home => br.q.res_cell.0 = 0,
                    _ => {}
                }
            }
        }
    }

    // ------------------------------------------------------- nav helpers

    fn move_sel(&mut self, delta: isize) {
        let Some(br) = self.br.as_mut() else { return };
        let n = br.filtered().len();
        if n == 0 {
            return;
        }
        let cur = br.sel as isize + delta;
        br.sel = cur.clamp(0, n as isize - 1) as usize;
    }

    fn jump_sel_first(&mut self) {
        if let Some(br) = self.br.as_mut() {
            br.sel = 0;
        }
    }

    fn jump_sel_last(&mut self) {
        if let Some(br) = self.br.as_mut() {
            br.sel = br.filtered().len().saturating_sub(1);
        }
    }

    fn open_selected(&mut self) {
        let t = {
            let Some(br) = self.br.as_ref() else { return };
            let filtered = br.filtered();
            if filtered.is_empty() {
                return;
            }
            let idx = br.sel.min(filtered.len() - 1);
            filtered[idx].clone()
        };
        self.select_table(t);
    }

    fn sort_by_cursor_col(&mut self) {
        let col = {
            let Some(br) = self.br.as_ref() else { return };
            let Some(rows) = br.rows.as_ref() else { return };
            match rows.grid.columns.get(br.cell.1) {
                Some(c) => c.clone(),
                None => return,
            }
        };
        if let Some(br) = self.br.as_mut() {
            br.toggle_sort(&col);
            br.page = 1;
            br.cell = (0, 0);
            br.row_off = 0;
        }
        self.request_rows();
    }

    fn move_cell(&mut self, dr: isize, dc: isize) {
        let Some(br) = self.br.as_mut() else { return };
        let Some(rows) = br.rows.as_ref() else { return };
        let nrows = rows.grid.rows.len();
        let ncols = rows.grid.columns.len();
        if nrows == 0 || ncols == 0 {
            return;
        }
        let r = (br.cell.0 as isize + dr).clamp(0, nrows as isize - 1) as usize;
        let c = (br.cell.1 as isize + dc).clamp(0, ncols as isize - 1) as usize;
        br.cell = (r, c);

        // keep the cell inside both scroll windows (sizes come from the
        // previous frame's draw — a one-frame lag is fine)
        let vw = br.rects.col_ranges.len().max(1);
        while br.cell.1 < br.col_off {
            br.col_off -= 1;
        }
        while br.cell.1 >= br.col_off + vw {
            br.col_off += 1;
        }
        br.col_off = br.col_off.min(ncols.saturating_sub(vw));

        let vh = br.rects.row_window.max(1);
        while br.cell.0 < br.row_off {
            br.row_off -= 1;
        }
        while br.cell.0 >= br.row_off + vh {
            br.row_off += 1;
        }
        br.row_off = br.row_off.min(nrows.saturating_sub(vh));
    }

    fn scroll_result(&mut self, delta: isize) {
        let Some(br) = self.br.as_mut() else { return };
        let Some(last) = br.q.last.as_ref() else {
            return;
        };
        let Some(g) = last.grid.as_ref() else { return };
        let max = g.rows.len().saturating_sub(1);
        let r = (br.q.res_cell.0 as isize + delta).clamp(0, max as isize) as usize;
        br.q.res_cell.0 = r;
    }

    // --------------------------------------------------------------- mouse

    pub fn on_mouse(&mut self, m: MouseEvent) {
        if self.help {
            return;
        }
        if self.screen == Screen::Form {
            if m.kind == MouseEventKind::Down(MouseButton::Left) {
                let target = self
                    .form_rects
                    .iter()
                    .find(|(r, _)| hit(*r, m.column, m.row))
                    .map(|(_, f)| *f);
                if let Some(f) = target {
                    if let Some(i) = CF_FIELDS.iter().position(|x| *x == f) {
                        self.form.focus_idx = i;
                    }
                }
            }
            return;
        }
        if self.br.is_none() {
            return;
        }
        match m.kind {
            MouseEventKind::Down(MouseButton::Left) => self.on_click(m.column, m.row),
            MouseEventKind::ScrollUp => self.on_wheel(-1, m.column, m.row),
            MouseEventKind::ScrollDown => self.on_wheel(1, m.column, m.row),
            _ => {}
        }
    }

    fn region_at(&self, x: u16, y: u16) -> Region {
        let Some(br) = self.br.as_ref() else {
            return Region::Outside;
        };
        if hit(br.rects.sidebar_list, x, y) || br.rects.sidebar_filter.is_some_and(|r| hit(r, x, y))
        {
            return Region::Sidebar;
        }
        if br.rects.rows_grid.is_some_and(|r| hit(r, x, y)) {
            return Region::RowsGrid;
        }
        if br.rects.pane.width > 0 && hit(br.rects.pane, x, y) {
            return Region::Pane(br.tab);
        }
        Region::Outside
    }

    fn on_click(&mut self, x: u16, y: u16) {
        // tab bar segments
        let clicked_tab = self.br.as_ref().and_then(|br| {
            br.rects
                .tab_spans
                .iter()
                .find(|(x0, x1, _)| x >= *x0 && x < *x1)
                .map(|(_, _, t)| *t)
        });
        if let Some(t) = clicked_tab {
            self.switch_tab(t);
            return;
        }

        match self.region_at(x, y) {
            Region::Sidebar => {
                if self
                    .br
                    .as_ref()
                    .and_then(|b| b.rects.sidebar_filter)
                    .is_some_and(|r| hit(r, x, y))
                {
                    if let Some(br) = self.br.as_mut() {
                        br.filtering = true;
                    }
                    return;
                }
                let target = {
                    let Some(br) = self.br.as_ref() else { return };
                    let row_idx = y.saturating_sub(br.rects.sidebar_list.y) as usize;
                    br.filtered()
                        .get(br.list_off + row_idx)
                        .map(|t| (*t).clone())
                };
                if let Some(t) = target {
                    if let Some(br) = self.br.as_mut() {
                        let row_idx = y.saturating_sub(br.rects.sidebar_list.y) as usize;
                        br.sel = br.list_off + row_idx;
                    }
                    self.select_table(t);
                }
            }
            Region::RowsGrid => self.click_rows_grid(x, y),
            Region::Pane(Tab::Query) => self.click_query_pane(x, y),
            Region::Pane(_) => {
                if let Some(br) = self.br.as_mut() {
                    br.focus = Focus::Content;
                }
            }
            Region::Outside => {}
        }
    }

    fn click_rows_grid(&mut self, x: u16, y: u16) {
        let (grid_area, col_idx) = {
            let br = self.br.as_ref().expect("browser");
            let Some(area) = br.rects.rows_grid else {
                return;
            };
            let pos = br
                .rects
                .col_ranges
                .iter()
                .position(|(cx0, cx1)| x >= *cx0 && x < *cx1)
                .map(|i| br.rects.col_window + i);
            (area, pos)
        };
        let Some(col_idx) = col_idx else { return };
        if y <= grid_area.y {
            // header row: toggle sort
            let col = {
                let br = self.br.as_ref().expect("browser");
                br.rows
                    .as_ref()
                    .and_then(|rows| rows.grid.columns.get(col_idx).cloned())
            };
            if let Some(col) = col {
                if let Some(br) = self.br.as_mut() {
                    br.toggle_sort(&col);
                    br.page = 1;
                    br.cell = (0, 0);
                    br.row_off = 0;
                }
                self.request_rows();
            }
            return;
        }
        // body row: place the cell cursor
        let row_idx = y.saturating_sub(grid_area.y).saturating_sub(1) as usize;
        if let Some(br) = self.br.as_mut() {
            let nrows = br.rows.as_ref().map(|r| r.grid.rows.len()).unwrap_or(0);
            let ncols = br.rows.as_ref().map(|r| r.grid.columns.len()).unwrap_or(0);
            if row_idx < nrows && col_idx < ncols {
                br.cell = (br.row_off + row_idx, col_idx);
            }
        }
    }

    fn click_query_pane(&mut self, x: u16, y: u16) {
        let hit_ed = self
            .br
            .as_ref()
            .and_then(|b| b.rects.editor)
            .is_some_and(|r| hit(r, x, y));
        if hit_ed {
            let br = self.br.as_mut().expect("browser");
            let r = match br.rects.editor {
                Some(r) => r,
                None => return,
            };
            br.q.mode = QMode::Editor;
            // rects.editor is the inner area; lines start at r.y
            let row = br.q.ed_off + y.saturating_sub(r.y) as usize;
            if row < br.q.lines.len() {
                br.q.cr = row;
                br.q.cc = (x.saturating_sub(r.x) as usize).min(br.q.lines[row].chars().count());
            }
            return;
        }
        let hit_res = self
            .br
            .as_ref()
            .and_then(|b| b.rects.result)
            .is_some_and(|r| hit(r, x, y));
        if !hit_res {
            return;
        }
        let br = self.br.as_mut().expect("browser");
        let Some(r) = br.rects.result else { return };
        let Some(last) = br.q.last.as_ref() else {
            return;
        };
        let Some(g) = last.grid.as_ref() else { return };
        // rects.result is the inner area; line r.y is the header row
        let row = br.q.res_row_off + y.saturating_sub(r.y).saturating_sub(1) as usize;
        br.q.res_cell = (
            row.min(g.rows.len().saturating_sub(1)),
            (br.q.res_col_off + x.saturating_sub(r.x) as usize)
                .min(g.columns.len().saturating_sub(1)),
        );
    }
    fn on_wheel(&mut self, dir: isize, x: u16, y: u16) {
        match self.region_at(x, y) {
            Region::Sidebar => {
                self.move_sel(dir.signum());
            }
            Region::RowsGrid => {
                self.move_cell(dir.signum(), 0);
            }
            Region::Pane(Tab::Structure) => {
                if let Some(br) = self.br.as_mut() {
                    br.st_off = (br.st_off as isize + 2 * dir).max(0) as usize;
                }
            }
            Region::Pane(Tab::Indexes) => {
                if let Some(br) = self.br.as_mut() {
                    br.ix_off = (br.ix_off as isize + 2 * dir).max(0) as usize;
                }
            }
            Region::Pane(Tab::Query) => {
                let mode = self.br.as_ref().map(|b| b.q.mode).unwrap_or(QMode::Editor);
                if mode == QMode::Result {
                    self.scroll_result(dir.signum() * 2);
                } else if let Some(br) = self.br.as_mut() {
                    br.q.ed_off = (br.q.ed_off as isize + dir).max(0) as usize;
                }
            }
            _ => {}
        }
    }
}

// ------------------------------------------------------------------- tests

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_encoding() {
        assert_eq!(pct_encode("alice"), "alice");
        assert_eq!(pct_encode("p@ss w/rd"), "p%40ss%20w%2Frd");
        assert_eq!(pct_encode("~._-"), "~._-");
    }

    #[test]
    fn builds_url_from_fields() {
        let mut f = ConnectForm::new();
        f.host = "db.local".into();
        f.port = "5433".into();
        f.user = "alice".into();
        f.password = "s3cret".into();
        f.database = "shop".into();
        assert_eq!(
            f.build_url().unwrap(),
            "postgres://alice:s3cret@db.local:5433/shop"
        );
    }

    #[test]
    fn url_field_wins_and_validates_port() {
        let mut f = ConnectForm::new();
        f.port = "not-a-port".into();
        assert!(f.build_url().is_err());
        f.url = "postgres://x/y".into();
        assert_eq!(f.build_url().unwrap(), "postgres://x/y");
    }

    #[test]
    fn editor_line_joins_and_splits() {
        let mut q = QueryState::new();
        q.set_content("abc");
        q.end();
        q.newline();
        q.insert_char('d');
        q.insert_char('e');
        assert_eq!(q.content(), "abc\nde");
        q.left();
        q.left();
        q.backspace(); // join: cursor at start of second line
        assert_eq!(q.content(), "abcde");
        assert_eq!(q.cr, 0);
        assert_eq!(q.cc, 3);
    }

    #[test]
    fn editor_kill_and_clear() {
        let mut q = QueryState::new();
        q.set_content("hello world");
        q.cc = 5;
        q.kill_to_end();
        assert_eq!(q.content(), "hello");
        q.clear_line_head();
        assert_eq!(q.content(), "");
        assert_eq!(q.cc, 0);
    }

    #[test]
    fn filters_tables_case_insensitive() {
        let t = |s: &str, n: &str, k: &str| TableInfo {
            schema: s.into(),
            name: n.into(),
            kind: k.into(),
            est_rows: 0,
        };
        let tables = vec![t("public", "users", "table"), t("app", "Events", "view")];
        assert_eq!(filter_tables(&tables, "").len(), 2);
        assert_eq!(filter_tables(&tables, "USER").len(), 1);
        assert_eq!(filter_tables(&tables, "view").len(), 1);
        assert_eq!(filter_tables(&tables, "zzz").len(), 0);
    }

    #[test]
    fn sort_cycle() {
        let mut b = Browser::new(ConnMeta {
            full_version: String::new(),
            short_version: String::new(),
            database: String::new(),
            user: String::new(),
            host: String::new(),
            port: String::new(),
        });
        b.toggle_sort("id");
        assert_eq!(b.sort_label("id"), Some("▲"));
        b.toggle_sort("id");
        assert_eq!(b.sort_label("id"), Some("▼"));
        b.toggle_sort("id");
        assert_eq!(b.sort_label("id"), None);
    }

    #[test]
    fn tab_toggles_focus_both_ways() {
        let (tx, _rx) = std::sync::mpsc::channel();
        let mut a = App::new(
            tx,
            StartupOpts {
                url: None,
                host: None,
                port: None,
                user: None,
                db: None,
            },
        );
        let meta = ConnMeta {
            full_version: String::new(),
            short_version: String::new(),
            database: "d".into(),
            user: "u".into(),
            host: "h".into(),
            port: "5432".into(),
        };
        a.br = Some(Browser::new(meta));
        let tab = KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE);

        // initial focus is Sidebar; Tab moves to Content (this was a bug)
        a.on_browser_key(tab);
        assert_eq!(a.br.as_ref().unwrap().focus, Focus::Content);

        // and back
        a.on_browser_key(tab);
        assert_eq!(a.br.as_ref().unwrap().focus, Focus::Sidebar);
    }
}
