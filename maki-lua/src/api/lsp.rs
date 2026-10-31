use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use maki_lua_macro::{lua_fn, lua_table};
use mlua::{Lua, Result as LuaResult, Table, Value};

use crate::api::fs::expand_tilde;
use crate::api::util::convert::{TableExt, json_to_lua, reject_unknown_fields};
use crate::api::util::pair::{Pair, err_pair, try_pair};
use crate::lsp::{LspManager, Speculation};
use crate::plugin_permissions::PluginPermissions;

const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const SPECULATION_FIELDS: &[&str] = &[
    "file_path",
    "root",
    "language_id",
    "command",
    "start_line",
    "end_line",
    "replacement_text",
    "timeout",
];

fn infer_language(path: &Path) -> Option<&'static str> {
    match path.extension()?.to_str()? {
        "rs" => Some("rust"),
        "py" | "pyi" => Some("python"),
        "js" | "mjs" | "cjs" => Some("javascript"),
        "jsx" => Some("javascriptreact"),
        "ts" | "mts" | "cts" => Some("typescript"),
        "tsx" => Some("typescriptreact"),
        "go" => Some("go"),
        _ => None,
    }
}

fn default_command(language_id: &str) -> Option<Vec<String>> {
    let command: &[&str] = match language_id {
        "rust" => &["rust-analyzer"],
        "python" => &["pyright-langserver", "--stdio"],
        "javascript" | "javascriptreact" | "typescript" | "typescriptreact" => {
            &["typescript-language-server", "--stdio"]
        }
        "go" => &["gopls"],
        _ => return None,
    };
    Some(command.iter().map(|part| (*part).to_owned()).collect())
}

fn parse_command(value: Value) -> LuaResult<Result<Option<Vec<String>>, String>> {
    let command = match value {
        value if value.is_null() => return Ok(Ok(None)),
        Value::String(command) => match shell_words::split(&command.to_str()?) {
            Ok(command) => command,
            Err(err) => return Ok(Err(format!("invalid LSP command: {err}"))),
        },
        Value::Table(command) => command
            .sequence_values::<String>()
            .collect::<LuaResult<Vec<_>>>()?,
        _ => {
            return Err(mlua::Error::runtime(
                "command must be an argv array or a string",
            ));
        }
    };
    if command
        .first()
        .is_none_or(|program| program.trim().is_empty())
    {
        return Ok(Err("LSP command must contain an executable".to_owned()));
    }
    if command.iter().any(|arg| arg.contains('\0')) {
        return Ok(Err("LSP command must not contain NUL bytes".to_owned()));
    }
    Ok(Ok(Some(command)))
}

fn resolve_paths(file_path: String, root: Option<String>) -> Result<(PathBuf, PathBuf), String> {
    let file_path = expand_tilde(&file_path)
        .canonicalize()
        .map_err(|err| format!("cannot resolve file_path: {err}"))?;
    if !file_path.is_file() {
        return Err("file_path must be a regular file".to_owned());
    }
    if file_path.to_str().is_none() {
        return Err("file_path is not UTF-8".to_owned());
    }
    let directory = file_path
        .parent()
        .ok_or_else(|| "file_path has no parent directory".to_owned())?;
    let root = match root {
        Some(root) => expand_tilde(&root)
            .canonicalize()
            .map_err(|err| format!("cannot resolve root: {err}"))?,
        None => directory
            .ancestors()
            .find(|ancestor| ancestor.join(".git").exists())
            .unwrap_or(directory)
            .to_owned(),
    };
    if !root.is_dir() {
        return Err("root must be a directory".to_owned());
    }
    if root.to_str().is_none() {
        return Err("root is not UTF-8".to_owned());
    }
    Ok((file_path, root))
}

/// Ask a language server to diagnose a replacement without writing the file.
/// The inclusive, 1-based range is replaced only in the server's open document.
/// The disk source is restored and the document closed after every trial.
/// Concurrent trials sharing a server are serialized.
///
/// Unrelated baseline diagnostics are excluded. Errors and warnings touching the
/// replacement or newly reported elsewhere in the workspace are retained. Messages are the
/// original language-server messages, not model judgments. A timeout, unsupported
/// server, or failed request returns an error, never a clean result.
///
/// @param opts table Options. Fields:
///   - file_path (string): existing file, relative to cwd or absolute; ~/ is supported.
///   - start_line (integer): first line to replace, inclusive and 1-based.
///   - end_line (integer): last line to replace, inclusive and 1-based.
///   - replacement_text (string): candidate text; an empty string removes the range.
///   - root (string?): workspace directory; defaults to the nearest .git ancestor or file directory.
///   - language_id (string?): LSP language id; inferred for Rust, Python, JS/TS, and Go.
///   - command (table|string?): argv array or shell-words-quoted command; executed directly, never through a shell.
///   - timeout (integer?): total deadline in milliseconds, default 30000.
/// @return (table?, string?) `{ file_path, diagnostics, clean }` or `(nil, err)`.
///   Diagnostics contain file_path, 1-based line and end_line, severity, message,
///   and optional source and code. clean means no retained errors or warnings.
/// @example
/// local result, err = maki.lsp.speculate({
///   file_path = "src/main.rs", start_line = 3, end_line = 3,
///   replacement_text = "let answer: i32 = 42;",
/// })
/// if not result then error(err) end
/// print(maki.json.encode(result))
/// @example
/// -- Inside the code_execution tool, try candidates without intermediate writes:
/// -- for candidate in candidates:
/// --     result = json.loads(await lsp_speculate(file_path="src/main.rs",
/// --         start_line=3, end_line=3, replacement_text=candidate))
/// --     if result["clean"]:
/// --         print(candidate)
/// --         break
#[lua_fn(guard = [FsRead, Run])]
async fn speculate(lua: Lua, opts: Table) -> LuaResult<Pair<Table>> {
    reject_unknown_fields(&opts, SPECULATION_FIELDS, "maki.lsp.speculate")?;
    let Some(file_path) = opts.opt::<String>("file_path")? else {
        return Ok(err_pair("file_path is required"));
    };
    let Some(start_line) = opts.opt::<i64>("start_line")? else {
        return Ok(err_pair("start_line is required"));
    };
    let Some(end_line) = opts.opt::<i64>("end_line")? else {
        return Ok(err_pair("end_line is required"));
    };
    if start_line < 1 || end_line < start_line {
        return Ok(err_pair(
            "line range must be inclusive and 1-based with end_line >= start_line",
        ));
    }
    let Some(replacement_text) = opts.opt::<String>("replacement_text")? else {
        return Ok(err_pair("replacement_text is required"));
    };
    let timeout = opts.opt::<i64>("timeout")?;
    if timeout.is_some_and(|timeout| timeout <= 0) {
        return Ok(err_pair(
            "timeout must be a positive number of milliseconds",
        ));
    }
    let timeout =
        Duration::from_millis(timeout.map_or(DEFAULT_TIMEOUT_MS, |timeout| timeout as u64));
    let command = try_pair!(parse_command(opts.get("command")?)?);
    let language_id = opts.opt::<String>("language_id")?;
    let root = opts.opt::<String>("root")?;
    let (file_path, root) = try_pair!(smol::unblock(move || resolve_paths(file_path, root)).await);
    let language_id = match language_id {
        Some(language_id) if !language_id.trim().is_empty() => language_id,
        Some(_) => return Ok(err_pair("language_id must not be empty")),
        None => match infer_language(&file_path) {
            Some(language_id) => language_id.to_owned(),
            None => {
                return Ok(err_pair(
                    "cannot infer language_id; configure an LSP language_id and command",
                ));
            }
        },
    };
    let command = match command.or_else(|| default_command(&language_id)) {
        Some(command) => command,
        None => {
            return Ok(err_pair(format!(
                "no LSP adapter for language_id {language_id:?}; configure command"
            )));
        }
    };
    let manager = lua
        .app_data_ref::<Arc<LspManager>>()
        .map(|manager| Arc::clone(&manager));
    let manager = manager.unwrap_or_else(|| {
        let manager = Arc::new(LspManager::default());
        lua.set_app_data(Arc::clone(&manager));
        manager
    });
    let result = try_pair!(
        manager
            .speculate(Speculation {
                file_path,
                root,
                language_id,
                command,
                start_line: start_line as usize,
                end_line: end_line as usize,
                replacement_text,
                timeout,
            })
            .await
    );
    let Value::Table(result) = json_to_lua(&lua, &result)? else {
        return Ok(err_pair(
            "language server returned an invalid speculation result",
        ));
    };
    Ok((Some(result), None))
}

lua_table! {
    /// In-memory language-server diagnostics for speculative line replacements.
    /// Default adapters: rust-analyzer, pyright-langserver --stdio,
    /// typescript-language-server --stdio, and gopls. Servers must be installed.
    "maki.lsp" => pub(crate) fn create_lsp_table(perms: &PluginPermissions), DOCS [
        speculate(perms),
    ]
}
