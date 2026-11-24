//! ratatui UI: entry-point tree (left), per-symbol diff (right).

use crate::analysis::{self, Analysis, CallerBracket, EntryRole, Status, TreeRow};
use crate::diffview::{DiffItem, Sign, fn_diff_items};
use crate::git::{BINARY_SOURCE_PREFIX, RevCandidate};
use crate::highlight::{
    DiffPalette, FileWalker, dim_context_spans, merge_syntax_with_diff, syntax_for_path,
    warmup_paths,
};
use crate::syntax::SymbolKind;
use ratatui::{
    crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers},
    layout::{Constraint, Direction, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
};
use std::collections::{HashMap, HashSet};
use std::time::Duration;
use unicode_width::UnicodeWidthStr;
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Tree,
    Detail,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffAnchor {
    pub old_line: Option<usize>,
    pub new_line: Option<usize>,
}

#[derive(Clone)]
struct CachedDetail {
    lines: Vec<Line<'static>>,
    anchors: Vec<Option<DiffAnchor>>,
    gap_indices: Vec<Option<usize>>,
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
    pub tree_viewport: usize,
    pub detail_scroll: usize,
    pub detail_cursor: Option<usize>,
    pub detail_selection: Option<usize>,
    pub detail_viewport: usize,
    pub tree_horizontal: u16,
    pub detail_horizontal: u16,
    pub expanded_gaps: HashSet<usize>,
    pub expand_all_gaps: bool,
    pub show_help: bool,
    pub help_scroll: usize,
    pub help_viewport: usize,
    pub palette: DiffPalette,
    pub quit: bool,
    pub flash: Option<String>,
    /// rendered detail pane (header + diff) for the current selection
    pub detail: Vec<Line<'static>>,
    pub detail_anchors: Vec<Option<DiffAnchor>>,
    pub detail_gap_indices: Vec<Option<usize>>,
    last_detail_key: Option<String>,
    detail_cache: HashMap<String, CachedDetail>,
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
            tree_viewport: 10,
            detail_scroll: 0,
            detail_cursor: None,
            detail_selection: None,
            detail_viewport: 0,
            tree_horizontal: 0,
            detail_horizontal: 0,
            expanded_gaps: HashSet::new(),
            expand_all_gaps: false,
            show_help: false,
            help_scroll: 0,
            help_viewport: 20,
            palette: DiffPalette::default(),
            quit: false,
            flash: None,
            detail: vec![],
            detail_anchors: vec![],
            detail_gap_indices: vec![],
            last_detail_key: None,
            detail_cache: HashMap::new(),
        };
        warmup_paths(app.a.nodes.values().map(|n| n.file.as_str()));
        app.reset_default_expansion();
        app.rebuild();
        let any_grouped = app
            .rows
            .iter()
            .any(|r| r.group_label.is_some() || r.display_name.is_some());
        if !any_grouped
            && let Some(row) = app.rows.get(app.sel)
            && let Some(node) = app.a.nodes.get(&row.key)
        {
            let name = &node.name;
            let qualified = name.split('(').next().unwrap_or(name);
            if let Some(separator) = qualified.rfind("::") {
                let columns = row.depth * 2 + 4 + name[..separator + 2].width();
                app.tree_horizontal = columns.min(usize::from(u16::MAX)) as u16;
            }
        }
        app
    }

    fn reset_default_expansion(&mut self) {
        self.expanded.clear();
        loop {
            let rows =
                analysis::build_grouped_rows(&self.a, &self.roots, &self.expanded, self.focus_mode);
            let mut added = false;
            for r in rows {
                let expand_root = r.is_root
                    && r.has_children
                    && self.a.nodes.get(&r.key).is_some_and(|n| n.relevant);
                if (r.group_label.is_some() || expand_root) && self.expanded.insert(r.expand_key) {
                    added = true;
                }
            }
            if !added {
                break;
            }
        }
    }

    pub fn set_palette(&mut self, palette: DiffPalette) {
        let saved_cursor = self.detail_cursor;
        let saved_selection = self.detail_selection;
        let saved_scroll = self.detail_scroll;
        self.palette = palette;
        self.detail_cache.clear();
        self.update_detail();
        self.detail_scroll = saved_scroll.min(self.max_detail_scroll());
        self.detail_cursor = saved_cursor.map(|c| c.min(self.detail.len().saturating_sub(1)));
        self.detail_selection = saved_selection.map(|s| s.min(self.detail.len().saturating_sub(1)));
    }

    pub fn rebuild(&mut self) {
        self.rebuild_preserving(None);
    }

    fn rebuild_preserving(&mut self, target_id: Option<&str>) {
        let wanted_id = target_id
            .map(str::to_string)
            .or_else(|| self.rows.get(self.sel).map(|r| r.id.clone()));
        let wanted_key = self.rows.get(self.sel).map(|r| r.key.clone());
        let roots = self.roots.clone();
        self.rows = analysis::build_grouped_rows(&self.a, &roots, &self.expanded, self.focus_mode);
        if self.rows.is_empty() {
            self.sel = 0;
        } else if let Some(ref id) = wanted_id {
            if let Some(pos) = self.rows.iter().position(|r| r.id == *id) {
                self.sel = pos;
            } else if let Some(pos) = self.rows.iter().rposition(|r| {
                id.starts_with(&format!("{}/", r.id))
                    || id.starts_with(&format!("{}/", r.expand_key))
            }) {
                self.sel = pos;
            } else if let Some(ref key) = wanted_key
                && let Some(pos) = self.rows.iter().position(|r| &r.key == key)
            {
                self.sel = pos;
            } else if self.sel >= self.rows.len() {
                self.sel = self.rows.len() - 1;
            }
        } else if self.sel >= self.rows.len() {
            self.sel = self.rows.len() - 1;
        }
        self.update_detail();
    }

    fn selected_key(&self) -> Option<&String> {
        self.rows.get(self.sel).map(|r| &r.key)
    }

    /// Recompute the detail pane for the current selection.
    pub fn update_detail(&mut self) {
        let current_key = self.selected_key().cloned();
        if current_key != self.last_detail_key {
            self.expanded_gaps.clear();
            self.expand_all_gaps = false;
            self.last_detail_key = current_key.clone();
        }
        self.detail.clear();
        self.detail_anchors.clear();
        self.detail_gap_indices.clear();
        self.detail_cursor = None;
        self.detail_selection = None;
        let Some(key) = current_key else {
            return;
        };
        let cacheable = !self.expand_all_gaps && self.expanded_gaps.is_empty();
        if cacheable && let Some(cached) = self.detail_cache.get(&key) {
            self.detail = cached.lines.clone();
            self.detail_anchors = cached.anchors.clone();
            self.detail_gap_indices = cached.gap_indices.clone();
            return;
        }
        if let Some(row) = self.rows.get(self.sel)
            && let Some(group) = &row.group_label
        {
            self.detail.push(Line::from(vec![
                Span::styled(
                    group.clone(),
                    Style::default()
                        .fg(Color::Blue)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("  "),
                Span::styled(
                    "[namespace / container group]",
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
            self.detail.push(Line::from(Span::styled(
                "Press Enter / Space / l to toggle group, j/k to inspect symbols inside.",
                Style::default().fg(Color::DarkGray),
            )));
            self.detail_anchors.resize(self.detail.len(), None);
            self.detail_gap_indices.resize(self.detail.len(), None);
            return;
        }
        let Some(n) = self.a.nodes.get(&key) else {
            return;
        };

        let role = self.a.classify_node(n);
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
                format!(
                    "[{}; {}; {}]",
                    n.status.label(),
                    n.kind.label(),
                    role.label()
                ),
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
            if !n.consumers.is_empty() || !n.external_consumers.is_empty() {
                let mut con = vec![Span::raw("consumers: ")];
                let mut first = true;
                for c in n.consumers.iter().take(6) {
                    if !first {
                        con.push(Span::raw(", "));
                    }
                    first = false;
                    if let Some(cn) = self.a.nodes.get(c) {
                        con.push(Span::styled(
                            format!("> {}", cn.name),
                            Style::default().fg(status_color(cn.status)),
                        ));
                    }
                }
                for ext in n
                    .external_consumers
                    .iter()
                    .take(6usize.saturating_sub(n.consumers.len()))
                {
                    if !first {
                        con.push(Span::raw(", "));
                    }
                    first = false;
                    let label = match ext.split_once("::") {
                        Some((path, qual)) => format!("{}::{qual}", self.a.label(path)),
                        None => self.a.label(ext).to_string(),
                    };
                    con.push(Span::styled(
                        format!("> {label} (outside diff)"),
                        Style::default().fg(Color::Cyan),
                    ));
                }
                let total = n.consumers.len() + n.external_consumers.len();
                if total > 6 {
                    con.push(Span::raw(format!(" … +{} more", total - 6)));
                }
                self.detail.push(Line::from(con));
            }
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
        let items = fn_diff_items(
            base_src.as_deref(),
            head_src.as_deref(),
            &self.expanded_gaps,
            self.expand_all_gaps,
        );
        self.detail_anchors.resize(self.detail.len(), None);
        self.detail_gap_indices.resize(self.detail.len(), None);
        if items.is_empty() {
            self.detail.push(Line::from(Span::styled(
                "(no textual difference)",
                Style::default().fg(Color::DarkGray),
            )));
        } else {
            let use_syntax = !binary && n.status != Status::Unchanged;
            let syntax = use_syntax.then(|| syntax_for_path(&n.file)).flatten();
            let full_base = n
                .base
                .as_ref()
                .and_then(|s| self.a.base_srcs.get(&s.file).map(String::as_str));
            let full_head = n
                .head
                .as_ref()
                .and_then(|s| self.a.head_srcs.get(&s.file).map(String::as_str));
            let base_prefix_end = n.base.as_ref().map_or(0, |s| s.start_byte);
            let head_prefix_end = n.head.as_ref().map_or(0, |s| s.start_byte);
            let mut before_walker = syntax.zip(base_src.as_deref()).map(|(syn, body)| {
                FileWalker::new(
                    full_base,
                    base_prefix_end,
                    body,
                    syn,
                    self.palette.syntax_theme.clone(),
                    self.palette.default_fg,
                )
            });
            let mut after_walker = syntax.zip(head_src.as_deref()).map(|(syn, body)| {
                FileWalker::new(
                    full_head,
                    head_prefix_end,
                    body,
                    syn,
                    self.palette.syntax_theme.clone(),
                    self.palette.default_fg,
                )
            });
            for item in items {
                match item {
                    DiffItem::Collapsed {
                        gap_index,
                        hidden_count,
                        old_start: _,
                        old_end,
                        new_start,
                        new_end,
                    } => {
                        if let Some(bw) = before_walker.as_mut() {
                            bw.skip_to(old_end + 1);
                        }
                        if let Some(aw) = after_walker.as_mut() {
                            aw.skip_to(new_end + 1);
                        }
                        let abs_new_end =
                            n.head.as_ref().map_or(new_end, |s| s.start_row + new_end);
                        let abs_new_start = n
                            .head
                            .as_ref()
                            .map_or(new_start, |s| s.start_row + new_start);
                        let banner = format!(
                            "  ⋯ {hidden_count} unchanged lines (lines {abs_new_start}–{abs_new_end}) · Enter/z expand · Z expand all ⋯  "
                        );
                        self.detail.push(Line::from(Span::styled(
                            banner,
                            self.palette.collapsed_banner,
                        )));
                        self.detail_anchors.push(None);
                        self.detail_gap_indices.push(Some(gap_index));
                    }
                    DiffItem::Line(dl) => {
                        let anchor = DiffAnchor {
                            old_line: dl
                                .old_line
                                .and_then(|line| n.base.as_ref().map(|span| span.start_row + line)),
                            new_line: dl
                                .new_line
                                .and_then(|line| n.head.as_ref().map(|span| span.start_row + line)),
                        };
                        let show_numbers = n.kind == SymbolKind::File && !binary;
                        let gutter_prefix = if show_numbers {
                            format!(
                                "{:>5} {:>5} ",
                                anchor
                                    .old_line
                                    .map(|line| line.to_string())
                                    .unwrap_or_default(),
                                anchor
                                    .new_line
                                    .map(|line| line.to_string())
                                    .unwrap_or_default(),
                            )
                        } else {
                            String::new()
                        };
                        let line = match dl.sign {
                            Sign::Same => {
                                if let Some(bw) = before_walker.as_mut()
                                    && let Some(ol) = dl.old_line
                                {
                                    bw.skip_to(ol + 1);
                                }
                                let syn_spans = after_walker
                                    .as_mut()
                                    .zip(dl.new_line)
                                    .and_then(|(aw, nl)| aw.highlight_at(nl));
                                match (n.status, syn_spans) {
                                    (Status::Unchanged, _) | (_, None) => Line::from(Span::styled(
                                        format!("{gutter_prefix}{}{}", dl.sign.marker(), dl.text),
                                        Style::default().fg(Color::DarkGray),
                                    )),
                                    (_, Some(syn)) => {
                                        let mut spans = Vec::new();
                                        if !gutter_prefix.is_empty() {
                                            spans.push(Span::styled(
                                                gutter_prefix,
                                                self.palette.diff_line_nr,
                                            ));
                                        }
                                        spans.push(Span::styled(
                                            dl.sign.marker(),
                                            Style::default().fg(Color::DarkGray),
                                        ));
                                        spans.extend(dim_context_spans(
                                            syn,
                                            self.palette.background,
                                        ));
                                        Line::from(spans)
                                    }
                                }
                            }
                            Sign::Minus => {
                                let syn_spans = before_walker
                                    .as_mut()
                                    .zip(dl.old_line)
                                    .and_then(|(bw, ol)| bw.highlight_at(ol));
                                if let Some(syn) = syn_spans {
                                    let mut spans = Vec::new();
                                    if !gutter_prefix.is_empty() {
                                        spans.push(Span::styled(
                                            gutter_prefix,
                                            Style::default().fg(Color::Red),
                                        ));
                                    }
                                    spans.push(Span::styled(
                                        dl.sign.marker(),
                                        self.palette.diff_old_sign.fg(Color::Red),
                                    ));
                                    spans.extend(merge_syntax_with_diff(
                                        &syn,
                                        &dl.spans,
                                        self.palette.diff_old,
                                        self.palette.diff_old_emphasis,
                                    ));
                                    Line::from(spans)
                                } else {
                                    Line::from(Span::styled(
                                        format!("{gutter_prefix}{}{}", dl.sign.marker(), dl.text),
                                        Style::default().fg(Color::Red).patch(if binary {
                                            Style::default()
                                        } else {
                                            self.palette.diff_old
                                        }),
                                    ))
                                }
                            }
                            Sign::Plus => {
                                let syn_spans = after_walker
                                    .as_mut()
                                    .zip(dl.new_line)
                                    .and_then(|(aw, nl)| aw.highlight_at(nl));
                                if let Some(syn) = syn_spans {
                                    let mut spans = Vec::new();
                                    if !gutter_prefix.is_empty() {
                                        spans.push(Span::styled(
                                            gutter_prefix,
                                            Style::default().fg(Color::Green),
                                        ));
                                    }
                                    spans.push(Span::styled(
                                        dl.sign.marker(),
                                        self.palette.diff_new_sign.fg(Color::Green),
                                    ));
                                    spans.extend(merge_syntax_with_diff(
                                        &syn,
                                        &dl.spans,
                                        self.palette.diff_new,
                                        self.palette.diff_new_emphasis,
                                    ));
                                    Line::from(spans)
                                } else {
                                    Line::from(Span::styled(
                                        format!("{gutter_prefix}{}{}", dl.sign.marker(), dl.text),
                                        Style::default().fg(Color::Green).patch(if binary {
                                            Style::default()
                                        } else {
                                            self.palette.diff_new
                                        }),
                                    ))
                                }
                            }
                        };
                        self.detail.push(line);
                        self.detail_anchors.push((!binary).then_some(anchor));
                        self.detail_gap_indices.push(None);
                    }
                }
            }
        }
        self.detail_anchors.resize(self.detail.len(), None);
        self.detail_gap_indices.resize(self.detail.len(), None);
        if cacheable && self.detail_cache.len() < 512 {
            self.detail_cache.insert(
                key,
                CachedDetail {
                    lines: self.detail.clone(),
                    anchors: self.detail_anchors.clone(),
                    gap_indices: self.detail_gap_indices.clone(),
                },
            );
        }
    }

    fn toggle_expand(&mut self) {
        let Some(row) = self.rows.get(self.sel) else {
            return;
        };
        if !row.has_children {
            return;
        }
        let ek = row.expand_key.clone();
        if !self.expanded.remove(&ek) {
            self.expanded.insert(ek);
        }
        self.rebuild();
    }

    fn toggle_gap_at_cursor(&mut self) -> bool {
        let target_row = self.detail_cursor.unwrap_or(self.detail_scroll);
        let gap = self
            .detail_gap_indices
            .get(target_row)
            .and_then(|g| *g)
            .or_else(|| {
                let end = (self.detail_scroll + self.detail_viewport.max(1))
                    .min(self.detail_gap_indices.len());
                self.detail_cursor
                    .is_none()
                    .then(|| {
                        self.detail_gap_indices[self.detail_scroll..end]
                            .iter()
                            .find_map(|g| *g)
                    })
                    .flatten()
            });
        if let Some(gap_idx) = gap {
            let saved_cursor = self.detail_cursor;
            let saved_scroll = self.detail_scroll;
            if !self.expanded_gaps.remove(&gap_idx) {
                self.expanded_gaps.insert(gap_idx);
            }
            self.update_detail();
            self.detail_scroll = saved_scroll.min(self.max_detail_scroll());
            if let Some(c) = saved_cursor {
                self.select_detail(c);
            }
            return true;
        }
        false
    }

    fn toggle_all_gaps(&mut self) {
        let saved_cursor = self.detail_cursor;
        let saved_scroll = self.detail_scroll;
        self.expand_all_gaps = !self.expand_all_gaps;
        if !self.expand_all_gaps {
            self.expanded_gaps.clear();
        }
        self.update_detail();
        self.detail_scroll = saved_scroll.min(self.max_detail_scroll());
        if let Some(c) = saved_cursor {
            self.select_detail(c);
        }
        self.flash = Some(if self.expand_all_gaps {
            "diff: expanded all unchanged lines".into()
        } else {
            "diff: collapsed unchanged sections".into()
        });
    }

    pub fn handle_event(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        if self.show_help {
            self.handle_help_event(key);
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
            } else {
                let step = (self.tree_viewport / 2).max(1);
                if !self.rows.is_empty() {
                    let max_sel = self.rows.len().saturating_sub(1);
                    let next = if key.code == KeyCode::Char('d') {
                        self.sel.saturating_add(step).min(max_sel)
                    } else {
                        self.sel.saturating_sub(step)
                    };
                    if next != self.sel {
                        self.sel = next;
                        self.detail_scroll = 0;
                        self.update_detail();
                    }
                }
            }
            return;
        }
        self.handle_key(key.code);
    }

    fn handle_help_event(&mut self, key: KeyEvent) {
        let lines = build_help_lines(false);
        let max_scroll = lines.len().saturating_sub(self.help_viewport.max(1));
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('d') => {
                    let step = (self.help_viewport / 2).max(1);
                    self.help_scroll = self.help_scroll.saturating_add(step).min(max_scroll);
                }
                KeyCode::Char('u') => {
                    let step = (self.help_viewport / 2).max(1);
                    self.help_scroll = self.help_scroll.saturating_sub(step);
                }
                KeyCode::Char('c') => self.show_help = false,
                _ => {}
            }
            return;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('?' | 'q') => self.show_help = false,
            KeyCode::Down | KeyCode::Char('j') => {
                self.help_scroll = self.help_scroll.saturating_add(1).min(max_scroll);
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.help_scroll = self.help_scroll.saturating_sub(1);
            }
            KeyCode::PageDown => {
                let step = self.help_viewport.max(1);
                self.help_scroll = self.help_scroll.saturating_add(step).min(max_scroll);
            }
            KeyCode::PageUp => {
                let step = self.help_viewport.max(1);
                self.help_scroll = self.help_scroll.saturating_sub(step);
            }
            KeyCode::Home | KeyCode::Char('g') => self.help_scroll = 0,
            KeyCode::End | KeyCode::Char('G') => self.help_scroll = max_scroll,
            _ => {}
        }
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
            KeyCode::Char('?') => {
                self.show_help = !self.show_help;
                self.help_scroll = 0;
            }
            KeyCode::Char('Z') => {
                self.toggle_all_gaps();
            }
            KeyCode::Char('z') if self.focus == Focus::Detail => {
                if !self.toggle_gap_at_cursor() {
                    self.toggle_all_gaps();
                }
            }
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
                    if self.sel > 0 {
                        self.sel -= 1;
                        self.detail_scroll = 0;
                        self.update_detail();
                    }
                } else {
                    self.detail_scroll = self.detail_scroll.saturating_sub(1);
                }
            }
            KeyCode::Char('g') | KeyCode::Home => {
                if self.focus == Focus::Tree {
                    if self.sel != 0 {
                        self.sel = 0;
                        self.detail_scroll = 0;
                        self.update_detail();
                    }
                } else {
                    self.detail_scroll = 0;
                }
            }
            KeyCode::Char('G') | KeyCode::End => {
                if self.focus == Focus::Tree {
                    let last = self.rows.len().saturating_sub(1);
                    if self.sel != last {
                        self.sel = last;
                        self.detail_scroll = 0;
                        self.update_detail();
                    }
                } else {
                    self.detail_scroll = self.max_detail_scroll();
                }
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                if self.focus == Focus::Detail {
                    self.toggle_gap_at_cursor();
                } else {
                    self.toggle_expand();
                }
            }
            KeyCode::Char('l') => self.toggle_expand(),
            KeyCode::Char('h') => {
                let Some(r) = self.rows.get(self.sel) else {
                    return;
                };
                let ek = r.expand_key.clone();
                if r.has_children && self.expanded.contains(&ek) {
                    self.expanded.remove(&ek);
                    self.rebuild();
                } else if r.depth > 0 {
                    // collapse the nearest expanded ancestor above and focus that ancestor
                    let mut target_anc: Option<(String, String)> = None;
                    for row in self.rows[..self.sel].iter().rev() {
                        if row.depth < r.depth
                            && row.has_children
                            && self.expanded.contains(&row.expand_key)
                        {
                            target_anc = Some((row.id.clone(), row.expand_key.clone()));
                            break;
                        }
                    }
                    if let Some((anc_id, anc_ek)) = target_anc {
                        self.expanded.remove(&anc_ek);
                        self.rebuild_preserving(Some(&anc_id));
                    }
                }
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
                self.reset_default_expansion();
                self.rebuild();
                self.flash = Some("collapsed to roots".into());
            }
            KeyCode::Char('x') => {
                if let Some(base) = self.rows.get(self.sel) {
                    let root_id = base.id.clone();
                    let base_depth = base.depth;
                    let max_levels = 5usize;
                    for _ in 0..max_levels {
                        let rows = analysis::build_grouped_rows(
                            &self.a,
                            &self.roots,
                            &self.expanded,
                            self.focus_mode,
                        );
                        let Some(start_idx) = rows.iter().position(|r| r.id == root_id) else {
                            break;
                        };
                        let mut added = false;
                        let start_ek = rows[start_idx].expand_key.clone();
                        let start_coupled = rows[start_idx].caller_bracket.is_some();
                        if rows[start_idx].has_children && self.expanded.insert(start_ek.clone()) {
                            added = true;
                        }
                        for row in &rows[start_idx + 1..] {
                            if row.depth == base_depth {
                                if start_coupled && row.expand_key == start_ek {
                                    continue;
                                }
                                break;
                            }
                            if row.depth < base_depth {
                                break;
                            }
                            if row.depth < base_depth + max_levels
                                && row.has_children
                                && self.expanded.insert(row.expand_key.clone())
                            {
                                added = true;
                            }
                        }
                        if !added {
                            break;
                        }
                    }
                    self.rebuild_preserving(Some(&root_id));
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

fn format_shared_callers_badge(callers: &[String], from_coupled: bool) -> String {
    let joined = callers.join(", ");
    if from_coupled && joined.width() > 32 {
        if callers.len() == 2 {
            "[← both above]".to_string()
        } else {
            format!("[← {} above]", callers.len())
        }
    } else if callers.len() <= 2 {
        format!("[← {joined}]")
    } else {
        format!("[← {}, {} +{}]", callers[0], callers[1], callers.len() - 2)
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
            " logidiff ",
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
            let sel = i == app.sel && app.focus == Focus::Tree;
            let indent = "  ".repeat(r.depth);
            let expand_glyph = if r.has_children {
                if app.expanded.contains(&r.expand_key) {
                    "▾ "
                } else {
                    "▸ "
                }
            } else {
                "  "
            };
            let mut spans = if let Some(group_label) = &r.group_label {
                vec![
                    Span::raw(indent),
                    Span::styled(expand_glyph, Style::default().fg(Color::Blue)),
                    Span::styled(
                        group_label.clone(),
                        Style::default()
                            .fg(Color::Blue)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]
            } else {
                let n = &app.a.nodes[&r.key];
                let name_style = if r.is_repeat {
                    Style::default().fg(status_color(n.status))
                } else if n.status.is_change() {
                    Style::default()
                        .fg(status_color(n.status))
                        .add_modifier(Modifier::BOLD)
                } else if n.relevant {
                    Style::default().fg(Color::White)
                } else {
                    Style::default().fg(Color::DarkGray)
                };
                let shown_name = r
                    .display_name
                    .as_deref()
                    .unwrap_or(n.name.as_str())
                    .to_string();
                let role = app.a.classify_node(n);
                let show_role = n.status.is_change() && r.is_root && !r.is_repeat;
                let mut row_spans = vec![Span::raw(indent)];
                if let Some(bracket) = r.caller_bracket {
                    let b_ch = match bracket {
                        CallerBracket::First => "┌",
                        CallerBracket::Middle => "├",
                        CallerBracket::Last => "└",
                    };
                    row_spans.push(Span::styled(b_ch, Style::default().fg(Color::Cyan)));
                    row_spans.push(Span::styled(expand_glyph, Style::default().fg(Color::Cyan)));
                } else {
                    row_spans.push(Span::raw(expand_glyph));
                }
                row_spans.push(Span::styled(
                    n.status.glyph().to_string() + " ",
                    Style::default().fg(status_color(n.status)),
                ));
                if r.is_consumer {
                    row_spans.push(Span::styled(
                        "> ",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ));
                }
                row_spans.push(Span::styled(shown_name, name_style));
                if r.shared_callers.len() >= 2 {
                    row_spans.push(Span::styled(
                        format!(
                            "  {}",
                            format_shared_callers_badge(&r.shared_callers, r.shared_from_coupled)
                        ),
                        Style::default().fg(Color::Cyan),
                    ));
                }
                if r.is_repeat {
                    row_spans.push(Span::styled(
                        "  [↗ above]",
                        Style::default().fg(Color::Cyan),
                    ));
                }
                if let Some(hint) = &r.collapsed_callee_hint {
                    row_spans.push(Span::styled(
                        format!("  {hint}"),
                        Style::default().fg(Color::Cyan),
                    ));
                }
                if show_role {
                    row_spans.push(Span::styled(
                        format!("  [{}]", role.label()),
                        Style::default().fg(role_color(role)),
                    ));
                }
                row_spans.push(Span::styled(
                    format!(
                        "  {}:{}",
                        app.a.label(&n.file),
                        n.head
                            .as_ref()
                            .or(n.base.as_ref())
                            .map_or(0, |span| span.start_row + 1)
                    ),
                    Style::default().fg(Color::DarkGray),
                ));
                row_spans
            };
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
    app.tree_viewport = usize::from(tree_block.inner(main[0]).height).max(1);
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
        " j/k move · Ctrl+D/U page · H/L scroll · enter/z expand · Z all gaps · x subtree · e collapse · f focus · tab pane · ? help · q quit "
            .to_string()
    };
    f.render_widget(
        Paragraph::new(Line::from(Span::styled(
            help,
            Style::default().fg(Color::DarkGray),
        ))),
        root[2],
    );

    if app.show_help {
        draw_help_modal(f, app);
    }
}

fn role_color(role: EntryRole) -> Color {
    match role {
        EntryRole::UseCase => Color::LightBlue,
        EntryRole::DataFlow => Color::Cyan,
        EntryRole::Logic => Color::Yellow,
        EntryRole::SelfContained => Color::DarkGray,
    }
}

pub fn build_help_lines(include_annotations: bool) -> Vec<Line<'static>> {
    let heading = |title: &'static str| {
        Line::from(Span::styled(
            format!("  {title}"),
            Style::default()
                .fg(Color::Blue)
                .add_modifier(Modifier::BOLD),
        ))
    };
    let key_row = |keys: &'static str, desc: &'static str| {
        let pad = 20usize.saturating_sub(keys.width());
        Line::from(vec![
            Span::raw("    "),
            Span::styled(
                format!("{keys}{:pad$}", ""),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(desc, Style::default().fg(Color::White)),
        ])
    };
    let swatch_row =
        |swatch: &'static str, swatch_style: Style, label: &'static str, desc: &'static str| {
            let pad = 10usize.saturating_sub(swatch.width());
            let label_pad = 22usize.saturating_sub(label.width());
            Line::from(vec![
                Span::raw("    "),
                Span::styled(swatch, swatch_style),
                Span::raw(format!("{:pad$}", "")),
                Span::styled(
                    format!("{label}{:label_pad$}", ""),
                    swatch_style.add_modifier(Modifier::BOLD),
                ),
                Span::styled(desc, Style::default().fg(Color::Gray)),
            ])
        };

    let mut lines = vec![
        Line::from(""),
        heading("Overview"),
        Line::from(Span::styled(
            "    logidiff parses both revisions with tree-sitter and organizes diffs into a",
            Style::default().fg(Color::Gray),
        )),
        Line::from(Span::styled(
            "    reviewable flow: entry points expand into downstream callees, data/settings",
            Style::default().fg(Color::Gray),
        )),
        Line::from(Span::styled(
            "    changes expand into consumers (>), scripts group local helpers under their",
            Style::default().fg(Color::Gray),
        )),
        Line::from(Span::styled(
            "    file, and siblings sharing a namespace/class collapse under a common header.",
            Style::default().fg(Color::Gray),
        )),
        Line::from(""),
        heading("Status Glyphs & Color Legend"),
        swatch_row(
            "+  Added",
            Style::default().fg(Color::Green),
            "Green (+)",
            "Newly added function, type, property/constant, or file",
        ),
        swatch_row(
            "-  Removed",
            Style::default().fg(Color::Red),
            "Red (-)",
            "Deleted symbol or file (rendered from the base revision)",
        ),
        swatch_row(
            "~  Changed",
            Style::default().fg(Color::Yellow),
            "Yellow (~)",
            "Modified body, signature, type header, or initializer",
        ),
        swatch_row(
            ">  Moved",
            Style::default().fg(Color::Cyan),
            "Cyan (>)",
            "Renamed or moved symbol/file; in subtrees '> Name' marks a downstream consumer",
        ),
        swatch_row(
            "   Impact",
            Style::default().fg(Color::White),
            "White (blank)",
            "Unchanged caller, contract, or implementation on an impact path to a change",
        ),
        swatch_row(
            "   Context",
            Style::default().fg(Color::DarkGray),
            "Dim Gray (blank)",
            "Unchanged branch with no downstream edits (hidden in 'f' changes-only mode)",
        ),
        swatch_row(
            "▾  Group",
            Style::default().fg(Color::Blue),
            "Blue",
            "Shared namespace/module/class container header, or active focused pane border",
        ),
        swatch_row(
            "┌/└  Shared",
            Style::default().fg(Color::Cyan),
            "Cyan bracket",
            "Sibling callers sharing the same downstream callee(s), rendered once below ([← ...])",
        ),
        swatch_row(
            "[↗ above]",
            Style::default().fg(Color::Cyan),
            "Cyan ref",
            "Callee whose subtree was already rendered in an earlier branch",
        ),
        swatch_row(
            "external",
            Style::default().fg(Color::Cyan),
            "Cyan (detail)",
            "Entry-point indicator or callers/consumers located outside the diff files",
        ),
        Line::from(""),
        heading("Entry Role Badges"),
        swatch_row(
            "[use case]",
            Style::default().fg(Color::LightBlue),
            "Workflow / Root",
            "Entry-point workflow, handler, or script orchestrating downstream changes",
        ),
        swatch_row(
            "[data flow]",
            Style::default().fg(Color::Cyan),
            "Data / Settings",
            "Property, constant, DTO, or settings change propagating to consumers (>)",
        ),
        swatch_row(
            "[logic]",
            Style::default().fg(Color::Yellow),
            "Component Logic",
            "Internal method, constructor, or contract implementation logic change",
        ),
        swatch_row(
            "[self-contained]",
            Style::default().fg(Color::DarkGray),
            "Isolated Change",
            "Standalone symbol, config, or file edit with no in-diff callers/consumers",
        ),
        Line::from(""),
        heading("Diff Highlighting"),
        Line::from(vec![
            Span::raw("    "),
            Span::styled(
                "- removed ",
                Style::default()
                    .fg(Color::Rgb(0xf2, 0x6d, 0x78))
                    .bg(Color::Rgb(0x3d, 0x1c, 0x1c)),
            ),
            Span::styled(
                "changed",
                Style::default()
                    .fg(Color::White)
                    .bg(Color::Rgb(0x5c, 0x2a, 0x2a)),
            ),
            Span::raw("   "),
            Span::styled(
                "+ added ",
                Style::default()
                    .fg(Color::Rgb(0xaa, 0xd9, 0x4c))
                    .bg(Color::Rgb(0x1c, 0x30, 0x20)),
            ),
            Span::styled(
                "changed",
                Style::default()
                    .fg(Color::White)
                    .bg(Color::Rgb(0x2a, 0x4a, 0x30)),
            ),
            Span::styled(
                "   (inline token changes have brighter emphasis)",
                Style::default().fg(Color::Gray),
            ),
        ]),
        Line::from(vec![
            Span::raw("    "),
            Span::styled(
                "⋯ N unchanged lines ⋯",
                Style::default()
                    .fg(Color::Rgb(0x8a, 0x99, 0xad))
                    .bg(Color::Rgb(0x16, 0x1a, 0x24)),
            ),
            Span::styled(
                "   Folded unchanged gap between hunks (Enter / z to expand, Z for all)",
                Style::default().fg(Color::Gray),
            ),
        ]),
        Line::from(""),
        heading("Call Tree Keys"),
        key_row("j / k, ↑ / ↓", "Move selection up / down"),
        key_row("Ctrl+D / Ctrl+U", "Jump half a tree viewport down / up"),
        key_row("g / G, Home / End", "Jump to first / last tree entry"),
        key_row(
            "Enter / Space / l",
            "Expand or collapse selected node or namespace group",
        ),
        key_row("h", "Collapse current node, or jump to & collapse parent"),
        key_row(
            "x / e",
            "Expand subtree (5 levels) / Reset & collapse to roots",
        ),
        key_row("f", "Toggle focus mode (changes-only vs full subtree)"),
        key_row("H / L", "Scroll tree horizontally by 8 columns"),
        Line::from(""),
        heading("Diff Pane Keys"),
        key_row("Tab", "Switch focus between Call Tree and Symbol Diff"),
        key_row("j / k, ↑ / ↓", "Scroll / move line cursor in diff"),
        key_row("Ctrl+D / Ctrl+U", "Scroll half a diff page down / up"),
        key_row("g / G, Home / End", "Jump to top / bottom of diff"),
        key_row(
            "Enter / Space / z",
            "Expand or collapse folded unchanged gap under cursor",
        ),
        key_row("Z", "Toggle expanding / collapsing all unchanged sections"),
        key_row("H / L", "Scroll diff horizontally by 8 columns"),
    ];

    if include_annotations {
        lines.push(Line::from(""));
        lines.push(heading("Review Annotations (/review)"));
        lines.push(key_row(
            "v / V",
            "Start or cancel linewise visual selection in diff",
        ));
        lines.push(key_row(
            "c",
            "Annotate current symbol, diff line, or visual range",
        ));
        lines.push(key_row(
            "a",
            "Open saved review annotations list (e edit, d delete)",
        ));
        lines.push(key_row("s", "Submit all saved annotations to the agent"));
    }

    lines.push(Line::from(""));
    lines.push(heading("Global"));
    lines.push(key_row("?", "Toggle this help & color legend modal"));
    lines.push(key_row("q / Esc", "Close help modal or exit review"));
    lines.push(Line::from(""));
    lines
}

fn draw_help_modal(f: &mut ratatui::Frame, app: &mut App) {
    let area = f.area();
    let width = (area.width * 88 / 100)
        .max(44)
        .min(area.width.saturating_sub(2));
    let height = (area.height * 88 / 100)
        .max(12)
        .min(area.height.saturating_sub(2));
    let x = area.x + (area.width.saturating_sub(width)) / 2;
    let y = area.y + (area.height.saturating_sub(height)) / 2;
    let rect = Rect::new(x, y, width, height);

    f.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" logidiff help & color legend (j/k/Ctrl+D/U scroll · ?/Esc close) ")
        .border_style(Style::default().fg(Color::Blue))
        .style(Style::default().bg(app.palette.background));
    let inner = block.inner(rect);
    f.render_widget(block, rect);

    app.help_viewport = usize::from(inner.height).max(1);
    let lines = build_help_lines(false);
    let max_scroll = lines.len().saturating_sub(app.help_viewport);
    app.help_scroll = app.help_scroll.min(max_scroll);
    let end = (app.help_scroll + app.help_viewport).min(lines.len());
    let para = Paragraph::new(lines[app.help_scroll..end].to_vec());
    f.render_widget(para, inner);
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
            let ev = event::read()?;
            if let Event::Key(k) = ev
                && matches!(k.kind, KeyEventKind::Press | KeyEventKind::Repeat)
            {
                app.handle_event(k);
            }
            // Drain any buffered events that arrived during rendering so held-key
            // repeat never builds up input lag.
            while !app.quit && event::poll(Duration::ZERO)? {
                if let Event::Key(k) = event::read()?
                    && matches!(k.kind, KeyEventKind::Press | KeyEventKind::Repeat)
                {
                    app.handle_event(k);
                }
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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SelectFocus {
    Source,
    Target,
}

pub struct Selector {
    pub source_items: Vec<RevCandidate>,
    pub target_items: Vec<RevCandidate>,
    pub source_sel: usize,
    pub target_sel: usize,
    pub source_state: ListState,
    pub target_state: ListState,
    pub focus: SelectFocus,
    pub filter: String,
    pub filter_mode: bool,
    pub custom_input: Option<String>,
    pub flash: Option<String>,
    pub confirmed: Option<(String, String)>,
    pub quit: bool,
}

impl Selector {
    pub fn new(candidates: Vec<RevCandidate>) -> Self {
        let source_items: Vec<RevCandidate> = candidates
            .iter()
            .filter(|c| !c.is_worktree)
            .cloned()
            .collect();
        let target_items = candidates;

        let worktree_dirty = target_items
            .iter()
            .find(|c| c.is_worktree)
            .map(|c| !c.detail.starts_with("clean"))
            .unwrap_or(false);

        let source_default = if worktree_dirty {
            source_items
                .iter()
                .position(|c| c.rev == "HEAD")
                .unwrap_or(0)
        } else {
            source_items
                .iter()
                .position(|c| c.rev == "HEAD~1")
                .or_else(|| {
                    source_items
                        .iter()
                        .position(|c| c.rev == "origin/main" || c.rev == "main")
                })
                .unwrap_or(0)
        };

        let target_default = if worktree_dirty {
            target_items.iter().position(|c| c.is_worktree).unwrap_or(0)
        } else {
            target_items
                .iter()
                .position(|c| c.rev == "HEAD")
                .unwrap_or(0)
        };

        Self {
            source_items,
            target_items,
            source_sel: source_default,
            target_sel: target_default,
            source_state: ListState::default(),
            target_state: ListState::default(),
            focus: SelectFocus::Source,
            filter: String::new(),
            filter_mode: false,
            custom_input: None,
            flash: None,
            confirmed: None,
            quit: false,
        }
    }

    pub fn selected_source(&self) -> Option<&str> {
        self.source_items
            .get(self.source_sel)
            .map(|c| c.rev.as_str())
    }

    pub fn selected_target(&self) -> Option<&str> {
        self.target_items
            .get(self.target_sel)
            .map(|c| c.rev.as_str())
    }

    fn visible_indices(&self, focus: SelectFocus) -> Vec<usize> {
        let items = match focus {
            SelectFocus::Source => &self.source_items,
            SelectFocus::Target => &self.target_items,
        };
        if self.filter.is_empty() || self.focus != focus {
            return (0..items.len()).collect();
        }
        let query = self.filter.to_ascii_lowercase();
        items
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                item.rev.to_ascii_lowercase().contains(&query)
                    || item.detail.to_ascii_lowercase().contains(&query)
            })
            .map(|(i, _)| i)
            .collect()
    }

    fn clamp_active_selection(&mut self) {
        let indices = self.visible_indices(self.focus);
        if indices.is_empty() {
            return;
        }
        let sel = match self.focus {
            SelectFocus::Source => &mut self.source_sel,
            SelectFocus::Target => &mut self.target_sel,
        };
        if !indices.contains(sel) {
            let query = self.filter.to_ascii_lowercase();
            let items = match self.focus {
                SelectFocus::Source => &self.source_items,
                SelectFocus::Target => &self.target_items,
            };
            *sel = indices
                .iter()
                .copied()
                .find(|&i| items[i].rev.to_ascii_lowercase().contains(&query))
                .unwrap_or(indices[0]);
        }
    }

    pub fn handle_event(&mut self, key: KeyEvent) {
        if let Some(input) = &mut self.custom_input {
            match key.code {
                KeyCode::Esc => {
                    self.custom_input = None;
                }
                KeyCode::Enter => {
                    let typed = input.trim().to_string();
                    self.custom_input = None;
                    if !typed.is_empty() {
                        self.apply_custom_ref(typed);
                    }
                }
                KeyCode::Backspace => {
                    input.pop();
                }
                KeyCode::Char(ch)
                    if !key.modifiers.contains(KeyModifiers::CONTROL)
                        && !key.modifiers.contains(KeyModifiers::ALT) =>
                {
                    input.push(ch);
                }
                _ => {}
            }
            return;
        }

        if self.filter_mode {
            match key.code {
                KeyCode::Esc => {
                    self.filter.clear();
                    self.filter_mode = false;
                }
                KeyCode::Enter => {
                    self.filter_mode = false;
                }
                KeyCode::Backspace => {
                    self.filter.pop();
                    self.clamp_active_selection();
                }
                KeyCode::Down => self.move_selection(1),
                KeyCode::Up => self.move_selection(-1),
                KeyCode::Char(ch)
                    if !key.modifiers.contains(KeyModifiers::CONTROL)
                        && !key.modifiers.contains(KeyModifiers::ALT) =>
                {
                    self.filter.push(ch);
                    self.clamp_active_selection();
                }
                _ => {}
            }
            return;
        }

        self.handle_key(key.code);
    }

    pub fn handle_key(&mut self, code: KeyCode) {
        match code {
            KeyCode::Char('q') => self.quit = true,
            KeyCode::Esc => {
                if !self.filter.is_empty() {
                    self.filter.clear();
                } else {
                    self.quit = true;
                }
            }
            KeyCode::Tab
            | KeyCode::Left
            | KeyCode::Right
            | KeyCode::Char('h')
            | KeyCode::Char('l') => {
                self.filter.clear();
                self.focus = match self.focus {
                    SelectFocus::Source => SelectFocus::Target,
                    SelectFocus::Target => SelectFocus::Source,
                };
            }
            KeyCode::Down | KeyCode::Char('j') => self.move_selection(1),
            KeyCode::Up | KeyCode::Char('k') => self.move_selection(-1),
            KeyCode::Char('g') | KeyCode::Home => {
                let indices = self.visible_indices(self.focus);
                if let Some(&first) = indices.first() {
                    match self.focus {
                        SelectFocus::Source => self.source_sel = first,
                        SelectFocus::Target => self.target_sel = first,
                    }
                }
            }
            KeyCode::Char('G') | KeyCode::End => {
                let indices = self.visible_indices(self.focus);
                if let Some(&last) = indices.last() {
                    match self.focus {
                        SelectFocus::Source => self.source_sel = last,
                        SelectFocus::Target => self.target_sel = last,
                    }
                }
            }
            KeyCode::Char('/') => {
                self.filter_mode = true;
            }
            KeyCode::Char('i') => {
                self.custom_input = Some(String::new());
            }
            KeyCode::Char('s') => {
                let Some(src) = self.selected_source().map(str::to_string) else {
                    return;
                };
                let Some(tgt_item) = self.target_items.get(self.target_sel).cloned() else {
                    return;
                };
                if tgt_item.is_worktree {
                    self.flash = Some("cannot swap: base revision cannot be worktree".into());
                    return;
                }
                if let Some(new_src_idx) =
                    self.source_items.iter().position(|c| c.rev == tgt_item.rev)
                    && let Some(new_tgt_idx) = self.target_items.iter().position(|c| c.rev == src)
                {
                    self.source_sel = new_src_idx;
                    self.target_sel = new_tgt_idx;
                    self.flash = Some(format!("swapped to {} → {}", tgt_item.rev, src));
                }
            }
            KeyCode::Enter => {
                let (Some(src), Some(tgt)) = (self.selected_source(), self.selected_target())
                else {
                    self.flash = Some("select both a source and target revision".into());
                    return;
                };
                if src == tgt {
                    self.flash = Some(format!(
                        "source and target are both '{src}' — choose two distinct revisions"
                    ));
                    return;
                }
                self.confirmed = Some((src.to_string(), tgt.to_string()));
            }
            _ => {}
        }
    }

    fn move_selection(&mut self, delta: isize) {
        let indices = self.visible_indices(self.focus);
        if indices.is_empty() {
            return;
        }
        let current = match self.focus {
            SelectFocus::Source => self.source_sel,
            SelectFocus::Target => self.target_sel,
        };
        let pos = indices.iter().position(|&i| i == current).unwrap_or(0);
        let next_pos = if delta > 0 {
            (pos + (delta as usize)).min(indices.len() - 1)
        } else {
            pos.saturating_sub(delta.unsigned_abs())
        };
        let next_idx = indices[next_pos];
        match self.focus {
            SelectFocus::Source => self.source_sel = next_idx,
            SelectFocus::Target => self.target_sel = next_idx,
        }
    }

    fn apply_custom_ref(&mut self, typed: String) {
        if self.focus == SelectFocus::Source && crate::git::is_worktree(&typed) {
            self.flash = Some("source (base) revision cannot be worktree".into());
            return;
        }
        self.filter.clear();
        match self.focus {
            SelectFocus::Source => {
                if let Some(pos) = self.source_items.iter().position(|c| c.rev == typed) {
                    self.source_sel = pos;
                } else {
                    self.source_items.insert(
                        0,
                        RevCandidate {
                            rev: typed.clone(),
                            detail: "custom revision".into(),
                            is_worktree: false,
                        },
                    );
                    self.source_sel = 0;
                }
                if !self.target_items.iter().any(|c| c.rev == typed) {
                    self.target_items.insert(
                        0,
                        RevCandidate {
                            rev: typed,
                            detail: "custom revision".into(),
                            is_worktree: false,
                        },
                    );
                    self.target_sel += 1;
                }
            }
            SelectFocus::Target => {
                if let Some(pos) = self.target_items.iter().position(|c| c.rev == typed) {
                    self.target_sel = pos;
                } else {
                    let is_wt = crate::git::is_worktree(&typed);
                    self.target_items.insert(
                        0,
                        RevCandidate {
                            rev: typed.clone(),
                            detail: "custom revision".into(),
                            is_worktree: is_wt,
                        },
                    );
                    self.target_sel = 0;
                    if !is_wt && !self.source_items.iter().any(|c| c.rev == typed) {
                        self.source_items.insert(
                            0,
                            RevCandidate {
                                rev: typed,
                                detail: "custom revision".into(),
                                is_worktree: false,
                            },
                        );
                        self.source_sel += 1;
                    }
                }
            }
        }
    }
}

pub fn draw_selector(f: &mut ratatui::Frame, sel: &mut Selector) {
    let root = ratatui::layout::Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(8),
            Constraint::Length(1),
        ])
        .split(f.area());

    let src_label = sel.selected_source().unwrap_or("—");
    let tgt_label = sel.selected_target().unwrap_or("—");
    let header = Line::from(vec![
        Span::styled(
            " logidiff ",
            Style::default().bg(Color::Blue).fg(Color::White),
        ),
        Span::raw("  select revisions to diff: "),
        Span::styled(
            src_label.to_string(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" → "),
        Span::styled(
            tgt_label.to_string(),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    f.render_widget(Paragraph::new(header), root[0]);

    let panes = ratatui::layout::Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(root[1]);

    let render_pane = |items: &[RevCandidate],
                       visible: &[usize],
                       selected_idx: usize,
                       focused: bool,
                       active_color: Color,
                       title: String|
     -> (List<'static>, Option<usize>) {
        let mut state_sel = None;
        let list_items: Vec<ListItem<'static>> = visible
            .iter()
            .enumerate()
            .map(|(row_pos, &orig_idx)| {
                let c = &items[orig_idx];
                let is_sel = orig_idx == selected_idx;
                if is_sel {
                    state_sel = Some(row_pos);
                }
                let marker = if is_sel { "● " } else { "  " };
                let rev_style = if is_sel {
                    Style::default()
                        .fg(active_color)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::White)
                };
                let mut spans = vec![
                    Span::styled(marker, Style::default().fg(active_color)),
                    Span::styled(c.rev.clone(), rev_style),
                    Span::raw("  "),
                    Span::styled(c.detail.clone(), Style::default().fg(Color::DarkGray)),
                ];
                if is_sel && focused {
                    for s in &mut spans {
                        s.style = s.style.add_modifier(Modifier::REVERSED);
                    }
                }
                ListItem::new(Line::from(spans))
            })
            .collect();

        let block = Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(if focused {
                Style::default().fg(Color::Blue)
            } else {
                Style::default().fg(Color::DarkGray)
            });
        (
            List::new(list_items)
                .block(block)
                .highlight_style(Style::default()),
            state_sel,
        )
    };

    let src_visible = sel.visible_indices(SelectFocus::Source);
    let tgt_visible = sel.visible_indices(SelectFocus::Target);
    let src_title = if sel.focus == SelectFocus::Source && !sel.filter.is_empty() {
        format!(" source / base [filter: {}] ", sel.filter)
    } else {
        " source / base ".to_string()
    };
    let tgt_title = if sel.focus == SelectFocus::Target && !sel.filter.is_empty() {
        format!(" target / head [filter: {}] ", sel.filter)
    } else {
        " target / head ".to_string()
    };

    let (src_list, src_pos) = render_pane(
        &sel.source_items,
        &src_visible,
        sel.source_sel,
        sel.focus == SelectFocus::Source,
        Color::Yellow,
        src_title,
    );
    sel.source_state.select(src_pos);
    f.render_stateful_widget(src_list, panes[0], &mut sel.source_state);

    let (tgt_list, tgt_pos) = render_pane(
        &sel.target_items,
        &tgt_visible,
        sel.target_sel,
        sel.focus == SelectFocus::Target,
        Color::Green,
        tgt_title,
    );
    sel.target_state.select(tgt_pos);
    f.render_stateful_widget(tgt_list, panes[1], &mut sel.target_state);

    let footer_line = if let Some(input) = &sel.custom_input {
        let pane_name = match sel.focus {
            SelectFocus::Source => "source",
            SelectFocus::Target => "target",
        };
        Line::from(vec![
            Span::styled(
                format!(" custom {pane_name} rev: "),
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("{input}█"), Style::default().fg(Color::White)),
            Span::styled(
                "  (enter apply · esc cancel)",
                Style::default().fg(Color::DarkGray),
            ),
        ])
    } else if sel.filter_mode {
        Line::from(vec![
            Span::styled(
                " filter: ",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("{}█", sel.filter),
                Style::default().fg(Color::White),
            ),
            Span::styled(
                "  (enter done · esc clear)",
                Style::default().fg(Color::DarkGray),
            ),
        ])
    } else if let Some(msg) = sel.flash.take() {
        Line::from(Span::styled(
            format!(" {msg}"),
            Style::default().fg(Color::Yellow),
        ))
    } else {
        Line::from(Span::styled(
            " j/k move · tab/h/l switch pane · enter diff · s swap · / filter · i custom rev · q quit ",
            Style::default().fg(Color::DarkGray),
        ))
    };
    f.render_widget(Paragraph::new(footer_line), root[2]);
}

pub fn select_revisions(candidates: Vec<RevCandidate>) -> anyhow::Result<Option<(String, String)>> {
    let mut selector = Selector::new(candidates);
    let mut terminal = ratatui::init();
    let res = (|| -> anyhow::Result<Option<(String, String)>> {
        loop {
            terminal.draw(|f| draw_selector(f, &mut selector))?;
            if let Event::Key(k) = event::read()?
                && matches!(k.kind, KeyEventKind::Press | KeyEventKind::Repeat)
            {
                selector.handle_event(k);
            }
            while selector.confirmed.is_none() && !selector.quit && event::poll(Duration::ZERO)? {
                if let Event::Key(k) = event::read()?
                    && matches!(k.kind, KeyEventKind::Press | KeyEventKind::Repeat)
                {
                    selector.handle_event(k);
                }
            }
            if let Some(pair) = selector.confirmed.take() {
                return Ok(Some(pair));
            }
            if selector.quit {
                return Ok(None);
            }
        }
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

    #[test]
    fn selector_defaults_navigation_swap_filter_and_custom_input() {
        use ratatui::{Terminal, backend::TestBackend};
        let candidates = vec![
            RevCandidate {
                rev: "worktree".into(),
                detail: "clean working tree".into(),
                is_worktree: true,
            },
            RevCandidate {
                rev: "HEAD".into(),
                detail: "15c006d latest".into(),
                is_worktree: false,
            },
            RevCandidate {
                rev: "HEAD~1".into(),
                detail: "555ff2f previous".into(),
                is_worktree: false,
            },
            RevCandidate {
                rev: "origin/main".into(),
                detail: "555ff2f remote main".into(),
                is_worktree: false,
            },
        ];
        let mut sel = Selector::new(candidates);
        assert!(!sel.source_items.iter().any(|c| c.is_worktree));
        assert_eq!(sel.selected_source(), Some("HEAD~1"));
        assert_eq!(sel.selected_target(), Some("HEAD"));

        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal.draw(|f| draw_selector(f, &mut sel)).unwrap();

        sel.handle_key(KeyCode::Char('s'));
        assert_eq!(sel.selected_source(), Some("HEAD"));
        assert_eq!(sel.selected_target(), Some("HEAD~1"));

        sel.handle_key(KeyCode::Tab);
        assert_eq!(sel.focus, SelectFocus::Target);
        sel.handle_key(KeyCode::Char('/'));
        for ch in "origin".chars() {
            sel.handle_event(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        sel.handle_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(sel.selected_target(), Some("origin/main"));

        sel.handle_key(KeyCode::Tab);
        assert_eq!(sel.focus, SelectFocus::Source);
        sel.handle_key(KeyCode::Char('i'));
        for ch in "feature/login".chars() {
            sel.handle_event(KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE));
        }
        sel.handle_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(sel.selected_source(), Some("feature/login"));

        sel.handle_key(KeyCode::Enter);
        assert_eq!(
            sel.confirmed,
            Some(("feature/login".into(), "origin/main".into()))
        );
    }

    #[test]
    fn selector_rejects_identical_source_and_target_and_worktree_base() {
        let candidates = vec![
            RevCandidate {
                rev: "worktree".into(),
                detail: "2 changed files".into(),
                is_worktree: true,
            },
            RevCandidate {
                rev: "HEAD".into(),
                detail: "15c006d latest".into(),
                is_worktree: false,
            },
        ];
        let mut sel = Selector::new(candidates);
        assert_eq!(sel.selected_source(), Some("HEAD"));
        assert_eq!(sel.selected_target(), Some("worktree"));

        sel.handle_key(KeyCode::Char('s'));
        assert_eq!(sel.selected_source(), Some("HEAD"));
        assert_eq!(sel.selected_target(), Some("worktree"));
        assert!(sel.flash.as_deref().unwrap_or("").contains("worktree"));

        sel.handle_key(KeyCode::Tab);
        sel.handle_key(KeyCode::Char('j'));
        assert_eq!(sel.selected_target(), Some("HEAD"));
        sel.handle_key(KeyCode::Enter);
        assert!(sel.confirmed.is_none());
    }

    #[test]
    fn tree_control_paging_uses_tree_viewport_and_clamps() {
        use ratatui::{Terminal, backend::TestBackend};
        let mut head = String::new();
        for i in 0..30 {
            head.push_str(&format!("pub fn fn_{i:02}() -> usize {{ {i} }}\n"));
        }
        let a = analysis::analyze(&analysis::Inputs {
            files: vec![("src/lib.rs".into(), None, Some(head))],
            extra_head: vec![],
            include_tests: false,
        });
        let roots = a.entry_points.clone();
        let mut app = App::new(a, roots, false);
        let mut terminal = Terminal::new(TestBackend::new(100, 16)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let step = (app.tree_viewport / 2).max(1);
        let down = KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL);
        let up = KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL);
        assert_eq!(app.focus, Focus::Tree);
        assert_eq!(app.sel, 0);
        app.handle_event(down);
        assert_eq!(app.sel, step);
        app.handle_event(up);
        assert_eq!(app.sel, 0);
        for _ in 0..50 {
            app.handle_event(down);
        }
        assert_eq!(app.sel, app.rows.len() - 1);
        for _ in 0..50 {
            app.handle_event(up);
        }
        assert_eq!(app.sel, 0);
    }

    #[test]
    fn collapsed_diff_banners_have_no_anchors_and_expand_on_enter_or_z() {
        let mut base = String::from("fn target() {\n");
        for i in 1..=25 {
            base.push_str(&format!("    let v{i} = {i};\n"));
        }
        base.push_str("}\n");
        let head = base
            .replace("let v2 = 2;", "let v2 = 200;")
            .replace("let v24 = 24;", "let v24 = 240;");
        let a = analyze(&base, &head);
        let mut app = target_app(a);
        let banner_idx = app
            .detail_gap_indices
            .iter()
            .position(Option::is_some)
            .expect("collapsed gap banner");
        assert!(plain(&app.detail[banner_idx]).contains("unchanged lines"));
        assert!(app.detail_anchors[banner_idx].is_none());

        app.focus = Focus::Detail;
        app.select_detail(banner_idx);
        app.handle_key(KeyCode::Enter);
        assert!(
            !app.detail
                .iter()
                .any(|l| plain(l).contains("unchanged lines"))
        );

        app.handle_key(KeyCode::Char('Z'));
        assert!(app.expand_all_gaps);
        app.handle_key(KeyCode::Char('Z'));
        assert!(!app.expand_all_gaps);
        assert!(app.detail_gap_indices.iter().any(Option::is_some));
    }

    #[test]
    fn help_modal_renders_color_legend_and_scrolls() {
        use ratatui::{Terminal, backend::TestBackend};
        let a = analyze("fn target() { let x = 1; }", "fn target() { let x = 2; }");
        let mut app = target_app(a);
        let mut terminal = Terminal::new(TestBackend::new(110, 22)).unwrap();
        app.handle_key(KeyCode::Char('?'));
        assert!(app.show_help);
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(rendered.contains("Status Glyphs & Color Legend"));
        app.handle_event(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL));
        assert!(app.help_scroll > 0);
        app.handle_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!app.show_help);
    }

    #[test]
    fn syntax_highlighting_applies_rgb_backgrounds_and_inline_emphasis() {
        let a = analyze(
            "fn target() {\n    let timeout = 30;\n}\n",
            "fn target() {\n    let timeout = 60;\n}\n",
        );
        let app = target_app(a);
        let plus_line = app
            .detail
            .iter()
            .find(|l| plain(l).starts_with("+ "))
            .expect("added line");
        // Sign span uses green fg over diff_new bg; inline changed token '60' gets diff_new_emphasis bg
        assert!(plus_line.spans.len() > 2);
        assert!(
            plus_line
                .spans
                .iter()
                .any(|s| s.style.bg == app.palette.diff_new_emphasis.bg),
            "expected inline emphasis background on changed literal: {plus_line:?}"
        );
    }

    #[test]
    fn nearest_common_parent_groups_roots_sharing_namespace_prefix() {
        let cs = "namespace EA.TriageEngine.Core;\npublic class Alpha { public static int X { get; } = 1; }\npublic class Beta { public static int Y { get; } = 2; }\n";
        let other =
            "namespace Other.Single;\npublic class Solo { public static int Z { get; } = 3; }\n";
        let a = analysis::analyze(&analysis::Inputs {
            files: vec![
                ("Alpha.cs".into(), None, Some(cs.into())),
                ("Solo.cs".into(), None, Some(other.into())),
            ],
            extra_head: vec![],
            include_tests: false,
        });
        let roots = a.entry_points.clone();
        let app = App::new(a, roots, false);
        let groups: Vec<_> = app
            .rows
            .iter()
            .filter_map(|r| r.group_label.as_deref())
            .collect();
        assert!(
            groups.contains(&"EA.TriageEngine.Core"),
            "expected compact group header, got: {groups:?}"
        );
        assert!(
            !groups.iter().any(|g| g.contains("Other")),
            "singleton prefix must remain flat: {groups:?}"
        );
    }

    #[test]
    fn script_file_groups_top_level_helpers_under_file_node() {
        let script = "using System;\nConsole.WriteLine(Helper(1));\nstatic int Helper(int x) => x + 1;\nsealed class LocalModel { public int Id { get; set; } }\n";
        let a = analysis::analyze(&analysis::Inputs {
            files: vec![("tools/Repair.cs".into(), None, Some(script.into()))],
            extra_head: vec![],
            include_tests: false,
        });
        let file_key = analysis::node_key("tools/Repair.cs", "<file>");
        assert_eq!(a.entry_points, vec![file_key.clone()]);
        let app = App::new(a, vec![file_key], false);
        assert!(app.rows.len() >= 3);
        assert_eq!(app.rows[0].depth, 0);
        assert!(app.rows[1..].iter().all(|r| r.depth >= 1));
    }

    #[test]
    fn navigating_between_subtrees_in_large_csharp_file_stays_instant_and_cached() {
        use std::time::Instant;
        // Simulate a 600-line C# class with 80 preceding methods followed by
        // two changed entry points and unchanged callees across subtrees.
        let mut prefix =
            String::from("namespace Demo.Storage;\npublic sealed class BigStorage {\n");
        for i in 0..80 {
            prefix.push_str(&format!(
                "    public async Task<int> UnchangedMethod{i}(CancellationToken ct) {{ var v = await QueryAsync(\"SELECT {i}\", ct); return v + {i}; }}\n"
            ));
        }
        let mut base = prefix.clone();
        base.push_str("    public int EntryOne() { return HelperA() + 1; }\n");
        base.push_str("    public int EntryTwo() { return HelperB() + 2; }\n");
        base.push_str("    private int HelperA() { return 10; }\n");
        base.push_str("    private int HelperB() { return 20; }\n");
        base.push_str("}\n");

        let mut head = prefix;
        head.push_str("    public int EntryOne() { return HelperA() + 100; }\n");
        head.push_str("    public int EntryTwo() { return HelperB() + 200; }\n");
        head.push_str("    private int HelperA() { return 10; }\n");
        head.push_str("    private int HelperB() { return 20; }\n");
        head.push_str("}\n");

        let a = analysis::analyze(&analysis::Inputs {
            files: vec![("Storage/BigStorage.cs".into(), Some(base), Some(head))],
            extra_head: vec![],
            include_tests: false,
        });
        let roots = a.entry_points.clone();
        let mut app = App::new(a, roots, false);
        assert!(app.rows.len() >= 4);

        let t0 = Instant::now();
        for _pass in 0..3 {
            for _ in 0..app.rows.len() {
                app.handle_key(KeyCode::Char('j'));
            }
            for _ in 0..app.rows.len() {
                app.handle_key(KeyCode::Char('k'));
            }
        }
        let elapsed = t0.elapsed();
        assert!(
            elapsed.as_millis() < 250,
            "moving across C# subtrees should stay responsive (<250ms for 3 full passes), took {elapsed:?}"
        );
    }

    #[test]
    fn csharp_materializer_diamond_deduplicates_type_members_and_shared_callees() {
        use ratatui::{Terminal, backend::TestBackend};

        let mat_base = r#"namespace EA.TriageEngine.Core.AI.RootCauseAnalysis.RootCauseHypothesisWorkflow.Executors;
public sealed class RootCauseHypothesisMaterializer(ILogger logger) {
    public void MaterializeAsync() {
        CreateBuildInfrastructureAsync();
        CreatePreflightIssueAsync();
    }
    private int CreateBuildInfrastructureAsync() {
        return 1;
    }
    private int CreatePreflightIssueAsync() {
        return 2;
    }
}
"#;
        let mat_head = r#"namespace EA.TriageEngine.Core.AI.RootCauseAnalysis.RootCauseHypothesisWorkflow.Executors;
public sealed class RootCauseHypothesisMaterializer(ILogger logger, IssueOwnershipSettings settings) {
    public void MaterializeAsync() {
        CreateBuildInfrastructureAsync();
        CreatePreflightIssueAsync();
    }
    private int CreateBuildInfrastructureAsync() {
        return ResolveBuildInfrastructureOwnership() + 1;
    }
    private int CreatePreflightIssueAsync() {
        return ResolveBuildInfrastructureOwnership() + 2;
    }
    private int ResolveBuildInfrastructureOwnership() {
        return settings.GetBuildInfrastructureOwner();
    }
}
"#;
        let own_base = r#"namespace EA.TriageEngine.Core.System.FailureBucketSolver.Ownership;
public sealed class IssueOwnershipSettings(string defaultOwner) {
}
"#;
        let own_head = r#"namespace EA.TriageEngine.Core.System.FailureBucketSolver.Ownership;
public sealed class IssueOwnershipSettings(string defaultOwner, string infraOwner) {
    public int GetBuildInfrastructureOwner() {
        return 42;
    }
}
"#;

        let a = analysis::analyze(&analysis::Inputs {
            files: vec![
                (
                    "RootCauseHypothesisMaterializer.cs".into(),
                    Some(mat_base.into()),
                    Some(mat_head.into()),
                ),
                (
                    "IssueOwnershipSettings.cs".into(),
                    Some(own_base.into()),
                    Some(own_head.into()),
                ),
            ],
            extra_head: vec![],
            include_tests: false,
        });
        let roots: Vec<String> = a
            .entry_points
            .iter()
            .filter(|k| a.nodes.get(*k).is_some_and(|n| n.relevant))
            .cloned()
            .collect();
        let mut app = App::new(a, roots, true);

        // Before expanding the coupled callers, verify bracket + collapsed preview hint
        let create_build_idx = app
            .rows
            .iter()
            .position(|r| r.key.ends_with("::CreateBuildInfrastructureAsync"))
            .expect("CreateBuildInfrastructureAsync present");
        let create_preflight_idx = app
            .rows
            .iter()
            .position(|r| r.key.ends_with("::CreatePreflightIssueAsync"))
            .expect("CreatePreflightIssueAsync present");
        assert_eq!(
            app.rows[create_build_idx].caller_bracket,
            Some(CallerBracket::First)
        );
        assert_eq!(
            app.rows[create_preflight_idx].caller_bracket,
            Some(CallerBracket::Last)
        );
        assert_eq!(
            app.rows[create_preflight_idx]
                .collapsed_callee_hint
                .as_deref(),
            Some("→ + ResolveBuildInfrastructureOwnership")
        );

        // Move cursor to CreatePreflightIssueAsync and press Enter: cursor must stay on it!
        app.sel = create_preflight_idx;
        app.update_detail();
        let selected_before = app.rows[app.sel].key.clone();
        app.handle_key(KeyCode::Enter);
        assert_eq!(app.rows[app.sel].key, selected_before);

        // Expand ResolveBuildInfrastructureOwnership directly below
        let resolve_idx = app
            .rows
            .iter()
            .position(|r| r.key.ends_with("::ResolveBuildInfrastructureOwnership"))
            .expect("ResolveBuildInfrastructureOwnership expanded");
        assert_eq!(resolve_idx, app.sel + 1);
        assert_eq!(
            app.rows[resolve_idx].shared_callers,
            vec![
                "CreateBuildInfrastructureAsync".to_string(),
                "CreatePreflightIssueAsync".to_string()
            ]
        );
        app.sel = resolve_idx;
        app.update_detail();
        app.handle_key(KeyCode::Enter);
        assert_eq!(
            app.rows[app.sel].key, app.rows[resolve_idx].key,
            "cursor must remain on ResolveBuildInfrastructureOwnership"
        );

        // GetBuildInfrastructureOwner must be a leaf (no bogus consumer back-edge cycle)
        let owner_fn_idx = app
            .rows
            .iter()
            .position(|r| r.key.ends_with("::GetBuildInfrastructureOwner"))
            .expect("GetBuildInfrastructureOwner visible");
        assert!(
            !app.rows[owner_fn_idx].has_children,
            "leaf callee must not have a cyclic consumer back-edge"
        );

        // Every symbol key in the entire expanded tree must appear ONCE (zero duplicates!)
        let symbol_keys: Vec<&str> = app
            .rows
            .iter()
            .filter(|r| r.group_label.is_none())
            .map(|r| r.key.as_str())
            .collect();
        let unique_keys: HashSet<&str> = symbol_keys.iter().copied().collect();
        assert_eq!(
            symbol_keys.len(),
            unique_keys.len(),
            "duplicate symbol entries found in tree: {symbol_keys:#?}"
        );

        // Render and verify bracket + badge visuals
        let mut terminal = Terminal::new(TestBackend::new(160, 24)).unwrap();
        terminal.draw(|f| draw(f, &mut app)).unwrap();
        let rendered: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(rendered.contains("┌▾"));
        assert!(rendered.contains("└▾"));
        assert!(rendered.contains("[← both above]"));
    }

    #[test]
    fn partial_overlap_sibling_callers_render_shared_callee_once_and_preserve_cursor() {
        let base = r#"
fn entry() { sym_a(); sym_b(); }
fn sym_a() { old_a(); }
fn sym_b() { old_b(); }
"#;
        let head = r#"
fn entry() { sym_a(); sym_b(); }
fn sym_a() { unique_x(); shared_c(); }
fn sym_b() { unique_y(); shared_c(); }
fn unique_x() {}
fn unique_y() {}
fn shared_c() { leaf_d(); }
fn leaf_d() {}
"#;
        let a = analyze(base, head);
        let entry_key = analysis::node_key("src/lib.rs", "entry");
        let mut app = App::new(a, vec![entry_key], true);

        // Expand sym_a -> reveals unique_x and shared_c [← sym_a, sym_b]
        let a_idx = app
            .rows
            .iter()
            .position(|r| r.key.ends_with("::sym_a"))
            .unwrap();
        app.sel = a_idx;
        app.handle_key(KeyCode::Enter);
        assert!(app.rows[app.sel].key.ends_with("::sym_a"));

        // Expand shared_c -> reveals leaf_d
        let c_idx = app
            .rows
            .iter()
            .position(|r| r.key.ends_with("::shared_c"))
            .unwrap();
        assert_eq!(app.rows[c_idx].shared_callers, vec!["sym_a", "sym_b"]);
        app.sel = c_idx;
        app.handle_key(KeyCode::Enter);
        assert!(app.rows.iter().any(|r| r.key.ends_with("::leaf_d")));

        // Now move cursor to sym_b (below shared_c/leaf_d) and press Enter:
        // cursor MUST stay on sym_b, shared_c MUST stay expanded (leaf_d visible),
        // and neither shared_c nor leaf_d may be duplicated!
        let b_idx = app
            .rows
            .iter()
            .position(|r| r.key.ends_with("::sym_b"))
            .unwrap();
        app.sel = b_idx;
        app.handle_key(KeyCode::Enter);
        assert!(
            app.rows[app.sel].key.ends_with("::sym_b"),
            "cursor jumped away from sym_b onto {}",
            app.rows[app.sel].key
        );
        assert!(app.rows.iter().any(|r| r.key.ends_with("::unique_y")));
        assert!(app.rows.iter().any(|r| r.key.ends_with("::leaf_d")));

        let symbol_keys: Vec<&str> = app.rows.iter().map(|r| r.key.as_str()).collect();
        let unique_keys: HashSet<&str> = symbol_keys.iter().copied().collect();
        assert_eq!(
            symbol_keys.len(),
            unique_keys.len(),
            "expected zero duplicate rows: {symbol_keys:?}"
        );
    }

    #[test]
    fn cross_branch_repeats_and_collapse_keys_keep_cursor_on_target_entry() {
        let head = r#"
mod alpha {
    pub fn run_a() { work(); }
    pub fn extra_a() {}
}
mod beta {
    pub fn run_b() { work(); }
    pub fn extra_b() {}
}
pub fn work() { sub(); }
fn sub() {}
"#;
        let a = analysis::analyze(&analysis::Inputs {
            files: vec![("src/lib.rs".into(), None, Some(head.into()))],
            extra_head: vec![],
            include_tests: false,
        });
        let roots = a.entry_points.clone();
        let mut app = App::new(a, roots, true);

        // Expand alpha::run_a's child `work` -> shows `sub`
        let first_helper_idx = app
            .rows
            .iter()
            .position(|r| r.key.ends_with("::work") && !r.is_repeat)
            .expect("primary work row");
        app.sel = first_helper_idx;
        app.handle_key(KeyCode::Enter);
        assert_eq!(
            app.rows.iter().filter(|r| r.key.ends_with("::sub")).count(),
            1
        );

        // Second occurrence under beta::run_b is marked as `[↗ above]` repeat,
        // has_children == false, and never duplicates `sub`.
        let second_helper_idx = app
            .rows
            .iter()
            .position(|r| r.key.ends_with("::work") && r.is_repeat)
            .expect("reference work row");
        assert!(!app.rows[second_helper_idx].has_children);

        // Pressing `h` on the leaf under run_b collapses run_b and lands cursor directly on run_b
        // without collapsing alpha::run_a above!
        app.sel = second_helper_idx;
        app.handle_key(KeyCode::Char('h'));
        assert!(
            app.rows[app.sel].key.ends_with("beta::run_b"),
            "expected cursor to land on collapsed parent run_b, got {}",
            app.rows[app.sel].key
        );
        assert_eq!(
            app.rows.iter().filter(|r| r.key.ends_with("::sub")).count(),
            1,
            "collapsing beta branch must not collapse alpha branch above"
        );
    }
}
