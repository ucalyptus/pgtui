//! Rendering: connect form, browser panes, overlays, status bar.

use crate::app::{App, Browser, CfField, Focus, QMode, Screen, Tab, ToastKind, CF_FIELDS};
use ratatui::layout::{Alignment, Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Cell, Clear, Paragraph, Row, Table, Wrap};
use ratatui::Frame;
use std::cmp::min;

const ACCENT: Color = Color::Cyan;
const DIM: Color = Color::DarkGray;
const BORDER: Color = Color::DarkGray;
const ERR: Color = Color::Red;
const OK: Color = Color::Green;
const WARN: Color = Color::Yellow;
const SEL_BG: Color = Color::Rgb(38, 50, 76);
const NULL_STR: &str = "<NULL>";

const SPIN: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn spin(app: &App) -> &'static str {
    SPIN[(app.tick as usize / 4) % SPIN.len()]
}

fn kind_color(kind: &str) -> Color {
    match kind {
        "table" | "partitioned table" => OK,
        "view" => Color::Magenta,
        "materialized view" => WARN,
        _ => Color::Gray,
    }
}

fn bordered(title: &str) -> Block<'_> {
    Block::bordered()
        .border_style(Style::new().fg(BORDER))
        .title(Span::styled(
            format!(" {title} "),
            Style::new().fg(ACCENT).bold(),
        ))
}
/// Keep `sel` visible inside the `[off, off+view)` window.
fn ensure_visible(off: &mut usize, len: usize, view: usize, sel: usize) {
    if view == 0 || len == 0 {
        return;
    }
    if sel < *off {
        *off = sel;
    } else if sel >= *off + view {
        *off = sel + 1 - view;
    }
    *off = (*off).min(len.saturating_sub(view));
}

pub fn draw(f: &mut Frame, app: &mut App) {
    app.tick = app.tick.wrapping_add(1);
    app.toast_expired();
    match app.screen {
        Screen::Form => draw_form(f, app),
        Screen::Browser => draw_browser(f, app),
    }
    if app.help && app.screen == Screen::Browser {
        draw_help(f);
    }
}

// ------------------------------------------------------------------- form

fn draw_form(f: &mut Frame, app: &mut App) {
    let area = centered(f.area(), 66, 15);
    f.render_widget(Clear, area);
    let title = "pgtui · connect to postgres";
    let block = bordered(title).border_style(Style::new().fg(ACCENT));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let rows = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .split(inner);

    let label_w: usize = 11;
    app.form_rects.clear();
    for (i, field) in CF_FIELDS.iter().enumerate() {
        let line_y = rows[i].y;
        let focused = app.form.focus_idx == i && !app.form.connecting;
        let raw = app.form.value(*field);
        let shown = if matches!(field, CfField::Password) {
            "*".repeat(raw.chars().count())
        } else {
            raw.to_string()
        };
        let mut spans = vec![
            Span::styled(
                format!("{:>width$}", field.label(), width = label_w - 2),
                Style::new().fg(if focused { ACCENT } else { DIM }).bold(),
            ),
            Span::raw("  "),
            Span::styled(shown.clone(), Style::new().fg(Color::White)),
        ];
        if focused {
            spans.push(Span::styled("▏", Style::new().fg(ACCENT)));
        }
        let val_x = inner.x + label_w as u16 + 2;
        f.render_widget(
            Paragraph::new(Line::from(spans)),
            Rect::new(inner.x, line_y, inner.width, 1),
        );
        app.form_rects
            .push((Rect::new(inner.x, line_y, inner.width, 1), *field));
        if focused {
            let cx = (val_x + shown.chars().count() as u16).min(area.right().saturating_sub(2));
            f.set_cursor_position(Position::new(cx, line_y));
        }
    }

    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            " ↑/↓ select · type edits · ⏎ connect · esc clears error ",
            Style::new().fg(DIM),
        ))),
        rows[6],
    );

    let foot = rows[7];
    if let Some(e) = &app.form.error {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(" ✗ {e}"),
                Style::new().fg(ERR),
            )))
            .wrap(Wrap { trim: false }),
            foot,
        );
    } else if app.form.connecting {
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!("{} connecting…", spin(app)),
                Style::new().fg(WARN),
            ))),
            foot,
        );
    }
}

fn draw_browser(f: &mut Frame, app: &mut App) {
    let v = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).split(f.area());
    let [main_area, status_area] = v[..] else {
        return;
    };
    let h = Layout::horizontal([Constraint::Length(30), Constraint::Min(0)]).split(main_area);
    let [sidebar_a, content] = h[..] else { return };

    let sp = spin(app);
    let Some(br) = app.br.as_mut() else { return };

    draw_sidebar(f, br, sp, sidebar_a);

    // tab bar
    let tabs_rect = Rect::new(content.x, content.y, content.width, 1);
    let mut spans: Vec<Span> = vec![Span::raw(" ")];
    br.rects.tab_spans.clear();
    let mut x = tabs_rect.x + 1;
    for t in Tab::ALL {
        let seg = format!(" {} {} ", t.hotkey(), t.title());
        let w = seg.chars().count() as u16;
        let selected = br.tab == t;
        spans.push(Span::styled(
            seg,
            if selected {
                Style::new().fg(Color::Black).bg(ACCENT).bold()
            } else {
                Style::new().fg(DIM)
            },
        ));
        br.rects.tab_spans.push((x, x + w, t));
        x += w;
        if t != Tab::Info {
            spans.push(Span::styled("│", Style::new().fg(BORDER)));
            x += 1;
        }
    }
    f.render_widget(Paragraph::new(Line::from(spans)), tabs_rect);

    let pane = Rect::new(
        content.x,
        content.y + 1,
        content.width,
        content.height.saturating_sub(1),
    );
    br.rects.pane = pane;

    match br.tab {
        Tab::Rows => draw_rows(f, br, sp, pane),
        Tab::Structure => draw_structure(f, br, sp, pane),
        Tab::Indexes => draw_indexes(f, br, sp, pane),
        Tab::Query => draw_query(f, br, sp, pane),
        Tab::Info => draw_info(f, br, sp, pane),
    }

    draw_status(f, app, status_area);
}

fn draw_sidebar(f: &mut Frame, br: &mut Browser, sp: &str, area: Rect) {
    let title = if br.tables_loading {
        format!(" {sp} loading relations… ")
    } else {
        format!(" relations ({}) ", br.tables.len())
    };
    let block = bordered(&title);
    let inner = block.inner(area);
    f.render_widget(block, area);
    if let Some(e) = &br.tables_error {
        f.render_widget(
            Paragraph::new(Span::styled(format!(" ✗ {e}"), Style::new().fg(ERR)))
                .wrap(Wrap { trim: false }),
            inner,
        );
        br.rects.sidebar_filter = None;
        br.rects.sidebar_list = Rect::new(inner.x, inner.y, inner.width, 0);
        return;
    }

    let mut rest = inner;
    if br.filtering || !br.filter.is_empty() {
        let fb = Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).split(inner);
        let [fbox, list] = fb[..] else { return };
        let fblock = Block::bordered()
            .border_style(Style::new().fg(if br.filtering { ACCENT } else { BORDER }))
            .title(Span::styled(" filter ", Style::new().fg(DIM)));
        let finner = fblock.inner(fbox);
        f.render_widget(fblock, fbox);
        let mut spans = vec![Span::styled(
            br.filter.clone(),
            Style::new().fg(Color::White),
        )];
        if br.filtering {
            spans.push(Span::styled("▏", Style::new().fg(ACCENT)));
            let cx = finner.x + br.filter.chars().count() as u16;
            f.set_cursor_position(Position::new(cx.min(finner.right()), finner.y));
        }
        f.render_widget(
            Paragraph::new(Line::from(spans)),
            Rect::new(finner.x, finner.y, finner.width, 1),
        );
        br.rects.sidebar_filter = Some(Rect::new(finner.x, finner.y, finner.width, 1));
        rest = list;
    } else {
        br.rects.sidebar_filter = None;
    }

    let flen = br.filtered().len();
    let view = rest.height as usize;
    ensure_visible(&mut br.list_off, flen, view, br.sel);

    let filtered = br.filtered();
    if filtered.is_empty() {
        let msg = if br.tables.is_empty() {
            if br.tables_loading {
                ""
            } else {
                "no relations found"
            }
        } else {
            "no matches"
        };
        if !msg.is_empty() {
            f.render_widget(
                Paragraph::new(Span::styled(msg, Style::new().fg(DIM).italic()))
                    .alignment(Alignment::Center),
                rest,
            );
        }
        br.rects.sidebar_list = Rect::new(rest.x, rest.y, rest.width, 0);
        return;
    }

    let end = min(br.list_off + view.max(1), filtered.len());
    let cur_name = br.cur.as_ref().map(|c| c.label());
    for (i, t) in filtered[br.list_off..end].iter().enumerate() {
        let selected_idx = br.list_off + i == br.sel;
        let open = cur_name.as_deref() == Some(t.label().as_str());
        let mut style = Style::new();
        if selected_idx {
            style = style.bg(SEL_BG).add_modifier(Modifier::BOLD);
        }
        let marker = if open { "●" } else { " " };
        let mut spans = vec![
            Span::styled(
                format!("{marker} "),
                Style::new().fg(if open { ACCENT } else { DIM }),
            ),
            Span::styled(t.name.clone(), style.fg(Color::White)),
            Span::styled(
                format!(" {}", t.kind),
                Style::new().fg(kind_color(&t.kind)).dim(),
            ),
        ];
        if t.est_rows > 0 {
            spans.push(Span::styled(
                format!(" {}", human_count(t.est_rows)),
                Style::new().fg(DIM),
            ));
        }
        let line = Line::from(spans);
        f.render_widget(
            Paragraph::new(line),
            Rect::new(rest.x, rest.y + i as u16, rest.width, 1),
        );
    }
    br.rects.sidebar_list = rest;
}

fn human_count(n: i64) -> String {
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => format!("{:.1}k", n as f64 / 1_000.0),
        _ => format!("{:.1}m", n as f64 / 1_000_000.0),
    }
}

fn cell_text(v: &Option<String>) -> (&str, Style) {
    match v {
        None => (NULL_STR, Style::new().fg(DIM).italic()),
        Some(s) => (s.as_str(), Style::new()),
    }
}

/// Column widths sampled from a grid, clamped.
fn col_widths(columns: &[String], rows: &[Vec<Option<String>>], sample: usize) -> Vec<u16> {
    columns
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let mut w = c.chars().count();
            for r in rows.iter().take(sample) {
                if let Some(Some(v)) = r.get(i) {
                    w = w.max(v.chars().count().max(NULL_STR.len()));
                }
            }
            (w as u16).clamp(7, 44)
        })
        .collect()
}

fn rows_filter_bar(f: &mut Frame, br: &mut Browser, area: Rect) -> Option<Rect> {
    if !br.rows_filtering && br.rows_filter.is_empty() {
        return None;
    }
    let parts = Layout::vertical([Constraint::Length(3), Constraint::Min(0)]).split(area);
    let [fb, g] = parts[..] else { return None };
    let fblock = Block::bordered()
        .border_style(Style::new().fg(if br.rows_filtering { WARN } else { BORDER }))
        .title(Span::styled(
            " where (raw sql · ⏎ applies) ",
            Style::new().fg(DIM),
        ));
    let fi = fblock.inner(fb);
    f.render_widget(fblock, fb);
    let mut spans = vec![Span::styled(
        br.rows_filter.clone(),
        Style::new().fg(Color::White),
    )];
    if br.rows_filtering {
        spans.push(Span::styled("▏", Style::new().fg(WARN)));
        let cx = fi.x + br.rows_filter.chars().count() as u16;
        f.set_cursor_position(Position::new(cx.min(fi.right()), fi.y));
    }
    f.render_widget(
        Paragraph::new(Line::from(spans)),
        Rect::new(fi.x, fi.y, fi.width, 1),
    );
    Some(g)
}

fn draw_rows(f: &mut Frame, br: &mut Browser, sp: &str, area: Rect) {
    br.rects.rows_grid = None;
    br.rects.col_ranges.clear();
    let grid_area = rows_filter_bar(f, br, area).unwrap_or(area);

    if br.rows_loading {
        f.render_widget(
            Paragraph::new(Span::styled(
                format!(
                    "{sp} querying {}…",
                    br.cur.as_ref().map(|c| c.label()).unwrap_or_default()
                ),
                Style::new().fg(WARN),
            ))
            .alignment(Alignment::Center),
            grid_area,
        );
        return;
    }
    if let Some(e) = &br.rows_error {
        f.render_widget(
            Paragraph::new(Span::styled(format!(" ✗ {e} "), Style::new().fg(ERR)))
                .alignment(Alignment::Center),
            grid_area,
        );
        return;
    }

    let qualified = br
        .cur
        .as_ref()
        .map(|c| c.qualified())
        .unwrap_or_else(|| "(none)".into());
    let (total, elapsed_ms) = match br.rows.as_ref() {
        Some(r) => (r.total, r.elapsed.as_millis()),
        None => (0, 0),
    };
    let sort_note = match &br.order {
        Some((c, false)) => format!(" · sorted {c} ▲"),
        Some((c, true)) => format!(" · sorted {c} ▼"),
        None => String::new(),
    };
    let pages = br.total_pages();
    let title = format!(
        " {qualified} · page {}/{} · {total} rows · {elapsed_ms} ms{sort_note} ",
        br.page, pages
    );

    let Some(rowsres) = br.rows.as_ref() else {
        f.render_widget(bordered(&title), grid_area);
        return;
    };
    let block = bordered(&title);
    let inner = block.inner(grid_area);
    f.render_widget(block, grid_area);
    if rowsres.grid.rows.is_empty() || rowsres.grid.columns.is_empty() {
        f.render_widget(
            Paragraph::new(Span::styled("no rows", Style::new().fg(DIM).italic()))
                .alignment(Alignment::Center),
            inner,
        );
        return;
    }

    let widths_all = col_widths(&rowsres.grid.columns, &rowsres.grid.rows, 200);
    let avail = inner.width.saturating_sub(1) as usize;
    let ncols = widths_all.len();

    let vw_max = {
        let mut acc = 0usize;
        let mut n = 0usize;
        for &w in &widths_all[br.col_off..ncols] {
            acc += w as usize + 1;
            if acc > avail {
                break;
            }
            n += 1;
        }
        n.max(1)
    };
    br.col_off = br.col_off.min(ncols.saturating_sub(vw_max));
    while br.cell.1 < br.col_off {
        br.col_off -= 1;
    }
    while br.cell.1 >= br.col_off + vw_max {
        br.col_off += 1;
    }
    br.col_off = br.col_off.min(ncols.saturating_sub(vw_max));

    let vis_end = min(br.col_off + vw_max, ncols);
    let widths: Vec<u16> = widths_all[br.col_off..vis_end].to_vec();

    let vh = inner.height.saturating_sub(1) as usize;
    let nrows = rowsres.grid.rows.len();
    ensure_visible(&mut br.row_off, nrows, vh.max(1), br.cell.0);

    br.rects.rows_grid = Some(inner);
    br.rects.row_window = vh;
    br.rects.col_window = br.col_off;
    let mut cx = inner.x;
    for &w in &widths {
        br.rects.col_ranges.push((cx, cx + w));
        cx += w + 1;
    }

    let header = Row::new(
        rowsres.grid.columns[br.col_off..vis_end]
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let mut s = Style::new().fg(ACCENT).bold();
                if br.col_off + i == br.cell.1 {
                    s = s.add_modifier(Modifier::UNDERLINED);
                }
                let arrow = br.sort_label(c).unwrap_or("");
                Cell::from(format!("{c}{arrow}")).style(s)
            })
            .collect::<Vec<_>>(),
    );

    let body_end = min(br.row_off + vh.max(1), nrows);
    let trs: Vec<Row> = rowsres.grid.rows[br.row_off..body_end]
        .iter()
        .enumerate()
        .map(|(ri, row)| {
            let gi = br.row_off + ri;
            let sel_row = gi == br.cell.0;
            Row::new(
                row[br.col_off..vis_end]
                    .iter()
                    .map(|v| {
                        let (t, s) = cell_text(v);
                        Cell::from(t.to_string()).style(if sel_row { s.bg(SEL_BG) } else { s })
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .collect();

    f.render_widget(
        Table::new(trs, widths).header(header).column_spacing(1),
        inner,
    );
}

fn draw_structure(f: &mut Frame, br: &mut Browser, sp: &str, area: Rect) {
    if br.detail_loading {
        f.render_widget(
            Paragraph::new(Span::styled(
                format!(
                    "{} describing {}…",
                    sp,
                    br.cur.as_ref().map(|c| c.label()).unwrap_or_default()
                ),
                Style::new().fg(WARN),
            ))
            .alignment(Alignment::Center),
            area,
        );
        return;
    }
    let Some(d) = br.detail.as_ref() else {
        f.render_widget(
            Paragraph::new(Span::styled(
                "select a relation in the sidebar",
                Style::new().fg(DIM).italic(),
            ))
            .alignment(Alignment::Center),
            area,
        );
        return;
    };
    let parts =
        Layout::vertical([Constraint::Percentage(55), Constraint::Percentage(45)]).split(area);
    let [cols_a, cons_a] = parts[..] else { return };

    let cols_title = format!(
        " columns ({}) · {} ms ",
        d.columns.len(),
        d.elapsed.as_millis()
    );
    let cb = bordered(&cols_title);
    let ci = cb.inner(cols_a);
    f.render_widget(cb, cols_a);
    if !d.columns.is_empty() && ci.height > 0 {
        let start = br.st_off.min(d.columns.len() - 1);
        let view = ci.height as usize;
        let rows: Vec<Row> = d.columns[start..]
            .iter()
            .take(view)
            .map(|c| {
                Row::new(vec![
                    Cell::from(c.name.clone()).style(Style::new().fg(Color::White)),
                    Cell::from(c.data_type.clone()).style(Style::new().fg(ACCENT)),
                    Cell::from(if c.nullable { "yes" } else { "no" }).style(Style::new().fg(DIM)),
                    Cell::from(c.default.clone()),
                    Cell::from(if c.pk { "pk" } else { "" }).style(Style::new().fg(WARN).bold()),
                    Cell::from(c.comment.clone()).style(Style::new().fg(DIM)),
                ])
            })
            .collect();
        let widths = [
            Constraint::Length(24),
            Constraint::Length(18),
            Constraint::Length(5),
            Constraint::Length(16),
            Constraint::Length(4),
            Constraint::Min(10),
        ];
        f.render_widget(Table::new(rows, widths).column_spacing(1), ci);
    }

    let cons_title = format!(" constraints ({}) ", d.constraints.len());
    let kb = bordered(&cons_title);
    let ki = kb.inner(cons_a);
    f.render_widget(kb, cons_a);
    if !d.constraints.is_empty() && ki.height > 0 {
        let start = br.st_off.min(d.constraints.len() - 1);
        let view = ki.height as usize;
        let rows: Vec<Row> = d.constraints[start..]
            .iter()
            .take(view)
            .map(|c| {
                Row::new(vec![
                    Cell::from(c.name.clone()).style(Style::new().fg(Color::White)),
                    Cell::from(c.kind.clone()).style(Style::new().fg(if c.kind == "PRIMARY KEY" {
                        WARN
                    } else {
                        ACCENT
                    })),
                    Cell::from(c.definition.clone()).style(Style::new().fg(DIM)),
                ])
            })
            .collect();
        let widths = [
            Constraint::Length(24),
            Constraint::Length(14),
            Constraint::Min(10),
        ];
        f.render_widget(Table::new(rows, widths).column_spacing(1), ki);
    }
}

fn draw_indexes(f: &mut Frame, br: &mut Browser, sp: &str, area: Rect) {
    if br.detail_loading {
        f.render_widget(
            Paragraph::new(Span::styled(
                format!("{} loading…", sp),
                Style::new().fg(WARN),
            ))
            .alignment(Alignment::Center),
            area,
        );
        return;
    }
    let Some(d) = br.detail.as_ref() else { return };
    let title = format!(" indexes ({}) ", d.indexes.len());
    let b = bordered(&title);
    let inner = b.inner(area);
    f.render_widget(b, area);
    if d.indexes.is_empty() {
        f.render_widget(
            Paragraph::new(Span::styled("none", Style::new().fg(DIM).italic()))
                .alignment(Alignment::Center),
            inner,
        );
        return;
    }
    let start = br.ix_off.min(d.indexes.len() - 1);
    let view = inner.height as usize;
    if view == 0 {
        return;
    }
    let rows: Vec<Row> = d.indexes[start..]
        .iter()
        .take(view)
        .map(|ix| {
            Row::new(vec![
                Cell::from(ix.name.clone()).style(Style::new().fg(Color::White)),
                Cell::from(if ix.is_unique { "uniq" } else { "" }).style(Style::new().fg(OK)),
                Cell::from(if ix.is_primary { "pk" } else { "" })
                    .style(Style::new().fg(WARN).bold()),
                Cell::from(ix.definition.clone()).style(Style::new().fg(DIM)),
            ])
        })
        .collect();
    let widths = [
        Constraint::Length(26),
        Constraint::Length(5),
        Constraint::Length(4),
        Constraint::Min(10),
    ];
    f.render_widget(Table::new(rows, widths).column_spacing(1), inner);
}

fn draw_query(f: &mut Frame, br: &mut Browser, sp: &str, area: Rect) {
    br.rects.editor = None;
    br.rects.result = None;

    let editor_h = (br.q.lines.len() as u16 + 1)
        .max(4)
        .min(area.height.saturating_sub(5).max(4));
    let parts = Layout::vertical([Constraint::Length(editor_h), Constraint::Min(3)]).split(area);
    let [ed_a, res_a] = parts[..] else { return };

    // ---- editor
    let active = br.q.mode == QMode::Editor;
    let eb = bordered(" sql — alt+⏎ / F5 run · ctrl+h history ")
        .border_style(Style::new().fg(if active { ACCENT } else { BORDER }));
    let ei = eb.inner(ed_a);
    f.render_widget(eb, ed_a);
    let evh = ei.height as usize;
    while br.q.cr < br.q.ed_off {
        br.q.ed_off -= 1;
    }
    while evh > 0 && br.q.cr >= br.q.ed_off + evh {
        br.q.ed_off += 1;
    }
    br.q.ed_off = br.q.ed_off.min(br.q.lines.len().saturating_sub(evh.max(1)));
    let lines: Vec<Line> = br.q.lines[br.q.ed_off.min(br.q.lines.len())..]
        .iter()
        .map(|l| Line::from(l.clone()))
        .collect();
    f.render_widget(Paragraph::new(lines), ei);
    br.rects.editor = Some(ei);
    if active && evh > 0 {
        let cy = ei.y + (br.q.cr - br.q.ed_off) as u16;
        let cx = ei.x + br.q.cc as u16;
        if cy < ei.bottom() {
            f.set_cursor_position(Position::new(cx.min(ei.right().saturating_sub(1)), cy));
        }
    }

    // ---- result
    let rb_title = if br.q.executing {
        format!("{sp} running… ")
    } else if let Some(err) = &br.q.error {
        format!(" error — {err} ")
    } else {
        match br.q.last.as_ref() {
            Some(qr) => match (&qr.grid, qr.affected) {
                (Some(g), _) => format!(
                    " results · {} rows · {} ms ",
                    g.rows.len(),
                    qr.elapsed.as_millis()
                ),
                (None, Some(n)) => {
                    format!(" ok · {n} rows affected · {} ms ", qr.elapsed.as_millis())
                }
                (None, None) => format!(" ok · {} ms ", qr.elapsed.as_millis()),
            },
            None => " results — alt+⏎ runs ".into(),
        }
    };
    let rb = bordered(&rb_title).border_style(Style::new().fg(if br.q.error.is_some() {
        ERR
    } else if br.q.mode == QMode::Result {
        ACCENT
    } else {
        BORDER
    }));
    let ri = rb.inner(res_a);
    f.render_widget(rb, res_a);
    br.rects.result = Some(ri);

    if let Some(e) = &br.q.error {
        f.render_widget(
            Paragraph::new(Span::styled(e.clone(), Style::new().fg(ERR)))
                .wrap(Wrap { trim: false }),
            ri,
        );
    } else if !br.q.executing {
        let grid = br.q.last.as_ref().and_then(|qr| qr.grid.clone());
        match grid.as_ref() {
            Some(g) if !g.columns.is_empty() && !g.rows.is_empty() => {
                render_grid_readonly(f, br, g, ri);
            }
            Some(_) => {
                f.render_widget(
                    Paragraph::new(Span::styled("no rows", Style::new().fg(DIM).italic()))
                        .alignment(Alignment::Center),
                    ri,
                );
            }
            None if br.q.last.is_some() => {
                f.render_widget(
                    Paragraph::new(Span::styled("statement finished", Style::new().fg(OK)))
                        .alignment(Alignment::Center),
                    ri,
                );
            }
            None => {}
        }
    }

    // ---- history popup
    if br.q.mode == QMode::History {
        let w = res_a.width * 45 / 100;
        let h = res_a.height.min(br.q.history.len() as u16 + 2).max(5);
        let pop = Rect::new(res_a.right().saturating_sub(w), res_a.y, w, h);
        f.render_widget(Clear, pop);
        let pb = bordered(" history ↑/↓ · ⏎ load ");
        let pi = pb.inner(pop);
        f.render_widget(pb, pop);
        let view = pi.height as usize;
        if view > 0 {
            let start = br.q.hist_pick.min(br.q.history.len().saturating_sub(1));
            for (i, entry) in br.q.history.iter().rev().skip(start).take(view).enumerate() {
                let picked = start + i == br.q.hist_pick;
                let mut text: String = entry
                    .lines()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(pi.width.saturating_sub(2) as usize)
                    .collect();
                if entry.lines().count() > 1 {
                    text.push_str(" …");
                }
                let style = if picked {
                    Style::new().bg(SEL_BG).fg(Color::White).bold()
                } else {
                    Style::new().fg(DIM)
                };
                f.render_widget(
                    Paragraph::new(Line::from(Span::styled(text, style))),
                    Rect::new(pi.x, pi.y + i as u16, pi.width, 1),
                );
            }
        }
    }
}

fn render_grid_readonly(f: &mut Frame, br: &mut Browser, g: &crate::db::Grid, ri: Rect) {
    let widths_all = col_widths(&g.columns, &g.rows, 200);
    let avail = ri.width.saturating_sub(1) as usize;
    let vw_max = {
        let mut acc = 0usize;
        let mut n = 0usize;
        for &w in &widths_all[br.q.res_col_off..] {
            acc += w as usize + 1;
            if acc > avail {
                break;
            }
            n += 1;
        }
        n.max(1)
    };
    let ncols = g.columns.len();
    br.q.res_col_off = br.q.res_col_off.min(ncols.saturating_sub(vw_max));
    while br.q.res_cell.1 < br.q.res_col_off {
        br.q.res_col_off -= 1;
    }
    while br.q.res_cell.1 >= br.q.res_col_off + vw_max {
        br.q.res_col_off += 1;
    }
    br.q.res_col_off = br.q.res_col_off.min(ncols.saturating_sub(vw_max));
    let vend = min(br.q.res_col_off + vw_max, ncols);
    let widths: Vec<u16> = widths_all[br.q.res_col_off..vend].to_vec();

    let vh = ri.height.saturating_sub(1) as usize;
    ensure_visible(
        &mut br.q.res_row_off,
        g.rows.len(),
        vh.max(1),
        br.q.res_cell.0,
    );

    let header = Row::new(
        g.columns[br.q.res_col_off..vend]
            .iter()
            .map(|c| Cell::from(c.clone()).style(Style::new().fg(ACCENT).bold()))
            .collect::<Vec<_>>(),
    );
    let bend = min(br.q.res_row_off + vh.max(1), g.rows.len());
    let trs: Vec<Row> = g.rows[br.q.res_row_off..bend]
        .iter()
        .enumerate()
        .map(|(i, row)| {
            let gi = br.q.res_row_off + i;
            let selr = gi == br.q.res_cell.0;
            Row::new(
                row[br.q.res_col_off..vend]
                    .iter()
                    .map(|v| {
                        let (t, s) = cell_text(v);
                        Cell::from(t.to_string()).style(if selr { s.bg(SEL_BG) } else { s })
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .collect();
    f.render_widget(Table::new(trs, widths).header(header).column_spacing(1), ri);
}

fn draw_info(f: &mut Frame, br: &mut Browser, sp: &str, area: Rect) {
    let b = bordered(" server ");
    let inner = b.inner(area);
    f.render_widget(b, area);
    if br.info_loading {
        f.render_widget(
            Paragraph::new(Span::styled(
                format!("{} loading…", sp),
                Style::new().fg(WARN),
            )),
            inner,
        );
        return;
    }
    let Some(s) = br.stats.as_ref() else {
        f.render_widget(
            Paragraph::new(Span::styled(
                "press r to fetch server info",
                Style::new().fg(DIM).italic(),
            )),
            inner,
        );
        return;
    };
    let kv = [
        (
            "connected to",
            format!("{}:{}/{}", br.meta.host, br.meta.port, br.meta.database),
        ),
        ("user", br.meta.user.clone()),
        ("version", s.short_version.clone()),
        ("full", br.meta.full_version.clone()),
        ("database", s.database.clone()),
        ("size", s.size_pretty.clone()),
        (
            "connections",
            format!("{} total / {} active", s.connections, s.active),
        ),
        ("started", s.started.clone()),
    ];
    let lines: Vec<Line> = kv
        .iter()
        .map(|(k, v)| {
            Line::from(vec![
                Span::styled(format!("{k:<14}"), Style::new().fg(ACCENT).bold()),
                Span::styled(v.clone(), Style::new().fg(Color::White)),
            ])
        })
        .collect();
    f.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), inner);
}

fn draw_status(f: &mut Frame, app: &App, area: Rect) {
    let Some(br) = app.br.as_ref() else { return };
    let conn = format!(
        "{}@{}:{}/{} ({})",
        br.meta.user, br.meta.host, br.meta.port, br.meta.database, br.meta.short_version
    );
    let hints = if br.editing() {
        "typing — ⏎ apply · esc cancel".to_string()
    } else {
        match (br.focus, br.tab) {
            (Focus::Sidebar, _) => {
                "j/k move · ⏎ open · / filter · r refresh · tab panes".to_string()
            }
            (_, Tab::Query) => {
                "alt+⏎/F5 run · ctrl+h history · esc back · e export csv".to_string()
            }
            (_, Tab::Rows) => {
                let total = br.rows.as_ref().map(|r| r.total).unwrap_or(0);
                format!("arrows move · s sort · n/p page · / where · e export · {total} rows")
            }
            (_, Tab::Info) => "r refresh".to_string(),
            _ => "? help · tab panes".to_string(),
        }
    };
    let mut spans = vec![
        Span::styled(conn, Style::new().fg(ACCENT)),
        Span::styled("  │  ", Style::new().fg(BORDER)),
        Span::styled(hints, Style::new().fg(DIM)),
    ];
    if let Some(t) = &app.toast {
        let color = match t.kind {
            ToastKind::Info => OK,
            ToastKind::Error => ERR,
        };
        spans.push(Span::styled(
            format!("  ◆ {} ", t.msg),
            Style::new().fg(color),
        ));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_help(f: &mut Frame) {
    let area = centered(f.area(), 80, 32);
    f.render_widget(Clear, area);
    let text = "\
 global
   tab          focus sidebar ⇄ content      1..5   switch tab
   r            refresh current view         ?      this help
   q / ctrl+c   quit
 sidebar
   j/k · arrows move       enter open relation     / filter (live)
 rows
   arrows/hjkl move cell   s sort column           n/p · pgup/pgdn page
   g/G first/last page     / raw WHERE filter      e export page → csv
 query editor
   alt+enter / F5 run      ctrl+h history          tab indent
   ctrl+k kill to eol      ctrl+u clear head       esc results→editor
 results
   arrows/hjkl scroll      e export → csv          click cells
 mouse
   wheel scrolls lists/grids/results
   click: relation opens it · tab switches · header sorts · cell moves cursor
 notes
   kitty/ghostty keyboard protocol enabled when supported
   identifiers quoted; sort columns whitelisted against catalog";
    let title = " keys ";
    f.render_widget(
        Paragraph::new(text)
            .style(Style::new().fg(Color::White))
            .block(bordered(title))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn centered(area: Rect, w: u16, h: u16) -> Rect {
    let w = w.min(area.width);
    let h = h.min(area.height);
    Rect::new(
        area.x + (area.width - w) / 2,
        area.y + (area.height - h) / 2,
        w,
        h,
    )
}
