//! ratatui UI: entry-point tree (left), per-symbol diff (right).

use crate::analysis::{self, Analysis, Status, TreeRow};
use crate::diffview::{DiffLine, Sign, fn_diff};
use crate::git::BINARY_SOURCE_PREFIX;
use crate::syntax::SymbolKind;
use ratatui::{
    crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    layout::{Constraint, Direction},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};
use std::collections::HashSet;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Tree,
    Detail,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffAnchor {
    pub old_line: Option<usize>,
    pub new_line: Option<usize>,
}

pub struct App {
    pub a: Analysis,
    pub roots: Vec<String>,
    pub expanded: HashSet<String>,
    pub focus_mode: bool,
    pub rows: Vec<TreeRow>,
    pub sel: usize,
    pub tree_state: ListState,
    pub focus: Focus,
    pub detail_scroll: usize,
    pub detail_cursor: Option<usize>,
    pub detail_selection: Option<usize>,
    pub detail_viewport: usize,
    pub tree_horizontal: u16,
    pub detail_horizontal: u16,
    pub quit: bool,
    pub flash: Option<String>,
    /// rendered detail pane (header + diff) for the current selection
    pub detail: Vec<Line<'static>>,
    pub detail_anchors: Vec<Option<DiffAnchor>>,
}

impl App {
    pub fn new(a: Analysis, roots: Vec<String>, focus_mode: bool) -> Self {
        let mut app = Self {
            a,
            roots,
            expanded: HashSet::new(),
            focus_mode,
            rows: vec![],
            sel: 0,
            tree_state: ListState::default(),
            focus: Focus::Tree,
            detail_scroll: 0,
            detail_cursor: None,
            detail_selection: None,
            detail_viewport: 0,
            tree_horizontal: 0,
            detail_horizontal: 0,
            quit: false,
            flash: None,
            detail: vec![],
            detail_anchors: vec![],
        };
        let roots = app.roots.clone();
        for r in &roots {
            if app.a.nodes.get(r).map(|n| n.relevant).unwrap_or(false) {
                app.expanded.insert(r.clone());
            }
        }
        app.rebuild();
        if let Some(row) = app.rows.get(app.sel) {
            use unicode_width::UnicodeWidthStr;
            let name = &app.a.nodes[&row.key].name;
            let qualified = name.split('(').next().unwrap_or(name);
            if let Some(separator) = qualified.rfind("::") {
                let columns = row.depth * 2 + 4 + name[..separator + 2].width();
                app.tree_horizontal = columns.min(usize::from(u16::MAX)) as u16;
            }
        }
        app.update_detail();
        app
    }

    pub fn rebuild(&mut self) {
        let roots = self.roots.clone();
        self.rows = analysis::build_rows(&self.a, &roots, &self.expanded, self.focus_mode);
        if self.sel >= self.rows.len() {
            self.sel = self.rows.len().saturating_sub(1);
        }
        self.update_detail();
    }

    fn selected_key(&self) -> Option<&String> {
        self.rows.get(self.sel).map(|r| &r.key)
    }

    /// Recompute the detail pane for the current selection.
    pub fn update_detail(&mut self) {
        self.detail.clear();
        self.detail_anchors.clear();
        self.detail_cursor = None;
        self.detail_selection = None;
        let Some(key) = self.selected_key().cloned() else {
            return;
        };
        let Some(n) = self.a.nodes.get(&key) else {
            return;
        };

        // header
        self.detail.push(Line::from(vec![
            Span::styled(
                n.name.clone(),
                Style::default()
                    .fg(status_color(n.status))
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("  "),
            Span::styled(
                format!("[{}; {}]", n.status.label(), n.kind.label()),
                Style::default().fg(status_color(n.status)),
            ),
        ]));
        if let Some(span) = n.head.as_ref().or(n.base.as_ref()) {
            self.detail.push(Line::from(Span::styled(
                format!(
                    "{} : rows {}-{}",
                    self.a.label(&span.file),
                    span.start_row + 1,
                    span.end_row
                ),
                Style::default().fg(Color::DarkGray),
            )));
        }
        if let (Some(base), Some(origin)) = (&n.base, &n.moved_from) {
            let location = if n.kind == SymbolKind::File {
                self.a.label(&base.file).to_string()
            } else {
                format!("{}::{origin}", self.a.label(&base.file))
            };
            self.detail
                .push(Line::from(format!("moved from {location}")));
        }

        if n.kind != SymbolKind::File {
            // callers (upstream impact)
            let in_scope = &n.callers;
            let external = &n.external_callers;
            let mut cl = vec![Span::raw("callers: ")];
            if in_scope.is_empty() && external.is_empty() {
                cl.push(Span::styled(
                    "none (entry point)",
                    Style::default().fg(Color::Cyan),
                ));
            } else {
                for (i, c) in in_scope.iter().take(5).enumerate() {
                    if i > 0 {
                        cl.push(Span::raw(", "));
                    }
                    if let Some(cn) = self.a.nodes.get(c) {
                        cl.push(Span::styled(
                            cn.name.clone(),
                            Style::default().fg(status_color(cn.status)),
                        ));
                    }
                }
                if in_scope.len() > 5 {
                    cl.push(Span::raw(format!(" … +{} more", in_scope.len() - 5)));
                }
                if !external.is_empty() {
                    if !in_scope.is_empty() {
                        cl.push(Span::raw(", "));
                    }
                    let names: Vec<String> = external
                        .iter()
                        .take(3)
                        .map(|k| match k.split_once("::") {
                            Some((path, qual)) => format!("{}::{qual}", self.a.label(path)),
                            None => self.a.label(k).to_string(),
                        })
                        .collect();
                    let more = if external.len() > 3 {
                        format!(" +{} more", external.len() - 3)
                    } else {
                        String::new()
                    };
                    cl.push(Span::styled(
                        format!("{}{more} (outside diff)", names.join(", ")),
                        Style::default().fg(Color::Cyan),
                    ));
                }
            }
            self.detail.push(Line::from(cl));

            // callees (downstream)
            let mut cal = vec![Span::raw("callees: ")];
            if n.callees.is_empty() && n.lost_callees.is_empty() {
                cal.push(Span::styled("leaf", Style::default().fg(Color::DarkGray)));
            } else {
                let mut first = true;
                for c in n.callees.iter().take(8) {
                    if !first {
                        cal.push(Span::raw(", "));
                    }
                    first = false;
                    if let Some(cn) = self.a.nodes.get(c) {
                        cal.push(Span::styled(
                            cn.name.clone(),
                            Style::default().fg(status_color(cn.status)),
                        ));
                    }
                }
                for ext in n
                    .lost_callees
                    .iter()
                    .take(8usize.saturating_sub(n.callees.len()))
                {
                    if !first {
                        cal.push(Span::raw(", "));
                    }
                    first = false;
                    cal.push(Span::styled(
                        format!("{ext} (ext)"),
                        Style::default().fg(Color::DarkGray),
                    ));
                }
                let total = n.callees.len() + n.lost_callees.len();
                if total > 8 {
                    cal.push(Span::raw(format!(" … +{} more", total - 8)));
                }
            }
            self.detail.push(Line::from(cal));
            for (label, related) in [
                ("contracts (base/head)", &n.contracts),
                ("implementations / members (base/head)", &n.implementations),
            ] {
                if related.is_empty() {
                    continue;
                }
                let names: Vec<_> = related
                    .iter()
                    .take(8)
                    .map(|key| self.a.nodes[key].name.as_str())
                    .collect();
                let remainder = if related.len() > 8 {
                    format!(" … +{} more", related.len() - 8)
                } else {
                    String::new()
                };
                self.detail.push(Line::from(format!(
                    "{label}: {}{remainder}",
                    names.join(", ")
                )));
            }
        }
        self.detail.push(Line::from(""));

        let head_src = self.a.head_body(n);
        let base_src = self.a.base_body(n);
        let binary = [base_src.as_deref(), head_src.as_deref()]
            .into_iter()
            .flatten()
            .any(|source| source.starts_with(BINARY_SOURCE_PREFIX));
        let diff: Vec<DiffLine> = fn_diff(base_src.as_deref(), head_src.as_deref());
        self.detail_anchors.resize(self.detail.len(), None);
        if diff.is_empty() {
            self.detail.push(Line::from(Span::styled(
                "(no textual difference)",
                Style::default().fg(Color::DarkGray),
            )));
        } else {
            for dl in diff {
                let style = match dl.sign {
                    Sign::Plus => Style::default().fg(Color::Green),
                    Sign::Minus => Style::default().fg(Color::Red),
                    Sign::Same => Style::default().fg(Color::DarkGray),
                };
                let anchor = DiffAnchor {
                    old_line: dl
                        .old_line
                        .and_then(|line| n.base.as_ref().map(|span| span.start_row + line)),
                    new_line: dl
                        .new_line
                        .and_then(|line| n.head.as_ref().map(|span| span.start_row + line)),
                };
                let text = if n.kind == SymbolKind::File && !binary {
                    format!(
                        "{:>5} {:>5} {}{}",
                        anchor
                            .old_line
                            .map(|line| line.to_string())
                            .unwrap_or_default(),
                        anchor
                            .new_line
                            .map(|line| line.to_string())
                            .unwrap_or_default(),
                        dl.sign.marker(),
                        dl.text
                    )
                } else {
                    format!("{}{}", dl.sign.marker(), dl.text)
                };
                self.detail.push(Line::from(Span::styled(text, style)));
                self.detail_anchors.push((!binary).then_some(anchor));
            }
        }
        self.detail_anchors.resize(self.detail.len(), None);
    }

    fn toggle_expand(&mut self) {
        let Some(k) = self.selected_key().cloned() else {
            return;
        };
        let has_children = self
            .rows
            .get(self.sel)
            .map(|r| r.has_children)
            .unwrap_or(false);
        if has_children {
            if self.expanded.contains(&k) {
                self.expanded.remove(&k);
            } else {
                self.expanded.insert(k);
            }
            self.rebuild();
        }
    }

    pub fn handle_event(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('d' | 'u'))
        {
            if self.focus == Focus::Detail {
                let step = (self.detail_viewport / 2).max(1);
                if let Some(cursor) = self.detail_cursor {
                    let target = if key.code == KeyCode::Char('d') {
                        cursor.saturating_add(step)
                    } else {
                        cursor.saturating_sub(step)
                    };
                    self.select_detail(target);
                    return;
                }
                self.detail_scroll = if key.code == KeyCode::Char('d') {
                    self.detail_scroll
                        .saturating_add(step)
                        .min(self.max_detail_scroll())
                } else {
                    self.detail_scroll.saturating_sub(step)
                };
            }
            return;
        }
        self.handle_key(key.code);
    }

    pub fn select_detail(&mut self, row: usize) {
        let row = row.min(self.detail.len().saturating_sub(1));
        self.detail_cursor = Some(row);
        if row < self.detail_scroll {
            self.detail_scroll = row;
        } else if row
            >= self
                .detail_scroll
                .saturating_add(self.detail_viewport.max(1))
        {
            self.detail_scroll = row.saturating_sub(self.detail_viewport.saturating_sub(1));
        }
        self.detail_scroll = self.detail_scroll.min(self.max_detail_scroll());
    }

    pub fn handle_key(&mut self, code: KeyCode) {
        if self.focus == Focus::Detail
            && let Some(cursor) = self.detail_cursor
        {
            let target = match code {
                KeyCode::Down | KeyCode::Char('j') => Some(cursor.saturating_add(1)),
                KeyCode::Up | KeyCode::Char('k') => Some(cursor.saturating_sub(1)),
                KeyCode::Home | KeyCode::Char('g') => Some(0),
                KeyCode::End | KeyCode::Char('G') => Some(self.detail.len().saturating_sub(1)),
                _ => None,
            };
            if let Some(target) = target {
                self.select_detail(target);
                return;
            }
        }
        match code {
            KeyCode::Char('H') | KeyCode::Char('L') => {
                let offset = match self.focus {
                    Focus::Tree => &mut self.tree_horizontal,
                    Focus::Detail => &mut self.detail_horizontal,
                };
                *offset = if code == KeyCode::Char('H') {
                    offset.saturating_sub(8)
                } else {
                    offset.saturating_add(8)
                };
            }
            KeyCode::Char('q') | KeyCode::Esc => self.quit = true,
            KeyCode::Down | KeyCode::Char('j') => {
                if self.focus == Focus::Tree {
                    if self.sel + 1 < self.rows.len() {
                        self.sel += 1;
                        self.detail_scroll = 0;
                        self.update_detail();
                    }
                } else {
                    self.detail_scroll = self.detail_scroll.saturating_add(1);
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if self.focus == Focus::Tree {
                    self.sel = self.sel.saturating_sub(1);
                    self.detail_scroll = 0;
                    self.update_detail();
                } else {
                    self.detail_scroll = self.detail_scroll.saturating_sub(1);
                }
            }
            KeyCode::Char('g') | KeyCode::Home => {
                if self.focus == Focus::Tree {
                    self.sel = 0;
                    self.update_detail();
                } else {
                    self.detail_scroll = 0;
                }
            }
            KeyCode::Char('G') | KeyCode::End => {
                if self.focus == Focus::Tree {
                    self.sel = self.rows.len().saturating_sub(1);
                    self.update_detail();
                } else {
                    self.detail_scroll = self.max_detail_scroll();
                }
            }
            KeyCode::Enter | KeyCode::Char(' ') | KeyCode::Char('l') => self.toggle_expand(),
            KeyCode::Char('h') => {
                let Some(r) = self.rows.get(self.sel) else {
                    return;
                };
                let k = r.key.clone();
                if self.expanded.contains(&k) {
                    self.expanded.remove(&k);
                } else if r.depth > 0 {
                    // collapse the nearest expanded ancestor above
                    for row in self.rows[..self.sel].iter().rev() {
                        if row.depth < r.depth && self.expanded.contains(&row.key) {
                            self.expanded.remove(&row.key);
                            break;
                        }
                    }
                }
                self.rebuild();
            }
            KeyCode::Tab => {
                self.detail_selection = None;
                self.focus = match self.focus {
                    Focus::Tree => Focus::Detail,
                    Focus::Detail => Focus::Tree,
                };
            }
            KeyCode::Char('f') => {
                self.focus_mode = !self.focus_mode;
                self.rebuild();
                self.flash = Some(if self.focus_mode {
                    "focus: changes only".into()
                } else {
                    "focus: full subtree".into()
                });
            }
            KeyCode::Char('e') => {
                self.expanded.clear();
                let roots = self.roots.clone();
                for r in &roots {
                    if self.a.nodes.get(r).map(|n| n.relevant).unwrap_or(false) {
                        self.expanded.insert(r.clone());
                    }
                }
                self.rebuild();
                self.flash = Some("collapsed to roots".into());
            }
            KeyCode::Char('x') => {
                if let Some(k) = self.selected_key().cloned() {
                    expand_all(&self.a, &k, &mut self.expanded, 5);
                    self.rebuild();
                    self.flash = Some("expanded subtree".into());
                }
            }
            _ => {}
        }
        self.detail_scroll = self.detail_scroll.min(self.max_detail_scroll());
    }

    fn max_detail_scroll(&self) -> usize {
        self.detail.len().saturating_sub(self.detail_viewport)
    }
}

fn expand_all(a: &Analysis, key: &str, expanded: &mut HashSet<String>, depth: usize) {
    if depth == 0 {
        return;
    }
    expanded.insert(key.to_string());
    for child in analysis::children_of(a, key, false) {
        expand_all(a, &child, expanded, depth - 1);
    }
}

fn status_color(s: Status) -> Color {
    match s {
        Status::Added => Color::Green,
        Status::Removed => Color::Red,
        Status::BodyChanged => Color::Yellow,
        Status::Moved => Color::Cyan,
        Status::Unchanged => Color::DarkGray,
    }
}

pub fn draw(f: &mut ratatui::Frame, app: &mut App) {
    let root = ratatui::layout::Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(10),
            Constraint::Length(1),
        ])
        .split(f.area());

    let (ad, rm, ch, mv) = app.a.summary;
    let header = Line::from(vec![
        Span::styled(
            " calldive ",
            Style::default().bg(Color::Blue).fg(Color::White),
        ),
        Span::raw("  "),
        Span::styled(format!("+{ad}"), Style::default().fg(Color::Green)),
        Span::raw(" "),
        Span::styled(format!("-{rm}"), Style::default().fg(Color::Red)),
        Span::raw(" "),
        Span::styled(format!("~{ch}"), Style::default().fg(Color::Yellow)),
        Span::raw(" "),
        Span::styled(format!(">{mv}"), Style::default().fg(Color::Cyan)),
        Span::raw(format!(
            "   entry points: {}   focus: {}",
            app.roots.len(),
            if app.focus_mode { "changes" } else { "full" }
        )),
    ]);
    f.render_widget(Paragraph::new(header), root[0]);

    let main = ratatui::layout::Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(root[1]);

    // left: tree
    let items: Vec<ListItem> = app
        .rows
        .iter()
        .enumerate()
        .map(|(i, r)| {
            let n = &app.a.nodes[&r.key];
            let sel = i == app.sel && app.focus == Focus::Tree;
            let indent = "  ".repeat(r.depth);
            let expand_glyph = if r.has_children {
                if app.expanded.contains(&r.key) {
                    "▾ "
                } else {
                    "▸ "
                }
            } else {
                "  "
            };
            let name_style = if n.status.is_change() {
                Style::default()
                    .fg(status_color(n.status))
                    .add_modifier(Modifier::BOLD)
            } else if n.relevant {
                Style::default().fg(Color::White)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let mut spans = vec![
                Span::raw(indent),
                Span::raw(expand_glyph),
                Span::styled(
                    n.status.glyph().to_string() + " ",
                    Style::default().fg(status_color(n.status)),
                ),
                Span::styled(n.name.clone(), name_style),
                Span::styled(
                    format!(
                        "  {}:{}",
                        app.a.label(&n.file),
                        n.head
                            .as_ref()
                            .or(n.base.as_ref())
                            .map_or(0, |span| span.start_row + 1)
                    ),
                    Style::default().fg(Color::DarkGray),
                ),
            ];
            if sel {
                for s in spans.iter_mut() {
                    s.style = s.style.add_modifier(Modifier::REVERSED);
                }
            }
            ListItem::new(scroll_line(Line::from(spans), app.tree_horizontal))
        })
        .collect();

    let tree_block = Block::default()
        .borders(Borders::ALL)
        .title(" call tree (DFS from entry points) ")
        .border_style(if app.focus == Focus::Tree {
            Style::default().fg(Color::Blue)
        } else {
            Style::default().fg(Color::DarkGray)
        });
    app.tree_state.select(Some(app.sel));
    let list = List::new(items)
        .block(tree_block)
        .highlight_style(Style::default());
    f.render_stateful_widget(list, main[0], &mut app.tree_state);

    // right: detail
    let detail_block = Block::default()
        .borders(Borders::ALL)
        .title(" symbol diff ")
        .border_style(if app.focus == Focus::Detail {
            Style::default().fg(Color::Blue)
        } else {
            Style::default().fg(Color::DarkGray)
        });
    app.detail_viewport = usize::from(detail_block.inner(main[1]).height);
    if let Some(cursor) = app.detail_cursor {
        app.select_detail(cursor);
    }
    app.detail_scroll = app.detail_scroll.min(app.max_detail_scroll());
    let end = (app.detail_scroll + app.detail_viewport).min(app.detail.len());
    let visible: Vec<Line<'_>> = app.detail[app.detail_scroll..end]
        .iter()
        .enumerate()
        .map(|(index, line)| {
            let row = app.detail_scroll + index;
            let selected = app.focus == Focus::Detail
                && app.detail_cursor.is_some_and(|cursor| {
                    let anchor = app.detail_selection.unwrap_or(cursor);
                    (anchor.min(cursor)..=anchor.max(cursor)).contains(&row)
                });
            Line::from(
                line.spans
                    .iter()
                    .map(|span| {
                        Span::styled(
                            span.content.as_ref(),
                            if selected {
                                span.style.add_modifier(Modifier::REVERSED)
                            } else {
                                span.style
                            },
                        )
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .collect();
    let para = Paragraph::new(visible)
        .scroll((0, app.detail_horizontal))
        .block(detail_block);
    f.render_widget(para, main[1]);

    // footer
    let flash = app.flash.take();
    let help = if let Some(msg) = flash {
        format!(" {msg}")
    } else {
        " j/k move · H/L scroll 8 cols · Ctrl+D/U half-page diff · g/G top/bottom · enter expand · x subtree · e collapse · f focus · tab pane · q quit "
            .to_string()
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            help,
            Style::default().fg(Color::DarkGray),
        ))),
        root[2],
    );
}

/// Clip by terminal columns while preserving styles and grapheme boundaries.
fn scroll_line(line: Line<'static>, offset: u16) -> Line<'static> {
    if offset == 0 {
        return line;
    }
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;
    let mut skip = usize::from(offset);
    let mut spans = Vec::with_capacity(line.spans.len());
    for span in line.spans {
        let mut text = String::new();
        for grapheme in span.content.graphemes(true) {
            let width = grapheme.width();
            if skip > 0 {
                if width > skip {
                    text.push_str(&" ".repeat(width - skip));
                }
                skip = skip.saturating_sub(width);
            } else {
                text.push_str(grapheme);
            }
        }
        spans.push(Span::styled(text, span.style));
    }
    Line::from(spans)
}

pub fn run(app: &mut App) -> anyhow::Result<()> {
    let mut terminal = ratatui::init();
    let res = (|| -> anyhow::Result<()> {
        loop {
            terminal.draw(|f| draw(f, app))?;
            if let Event::Key(k) = event::read()?
                && k.kind == KeyEventKind::Press
            {
                app.handle_event(k);
            }
            if app.quit {
                break;
            }
        }
        Ok(())
    })();
    ratatui::restore();
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case(None, Some("alpha\nbeta\n"), None, Some(1), Color::Green; "added")]
    #[test_case(Some("alpha\nbeta\n"), None, Some(1), None, Color::Red; "removed")]
    #[test_case(Some("alpha\nold\n"), Some("alpha\nnew\n"), Some(2), None, Color::Red; "modified")]
    fn file_diff_has_colored_numbered_revision_anchors(
        base: Option<&str>,
        head: Option<&str>,
        old_line: Option<usize>,
        new_line: Option<usize>,
        color: Color,
    ) {
        let path = "notes.txt";
        let analysis = analysis::analyze(&analysis::Inputs {
            files: vec![(
                path.into(),
                base.map(str::to_string),
                head.map(str::to_string),
            )],
            extra_head: vec![],
            include_tests: true,
        });
        let app = App::new(analysis, vec![analysis::node_key(path, "<file>")], false);
        assert_eq!(app.detail.len(), app.detail_anchors.len());
        let index = app
            .detail_anchors
            .iter()
            .position(|anchor| {
                anchor.as_ref().is_some_and(|anchor| {
                    anchor.old_line == old_line && anchor.new_line == new_line
                })
            })
            .unwrap();
        let line = &app.detail[index];
        assert_eq!(line.spans[0].style.fg, Some(color));
        let number = old_line.or(new_line).unwrap().to_string();
        assert!(plain(line).contains(&number));
    }

    #[test]
    fn binary_summary_does_not_expose_fake_line_anchors() {
        let path = "image.bin";
        let source = crate::git::decode_source(vec![0, 1, 2]);
        let analysis = analysis::analyze(&analysis::Inputs {
            files: vec![(path.into(), None, Some(source))],
            extra_head: vec![],
            include_tests: true,
        });
        let app = App::new(analysis, vec![analysis::node_key(path, "<file>")], false);
        assert_eq!(app.detail.len(), app.detail_anchors.len());
        assert!(app.detail_anchors.iter().all(Option::is_none));
        assert!(
            app.detail
                .iter()
                .any(|line| plain(line).contains(BINARY_SOURCE_PREFIX))
        );
    }

    #[test]
    fn symbol_anchors_use_each_revision_start_row() {
        let analysis = analyze(
            "fn target() { old(); }\n",
            "// shifted\nfn target() { new(); }\n",
        );
        let app = target_app(analysis);
        assert!(app.detail_anchors.contains(&Some(DiffAnchor {
            old_line: Some(1),
            new_line: None
        })));
        assert!(app.detail_anchors.contains(&Some(DiffAnchor {
            old_line: None,
            new_line: Some(2)
        })));
    }

    #[test]
    fn tree_starts_at_symbol_and_can_reveal_namespace() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut a = analyze("fn target() { let z = 3; }", "fn target() { let z = 4; }");
        let key = analysis::node_key("src/lib.rs", "target");
        a.nodes.get_mut(&key).unwrap().name = "Production::VeryLongNamespace::target(int x)".into();
        let mut app = target_app(a);
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        assert_eq!(terminal.backend().buffer()[(1, 2)].symbol(), "t");
        let offset = app.tree_horizontal;
        for _ in 0..offset {
            app.handle_key(KeyCode::Char('H'));
        }
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        assert_eq!(terminal.backend().buffer()[(5, 2)].symbol(), "P");
        assert_eq!(app.detail_horizontal, 0);
    }

    #[test]
    fn horizontal_scroll_moves_only_focused_panel_content() {
        use ratatui::{Terminal, backend::TestBackend};
        let a = analyze("fn target() { let z = 3; }", "fn target() { let z = 4; }");
        let mut app = target_app(a);
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let before = terminal.backend().buffer().clone();
        app.handle_key(KeyCode::Char('L'));
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let after = terminal.backend().buffer();
        assert_eq!(after[(1, 2)].symbol(), before[(9, 2)].symbol());
        assert_eq!(after[(46, 2)], before[(46, 2)]);
        app.handle_key(KeyCode::Tab);
        app.handle_key(KeyCode::Char('L'));
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        assert_eq!(terminal.backend().buffer()[(46, 2)].symbol(), "[");
        app.handle_key(KeyCode::Char('H'));
        app.handle_key(KeyCode::Char('H'));
        assert_eq!(app.detail_horizontal, 0);
        assert_eq!(app.tree_horizontal, 8);
    }

    #[test]
    fn control_paging_uses_diff_viewport_and_clamps_at_edges() {
        use ratatui::{Terminal, backend::TestBackend};
        let base = format!("fn target() {{\n{}\n}}", "    old();\n".repeat(50));
        let head = base.replace("old", "new");
        let a = analyze(&base, &head);
        let mut app = target_app(a);
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let down = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL);
        let up = KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL);
        app.handle_event(down);
        assert_eq!(app.detail_scroll, 0);
        app.handle_key(KeyCode::Tab);
        app.handle_event(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
        assert_eq!(app.detail_scroll, 0);
        app.handle_event(down);
        assert_eq!(app.detail_scroll, app.detail_viewport / 2);
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        app.handle_event(up);
        assert_eq!(app.detail_scroll, 0);
        for _ in 0..100 {
            app.handle_event(down);
        }
        assert_eq!(app.detail_scroll, app.max_detail_scroll());
        for _ in 0..100 {
            app.handle_event(up);
        }
        assert_eq!(app.detail_scroll, 0);
    }

    #[test]
    fn horizontal_clip_preserves_wide_and_combining_graphemes() {
        assert_eq!(
            plain(&scroll_line(Line::from("界e\u{301}x"), 1)),
            " e\u{301}x"
        );
        assert_eq!(plain(&scroll_line(Line::from("界e\u{301}x"), 3)), "x");
    }

    fn analyze(base: &str, head: &str) -> Analysis {
        analysis::analyze(&analysis::Inputs {
            files: vec![("src/lib.rs".into(), Some(base.into()), Some(head.into()))],
            extra_head: vec![],
            include_tests: true,
        })
    }

    fn target_app(a: Analysis) -> App {
        let key = analysis::node_key("src/lib.rs", "target");
        App::new(a, vec![key], false)
    }

    fn plain(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn unchanged_callee_shows_complete_source_in_gray() {
        let source = "fn target() {\r\n    let one = 1;\r\n    let two = 2;\r\n    let three = 3;\r\n    let four = 4;\r\n    let five = 5;\r\n    let six = 6;\r\n    let seven = 7;\r\n    let eight = 8;\r\n    println!(\"{one} {two} {three} {four} {five} {six} {seven} {eight}\");\r\n}";
        let base = format!("fn caller() {{ target(); }}\r\n{source}\r\n");
        let head = base.replace("target();", "target(); target();");
        let a = analyze(&base, &head);
        let mut app = App::new(a, vec![analysis::node_key("src/lib.rs", "caller")], false);
        app.handle_key(KeyCode::Down);
        assert_eq!(
            app.selected_key(),
            Some(&analysis::node_key("src/lib.rs", "target"))
        );
        assert_eq!(
            app.a.nodes[app.selected_key().unwrap()].status,
            Status::Unchanged
        );
        let body_start = app
            .detail
            .iter()
            .position(|line| plain(line).is_empty())
            .unwrap()
            + 1;
        let body = &app.detail[body_start..];
        let actual: Vec<_> = body.iter().map(plain).collect();
        let expected: Vec<_> = source.lines().map(|line| format!("  {line}")).collect();
        assert_eq!(actual, expected);
        for line in body {
            for span in &line.spans {
                assert_eq!(span.style.fg, Some(Color::DarkGray));
            }
        }
        assert!(app.detail_anchors[body_start..].iter().all(Option::is_some));
    }

    #[test]
    fn shifted_function_diff_uses_each_revision_span() {
        let base = "fn target() {\n    let z = 3;\n}\n";
        let head = "// one\n// two\n// three\nfn target() {\n    let z = 4;\n}\n";
        let a = analyze(base, head);
        let app = target_app(a);
        let lines: Vec<String> = app.detail.iter().map(plain).collect();
        assert!(lines.iter().any(|line| line == "-     let z = 3;"));
        assert!(lines.iter().any(|line| line == "+     let z = 4;"));
    }

    #[test]
    fn added_file_symbols_render_complete_head_source_with_plus_markers() {
        for (path, prefix, source, suffix, name) in [
            (
                "added.rs",
                "// café\r\n",
                "fn target() {\r\n    println!(\"café\");\r\n}",
                "",
                "target",
            ),
            (
                "added.py",
                "# café\r\n",
                "def target():\r\n    return \"café\"",
                "",
                "target",
            ),
            (
                "added.ts",
                "// café\r\n",
                "function target() {\r\n    return \"café\";\r\n}",
                "",
                "target",
            ),
            (
                "added.tsx",
                "// café\r\n",
                "function target() {\r\n    return <span>café</span>;\r\n}",
                "",
                "target",
            ),
            (
                "added.js",
                "// café\r\n",
                "function target() {\r\n    return \"café\";\r\n}",
                "",
                "target",
            ),
            (
                "added.go",
                "package added\r\n// café\r\n",
                "func target() string {\r\n    return \"café\"\r\n}",
                "",
                "target",
            ),
            (
                "added.cs",
                "// café\r\nclass Added {\r\n",
                "string Target() {\r\n    return \"café\";\r\n}",
                "\r\n}",
                "Added::Target",
            ),
            (
                "declaration.cs",
                "// café\r\ninterface Added {\r\n",
                "string Target();",
                "\r\n}",
                "Added::Target",
            ),
            (
                "type.cs",
                "// café\r\n",
                "class Added",
                " {\r\n    string Target() => \"café\";\r\n}",
                "Added",
            ),
            (
                "added.c",
                "// café\r\n",
                "const char *target(void) {\r\n    return \"café\";\r\n}",
                "",
                "target",
            ),
            (
                "added.cpp",
                "// café\r\n",
                "const char *target() {\r\n    return \"café\";\r\n}",
                "",
                "target",
            ),
        ] {
            let a = analysis::analyze(&analysis::Inputs {
                files: vec![(path.into(), None, Some(format!("{prefix}{source}{suffix}")))],
                extra_head: vec![],
                include_tests: false,
            });
            let app = App::new(a, vec![analysis::node_key(path, name)], false);
            let actual: Vec<_> = app
                .detail
                .iter()
                .map(plain)
                .filter(|line| line.starts_with("+ ") || line.starts_with("- "))
                .collect();
            let expected: Vec<_> = source.lines().map(|line| format!("+ {line}")).collect();
            assert_eq!(actual, expected, "{path}");
        }
    }

    #[test]
    fn added_method_in_existing_file_renders_only_its_complete_head_source() {
        let path = "ErrorRecordFilter.cs";
        let base = "namespace Example;\r\nclass ErrorRecordFilter {\r\n    string Existing() => \"café\";\r\n}\r\n";
        let source = "private static string GetErrorSummaryFallback(string errorSummary)\r\n    {\r\n        int end = errorSummary.AsSpan().IndexOfAny(';', '\\r', '\\n');\r\n\r\n        return (end >= 0 ? errorSummary[..end] : errorSummary).Trim();\r\n    }";
        let head = base.replace("\r\n}\r\n", &format!("\r\n    {source}\r\n}}\r\n"));
        let a = analysis::analyze(&analysis::Inputs {
            files: vec![(path.into(), Some(base.into()), Some(head))],
            extra_head: vec![],
            include_tests: false,
        });
        let app = App::new(
            a,
            vec![analysis::node_key(
                path,
                "Example::ErrorRecordFilter::GetErrorSummaryFallback",
            )],
            false,
        );
        let actual: Vec<_> = app
            .detail
            .iter()
            .map(plain)
            .filter(|line| line.starts_with("+ ") || line.starts_with("- "))
            .collect();
        let expected: Vec<_> = source.lines().map(|line| format!("+ {line}")).collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn callers_are_capped_with_exact_remainder() {
        let mut base = String::from("fn target() { let z = 3; }\n");
        for i in 0..7 {
            base.push_str(&format!("fn caller{i}() {{ target(); }}\n"));
        }
        let head = base.replace("let z = 3", "let z = 4");
        let a = analyze(&base, &head);
        let app = target_app(a);
        let line = app
            .detail
            .iter()
            .map(plain)
            .find(|line| line.starts_with("callers: "))
            .unwrap();
        assert!(line.contains("… +2 more"));
        for i in 0..5 {
            assert!(line.contains(&format!("caller{i}")));
        }
        assert!(!line.contains("caller5"));
        assert!(!line.contains("caller6"));
    }

    #[test]
    fn detail_scroll_stops_at_last_full_viewport() {
        let a = analyze(
            "fn target() {\n    let z = 3;\n}\n",
            "fn target() {\n    let z = 4;\n}\n",
        );
        let mut app = target_app(a);
        app.focus = Focus::Detail;
        app.detail_viewport = 3;
        let max = app.detail.len() - app.detail_viewport;
        app.handle_key(KeyCode::Char('G'));
        assert_eq!(app.detail_scroll, max);
        app.handle_key(KeyCode::Char('j'));
        app.handle_key(KeyCode::Down);
        assert_eq!(app.detail_scroll, max);
        app.handle_key(KeyCode::Home);
        assert_eq!(app.detail_scroll, 0);
        app.handle_key(KeyCode::End);
        assert_eq!(app.detail_scroll, max);
    }
}
