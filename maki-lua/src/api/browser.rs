//! Chrome sessions driven over CDP, one per maki session.
//!
//! chromiumoxide is tokio-native, so every CDP round-trip runs on a private
//! tokio runtime and the reply is awaited from smol through a oneshot. The
//! browser process is launched on first `open` and kept alive for the session;
//! `close` (or session teardown) kills it.

use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::Duration;

use maki_lua_macro::{lua_fn, lua_table};
use maki_storage::id::MakiId;
use mlua::{Lua, Result as LuaResult, Table, Value as LuaValue};

use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::page::CaptureScreenshotFormat;
use chromiumoxide::page::{Page, ScreenshotParams};
use futures_lite::StreamExt;
use tokio::runtime::Runtime;

use crate::api::util::convert::TableExt;
use crate::api::util::pair::Pair;
use crate::plugin_permissions::PluginPermissions;

const TOKIO_WORKERS: usize = 2;
const DEFAULT_TIMEOUT_SECS: u64 = 30;
const BROWSER_BIN_ENV: &str = "MAKI_BROWSER_BIN";
const EXTRA_BROWSER_NAMES: [&str; 3] = ["brave", "brave-browser", "brave-browser-stable"];
const CHROME_NOT_FOUND: &str = "Chrome/Chromium/Brave not found; install one, set MAKI_BROWSER_BIN, or pass an explicit executable path";
const SESSION_REQUIRED: &str = "session is required";
const BROWSER_NOT_OPEN: &str = "no browser open for this session";
const TASK_CANCELLED: &str = "browser task cancelled";
const BROWSER_TIMEOUT: &str = "browser call timed out";
const ELEMENT_NOT_FOUND: &str = "element index not found in current snapshot";

const SNAPSHOT_JS: &str = r#"(() => {
  const sel = 'a,button,input,select,textarea,[role],[contenteditable="true"],[onclick],[tabindex]';
  const out = [];
  let i = 0;
  for (const el of document.querySelectorAll(sel)) {
    const r = el.getBoundingClientRect();
    if (r.width === 0 && r.height === 0) continue;
    el.setAttribute('data-maki-idx', String(i));
    out.push({
      idx: i,
      tag: el.tagName.toLowerCase(),
      type: el.getAttribute('type') || '',
      role: el.getAttribute('role') || '',
      text: (el.innerText || el.value || el.getAttribute('aria-label') || el.getAttribute('placeholder') || '').replace(/\s+/g, ' ').trim().slice(0, 120),
      href: el.getAttribute('href') || '',
    });
    i++;
  }
  return { count: i, title: document.title, url: location.href, elements: out };
})()"#;

fn selector_of(index: i64) -> String {
    format!("[data-maki-idx='{index}']")
}

fn discover_executable() -> Option<String> {
    if let Ok(path) = std::env::var(BROWSER_BIN_ENV) {
        return Some(path);
    }
    std::env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .find_map(|dir| {
            EXTRA_BROWSER_NAMES
                .iter()
                .map(|name| std::path::Path::new(dir).join(name))
                .find(|p| p.is_file())
                .map(|p| p.to_string_lossy().into_owned())
        })
}

fn canonical_key(name: &str) -> String {
    let lower = name.trim().to_lowercase();
    let canonical = match lower.as_str() {
        "enter" | "return" => "Enter",
        "escape" | "esc" => "Escape",
        "tab" => "Tab",
        "backspace" => "Backspace",
        "delete" | "del" => "Delete",
        "space" => "Space",
        "up" | "arrowup" => "ArrowUp",
        "down" | "arrowdown" => "ArrowDown",
        "left" | "arrowleft" => "ArrowLeft",
        "right" | "arrowright" => "ArrowRight",
        "home" => "Home",
        "end" => "End",
        "pageup" | "pgup" => "PageUp",
        "pagedown" | "pgdn" => "PageDown",
        "ctrl" | "control" => "Control",
        "shift" => "Shift",
        "alt" => "Alt",
        "meta" | "super" | "cmd" | "win" => "Meta",
        _ => return lower,
    };
    canonical.to_owned()
}

static TOKIO: OnceLock<Runtime> = OnceLock::new();

fn tokio_rt() -> &'static Runtime {
    TOKIO.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(TOKIO_WORKERS)
            .enable_all()
            .build()
            .expect("tokio runtime for browser sessions")
    })
}

async fn tokio_call<T>(
    timeout_secs: u64,
    fut: impl Future<Output = Result<T, String>> + Send + 'static,
) -> Result<T, String>
where
    T: Send + 'static,
{
    let (tx, rx) = tokio::sync::oneshot::channel();
    tokio_rt().spawn(async move {
        let _ = tx.send(fut.await);
    });
    let deadline = smol::Timer::after(Duration::from_secs(timeout_secs));
    futures_lite::pin!(deadline);
    futures_lite::future::or(
        async { rx.await.map_err(|_| TASK_CANCELLED.to_owned())? },
        async {
            (&mut deadline).await;
            Err(BROWSER_TIMEOUT.to_owned())
        },
    )
    .await
}

struct BrowserSession {
    browser: Browser,
    page: Page,
}

#[derive(Default)]
pub(crate) struct BrowserStore {
    sessions: HashMap<String, BrowserSession>,
}

impl BrowserStore {
    fn close(&mut self, session: &str) {
        if let Some(mut entry) = self.sessions.remove(session) {
            tokio_rt().spawn(async move {
                let _ = entry.browser.close().await;
            });
        }
    }

    pub(crate) fn kill_session(&mut self, session: MakiId) {
        self.close(&session.to_string());
    }
}

pub(crate) fn with_browser<R>(lua: &Lua, f: impl FnOnce(&mut BrowserStore) -> R) -> R {
    if lua.app_data_ref::<BrowserStore>().is_none() {
        lua.set_app_data(BrowserStore::default());
    }
    let mut store = lua
        .app_data_mut::<BrowserStore>()
        .expect("browser store was just installed");
    f(&mut store)
}

pub(crate) fn kill_session(lua: &Lua, session: MakiId) {
    with_browser(lua, |store| store.kill_session(session));
}

fn page_of(lua: &Lua, session: &str) -> Option<Page> {
    with_browser(lua, |store| {
        store.sessions.get(session).map(|e| e.page.clone())
    })
}

fn json_to_lua(lua: &Lua, value: serde_json::Value) -> LuaResult<LuaValue> {
    Ok(match value {
        serde_json::Value::Null => LuaValue::Nil,
        serde_json::Value::Bool(b) => LuaValue::Boolean(b),
        serde_json::Value::Number(n) => match n.as_i64() {
            Some(i) => LuaValue::Integer(i),
            None => LuaValue::Number(n.as_f64().unwrap_or(0.0)),
        },
        serde_json::Value::String(s) => LuaValue::String(lua.create_string(&s)?),
        serde_json::Value::Array(arr) => {
            let tbl = lua.create_table()?;
            for (i, v) in arr.into_iter().enumerate() {
                tbl.set(i + 1, json_to_lua(lua, v)?)?;
            }
            LuaValue::Table(tbl)
        }
        serde_json::Value::Object(map) => {
            let tbl = lua.create_table()?;
            for (k, v) in map {
                tbl.set(k, json_to_lua(lua, v)?)?;
            }
            LuaValue::Table(tbl)
        }
    })
}

async fn eval_json(page: Page, source: String) -> Result<serde_json::Value, String> {
    let result = page.evaluate(source).await.map_err(|e| e.to_string())?;
    result
        .into_value::<serde_json::Value>()
        .map_err(|e| e.to_string())
}

async fn eval_bool(page: Page, source: String) -> Result<bool, String> {
    let result = page.evaluate(source).await.map_err(|e| e.to_string())?;
    result.into_value::<bool>().map_err(|e| e.to_string())
}

/// Launch the session's browser (if needed) and navigate to a URL.
///
/// @param opts table Options. Fields:
///   - session (string): session id owning this browser. Required.
///   - url (string): URL to navigate to. Required.
///   - headless (boolean): launch without a window. Default true. Ignored
///     when the browser is already running.
///   - executable (string): path to the Chrome/Chromium/Brave binary. Default:
///     auto-discovered (MAKI_BROWSER_BIN env, then brave/chromium names on PATH).
///   - timeout (integer): seconds to wait. Default 30.
/// @return (table, string?) Pair: `{ url, title }` on success, `(nil, err)`.
#[lua_fn(guard = Net, name = "open")]
async fn browser_open(lua: Lua, opts: Table) -> LuaResult<Pair<Table>> {
    let session: Option<String> = opts.opt("session")?;
    let Some(session) = session else {
        return Ok((None, Some(SESSION_REQUIRED.to_owned())));
    };
    let url: Option<String> = opts.opt("url")?;
    let Some(url) = url else {
        return Ok((None, Some("url is required".to_owned())));
    };
    let headless: bool = opts.opt_bool("headless").unwrap_or(true);
    let executable: Option<String> = opts.opt("executable")?;
    let executable = executable.or_else(discover_executable);
    let timeout: u64 = opts.opt::<u64>("timeout")?.unwrap_or(DEFAULT_TIMEOUT_SECS);

    let existing = page_of(&lua, &session);
    let outcome = tokio_call(timeout, async move {
        let (browser, page) = match existing {
            Some(page) => {
                page.goto(&url).await.map_err(|e| e.to_string())?;
                (None, page)
            }
            None => {
                let mut builder = BrowserConfig::builder();
                if headless {
                    builder = builder.with_head();
                }
                if let Some(path) = executable {
                    builder = builder.chrome_executable(path);
                }
                let config = builder.build().map_err(|e| e.to_string())?;
                let (browser, mut handler) =
                    Browser::launch(config).await.map_err(|e| e.to_string())?;
                tokio_rt().spawn(async move { while handler.next().await.is_some() {} });
                let page = browser.new_page(&url).await.map_err(|e| e.to_string())?;
                (Some(browser), page)
            }
        };
        let title = page.get_title().await.map_err(|e| e.to_string())?;
        let current = page.url().await.map_err(|e| e.to_string())?;
        Ok((browser, page, current.unwrap_or(url), title))
    })
    .await;

    let outcome = match outcome {
        Ok(v) => v,
        Err(e)
            if e.contains("not found") || e.contains("No such file") || e.contains("chromium") =>
        {
            return Ok((None, Some(CHROME_NOT_FOUND.to_owned())));
        }
        Err(e) => return Ok((None, Some(e))),
    };
    let (browser, page, url, title) = outcome;
    with_browser(&lua, |store| {
        if let Some(browser) = browser {
            store.sessions.insert(
                session,
                BrowserSession {
                    browser,
                    page: page.clone(),
                },
            );
        } else if let Some(entry) = store.sessions.get_mut(&session) {
            entry.page = page;
        }
    });

    let tbl = lua.create_table()?;
    tbl.set("url", url)?;
    tbl.set("title", title)?;
    Ok((Some(tbl), None))
}

/// List the page's interactive elements, assigning each an index for later
/// `click`/`type` calls.
///
/// @param opts table Options. Fields:
///   - session (string): session id owning this browser. Required.
///   - timeout (integer): seconds to wait. Default 30.
/// @return (table, string?) Pair: `{ count, title, url, elements }` where each
///   element is `{ idx, tag, type, role, text, href }`, on success, `(nil, err)`.
#[lua_fn(guard = Net, name = "snapshot")]
async fn browser_snapshot(lua: Lua, opts: Table) -> LuaResult<Pair<Table>> {
    let session: Option<String> = opts.opt("session")?;
    let Some(session) = session else {
        return Ok((None, Some(SESSION_REQUIRED.to_owned())));
    };
    let timeout: u64 = opts.opt::<u64>("timeout")?.unwrap_or(DEFAULT_TIMEOUT_SECS);
    let Some(page) = page_of(&lua, &session) else {
        return Ok((None, Some(BROWSER_NOT_OPEN.to_owned())));
    };
    match tokio_call(timeout, eval_json(page, SNAPSHOT_JS.to_owned())).await {
        Ok(value) => match json_to_lua(&lua, value)? {
            LuaValue::Table(tbl) => Ok((Some(tbl), None)),
            _ => Ok((None, Some("snapshot returned a non-table".to_owned()))),
        },
        Err(e) => Ok((None, Some(e))),
    }
}

/// Click the element with the given snapshot index.
///
/// @param opts table Options. Fields:
///   - session (string): session id owning this browser. Required.
///   - index (integer): element idx from the latest snapshot. Required.
///   - timeout (integer): seconds to wait. Default 30.
/// @return (boolean, string?) `(true, nil)` on success, `(nil, err)`.
#[lua_fn(guard = Net, name = "click")]
async fn browser_click(lua: Lua, opts: Table) -> LuaResult<Pair<bool>> {
    let session: Option<String> = opts.opt("session")?;
    let Some(session) = session else {
        return Ok((None, Some(SESSION_REQUIRED.to_owned())));
    };
    let index: Option<i64> = opts.opt("index")?;
    let Some(index) = index else {
        return Ok((None, Some("index is required".to_owned())));
    };
    let timeout: u64 = opts.opt::<u64>("timeout")?.unwrap_or(DEFAULT_TIMEOUT_SECS);
    let Some(page) = page_of(&lua, &session) else {
        return Ok((None, Some(BROWSER_NOT_OPEN.to_owned())));
    };
    let clicked = tokio_call(timeout, async move {
        let element = page
            .find_element(selector_of(index))
            .await
            .map_err(|_| ELEMENT_NOT_FOUND.to_owned())?;
        element.click().await.map_err(|e| e.to_string())?;
        Ok(())
    })
    .await;
    match clicked {
        Ok(()) => Ok((Some(true), None)),
        Err(e) => Ok((None, Some(e))),
    }
}

/// Type text into the element with the given snapshot index, or into the
/// focused element when no index is given.
///
/// @param opts table Options. Fields:
///   - session (string): session id owning this browser. Required.
///   - text (string): text to type. Required.
///   - index (integer): element idx from the latest snapshot. Optional.
///   - timeout (integer): seconds to wait. Default 30.
/// @return (boolean, string?) `(true, nil)` on success, `(nil, err)`.
#[lua_fn(guard = Net, name = "type")]
async fn browser_type(lua: Lua, opts: Table) -> LuaResult<Pair<bool>> {
    let session: Option<String> = opts.opt("session")?;
    let Some(session) = session else {
        return Ok((None, Some(SESSION_REQUIRED.to_owned())));
    };
    let text: Option<String> = opts.opt("text")?;
    let Some(text) = text else {
        return Ok((None, Some("text is required".to_owned())));
    };
    let index: Option<i64> = opts.opt("index")?;
    let timeout: u64 = opts.opt::<u64>("timeout")?.unwrap_or(DEFAULT_TIMEOUT_SECS);
    let Some(page) = page_of(&lua, &session) else {
        return Ok((None, Some(BROWSER_NOT_OPEN.to_owned())));
    };
    let typed = tokio_call(timeout, async move {
        let selector = match index {
            Some(index) => selector_of(index),
            None => ":focus".to_owned(),
        };
        let element = page
            .find_element(selector)
            .await
            .map_err(|_| ELEMENT_NOT_FOUND.to_owned())?;
        element.focus().await.map_err(|e| e.to_string())?;
        element.type_str(&text).await.map_err(|e| e.to_string())?;
        Ok(())
    })
    .await;
    match typed {
        Ok(()) => Ok((Some(true), None)),
        Err(e) => Ok((None, Some(e))),
    }
}

/// Press a key (e.g. "Enter", "Escape", "Tab", "ArrowDown").
///
/// @param opts table Options. Fields:
///   - session (string): session id owning this browser. Required.
///   - key (string): key name. Required.
///   - timeout (integer): seconds to wait. Default 30.
/// @return (boolean, string?) `(true, nil)` on success, `(nil, err)`.
#[lua_fn(guard = Net, name = "press")]
async fn browser_press(lua: Lua, opts: Table) -> LuaResult<Pair<bool>> {
    let session: Option<String> = opts.opt("session")?;
    let Some(session) = session else {
        return Ok((None, Some(SESSION_REQUIRED.to_owned())));
    };
    let key: Option<String> = opts.opt("key")?;
    let Some(key) = key else {
        return Ok((None, Some("key is required".to_owned())));
    };
    let timeout: u64 = opts.opt::<u64>("timeout")?.unwrap_or(DEFAULT_TIMEOUT_SECS);
    let Some(page) = page_of(&lua, &session) else {
        return Ok((None, Some(BROWSER_NOT_OPEN.to_owned())));
    };
    let pressed = tokio_call(timeout, async move {
        let key = canonical_key(&key);
        chromiumoxide::keys::get_key_definition(&key)
            .ok_or_else(|| format!("unknown key '{key}'"))?;
        let element = page
            .find_element(":focus")
            .await
            .map_err(|_| "no focused element".to_owned())?;
        element.press_key(&key).await.map_err(|e| e.to_string())?;
        Ok(())
    })
    .await;
    match pressed {
        Ok(()) => Ok((Some(true), None)),
        Err(e) => Ok((None, Some(e))),
    }
}

/// Scroll the page by (x, y) pixels.
///
/// @param opts table Options. Fields:
///   - session (string): session id owning this browser. Required.
///   - x (integer): horizontal pixels. Default 0.
///   - y (integer): vertical pixels. Default 0.
///   - timeout (integer): seconds to wait. Default 30.
/// @return (boolean, string?) `(true, nil)` on success, `(nil, err)`.
#[lua_fn(guard = Net, name = "scroll")]
async fn browser_scroll(lua: Lua, opts: Table) -> LuaResult<Pair<bool>> {
    let session: Option<String> = opts.opt("session")?;
    let Some(session) = session else {
        return Ok((None, Some(SESSION_REQUIRED.to_owned())));
    };
    let x: i32 = opts.opt::<i32>("x")?.unwrap_or(0);
    let y: i32 = opts.opt::<i32>("y")?.unwrap_or(0);
    let timeout: u64 = opts.opt::<u64>("timeout")?.unwrap_or(DEFAULT_TIMEOUT_SECS);
    let Some(page) = page_of(&lua, &session) else {
        return Ok((None, Some(BROWSER_NOT_OPEN.to_owned())));
    };
    let js = format!("window.scrollBy({x}, {y})");
    match tokio_call(timeout, eval_bool(page, js)).await {
        Ok(_) => Ok((Some(true), None)),
        Err(e) => Ok((None, Some(e))),
    }
}

/// Capture a PNG screenshot of the page to a file.
///
/// @param opts table Options. Fields:
///   - session (string): session id owning this browser. Required.
///   - path (string): file to write. Required.
///   - timeout (integer): seconds to wait. Default 30.
/// @return (string, string?) Pair: the path on success, `(nil, err)`.
#[lua_fn(guard = Net, name = "screenshot")]
async fn browser_screenshot(lua: Lua, opts: Table) -> LuaResult<Pair<String>> {
    let session: Option<String> = opts.opt("session")?;
    let Some(session) = session else {
        return Ok((None, Some(SESSION_REQUIRED.to_owned())));
    };
    let path: Option<String> = opts.opt("path")?;
    let Some(path) = path else {
        return Ok((None, Some("path is required".to_owned())));
    };
    let timeout: u64 = opts.opt::<u64>("timeout")?.unwrap_or(DEFAULT_TIMEOUT_SECS);
    let Some(page) = page_of(&lua, &session) else {
        return Ok((None, Some(BROWSER_NOT_OPEN.to_owned())));
    };
    let capture_path = path.clone();
    let captured = tokio_call(timeout, async move {
        let params = ScreenshotParams::builder()
            .format(CaptureScreenshotFormat::Png)
            .build();
        page.save_screenshot(params, &capture_path)
            .await
            .map_err(|e| e.to_string())
    })
    .await;
    match captured {
        Ok(_) => Ok((Some(path), None)),
        Err(e) => Ok((None, Some(e))),
    }
}

/// Evaluate JavaScript in the page and return the JSON-serializable result.
///
/// @param opts table Options. Fields:
///   - session (string): session id owning this browser. Required.
///   - source (string): JavaScript expression to evaluate. Required.
///   - timeout (integer): seconds to wait. Default 30.
/// @return (any, string?) Pair: the result on success, `(nil, err)`.
#[lua_fn(guard = Net, name = "eval")]
async fn browser_eval(lua: Lua, opts: Table) -> LuaResult<Pair<LuaValue>> {
    let session: Option<String> = opts.opt("session")?;
    let Some(session) = session else {
        return Ok((None, Some(SESSION_REQUIRED.to_owned())));
    };
    let source: Option<String> = opts.opt("source")?;
    let Some(source) = source else {
        return Ok((None, Some("source is required".to_owned())));
    };
    let timeout: u64 = opts.opt::<u64>("timeout")?.unwrap_or(DEFAULT_TIMEOUT_SECS);
    let Some(page) = page_of(&lua, &session) else {
        return Ok((None, Some(BROWSER_NOT_OPEN.to_owned())));
    };
    match tokio_call(timeout, eval_json(page, source)).await {
        Ok(value) => Ok((Some(json_to_lua(&lua, value)?), None)),
        Err(e) => Ok((None, Some(e))),
    }
}

/// Report the session's browser state.
///
/// @param opts table Options. Fields:
///   - session (string): session id owning this browser. Required.
/// @return (table, string?) Pair: `{ open, url, title }` on success, `(nil, err)`.
#[lua_fn(guard = Net, name = "status")]
async fn browser_status(lua: Lua, opts: Table) -> LuaResult<Pair<Table>> {
    let session: Option<String> = opts.opt("session")?;
    let Some(session) = session else {
        return Ok((None, Some(SESSION_REQUIRED.to_owned())));
    };
    let Some(page) = page_of(&lua, &session) else {
        let tbl = lua.create_table()?;
        tbl.set("open", false)?;
        return Ok((Some(tbl), None));
    };
    let info = tokio_call(DEFAULT_TIMEOUT_SECS, async move {
        let url = page.url().await.map_err(|e| e.to_string())?;
        let title = page.get_title().await.map_err(|e| e.to_string())?;
        Ok::<(Option<String>, Option<String>), String>((url, title))
    })
    .await;
    match info {
        Ok((url, title)) => {
            let tbl = lua.create_table()?;
            tbl.set("open", true)?;
            tbl.set("url", url)?;
            tbl.set("title", title)?;
            Ok((Some(tbl), None))
        }
        Err(e) => Ok((None, Some(e))),
    }
}

/// Close the session's browser and kill the process.
///
/// @param opts table Options. Fields:
///   - session (string): session id owning this browser. Required.
/// @return (boolean, string?) `(true, nil)` on success, `(nil, err)`.
#[lua_fn(guard = Net, name = "close")]
fn browser_close(lua: &Lua, opts: Table) -> LuaResult<Pair<bool>> {
    let session: Option<String> = opts.opt("session")?;
    let Some(session) = session else {
        return Ok((None, Some(SESSION_REQUIRED.to_owned())));
    };
    with_browser(lua, |store| store.close(&session));
    Ok((Some(true), None))
}

lua_table! {
    /// Chrome over CDP: `maki.browser.open` launches a browser (headless by
    /// default) and navigates, `snapshot` indexes interactive elements for
    /// `click`/`type`, `press`/`scroll`/`eval` drive the page, `screenshot`
    /// captures it, `close` kills the process.
    "maki.browser" => pub(crate) fn create_browser_table(perms: &PluginPermissions), DOCS [
        browser_open(perms),
        browser_snapshot(perms),
        browser_click(perms),
        browser_type(perms),
        browser_press(perms),
        browser_scroll(perms),
        browser_screenshot(perms),
        browser_eval(perms),
        browser_status(perms),
        browser_close(perms),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case("enter" => "Enter".to_string(); "enter")]
    #[test_case("ESC" => "Escape".to_string(); "esc_canonical")]
    #[test_case("pgup" => "PageUp".to_string(); "pgup")]
    #[test_case("ctrl" => "Control".to_string(); "ctrl")]
    #[test_case("a" => "a".to_string(); "single_char")]
    fn key_canonicalization(name: &str) -> String {
        canonical_key(name)
    }

    #[test]
    fn selector_quotes_index() {
        assert_eq!(selector_of(7), "[data-maki-idx='7']");
    }

    #[test]
    fn store_close_is_idempotent() {
        let mut store = BrowserStore::default();
        store.close("missing");
        assert!(store.sessions.is_empty());
    }

    const LIVE_HTML: &str = "data:text/html,<title>BraveSmoke</title><body><input id=q><button onclick=\"document.title='clicked'\">Go</button></body>";
    const LIVE_SESSION: &str = "live-test";

    fn live_opts(lua: &Lua) -> LuaResult<Table> {
        let opts = lua.create_table()?;
        opts.set("session", LIVE_SESSION)?;
        Ok(opts)
    }

    #[test]
    #[ignore = "needs a real Chromium-family browser (MAKI_BROWSER_BIN or brave on PATH)"]
    fn brave_open_snapshot_click_screenshot() {
        let lua = Lua::new();

        let opts = live_opts(&lua).unwrap();
        opts.set("url", LIVE_HTML).unwrap();
        opts.set("headless", true).unwrap();
        let (out, err) = smol::block_on(browser_open(lua.clone(), opts)).unwrap();
        assert!(out.is_some(), "{}", err.unwrap_or_default());
        let opened = out.unwrap();
        assert_eq!(opened.get::<String>("title").unwrap(), "BraveSmoke");

        let (out, err) =
            smol::block_on(browser_snapshot(lua.clone(), live_opts(&lua).unwrap())).unwrap();
        assert!(out.is_some(), "{}", err.unwrap_or_default());
        let snap = out.unwrap();
        assert!(snap.get::<i64>("count").unwrap() >= 2);

        let opts = live_opts(&lua).unwrap();
        opts.set("index", 1).unwrap();
        let (out, err) = smol::block_on(browser_click(lua.clone(), opts)).unwrap();
        assert_eq!(out, Some(true), "{}", err.unwrap_or_default());

        let opts = live_opts(&lua).unwrap();
        opts.set("source", "document.title").unwrap();
        let (out, _) = smol::block_on(browser_eval(lua.clone(), opts)).unwrap();
        assert_eq!(
            out,
            Some(LuaValue::String(lua.create_string("clicked").unwrap()))
        );

        let path = std::env::temp_dir().join("maki-browser-smoke.png");
        let opts = live_opts(&lua).unwrap();
        opts.set("path", path.to_string_lossy().into_owned())
            .unwrap();
        let (out, err) = smol::block_on(browser_screenshot(lua.clone(), opts)).unwrap();
        assert!(out.is_some(), "{}", err.unwrap_or_default());
        assert!(path.exists());
        let _ = std::fs::remove_file(&path);

        let (out, _) = browser_close(&lua, live_opts(&lua).unwrap()).unwrap();
        assert_eq!(out, Some(true));
    }
}
