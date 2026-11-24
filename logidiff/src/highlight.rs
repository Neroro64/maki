//! Stateful two-side syntax highlighting for symbol and file diffs, modeled
//! after Maki's diff view (`syntect` + `two-face`, Ayu Dark default theme).

use crate::diffview::InlineSpan;
use ratatui::{
    style::{Color, Modifier, Style},
    text::Span,
};
use std::sync::{Arc, OnceLock};
use syntect::highlighting::{
    Color as SynColor, FontStyle, HighlightIterator, HighlightState, Highlighter as SynHighlighter,
    ScopeSelectors, Style as SynStyle, StyleModifier, Theme, ThemeItem, ThemeSettings,
};
use syntect::parsing::{ParseState, ScopeStack, SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;

const MAX_PREFIX_BYTES: usize = 2048;
const MAX_PREFIX_LINES: usize = 20;
const MAX_GAP_PARSE_LINES: usize = 20;
const ANSI_ALPHA_INDEX: u8 = 0x00;
const ANSI_ALPHA_DEFAULT: u8 = 0x01;

static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
static DEFAULT_THEME: OnceLock<Arc<Theme>> = OnceLock::new();

pub fn syntax_set() -> &'static SyntaxSet {
    SYNTAX_SET.get_or_init(two_face::syntax::extra_newlines)
}

pub fn default_syntax_theme() -> Arc<Theme> {
    DEFAULT_THEME
        .get_or_init(|| Arc::new(build_ayu_dark_theme()))
        .clone()
}

pub fn syntax_for_path(path: &str) -> Option<&'static SyntaxReference> {
    let ss = syntax_set();
    if let Ok(Some(syn)) = ss.find_syntax_for_file(path) {
        return Some(syn);
    }
    let ext = path.rsplit('.').next()?;
    let mapped = match ext {
        "jsx" => "js",
        other => other,
    };
    ss.find_syntax_by_token(mapped)
}

/// Pre-compile regex automata for every language present in the diff so the
/// first navigation onto a file extension never stalls (~150–350ms) compiling
/// Oniguruma/fancy-regex grammars on the UI thread.
pub fn warmup_paths<'a>(paths: impl IntoIterator<Item = &'a str>) {
    let mut seen_syntaxes: std::collections::HashSet<&'static str> =
        std::collections::HashSet::new();
    let ss = syntax_set();
    for path in paths {
        if let Some(syn) = syntax_for_path(path)
            && seen_syntaxes.insert(syn.name.as_str())
        {
            let mut ps = ParseState::new(syn);
            let _ = ps.parse_line("\n", ss);
        }
    }
}

#[derive(Clone)]
pub struct DiffPalette {
    pub syntax_theme: Arc<Theme>,
    pub diff_old: Style,
    pub diff_new: Style,
    pub diff_old_emphasis: Style,
    pub diff_new_emphasis: Style,
    pub diff_old_sign: Style,
    pub diff_new_sign: Style,
    pub diff_line_nr: Style,
    pub collapsed_banner: Style,
    pub default_fg: Color,
    pub background: Color,
}

impl Default for DiffPalette {
    fn default() -> Self {
        Self {
            syntax_theme: default_syntax_theme(),
            diff_old: Style::default().bg(Color::Rgb(0x3d, 0x1c, 0x1c)),
            diff_new: Style::default().bg(Color::Rgb(0x1c, 0x30, 0x20)),
            diff_old_emphasis: Style::default().bg(Color::Rgb(0x5c, 0x2a, 0x2a)),
            diff_new_emphasis: Style::default().bg(Color::Rgb(0x2a, 0x4a, 0x30)),
            diff_old_sign: Style::default()
                .fg(Color::Rgb(0xf2, 0x6d, 0x78))
                .bg(Color::Rgb(0x3d, 0x1c, 0x1c)),
            diff_new_sign: Style::default()
                .fg(Color::Rgb(0xaa, 0xd9, 0x4c))
                .bg(Color::Rgb(0x1c, 0x30, 0x20)),
            diff_line_nr: Style::default().fg(Color::Rgb(0x5a, 0x66, 0x73)),
            collapsed_banner: Style::default()
                .fg(Color::Rgb(0x8a, 0x99, 0xad))
                .bg(Color::Rgb(0x16, 0x1a, 0x24)),
            default_fg: Color::Rgb(0xbf, 0xbd, 0xb6),
            background: Color::Rgb(0x0d, 0x10, 0x17),
        }
    }
}

/// Stateful per-revision syntax walker starting at file line 1 (with bounded
/// prefix lookback for huge files) so symbol scopes are accurate.
pub struct FileWalker<'a> {
    lines: LinesWithEndings<'a>,
    pos: usize,
    theme: Arc<Theme>,
    parse_state: ParseState,
    highlight_state: HighlightState,
    default_fg: Color,
}

impl<'a> FileWalker<'a> {
    /// `full_source` is the full file content (used for lightweight container
    /// scope priming up to `prefix_end_byte`), and `body` is the exact symbol/file
    /// body whose lines `highlight_at(1..=N)` will render.
    pub fn new(
        full_source: Option<&'a str>,
        prefix_end_byte: usize,
        body: &'a str,
        syntax: &'static SyntaxReference,
        theme: Arc<Theme>,
        default_fg: Color,
    ) -> Self {
        let syn_hl = SynHighlighter::new(&theme);
        let mut highlight_state = HighlightState::new(&syn_hl, ScopeStack::new());
        let mut parse_state = ParseState::new(syntax);

        if let Some(full) = full_source
            && prefix_end_byte > 0
            && prefix_end_byte <= full.len()
        {
            for line in compact_scope_prefix(&full[..prefix_end_byte], syntax.name.as_str()) {
                if let Ok(ops) = parse_state.parse_line(&line, syntax_set()) {
                    for _ in HighlightIterator::new(&mut highlight_state, &ops, &line, &syn_hl) {}
                }
            }
        }

        Self {
            lines: LinesWithEndings::from(body),
            pos: 1,
            theme,
            parse_state,
            highlight_state,
            default_fg,
        }
    }

    pub fn skip_to(&mut self, target_line: usize) {
        if self.pos >= target_line {
            return;
        }
        let gap = target_line - self.pos;
        if gap > MAX_GAP_PARSE_LINES {
            let fast_skip = gap - MAX_GAP_PARSE_LINES;
            for _ in 0..fast_skip {
                if self.lines.next().is_none() {
                    return;
                }
                self.pos += 1;
            }
        }
        let syn_hl = SynHighlighter::new(&self.theme);
        while self.pos < target_line {
            let Some(line) = self.lines.next() else {
                break;
            };
            if let Ok(ops) = self.parse_state.parse_line(line, syntax_set()) {
                for _ in HighlightIterator::new(&mut self.highlight_state, &ops, line, &syn_hl) {}
            }
            self.pos += 1;
        }
    }

    pub fn highlight_at(&mut self, target_line: usize) -> Option<Vec<Span<'static>>> {
        self.skip_to(target_line);
        let line = self.lines.next()?;
        self.pos += 1;
        let ops = self.parse_state.parse_line(line, syntax_set()).ok()?;
        let syn_hl = SynHighlighter::new(&self.theme);
        let ranges: Vec<(SynStyle, &str)> =
            HighlightIterator::new(&mut self.highlight_state, &ops, line, &syn_hl).collect();
        let mut spans = Vec::with_capacity(ranges.len());
        for (style, piece) in ranges {
            let clean = piece.trim_end_matches('\n').trim_end_matches('\r');
            if clean.is_empty() {
                continue;
            }
            spans.push(Span::styled(
                clean.to_string(),
                syntect_to_ratatui(style, self.default_fg),
            ));
        }
        Some(spans)
    }
}

/// Extract a tiny scope-priming prefix before `body` without paying regex parse
/// costs over hundreds of preceding lines in large files:
/// - For C#/Java/C++/Rust class/impl members that start inside a type body,
///   syntect's C# grammar expects a surrounding `class _ {` scope so method
///   return types and attributes highlight identically to full-file context
///   without parsing 24 KiB of preceding methods!
/// - Also inspects at most `MAX_PREFIX_LINES` / `MAX_PREFIX_BYTES` immediately
///   preceding `body` if an unclosed block comment (`/*` without `*/`) is open.
fn compact_scope_prefix(before: &str, syntax_name: &str) -> Vec<String> {
    let start = before.len().saturating_sub(MAX_PREFIX_BYTES);
    let window = if start == 0 || before.as_bytes()[start - 1] == b'\n' {
        &before[start..]
    } else {
        before.as_bytes()[start..]
            .iter()
            .position(|&b| b == b'\n')
            .map_or("", |nl| &before[start + nl + 1..])
    };

    // If the immediate window has an unclosed `/*` block comment (or triple-quote),
    // replay the tail lines from that opener so comment state carries into `body`.
    if has_unclosed_multiline_token(window) {
        let tail: Vec<&str> = LinesWithEndings::from(window).collect();
        let keep = tail.len().saturating_sub(MAX_PREFIX_LINES);
        return tail[keep..]
            .iter()
            .map(|raw| {
                if raw.ends_with('\n') {
                    (*raw).to_string()
                } else {
                    format!("{raw}\n")
                }
            })
            .collect();
    }

    // Otherwise, if we are inside an indented brace block in C# / C++ / Rust,
    // prime syntect with a single synthetic container line (`class _ {\n` or `impl _ {\n`)
    // in ~0.2ms instead of parsing hundreds of lines of preceding methods (~500ms in C#)!
    let brace_depth = estimate_brace_depth(window);
    if brace_depth > 0 {
        match syntax_name {
            "C#" => return vec!["class _ {\n".to_string()],
            "Rust" => return vec!["impl _ {\n".to_string()],
            _ => {}
        }
    }
    Vec::new()
}

fn has_unclosed_multiline_token(s: &str) -> bool {
    if let Some(open_pos) = s.rfind("/*") {
        let after = &s[open_pos + 2..];
        if !after.contains("*/") {
            return true;
        }
    }
    false
}

fn estimate_brace_depth(s: &str) -> i32 {
    let mut depth = 0i32;
    for b in s.bytes() {
        match b {
            b'{' => depth += 1,
            b'}' => depth = (depth - 1).max(0),
            _ => {}
        }
    }
    depth
}

fn syntect_to_ratatui(style: SynStyle, default_fg: Color) -> Style {
    let fg = match style.foreground.a {
        ANSI_ALPHA_INDEX => ansi_index_to_color(style.foreground.r),
        ANSI_ALPHA_DEFAULT => default_fg,
        _ => Color::Rgb(style.foreground.r, style.foreground.g, style.foreground.b),
    };
    let mut out = Style::default().fg(fg);
    if style.font_style.contains(FontStyle::BOLD) {
        out = out.add_modifier(Modifier::BOLD);
    }
    if style.font_style.contains(FontStyle::ITALIC) {
        out = out.add_modifier(Modifier::ITALIC);
    }
    if style.font_style.contains(FontStyle::UNDERLINE) {
        out = out.add_modifier(Modifier::UNDERLINED);
    }
    out
}

fn ansi_index_to_color(i: u8) -> Color {
    match i {
        0 => Color::Black,
        1 => Color::Red,
        2 => Color::Green,
        3 => Color::Yellow,
        4 => Color::Blue,
        5 => Color::Magenta,
        6 => Color::Cyan,
        7 => Color::Gray,
        8 => Color::DarkGray,
        9 => Color::LightRed,
        10 => Color::LightGreen,
        11 => Color::LightYellow,
        12 => Color::LightBlue,
        13 => Color::LightMagenta,
        14 => Color::LightCyan,
        15 => Color::White,
        n => Color::Indexed(n),
    }
}

/// Blend unchanged syntax foreground slightly toward the background so changed
/// lines stand out clearly while context code keeps readable syntax colors.
pub fn dim_context_spans(spans: Vec<Span<'static>>, bg: Color) -> Vec<Span<'static>> {
    spans
        .into_iter()
        .map(|mut span| {
            if let (Some(Color::Rgb(fr, fg, fb)), Color::Rgb(br, bg_g, bb)) = (span.style.fg, bg) {
                let blend = |f: u8, b: u8| ((u16::from(f) * 62 + u16::from(b) * 38) / 100) as u8;
                span.style.fg = Some(Color::Rgb(blend(fr, br), blend(fg, bg_g), blend(fb, bb)));
            } else {
                span.style = span.style.add_modifier(Modifier::DIM);
            }
            span
        })
        .collect()
}

/// Merge syntax foreground spans with inline diff background emphasis, matching
/// Maki's `merge_syntax_with_diff`.
pub fn merge_syntax_with_diff(
    syntax_spans: &[Span<'static>],
    diff_spans: &[InlineSpan],
    base_bg: Style,
    emphasis_bg: Style,
) -> Vec<Span<'static>> {
    let mut result = Vec::new();
    let syn_iter = syntax_spans
        .iter()
        .flat_map(|span| span.content.chars().map(move |c| (c, span.style)));
    let mut diff_iter = diff_spans.iter().flat_map(|ds| {
        let bg = if ds.emphasized { emphasis_bg } else { base_bg };
        ds.text.chars().map(move |_| bg)
    });

    let mut current_text = String::new();
    let mut current_style: Option<Style> = None;

    for (syn_char, syn_style) in syn_iter {
        let bg = diff_iter.next().unwrap_or(base_bg);
        let combined = syn_style.patch(bg);
        if current_style == Some(combined) {
            current_text.push(syn_char);
        } else {
            if !current_text.is_empty() {
                result.push(Span::styled(
                    std::mem::take(&mut current_text),
                    current_style.unwrap(),
                ));
            }
            current_text.push(syn_char);
            current_style = Some(combined);
        }
    }
    if !current_text.is_empty() {
        result.push(Span::styled(current_text, current_style.unwrap()));
    }
    result
}

fn build_ayu_dark_theme() -> Theme {
    let rgb = |r: u8, g: u8, b: u8| SynColor { r, g, b, a: 0xFF };
    let bg = rgb(0x0d, 0x10, 0x17);
    let fg = rgb(0xbf, 0xbd, 0xb6);
    let comment = rgb(0x5a, 0x66, 0x73);
    let orange = rgb(0xff, 0x8f, 0x40);
    let yellow = rgb(0xff, 0xb4, 0x54);
    let green = rgb(0xaa, 0xd9, 0x4c);
    let cyan = rgb(0x95, 0xe6, 0xcb);
    let blue = rgb(0x59, 0xc2, 0xff);
    let pink = rgb(0xf2, 0x96, 0x68);
    let red = rgb(0xf2, 0x6d, 0x78);

    let rules: &[(&str, SynColor, Option<FontStyle>)] = &[
        (
            "comment, comment punctuation.definition.comment",
            comment,
            None,
        ),
        ("string, string punctuation.definition.string", green, None),
        (
            "constant.character.escape, constant.character.escaped",
            cyan,
            None,
        ),
        (
            "constant, constant.numeric, constant.language, variable.other.constant, entity.name.constant",
            orange,
            None,
        ),
        (
            "keyword, keyword.control, keyword.operator, keyword.other, storage.modifier, keyword.declaration.function, storage.type.function",
            orange,
            None,
        ),
        (
            "storage.type, keyword.declaration",
            cyan,
            Some(FontStyle::ITALIC),
        ),
        (
            "entity.name.type, entity.name.class, entity.name.struct, entity.name.enum, entity.name.trait, entity.name.union, entity.name.impl, support.type, support.class, storage.type.primitive",
            cyan,
            Some(FontStyle::ITALIC),
        ),
        (
            "entity.name.function, variable.function, support.function, meta.function-call",
            yellow,
            None,
        ),
        ("entity.name.function.macro, support.macro", orange, None),
        ("variable.parameter", pink, Some(FontStyle::ITALIC)),
        ("variable.language", orange, Some(FontStyle::ITALIC)),
        (
            "entity.other.attribute-name, meta.annotation, variable.annotation",
            yellow,
            Some(FontStyle::ITALIC),
        ),
        ("entity.name.tag", orange, None),
        ("entity.name.label, storage.modifier.lifetime", blue, None),
        ("keyword.control.exception, string.regexp", red, None),
        (
            "punctuation.section.embedded, punctuation.section.interpolation",
            orange,
            None,
        ),
    ];

    let scopes = rules
        .iter()
        .filter_map(|(sel, color, font)| {
            let scope = sel.parse::<ScopeSelectors>().ok()?;
            Some(ThemeItem {
                scope,
                style: StyleModifier {
                    foreground: Some(*color),
                    background: None,
                    font_style: *font,
                },
            })
        })
        .collect();

    Theme {
        name: Some("Ayu Dark".to_string()),
        author: None,
        settings: ThemeSettings {
            foreground: Some(fg),
            background: Some(bg),
            ..ThemeSettings::default()
        },
        scopes,
    }
}
