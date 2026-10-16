//! Snapcompact: archive conversation history as dense bitmap frames.
//!
//! Replaces the LLM summarization pass with a local, deterministic archival
//! pass. The discarded history is serialized, whitespace-collapsed, folded to
//! the glyphs the bundled pixel font can print, paginated onto a fixed grid,
//! and rasterized to PNG frames a vision-capable model reads back at a fraction
//! of the billed-token cost of the raw text. No model, API key, or network is
//! involved, so it is also safe for overflow recovery.
//!
//! The rebuilt context is ordered oldest→newest: verbatim text at the oldest
//! edge, the imaged middle, then verbatim text at the newest edge.

use std::collections::HashMap;
use std::io::Cursor;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use image::{ImageBuffer, Luma};
use maki_config::{AgentConfig, CompactionStrategy};
use maki_providers::{ContentBlock, ImageMediaType, ImageSource, Message, Model, Role};

use super::history::strip_thinking;

/// Frame edge in pixels. Width fixes the grid columns; height hugs the rows the
/// text actually fills.
const FRAME_SIZE: usize = 2576;
/// Upper bound on frames carried in one archive; the imaged middle foveates
/// (drops its oldest dense slice) past this.
const MAX_FRAMES: usize = 80;
/// Verbatim text kept at each chronological edge, in pages of frame capacity.
const TEXT_EDGE_PAGES: usize = 1;
/// Tool-result bodies are truncated head+tail to keep the archive dense.
const TOOL_RESULT_MAX_CHARS: usize = 2_000;
const TOOL_ARG_MAX_CHARS: usize = 500;
const TOOL_CALL_MAX_CHARS: usize = 2_000;
const TRUNCATE_HEAD_RATIO: f64 = 0.6;
/// Zero-width ink toggles: shift-out/shift-in wrap dim tool-output spans.
const DIM_ON: char = '\u{0e}';
const DIM_OFF: char = '\u{0f}';
/// FULL BLOCK fills its whole cell with pitch-black ink; newline runs fold to
/// it so line structure survives whitespace collapse at a one-cell cost.
const NEWLINE_GLYPH: char = '\u{2588}';
const INK_BLACK: u8 = 0;
const INK_DIM: u8 = 128;
const BG: u8 = 255;
const MAX_FRAME_SIZE: usize = 16_384;

const FONT_8X13: &str = include_str!("fonts/8x13.bdf");

/// Whether this compaction archives as bitmap frames. `snapcompact` forces it
/// (falling back to summarization only when the model has no vision); `auto`
/// picks it for vision-capable models; `summarize` never uses it.
pub fn strategy_enabled(config: &AgentConfig, model: &Model) -> bool {
    match config.compaction_strategy {
        CompactionStrategy::Summarize => false,
        CompactionStrategy::Snapcompact => model.supports_vision(),
        CompactionStrategy::Auto => model.supports_vision(),
    }
}

struct Glyph {
    w: u8,
    h: i32,
    xoff: i32,
    yoff: i32,
    rows: Vec<u8>,
}

struct Font {
    glyphs: HashMap<u32, Glyph>,
    ascent: i32,
    cell_w: usize,
    cell_h: usize,
}

fn parse_bdf(text: &str, cell_w: usize, cell_h: usize) -> Font {
    let mut glyphs = HashMap::new();
    let mut ascent = 0i32;
    let mut enc = -1i64;
    let mut bbx = [0i32; 4];
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        if let Some(rest) = line.strip_prefix("FONT_ASCENT") {
            ascent = rest.trim().parse().unwrap_or(0);
        } else if let Some(rest) = line.strip_prefix("ENCODING") {
            enc = rest.trim().parse().unwrap_or(-1);
        } else if let Some(rest) = line.strip_prefix("BBX") {
            let mut parts = rest.split_ascii_whitespace();
            for slot in &mut bbx {
                *slot = parts.next().and_then(|part| part.parse().ok()).unwrap_or(0);
            }
        } else if line.starts_with("BITMAP") {
            let mut rows = Vec::new();
            for row in lines.by_ref() {
                if row.starts_with("ENDCHAR") {
                    break;
                }
                rows.push(u8::from_str_radix(row.trim(), 16).unwrap_or(0));
            }
            if enc >= 0 {
                glyphs.insert(
                    enc as u32,
                    Glyph {
                        w: bbx[0].clamp(0, 8) as u8,
                        h: bbx[1],
                        xoff: bbx[2],
                        yoff: bbx[3],
                        rows,
                    },
                );
            }
        }
    }
    Font {
        glyphs,
        ascent,
        cell_w,
        cell_h,
    }
}

fn font() -> &'static Font {
    static FONT: std::sync::OnceLock<Font> = std::sync::OnceLock::new();
    FONT.get_or_init(|| parse_bdf(FONT_8X13, 8, 13))
}

/// Grid geometry shared with the pagination math: columns across the frame,
/// rows down it, and the cell budget one frame holds.
fn geometry() -> (usize, usize, usize) {
    let cols = FRAME_SIZE / font().cell_w;
    let rows = FRAME_SIZE / font().cell_h;
    (cols, rows, cols * rows)
}

/// East Asian Wide / Fullwidth code points occupy two grid cells.
const fn is_wide(cp: u32) -> bool {
    matches!(cp,
        0x1100..=0x115F
        | 0x2E80..=0x2EFF
        | 0x2F00..=0x2FDF
        | 0x3000..=0x303E
        | 0x3041..=0x33FF
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xA000..=0xA4CF
        | 0xAC00..=0xD7A3
        | 0xF900..=0xFAFF
        | 0xFE30..=0xFE4F
        | 0xFF00..=0xFF60
        | 0xFFE0..=0xFFE6
        | 0x20000..=0x2FFFD
        | 0x30000..=0x3FFFD
    )
}

/// Cells one code point consumes: zero for the zero-width dim toggles, two for
/// wide code points, one otherwise.
const fn cell_units(cp: u32) -> usize {
    match cp {
        0x0e | 0x0f => 0,
        _ if is_wide(cp) => 2,
        _ => 1,
    }
}

fn truncate_for_summary(text: &str, max_chars: usize, head_ratio: f64) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max_chars {
        return text.to_string();
    }
    let head = (max_chars as f64 * head_ratio) as usize;
    let tail = max_chars - head;
    let mut out: String = chars[..head].iter().collect();
    out.push('…');
    out.extend(chars[chars.len() - tail..].iter());
    out
}

fn strip_dim_markers(text: &str) -> String {
    text.chars()
        .filter(|c| *c != DIM_ON && *c != DIM_OFF)
        .collect()
}

/// Serialize the discarded history into conversation-dense text. Tool results
/// merge into their originating call, truncated head+tail and wrapped in a dim
/// span so archived conversation reads louder than archived tool noise.
pub fn serialize(messages: &[Message]) -> String {
    let results: HashMap<&str, &str> = messages
        .iter()
        .flat_map(|msg| msg.content.iter())
        .filter_map(|block| match block {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                ..
            } => Some((tool_use_id.as_str(), content.as_str())),
            _ => None,
        })
        .collect();

    let mut parts: Vec<String> = Vec::new();
    let mut merged: Vec<&str> = Vec::new();

    for msg in messages {
        match msg.role {
            Role::User => {
                for block in &msg.content {
                    match block {
                        ContentBlock::Text { text } => {
                            let text = strip_dim_markers(text);
                            if !text.trim().is_empty() {
                                parts.push(format!("¶user: {}", text.trim()));
                            }
                        }
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            ..
                        } => {
                            if merged.contains(&tool_use_id.as_str()) {
                                continue;
                            }
                            parts.push(format!("¶call:\n{}", render_result_block(content)));
                        }
                        _ => {}
                    }
                }
            }
            Role::Assistant => {
                let mut pending_thinking: Vec<String> = Vec::new();
                let mut pending_text: Vec<String> = Vec::new();
                let flush = |parts: &mut Vec<String>,
                             thinking: &mut Vec<String>,
                             text: &mut Vec<String>| {
                    if !thinking.is_empty() {
                        parts.push(format!("¶think: {}", thinking.join("\n")));
                    }
                    if !text.is_empty() {
                        parts.push(format!("¶ai: {}", text.join("\n")));
                    }
                    thinking.clear();
                    text.clear();
                };
                for block in &msg.content {
                    match block {
                        ContentBlock::Text { text } => {
                            let text = strip_dim_markers(text);
                            if !text.trim().is_empty() {
                                pending_text.push(text.trim().to_string());
                            }
                        }
                        ContentBlock::Thinking { thinking, .. } => {
                            let thinking = strip_dim_markers(thinking);
                            if !thinking.trim().is_empty() {
                                pending_thinking.push(thinking.trim().to_string());
                            }
                        }
                        ContentBlock::ToolUse {
                            id, name, input, ..
                        } => {
                            flush(&mut parts, &mut pending_thinking, &mut pending_text);
                            merged.push(id.as_str());
                            let args_str = input
                                .as_object()
                                .map(|map| {
                                    map.iter()
                                        .map(|(key, value)| {
                                            format!(
                                                "{key}={}",
                                                truncate_for_summary(
                                                    &value.to_string(),
                                                    TOOL_ARG_MAX_CHARS,
                                                    TRUNCATE_HEAD_RATIO,
                                                )
                                            )
                                        })
                                        .collect::<Vec<_>>()
                                        .join(", ")
                                })
                                .unwrap_or_default();
                            let args = truncate_for_summary(
                                &args_str,
                                TOOL_CALL_MAX_CHARS,
                                TRUNCATE_HEAD_RATIO,
                            );
                            let mut line = format!("¶call: {name}({args})");
                            if let Some(result) = results.get(id.as_str()) {
                                line.push('\n');
                                line.push_str(&render_result_block(result));
                            }
                            parts.push(line);
                        }
                        _ => {}
                    }
                }
                flush(&mut parts, &mut pending_thinking, &mut pending_text);
            }
        }
    }

    parts.join("\n\n")
}

fn render_result_block(raw: &str) -> String {
    let body = truncate_for_summary(
        &strip_dim_markers(raw),
        TOOL_RESULT_MAX_CHARS,
        TRUNCATE_HEAD_RATIO,
    );
    format!("<out>\n{DIM_ON}{body}{DIM_OFF}\n</out>")
}

fn fold_char(cp: u32, ch: char, font: &Font) -> Option<String> {
    if cp < 0x20 {
        return None;
    }
    if cp <= 0xff {
        return Some(ch.to_string());
    }
    if font.glyphs.contains_key(&cp) {
        return Some(ch.to_string());
    }
    let folded = match cp {
        0x2018 | 0x2019 | 0x201B => "'",
        0x201C..=0x201F => "\"",
        0x2039 | 0x203A => "'",
        0x00ab | 0x00bb => "\"",
        0x2013 | 0x2014 | 0x2012 | 0x2010 => "-",
        0x2026 => ".",
        0x2022 | 0x2023 => "*",
        0x2020 => "+",
        0x2021 => "#",
        0x20ac => "$",
        0x00a0 => " ",
        0x2502 | 0x2503 => "|",
        0x2500 | 0x2501 => "-",
        0x250c | 0x2510 | 0x2514 | 0x2518 | 0x251c | 0x2524 | 0x252c | 0x2534 | 0x253c => "+",
        0x2713 | 0x2714 => "v",
        0x2717 | 0x2718 => "x",
        0x2716 => "x",
        _ => return None,
    };
    Some(folded.to_string())
}

/// Prepare text for printing: collapse horizontal whitespace runs, fold
/// newlines to the full-block glyph, fold unsupported symbols to ASCII, and
/// drop decorative emoji.
pub fn normalize(text: &str) -> String {
    let font = font();
    let mut out = String::with_capacity(text.len());
    let mut space = false;
    for ch in text.chars() {
        let cp = ch as u32;
        if ch == DIM_ON || ch == DIM_OFF {
            out.push(ch);
            continue;
        }
        if ch == '\n' || ch == '\r' {
            out.push(NEWLINE_GLYPH);
            space = false;
            continue;
        }
        if ch.is_whitespace() {
            space = true;
            continue;
        }
        if let Some(folded) = fold_char(cp, ch, font) {
            if space && !out.is_empty() {
                out.push(' ');
            }
            space = false;
            out.push_str(&folded);
        }
    }
    out
}

/// Split normalized text into pages that each fill at most `capacity` grid
/// cells, inserting a one-cell pad before a wide glyph that would straddle the
/// right edge. Pages are contiguous substrings, so each renders from cell 0.
fn paginate(text: &str, capacity: usize, cols: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut pages: Vec<String> = Vec::new();
    let mut start = 0;
    let mut cell = 0;
    let mut has_cell = false;
    for (i, &ch) in chars.iter().enumerate() {
        let w = cell_units(ch as u32);
        if w == 0 {
            continue;
        }
        let mut at = cell;
        if w == 2 && cols >= 2 && at % cols == cols - 1 {
            at += 1;
        }
        if has_cell && at + w > capacity {
            pages.push(chars[start..i].iter().collect());
            start = i;
            at = 0;
        }
        cell = at + w;
        has_cell = true;
    }
    if has_cell {
        pages.push(chars[start..].iter().collect());
    }
    pages
}

fn used_rows(text: &str, cols: usize, rows: usize) -> usize {
    let mut cursor = 0usize;
    for ch in text.chars() {
        let w = cell_units(ch as u32);
        if w == 0 {
            continue;
        }
        let mut at = cursor;
        if w == 2 && cols >= 2 && at % cols == cols - 1 {
            at += 1;
        }
        cursor = at + w;
    }
    cursor.div_ceil(cols).clamp(1, rows)
}

/// Rasterize one page of normalized text onto a white canvas using the 8x13
/// pixel font, then encode it as a grayscale PNG. Returns base64 PNG data.
pub fn render_page(text: &str) -> Vec<u8> {
    let font = font();
    let (cols, rows, capacity) = geometry();
    let height = used_rows(text, cols, rows) * font.cell_h;
    let width = FRAME_SIZE.min(MAX_FRAME_SIZE);
    let mut pixels = vec![BG; width * height];

    let codes: Vec<char> = text.chars().collect();
    let mut dim = false;
    let mut cursor = 0usize;
    for &ch in &codes {
        if cursor >= capacity {
            break;
        }
        let cp = ch as u32;
        match cp {
            0x0e => {
                dim = true;
                continue;
            }
            0x0f => {
                dim = false;
                continue;
            }
            _ => {}
        }
        let ink = if dim { INK_DIM } else { INK_BLACK };
        let w = cell_units(cp);
        if w == 0 {
            continue;
        }
        let mut at = cursor;
        if w == 2 && cols >= 2 && at % cols == cols - 1 {
            at += 1;
        }
        cursor = at + w;
        if at >= capacity {
            break;
        }
        let row = at / cols;
        let col = at - row * cols;
        let left = (col * font.cell_w) as i32;
        let cell_top = (row * font.cell_h) as i32;
        if cp == NEWLINE_GLYPH as u32 {
            fill_cell(&mut pixels, width, height, font, left, cell_top, INK_BLACK);
            continue;
        }
        if let Some(glyph) = font.glyphs.get(&cp) {
            if glyph.rows.is_empty() {
                continue;
            }
            let gx = left + glyph.xoff;
            let top = cell_top + font.ascent - glyph.h - glyph.yoff;
            blit_glyph(&mut pixels, width, height, glyph, gx, top, ink);
        }
    }

    let buffer: ImageBuffer<Luma<u8>, Vec<u8>> =
        ImageBuffer::from_raw(width as u32, height as u32, pixels).expect("canvas sized to pixels");
    let mut out = Vec::new();
    buffer
        .write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
        .expect("encode PNG");
    STANDARD.encode(out).into()
}

fn blit_glyph(
    pixels: &mut [u8],
    width: usize,
    height: usize,
    glyph: &Glyph,
    left: i32,
    top: i32,
    ink: u8,
) {
    for (r, &bits) in glyph.rows.iter().enumerate() {
        if bits == 0 {
            continue;
        }
        let y = top + r as i32;
        if y < 0 || y >= height as i32 {
            continue;
        }
        let row_base = y as usize * width;
        for b in 0..glyph.w {
            if bits & (0x80u8 >> b) != 0 {
                let x = left + i32::from(b);
                if x >= 0 && (x as usize) < width {
                    pixels[row_base + x as usize] = ink;
                }
            }
        }
    }
}

fn fill_cell(
    pixels: &mut [u8],
    width: usize,
    height: usize,
    font: &Font,
    left: i32,
    top: i32,
    ink: u8,
) {
    let x0 = left.max(0) as usize;
    let x1 = (left + font.cell_w as i32).clamp(0, width as i32) as usize;
    if x0 >= x1 {
        return;
    }
    let y0 = top.max(0) as usize;
    let y1 = (top + font.cell_h as i32).clamp(0, height as i32) as usize;
    for y in y0..y1 {
        pixels[y * width + x0..y * width + x1].fill(ink);
    }
}

/// Lay out the accumulated archive text with verbatim text at both chronological
/// edges and images in the middle. Past `max_frames` the imaged middle drops its
/// oldest pages.
fn plan(text: &str, max_frames: usize) -> (String, Vec<String>, String, usize) {
    let (cols, _rows, capacity) = geometry();
    let edge_cap = TEXT_EDGE_PAGES * capacity;
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= 2 * edge_cap {
        return (text.to_string(), Vec::new(), String::new(), 0);
    }
    let text_head: String = chars[..edge_cap].iter().collect();
    let text_tail: String = chars[chars.len() - edge_cap..].iter().collect();
    if max_frames < 1 {
        let truncated = chars.len() - text_head.chars().count() - text_tail.chars().count();
        return (text_head, Vec::new(), text_tail, truncated);
    }
    let image_text: String = chars[edge_cap..chars.len() - edge_cap].iter().collect();
    if image_text.is_empty() {
        return (text.to_string(), Vec::new(), String::new(), 0);
    }
    let mut pages = paginate(&image_text, capacity, cols);
    let mut truncated = 0;
    if pages.len() > max_frames {
        let dropped: Vec<String> = pages.drain(..pages.len() - max_frames).collect();
        truncated = dropped.iter().map(String::len).sum();
    }
    (text_head, pages, text_tail, truncated)
}

pub struct SnapResult {
    pub blocks: Vec<ContentBlock>,
    pub summary: String,
}

/// Run a snapcompact pass over the messages being discarded. Fully local:
/// serializes them, lays the archive out with text edges and an imaged middle,
/// and renders the middle to PNG frames.
pub fn compact(messages: &[Message]) -> SnapResult {
    let mut stripped = messages.to_vec();
    strip_thinking(&mut stripped);
    strip_images(&mut stripped);
    let serialized = serialize(&stripped);
    let normalized = normalize(&serialized);
    let (text_head, pages, text_tail, truncated) = plan(&normalized, MAX_FRAMES);

    let mut blocks: Vec<ContentBlock> = Vec::new();
    if !text_head.is_empty() {
        let suffix = if pages.is_empty() {
            ""
        } else {
            "\n-------------- imaged middle below\n"
        };
        blocks.push(ContentBlock::Text {
            text: plain_text(&text_head) + suffix,
        });
    }
    for page in &pages {
        blocks.push(ContentBlock::Image {
            source: ImageSource::new(
                ImageMediaType::Png,
                STANDARD.encode(render_page(page)).into(),
            ),
        });
    }
    if !text_tail.is_empty() {
        let prefix = if pages.is_empty() {
            "-------------- middle history omitted above\n"
        } else {
            "-------------- imaged middle above\n"
        };
        let tail = prefix.to_string() + &plain_text(&text_tail);
        match blocks.last_mut() {
            Some(ContentBlock::Text { text }) => text.push_str(&tail),
            _ => blocks.push(ContentBlock::Text { text: tail }),
        }
    }

    let mut summary = format!(
        "Archived {} chars of history onto {} snapcompact frame{}.",
        normalized.chars().count(),
        pages.len(),
        if pages.len() == 1 { "" } else { "s" }
    );
    if truncated > 0 {
        summary.push_str(&format!(
            " {truncated} chars of the oldest middle were dropped to fit the frame budget."
        ));
    }
    SnapResult { blocks, summary }
}

fn plain_text(text: &str) -> String {
    text.chars()
        .map(|ch| if ch == NEWLINE_GLYPH { '\n' } else { ch })
        .filter(|ch| *ch != DIM_ON && *ch != DIM_OFF)
        .collect()
}

fn strip_images(messages: &mut [Message]) {
    for msg in messages {
        msg.content
            .retain(|block| !matches!(block, ContentBlock::Image { .. }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const OUT: &str = "build output";

    fn tool_use(id: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![ContentBlock::tool_use(
                id,
                "bash",
                json!({"command": "cargo build"}),
            )],
            ..Default::default()
        }
    }

    fn tool_result(id: &str) -> Message {
        Message {
            role: Role::User,
            content: vec![ContentBlock::tool_result(id, OUT, false)],
            ..Default::default()
        }
    }

    #[test]
    fn serialize_merges_result_into_call() {
        let messages = vec![
            Message::user("fix the build".into()),
            tool_use("t1"),
            tool_result("t1"),
        ];
        let text = serialize(&messages);
        assert!(text.contains("¶user: fix the build"));
        assert!(text.contains("¶call: bash("));
        assert!(text.contains("cargo build"));
        assert!(text.contains(OUT));
        assert_eq!(text.matches("¶call:").count(), 1);
    }

    #[test]
    fn normalize_collapses_whitespace_and_folds_newlines() {
        let out = normalize("a \t b   c\nd");
        assert_eq!(out, format!("a b c{NEWLINE_GLYPH}d"));
    }

    #[test]
    fn normalize_folds_unrenderable_symbols() {
        assert_eq!(normalize("✓"), "v");
        assert_eq!(normalize("✅"), "");
    }

    #[test]
    fn paginate_respects_capacity() {
        let (cols, _rows, capacity) = geometry();
        let text = "x".repeat(capacity + 10);
        let pages = paginate(&text, capacity, cols);
        assert_eq!(pages.len(), 2);
        assert_eq!(pages.concat(), text);
    }

    #[test]
    fn plan_keeps_text_edges_and_images_middle() {
        let capacity = {
            let (c, r, cap) = geometry();
            let _ = (c, r);
            cap
        };
        let text = "y".repeat(capacity * 3);
        let (head, pages, tail, truncated) = plan(&text, MAX_FRAMES);
        assert_eq!(head.chars().count(), capacity);
        assert_eq!(tail.chars().count(), capacity);
        assert!(!pages.is_empty());
        assert_eq!(truncated, 0);
    }

    #[test]
    fn plan_drops_oldest_middle_past_max_frames() {
        let capacity = {
            let (c, r, cap) = geometry();
            let _ = (c, r);
            cap
        };
        let text = "z".repeat(capacity * 200);
        let (_, pages, _, truncated) = plan(&text, 4);
        assert_eq!(pages.len(), 4);
        assert!(truncated > 0);
    }

    #[test]
    fn render_page_produces_decodable_png() {
        let data = render_page("Hello world. Again.");
        let bytes = STANDARD.decode(&data).unwrap();
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
    }

    #[test]
    fn compact_builds_text_and_image_blocks() {
        let mut messages = vec![Message::user("first task".into())];
        for i in 0..120 {
            let id = format!("t{i}");
            messages.push(Message {
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use(
                    &id,
                    "bash",
                    json!({"command": "x".repeat(2000)}),
                )],
                ..Default::default()
            });
            messages.push(Message {
                role: Role::User,
                content: vec![ContentBlock::tool_result(&id, "y".repeat(2000), false)],
                ..Default::default()
            });
        }
        let result = compact(&messages);
        assert!(matches!(result.blocks[0], ContentBlock::Text { .. }));
        assert!(
            result
                .blocks
                .iter()
                .any(|b| matches!(b, ContentBlock::Image { .. }))
        );
        assert!(result.summary.contains("snapcompact frame"));
    }

    #[test]
    fn compact_keeps_short_history_as_text() {
        let messages = vec![
            Message::user("first task".into()),
            tool_use("t1"),
            tool_result("t1"),
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: "done".into(),
                }],
                ..Default::default()
            },
        ];
        let result = compact(&messages);
        assert!(
            !result
                .blocks
                .iter()
                .any(|b| matches!(b, ContentBlock::Image { .. })),
            "a transcript under two edge pages stays verbatim text"
        );
        assert!(matches!(result.blocks[0], ContentBlock::Text { .. }));
    }
}
