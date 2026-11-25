//! Snapcompact: archive conversation history as dense bitmap frames.
//!
//! Replaces the LLM summarization pass with a local, deterministic archival
//! pass. The discarded history is serialized, whitespace-collapsed, folded to
//! the glyphs the bundled pixel font can print, paginated onto a fixed grid,
//! and rasterized to PNG frames a vision-capable model reads back at a fraction
//! of the billed-token cost of the raw text. No model, API key, or network is
//! involved, so it is also safe for overflow recovery.
//!
//! The rebuilt context is ordered oldest→newest inside a synthetic user turn:
//! verbatim text at the oldest edge, the imaged middle, then verbatim text at
//! the newest edge, followed by an assistant acknowledgement so every provider
//! receives valid alternating turns with images in the user role.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use image::ExtendedColorType;
use image::ImageEncoder as _;
use image::codecs::png::{CompressionType, FilterType as PngFilter, PngEncoder};
use maki_config::{AgentConfig, CompactionStrategy};
use maki_providers::{ContentBlock, ImageMediaType, ImageSource, Message, Model, Role};
use maki_storage::blocking;
use tracing::info;

use super::compaction::Compacted;
use super::history::{History, stamp_new};
use crate::AgentError;

/// Matches `maki_providers::image::MAX_EDGE` (1568px): any larger dimension
/// forces `adapt_images_for_model` (and Anthropic server-side) to downscale
/// with a triangle filter, blurring 1px strokes in the 8x13 bitmap font.
const FRAME_SIZE: usize = 1568;
/// Hard cap on frames in one archive, staying well below `MAX_IMAGES` (100) so
/// later tool screenshots do not evict archive pages and void the prompt cache.
const MAX_FRAMES: usize = 32;
/// Matches `maki_providers::tokens::TOKENS_PER_IMAGE` so frame budgeting agrees
/// with `ContextGauge::reset`.
const TOKENS_PER_FRAME: u32 = 1_500;
/// Fraction of usable context window a snapcompact archive's frames may occupy.
const MAX_WINDOW_FRACTION: u32 = 4;
/// Total base64 byte budget across all frames in one archive. Gateways such as
/// Copilot's `/chat/completions` enforce a hard 5 MiB (5,242,880 B) HTTP body
/// limit and reject anything larger with `413 Request Entity Too Large` even
/// when the prompt is only ~30k tokens. Keeping the archive's base64 frames
/// within 3 MiB leaves >2 MiB of headroom for tools, system prompt, and recent
/// turns across every provider.
const MAX_ARCHIVE_BASE64_BYTES: usize = 3 * 1024 * 1024;
/// Prior frames above this base64 size came from an older `CompressionType::Fast`
/// build (~1.6 MiB base64 per full frame vs ~170 KiB under `Best`) and are
/// losslessly re-encoded once on the next compaction so resumed sessions shrink
/// below gateway body limits without dropping archive history.
const RECOMPRESS_FRAME_BASE64_BYTES: usize = 512 * 1024;
/// Verbatim text kept at each chronological edge, in characters (~3k tokens per
/// edge), preserving formatting and indentation on the oldest and newest turns.
const TEXT_EDGE_CHARS: usize = 12_000;
const TOOL_RESULT_MAX_CHARS: usize = 2_000;
const TOOL_ARG_MAX_CHARS: usize = 500;
const TOOL_CALL_MAX_CHARS: usize = 2_000;
const THINKING_MAX_CHARS: usize = 1_000;
const TRUNCATE_HEAD_NUM: usize = 3;
const TRUNCATE_HEAD_DEN: usize = 5;
/// Zero-width ink toggles: shift-out/shift-in wrap dim tool-output spans.
const DIM_ON: char = '\u{0e}';
const DIM_OFF: char = '\u{0f}';
/// FULL BLOCK fills its whole cell with pitch-black ink; newline runs fold to
/// it so line structure survives whitespace collapse at a one-cell cost.
const NEWLINE_GLYPH: char = '\u{2588}';
const INK_BLACK: u8 = 0;
const INK_DIM: u8 = 128;
const BG: u8 = 255;

const ARCHIVE_HEADER: &str = concat!(
    "[Compacted session history archive]\n",
    "Legend: ¶user = user message, ¶think = reasoning, ¶ai = assistant reply, ",
    "¶call = tool call with <out>…</out> output. ",
    "In imaged frames, solid black blocks (█) mark line breaks.\n\n",
);
const ARCHIVE_ACK: &str =
    "Understood. I have reviewed the archived session history above and am ready to continue.";

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
    static FONT: OnceLock<Font> = OnceLock::new();
    FONT.get_or_init(|| parse_bdf(FONT_8X13, 8, 13))
}

/// Grid geometry shared with the pagination math: columns across the frame,
/// rows down it, and the cell budget one frame holds.
pub(crate) fn geometry() -> (usize, usize, usize) {
    let cols = FRAME_SIZE / font().cell_w;
    let rows = FRAME_SIZE / font().cell_h;
    (cols, rows, cols * rows)
}

/// Scales `MAX_FRAMES` down on small context windows so the archive itself
/// cannot exceed a quarter of usable context and immediately re-trigger
/// auto-compaction on the next turn.
pub(crate) fn frame_budget(usable_tokens: u32, max_images: usize) -> usize {
    let by_window = (usable_tokens / MAX_WINDOW_FRACTION / TOKENS_PER_FRAME) as usize;
    let ceiling = max_images.min(MAX_FRAMES);
    if ceiling == 0 {
        return 0;
    }
    by_window.clamp(1, ceiling)
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

fn char_byte_offset(text: &str, char_pos: usize) -> usize {
    text.char_indices()
        .nth(char_pos)
        .map_or(text.len(), |(idx, _)| idx)
}

fn truncate_for_summary(text: &str, max_chars: usize) -> String {
    if text.len() <= max_chars {
        return text.to_string();
    }
    let total_chars = text.chars().count();
    if total_chars <= max_chars {
        return text.to_string();
    }
    let head = max_chars * TRUNCATE_HEAD_NUM / TRUNCATE_HEAD_DEN;
    let tail = max_chars - head;
    let head_end = char_byte_offset(text, head);
    let tail_start = char_byte_offset(text, total_chars - tail);
    let mut out = String::with_capacity(head_end + (text.len() - tail_start) + '…'.len_utf8());
    out.push_str(&text[..head_end]);
    out.push('…');
    out.push_str(&text[tail_start..]);
    out
}

fn strip_dim_markers(text: &str) -> String {
    if !text.contains([DIM_ON, DIM_OFF]) {
        return text.to_string();
    }
    text.chars()
        .filter(|&c| c != DIM_ON && c != DIM_OFF)
        .collect()
}

/// Serialize the discarded history into conversation-dense text. Tool results
/// merge into their originating call, truncated head+tail and wrapped in a dim
/// span so archived conversation reads louder than archived tool noise.
///
/// Prior snapcompact archives keep their existing PNG frames via [`extract_prior_frames`]
/// rather than turning them into `[image]` placeholders, so their framing headers
/// are stripped if re-serialized.
pub fn serialize(messages: &[Message]) -> String {
    let results: HashMap<&str, (&str, bool)> = messages
        .iter()
        .flat_map(|msg| msg.content.iter())
        .filter_map(|block| match block {
            ContentBlock::ToolResult {
                tool_use_id,
                content,
                is_error,
                ..
            } => Some((tool_use_id.as_str(), (content.as_str(), *is_error))),
            _ => None,
        })
        .collect();

    let mut parts: Vec<String> = Vec::new();
    let mut merged: HashSet<&str> = HashSet::with_capacity(results.len());

    for msg in messages {
        if msg.is_context_update() {
            continue;
        }
        match msg.role {
            Role::User => {
                for block in &msg.content {
                    match block {
                        ContentBlock::Text { text } => {
                            let cleaned = strip_archive_chrome(&strip_dim_markers(text));
                            let trimmed = cleaned.trim();
                            if !trimmed.is_empty() {
                                if trimmed.starts_with("¶user:")
                                    || trimmed.starts_with("¶ai:")
                                    || trimmed.starts_with("¶call:")
                                    || trimmed.starts_with("¶think:")
                                {
                                    parts.push(trimmed.to_string());
                                } else {
                                    parts.push(format!("¶user: {trimmed}"));
                                }
                            }
                        }
                        ContentBlock::ToolResult {
                            tool_use_id,
                            content,
                            is_error,
                            ..
                        } => {
                            if merged.contains(tool_use_id.as_str()) {
                                continue;
                            }
                            parts.push(format!(
                                "¶call:\n{}",
                                render_result_block(content, *is_error)
                            ));
                        }
                        _ => {}
                    }
                }
            }
            Role::Assistant => {
                if msg.content.len() == 1
                    && matches!(&msg.content[0], ContentBlock::Text { text } if text == ARCHIVE_ACK)
                {
                    continue;
                }
                let mut pending_thinking: Vec<String> = Vec::new();
                let mut pending_text: Vec<String> = Vec::new();
                let flush = |parts: &mut Vec<String>,
                             thinking: &mut Vec<String>,
                             text: &mut Vec<String>| {
                    if !thinking.is_empty() {
                        let combined =
                            truncate_for_summary(&thinking.join("\n"), THINKING_MAX_CHARS);
                        parts.push(format!("¶think: {DIM_ON}{combined}{DIM_OFF}"));
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
                            merged.insert(id.as_str());
                            let args_str = format_tool_input(input);
                            let args = truncate_for_summary(&args_str, TOOL_CALL_MAX_CHARS);
                            let mut line = format!("¶call: {name}({args})");
                            if let Some(&(result, is_error)) = results.get(id.as_str()) {
                                line.push('\n');
                                line.push_str(&render_result_block(result, is_error));
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

fn format_tool_input(input: &serde_json::Value) -> String {
    match input.as_object() {
        Some(map) => map
            .iter()
            .map(|(key, value)| {
                let rendered = match value.as_str() {
                    Some(s) => truncate_for_summary(s, TOOL_ARG_MAX_CHARS),
                    None => truncate_for_summary(&value.to_string(), TOOL_ARG_MAX_CHARS),
                };
                format!("{key}={rendered}")
            })
            .collect::<Vec<_>>()
            .join(", "),
        None if input.is_null() => String::new(),
        None => truncate_for_summary(&input.to_string(), TOOL_ARG_MAX_CHARS),
    }
}

fn render_result_block(raw: &str, is_error: bool) -> String {
    let body = truncate_for_summary(&strip_dim_markers(raw), TOOL_RESULT_MAX_CHARS);
    let tag = if is_error { "err" } else { "out" };
    format!("<{tag}>\n{DIM_ON}{body}{DIM_OFF}\n</{tag}>")
}

fn strip_archive_chrome(text: &str) -> String {
    let text = text.strip_prefix(ARCHIVE_HEADER).unwrap_or(text);
    let mut lines: Vec<&str> = Vec::new();
    for line in text.lines() {
        if line.starts_with("-------------- imaged middle ")
            || line.starts_with("-------------- middle history ")
            || line.starts_with("-------------- compaction notes")
        {
            continue;
        }
        lines.push(line);
    }
    lines.join("\n")
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

/// Prepare text for bitmap printing: collapse horizontal whitespace runs, fold
/// single or repeated newlines to one full-block glyph, fold unsupported
/// symbols to ASCII, and drop unrenderable glyphs.
pub fn normalize(text: &str) -> String {
    let font = font();
    let mut out = String::with_capacity(text.len());
    let mut space = false;
    let mut newline = false;
    for ch in text.chars() {
        let cp = ch as u32;
        if ch == DIM_ON || ch == DIM_OFF {
            out.push(ch);
            continue;
        }
        if ch == '\n' || ch == '\r' {
            newline = true;
            space = false;
            continue;
        }
        if ch.is_whitespace() {
            if !newline {
                space = true;
            }
            continue;
        }
        if let Some(folded) = fold_char(cp, ch, font) {
            if newline {
                if !out.is_empty() {
                    out.push(NEWLINE_GLYPH);
                }
                newline = false;
            } else if space && !out.is_empty() {
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
/// right edge. Tracks active dim spans across page boundaries so each page
/// renders independently with accurate ink.
fn paginate(text: &str, capacity: usize, cols: usize) -> Vec<String> {
    let mut pages: Vec<String> = Vec::new();
    let mut start = 0usize;
    let mut cell = 0usize;
    let mut has_cell = false;
    let mut dim_active = false;
    let mut page_starts_dim = false;

    for (byte_idx, ch) in text.char_indices() {
        match ch {
            DIM_ON => {
                dim_active = true;
                continue;
            }
            DIM_OFF => {
                dim_active = false;
                continue;
            }
            _ => {}
        }
        let w = cell_units(ch as u32);
        if w == 0 {
            continue;
        }
        let mut at = cell;
        if w == 2 && cols >= 2 && at % cols == cols - 1 {
            at += 1;
        }
        if has_cell && at + w > capacity {
            let mut page = String::with_capacity(byte_idx - start + 2);
            if page_starts_dim {
                page.push(DIM_ON);
            }
            page.push_str(&text[start..byte_idx]);
            pages.push(page);
            start = byte_idx;
            page_starts_dim = dim_active;
            at = 0;
        }
        cell = at + w;
        has_cell = true;
    }
    if has_cell {
        let mut page = String::with_capacity(text.len() - start + 1);
        if page_starts_dim {
            page.push(DIM_ON);
        }
        page.push_str(&text[start..]);
        pages.push(page);
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
/// pixel font, then encode it once as base64 grayscale PNG.
pub fn render_page(text: &str) -> Arc<str> {
    let font = font();
    let (cols, rows, capacity) = geometry();
    let height = used_rows(text, cols, rows) * font.cell_h;
    let width = FRAME_SIZE;
    let mut pixels = vec![BG; width * height];

    let mut dim = false;
    let mut cursor = 0usize;
    for ch in text.chars() {
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

    encode_luma8_png(&pixels, width as u32, height as u32)
}

fn encode_luma8_png(pixels: &[u8], width: u32, height: u32) -> Arc<str> {
    let mut png_bytes = Vec::new();
    PngEncoder::new_with_quality(&mut png_bytes, CompressionType::Best, PngFilter::NoFilter)
        .write_image(pixels, width, height, ExtendedColorType::L8)
        .expect("encode grayscale PNG");
    Arc::from(STANDARD.encode(png_bytes))
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

/// Split serialized (un-normalized, indentation-preserving) text into a
/// verbatim oldest edge, a raw middle slice to normalize + rasterize, and a
/// verbatim newest edge.
fn split_edges(serialized: &str, edge_chars: usize) -> (&str, &str, &str) {
    if edge_chars == 0 {
        return ("", serialized, "");
    }
    let total_chars = serialized.chars().count();
    if total_chars <= 2 * edge_chars {
        return (serialized, "", "");
    }
    let head_end = char_byte_offset(serialized, edge_chars);
    let tail_start = char_byte_offset(serialized, total_chars - edge_chars);
    (
        &serialized[..head_end],
        &serialized[head_end..tail_start],
        &serialized[tail_start..],
    )
}

/// Lay out the middle slice onto at most `max_frames` pages, dropping the
/// oldest pages first when the archive exceeds the frame budget.
fn plan_middle(middle_raw: &str, max_frames: usize) -> (Vec<String>, usize) {
    if middle_raw.is_empty() {
        return (Vec::new(), 0);
    }
    let normalized = normalize(middle_raw);
    if normalized.is_empty() {
        return (Vec::new(), 0);
    }
    if max_frames == 0 {
        let dropped = strip_dim_markers(&normalized).chars().count();
        return (Vec::new(), dropped);
    }
    let (cols, _rows, capacity) = geometry();
    let mut pages = paginate(&normalized, capacity, cols);
    let mut truncated = 0usize;
    if pages.len() > max_frames {
        for dropped in pages.drain(..pages.len() - max_frames) {
            truncated += strip_dim_markers(&dropped).chars().count();
        }
    }
    (pages, truncated)
}

/// Extract snapcompact PNG frames carried by an earlier archive turn in
/// `messages`, removing them so user/tool images can still be replaced with
/// `[image]` placeholders without destroying prior archive frames. Frames
/// written by older `CompressionType::Fast` builds are losslessly re-encoded
/// under `CompressionType::Best` so resumed sessions heal on their next compact.
fn extract_prior_frames(messages: &mut [Message]) -> Vec<ImageSource> {
    let mut frames = Vec::new();
    for msg in messages {
        let is_archive = msg.role == Role::User
            && msg.display_text.as_deref() == Some("")
            && msg.content.first().is_some_and(
                |b| matches!(b, ContentBlock::Text { text } if text.starts_with(ARCHIVE_HEADER)),
            );
        if !is_archive {
            continue;
        }
        for block in &msg.content {
            if let ContentBlock::Image { source } = block {
                frames.push(recompress_prior_frame(source));
            }
        }
        msg.content
            .retain(|block| !matches!(block, ContentBlock::Image { .. }));
    }
    frames
}

fn recompress_prior_frame(source: &ImageSource) -> ImageSource {
    if source.data.len() <= RECOMPRESS_FRAME_BASE64_BYTES {
        return source.clone();
    }
    let Ok(bytes) = STANDARD.decode(source.data.as_bytes()) else {
        return source.clone();
    };
    let Ok(decoded) = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png) else {
        return source.clone();
    };
    let gray = decoded.into_luma8();
    let reencoded = encode_luma8_png(gray.as_raw(), gray.width(), gray.height());
    if reencoded.len() < source.data.len() {
        ImageSource::new(ImageMediaType::Png, reencoded)
    } else {
        source.clone()
    }
}

pub struct SnapResult {
    pub blocks: Vec<ContentBlock>,
    pub summary: String,
}

/// Run a snapcompact pass over the messages being discarded. Fully local:
/// preserves prior archive frames, serializes text/thinking/tool rounds, keeps
/// indentation-intact text edges, and renders the middle to 1568px-max PNG
/// frames.
#[cfg(test)]
pub fn compact(messages: &[Message]) -> SnapResult {
    compact_with_options(messages, MAX_FRAMES, None)
}

pub fn compact_with_options(
    messages: &[Message],
    max_frames: usize,
    instructions: Option<&str>,
) -> SnapResult {
    let mut working = messages.to_vec();
    let mut prior_frames = extract_prior_frames(&mut working);
    replace_non_archive_images(&mut working);

    let serialized = serialize(&working);
    let (text_head, middle_raw, text_tail) = split_edges(&serialized, TEXT_EDGE_CHARS);
    let (new_pages, mut truncated_chars) = plan_middle(middle_raw, max_frames);

    let mut kept_new_pages = new_pages;
    let total_frames = prior_frames.len() + kept_new_pages.len();
    if total_frames > max_frames {
        let mut excess = total_frames - max_frames;
        let drop_prior = excess.min(prior_frames.len());
        prior_frames.drain(..drop_prior);
        excess -= drop_prior;
        if excess > 0 && excess <= kept_new_pages.len() {
            for dropped in kept_new_pages.drain(..excess) {
                truncated_chars += strip_dim_markers(&dropped).chars().count();
            }
        }
    }

    let mut rendered_new: Vec<(ImageSource, usize)> = kept_new_pages
        .iter()
        .map(|page| {
            let chars = strip_dim_markers(page).chars().count();
            (
                ImageSource::new(ImageMediaType::Png, render_page(page)),
                chars,
            )
        })
        .collect();

    let mut total_b64_bytes: usize = prior_frames
        .iter()
        .map(|src| src.data.len())
        .chain(rendered_new.iter().map(|(src, _)| src.data.len()))
        .sum();
    while total_b64_bytes > MAX_ARCHIVE_BASE64_BYTES && !prior_frames.is_empty() {
        let dropped = prior_frames.remove(0);
        total_b64_bytes = total_b64_bytes.saturating_sub(dropped.data.len());
    }
    while total_b64_bytes > MAX_ARCHIVE_BASE64_BYTES && rendered_new.len() > 1 {
        let (dropped, chars) = rendered_new.remove(0);
        total_b64_bytes = total_b64_bytes.saturating_sub(dropped.data.len());
        truncated_chars += chars;
    }

    let rendered_frames: Vec<ImageSource> = prior_frames
        .into_iter()
        .chain(rendered_new.into_iter().map(|(src, _)| src))
        .collect();
    let has_frames = !rendered_frames.is_empty();
    let mut blocks: Vec<ContentBlock> = Vec::new();

    let mut head = String::from(ARCHIVE_HEADER);
    if !text_head.is_empty() {
        head.push_str(&plain_text(text_head));
        if has_frames {
            head.push_str("\n-------------- imaged middle below\n");
        }
    }
    blocks.push(ContentBlock::Text { text: head });

    for source in rendered_frames {
        blocks.push(ContentBlock::Image { source });
    }

    let mut tail = String::new();
    if !text_tail.is_empty() {
        let divider = if has_frames {
            "-------------- imaged middle above\n"
        } else {
            "-------------- middle history omitted above\n"
        };
        tail.push_str(divider);
        tail.push_str(&plain_text(text_tail));
    }
    if let Some(notes) = instructions.map(str::trim).filter(|s| !s.is_empty()) {
        if !tail.is_empty() {
            tail.push_str("\n\n");
        }
        tail.push_str("-------------- compaction notes\n");
        tail.push_str(notes);
    }
    if !tail.is_empty() {
        match blocks.last_mut() {
            Some(ContentBlock::Text { text }) => {
                if !text.ends_with('\n') {
                    text.push('\n');
                }
                text.push_str(&tail);
            }
            _ => blocks.push(ContentBlock::Text { text: tail }),
        }
    }

    let frame_count = blocks
        .iter()
        .filter(|b| matches!(b, ContentBlock::Image { .. }))
        .count();
    let archived_chars = serialized.chars().count();
    let mut summary = format!(
        "Archived {archived_chars} chars of history onto {frame_count} snapcompact frame{}.",
        if frame_count == 1 { "" } else { "s" }
    );
    if truncated_chars > 0 {
        summary.push_str(&format!(
            " {truncated_chars} chars of the oldest middle were dropped to fit the frame budget."
        ));
    }
    SnapResult { blocks, summary }
}

/// Replaces `history` with a bitmap archive of the discarded transcript on the
/// blocking pool, with no model call. Uses a hidden `Role::User` archive turn
/// and a `Role::Assistant` acknowledgement so every provider wire codec
/// forwards the PNG frames cleanly, and preserves monotonic timestamps and
/// `post_compaction_instructions` just like `rebuild_history`.
pub async fn history(
    history: &mut History,
    compaction_history: Vec<Message>,
    summarized: usize,
    max_frames: usize,
    instructions: Option<&str>,
    post_compaction_instructions: Option<&str>,
    compact_start: Instant,
) -> Result<Compacted, AgentError> {
    let notes = instructions.map(str::to_owned);
    let SnapResult { blocks, summary } = blocking(move || {
        Ok::<_, String>(compact_with_options(
            &compaction_history,
            max_frames,
            notes.as_deref(),
        ))
    })
    .await
    .map_err(|message| AgentError::Config { message })?;

    let carried_at = history
        .as_slice()
        .get(summarized)
        .and_then(|m| m.created_at);
    let stamp = |msg: Message| {
        stamp_new(Message {
            created_at: carried_at,
            ..msg
        })
    };
    let archive_user = stamp(Message {
        role: Role::User,
        content: blocks,
        display_text: Some(String::new()),
        ..Default::default()
    });
    let archive_ack = stamp(Message {
        role: Role::Assistant,
        content: vec![ContentBlock::Text {
            text: ARCHIVE_ACK.into(),
        }],
        ..Default::default()
    });

    let mut new_history = vec![archive_user, archive_ack];
    if let Some(post) = post_compaction_instructions
        .map(str::trim)
        .filter(|t| !t.is_empty())
    {
        new_history.push(stamp(Message::synthetic(post.to_owned())));
    }
    let summary_len = new_history.len();
    new_history.extend_from_slice(&history.as_slice()[summarized..]);
    history.restart(new_history);
    let duration = compact_start.elapsed();
    info!(
        duration_ms = duration.as_millis() as u64,
        "snapcompact completed"
    );

    Ok(Compacted {
        summary,
        duration,
        carried: history.len() - summary_len,
    })
}

fn plain_text(text: &str) -> String {
    text.chars()
        .map(|ch| if ch == NEWLINE_GLYPH { '\n' } else { ch })
        .filter(|&ch| ch != DIM_ON && ch != DIM_OFF)
        .collect()
}

fn replace_non_archive_images(messages: &mut [Message]) {
    for msg in messages {
        for block in &mut msg.content {
            if matches!(block, ContentBlock::Image { .. }) {
                *block = ContentBlock::Text {
                    text: maki_providers::IMAGE_PLACEHOLDER.into(),
                };
            }
        }
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
    fn split_edges_preserves_indentation_and_middle() {
        let indented = "    def foo():\n        return 42\n";
        let filler = "m".repeat(TEXT_EDGE_CHARS * 3);
        let serialized = format!("{indented}{filler}{indented}");
        let (head, middle, tail) = split_edges(&serialized, TEXT_EDGE_CHARS);
        assert!(head.starts_with(indented));
        assert!(tail.ends_with(indented));
        assert!(!middle.is_empty());
    }

    #[test]
    fn plan_middle_drops_oldest_pages_past_max_frames() {
        let (_cols, _rows, capacity) = geometry();
        let text = "z".repeat(capacity * 10);
        let (pages, truncated) = plan_middle(&text, 4);
        assert_eq!(pages.len(), 4);
        assert!(truncated > 0);
    }

    #[test]
    fn paginate_carries_dim_state_across_page_boundary() {
        let (cols, _rows, capacity) = geometry();
        let text = format!("{DIM_ON}{}{DIM_OFF}", "d".repeat(capacity + 20));
        let pages = paginate(&text, capacity, cols);
        assert_eq!(pages.len(), 2);
        assert!(pages[1].starts_with(DIM_ON));
    }

    #[test]
    fn serialize_preserves_thinking_and_marks_errors() {
        let messages = vec![
            Message::user("diagnose crash".into()),
            Message {
                role: Role::Assistant,
                content: vec![
                    ContentBlock::Thinking {
                        thinking: "root cause is null pointer in foo.rs".into(),
                        signature: Some("sig".into()),
                        reasoning_id: None,
                        reasoning_opaque: None,
                    },
                    ContentBlock::tool_use("t_err", "bash", json!({"command": "cargo test"})),
                ],
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: vec![ContentBlock::tool_result(
                    "t_err",
                    "panicked at foo.rs:12",
                    true,
                )],
                ..Default::default()
            },
        ];
        let text = serialize(&messages);
        assert!(text.contains("¶think:"));
        assert!(text.contains("root cause is null pointer in foo.rs"));
        assert!(text.contains("<err>"));
        assert!(text.contains("panicked at foo.rs:12"));
    }

    #[test]
    fn render_page_produces_decodable_png_within_provider_edge_limit() {
        let data = render_page("Hello world. Again.");
        let bytes = STANDARD.decode(data.as_bytes()).unwrap();
        assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
        let img = image::load_from_memory(&bytes).unwrap();
        assert!(img.width() <= 1568);
        assert!(img.height() <= 1568);
    }

    #[tokio::test]
    async fn compact_frames_pass_adapt_images_for_model_untouched() {
        let mut messages = vec![Message::user("first task".into())];
        for i in 0..40 {
            let id = format!("t{i}");
            messages.push(Message {
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use(
                    &id,
                    "bash",
                    json!({"command": "x".repeat(600)}),
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
        let frame_count = result
            .blocks
            .iter()
            .filter(|b| matches!(b, ContentBlock::Image { .. }))
            .count();
        assert!(frame_count > 0);
        assert!(result.summary.contains("snapcompact frame"));

        for block in &result.blocks {
            if let ContentBlock::Image { source } = block {
                let bytes = STANDARD
                    .decode(source.data.as_bytes())
                    .expect("single base64 layer");
                assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
            }
        }

        let mut vision_model = Model::from_spec("anthropic/claude-sonnet-4-20250514").unwrap();
        vision_model.supports_vision_override = Some(true);
        let wire_input = vec![Message {
            role: Role::User,
            content: result.blocks,
            display_text: Some(String::new()),
            ..Default::default()
        }];
        let adapted = maki_providers::adapt_images_for_model(&vision_model, &wire_input).await;
        assert!(
            matches!(adapted, std::borrow::Cow::Borrowed(_)),
            "wire-ready 1568px PNG frames must not be dropped or re-encoded"
        );
        let adapted_images = adapted[0]
            .content
            .iter()
            .filter(|b| matches!(b, ContentBlock::Image { .. }))
            .count();
        assert_eq!(adapted_images, frame_count);
    }

    #[test]
    fn second_compaction_retains_prior_archive_frames() {
        let mut messages = vec![Message::user("phase one".into())];
        for i in 0..30 {
            let id = format!("p1_{i}");
            messages.push(Message {
                role: Role::Assistant,
                content: vec![ContentBlock::tool_use(
                    &id,
                    "read",
                    json!({"path": format!("src/file_{i}.rs")}),
                )],
                ..Default::default()
            });
            messages.push(Message {
                role: Role::User,
                content: vec![ContentBlock::tool_result(&id, "a".repeat(1500), false)],
                ..Default::default()
            });
        }
        let first = compact(&messages);
        let first_frames = first
            .blocks
            .iter()
            .filter(|b| matches!(b, ContentBlock::Image { .. }))
            .count();
        assert!(first_frames > 0);

        let mut second_input = vec![
            Message {
                role: Role::User,
                content: first.blocks,
                display_text: Some(String::new()),
                ..Default::default()
            },
            Message {
                role: Role::Assistant,
                content: vec![ContentBlock::Text {
                    text: ARCHIVE_ACK.into(),
                }],
                ..Default::default()
            },
            Message::user("phase two short turn".into()),
        ];
        second_input.push(Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: "done phase two".into(),
            }],
            ..Default::default()
        });

        let second = compact(&second_input);
        let second_frames = second
            .blocks
            .iter()
            .filter(|b| matches!(b, ContentBlock::Image { .. }))
            .count();
        assert!(
            second_frames >= first_frames,
            "prior snapcompact frames must survive a second compaction instead of collapsing to [image]"
        );
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

    const MAX_DENSE_FRAME_BASE64_BYTES: usize = 350 * 1024;

    #[test]
    fn full_page_compresses_compactly_and_recompresses_bloated_legacy_frames() {
        let (cols, rows, _) = geometry();
        let line: String = (0..cols - 1)
            .map(|c| char::from(b'!' + (c % 90) as u8))
            .collect();
        let dense_page: String = (0..rows)
            .map(|_| line.as_str())
            .collect::<Vec<_>>()
            .join(&NEWLINE_GLYPH.to_string());

        let compressed_b64 = render_page(&dense_page);
        assert!(
            compressed_b64.len() <= MAX_DENSE_FRAME_BASE64_BYTES,
            "a full dense 1568px frame must stay compact in base64 (got {} bytes)",
            compressed_b64.len()
        );

        let decoded = image::load_from_memory(&STANDARD.decode(compressed_b64.as_bytes()).unwrap())
            .unwrap()
            .into_luma8();
        let mut bloated_png = Vec::new();
        PngEncoder::new_with_quality(&mut bloated_png, CompressionType::Fast, PngFilter::NoFilter)
            .write_image(
                decoded.as_raw(),
                decoded.width(),
                decoded.height(),
                ExtendedColorType::L8,
            )
            .unwrap();
        let bloated_source =
            ImageSource::new(ImageMediaType::Png, Arc::from(STANDARD.encode(bloated_png)));
        assert!(bloated_source.data.len() > RECOMPRESS_FRAME_BASE64_BYTES);

        let healed = recompress_prior_frame(&bloated_source);
        assert_eq!(healed.data.as_ref(), compressed_b64.as_ref());
    }

    #[test]
    fn archive_enforces_total_base64_byte_budget() {
        let incompressible =
            ImageSource::new(ImageMediaType::Png, Arc::from("A".repeat(1_200_000)));
        let archive_turn = Message {
            role: Role::User,
            content: vec![
                ContentBlock::Text {
                    text: format!("{ARCHIVE_HEADER}¶user: old"),
                },
                ContentBlock::Image {
                    source: incompressible.clone(),
                },
                ContentBlock::Image {
                    source: incompressible.clone(),
                },
                ContentBlock::Image {
                    source: incompressible,
                },
            ],
            display_text: Some(String::new()),
            ..Default::default()
        };
        let ack_turn = Message {
            role: Role::Assistant,
            content: vec![ContentBlock::Text {
                text: ARCHIVE_ACK.into(),
            }],
            ..Default::default()
        };
        let result = compact_with_options(&[archive_turn, ack_turn], MAX_FRAMES, None);
        let total_b64: usize = result
            .blocks
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Image { source } => Some(source.data.len()),
                _ => None,
            })
            .sum();
        assert!(total_b64 <= MAX_ARCHIVE_BASE64_BYTES);
        assert!(total_b64 > 0);
    }

    #[test]
    fn frame_budget_respects_model_max_images_cap() {
        let gemini = Model::from_spec("copilot/gemini-4-carbon").unwrap();
        assert_eq!(gemini.max_images(), 10);
        assert_eq!(frame_budget(264_000, gemini.max_images()), 10);
    }
}
