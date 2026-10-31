use std::sync::Arc;

use maki_lua_macro::{lua_class, lua_fn, lua_table};
use mlua::{Lua, LuaString, Result as LuaResult, Table, UserDataRef};
use regex::{Regex, RegexBuilder};

use super::util::pair::{Pair, err_pair, pair, try_pair};

const MAX_PATTERN_BYTES: usize = 4096;
const MAX_TEXT_BYTES: usize = 2 * 1024 * 1024;
const MAX_MATCHES: usize = 8192;
const REGEX_SIZE_LIMIT: usize = 1024 * 1024;

pub(crate) struct LuaRegex {
    inner: Arc<Regex>,
}

/// Compile a Rust regular expression off the Lua thread. Patterns are limited
/// to 4096 bytes and compiled programs to 1 MiB. Uses Rust regex syntax, not
/// Vim or Lua patterns; invalid patterns return an error before use.
///
/// @param pattern string Rust regular expression.
/// @return (maki.Regex?, string?) Compiled expression, or nil plus an error.
/// @example
/// local re, err = maki.regex("\\\\bunsafe\\\\b")
/// if not re then return end
/// local first, last = re:match_str("unsafe fn run() {}")
#[lua_fn]
async fn regex(_lua: Lua, pattern: String) -> LuaResult<Pair<LuaRegex>> {
    if pattern.len() > MAX_PATTERN_BYTES {
        return Ok(err_pair("regex pattern exceeds 4096-byte limit"));
    }
    Ok(pair(
        smol::unblock(move || {
            RegexBuilder::new(&pattern)
                .size_limit(REGEX_SIZE_LIMIT)
                .dfa_size_limit(REGEX_SIZE_LIMIT)
                .build()
                .map(|inner| LuaRegex {
                    inner: Arc::new(inner),
                })
        })
        .await,
    ))
}

/// Find the first match. Offsets are zero-based bytes; the end is exclusive.
/// Text is limited to 2 MiB.
///
/// @param text string Text to search.
/// @return (integer?, integer?) Start and end offsets, or nil, nil when absent.
/// @example
/// local first, last = re:match_str("unsafe fn run() {}")
#[lua_fn]
fn match_str(
    _lua: &Lua,
    this: &LuaRegex,
    text: LuaString,
) -> LuaResult<(Option<usize>, Option<usize>)> {
    let text = text.to_str()?;
    if text.len() > MAX_TEXT_BYTES {
        return Err(mlua::Error::runtime("regex text exceeds 2-MiB limit"));
    }
    Ok(this
        .inner
        .find(&text)
        .map_or((None, None), |m| (Some(m.start()), Some(m.end()))))
}

/// Find all non-overlapping matches off the Lua thread. At most 8192 matches
/// and 2 MiB of text are accepted; exceeding either limit returns an error.
///
/// @param text string Text to search.
/// @return (table?, string?) Array of zero-based, end-exclusive `{start, end}` byte ranges, or nil plus an error.
/// @example
/// local ranges, err = re:ranges(source)
#[lua_fn]
async fn ranges(lua: Lua, this: UserDataRef<LuaRegex>, text: String) -> LuaResult<Pair<Table>> {
    let re = Arc::clone(&this.inner);
    drop(this);
    if text.len() > MAX_TEXT_BYTES {
        return Ok(err_pair("regex text exceeds 2-MiB limit"));
    }
    let matches = try_pair!(
        smol::unblock(move || {
            let mut matches = Vec::new();
            for m in re.find_iter(&text) {
                if matches.len() == MAX_MATCHES {
                    return Err("regex match count exceeds 8192 limit");
                }
                matches.push([m.start(), m.end()]);
            }
            Ok(matches)
        })
        .await
    );
    let out = lua.create_table_with_capacity(matches.len(), 0)?;
    for range in matches {
        out.push(lua.create_sequence_from(range)?)?;
    }
    Ok((Some(out), None))
}

lua_class! {
    /// A compiled Rust regular expression returned by `maki.regex`.
    "maki.Regex" => LuaRegex, REGEX_DOCS [match_str, ranges]
}

lua_table! {
    extend "maki" => pub(crate) fn add_regex(), DOCS [regex]
}
