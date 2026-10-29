//! Keyboard, mouse, screenshot, and clipboard control for the desktop.
//!
//! Input uses enigo and clipboard uses arboard. Windows captures the virtual
//! desktop through GDI. Other platforms use X11 or the Unix desktop portal.

use std::time::{SystemTime, UNIX_EPOCH};

use maki_lua_macro::{lua_fn, lua_table};
use mlua::{Lua, Result as LuaResult, Table};

use enigo::{Axis, Button, Coordinate, Direction, Enigo, Key, Keyboard, Mouse, Settings};
use image::{ImageBuffer, Rgba, RgbaImage};

use crate::api::util::convert::TableExt;
use crate::api::util::pair::Pair;
use crate::plugin_permissions::PluginPermissions;

#[cfg(windows)]
#[path = "computer_windows.rs"]
mod windows;
#[cfg(windows)]
use windows::capture_screen;

const SCREENSHOTS_DIR: &str = "screenshots";
const NO_DISPLAY: &str = "cannot reach an interactive desktop";
const KEY_UNKNOWN: &str = "unknown key; use names like enter, escape, tab, up, ctrl+c, f1";
const MOUSE_ACTION_REQUIRED: &str = "action must be move, click, press, release, or scroll";
#[cfg(unix)]
const PORTAL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

fn enigo() -> Result<Enigo, String> {
    Enigo::new(&Settings::default()).map_err(|_| NO_DISPLAY.to_owned())
}

fn parse_key(name: &str) -> Option<Key> {
    let key = match name {
        "return" | "enter" => Key::Return,
        "escape" | "esc" => Key::Escape,
        "tab" => Key::Tab,
        "backspace" => Key::Backspace,
        "space" => Key::Space,
        "delete" | "del" => Key::Delete,
        "up" => Key::UpArrow,
        "down" => Key::DownArrow,
        "left" => Key::LeftArrow,
        "right" => Key::RightArrow,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" | "pgup" => Key::PageUp,
        "pagedown" | "pgdn" => Key::PageDown,
        "ctrl" | "control" => Key::Control,
        "shift" => Key::Shift,
        "alt" => Key::Alt,
        "meta" | "super" | "cmd" | "win" => Key::Meta,
        "capslock" => Key::CapsLock,
        "f1" => Key::F1,
        "f2" => Key::F2,
        "f3" => Key::F3,
        "f4" => Key::F4,
        "f5" => Key::F5,
        "f6" => Key::F6,
        "f7" => Key::F7,
        "f8" => Key::F8,
        "f9" => Key::F9,
        "f10" => Key::F10,
        "f11" => Key::F11,
        "f12" => Key::F12,
        _ => return None,
    };
    Some(key)
}

fn parse_button(name: Option<String>) -> Button {
    match name.as_deref() {
        Some("right") => Button::Right,
        Some("middle") => Button::Middle,
        _ => Button::Left,
    }
}

#[cfg(not(windows))]
fn grab<C>(
    conn: &C,
    drawable: u32,
    width: u16,
    height: u16,
) -> Result<x11rb::protocol::xproto::GetImageReply, String>
where
    C: x11rb::connection::Connection + x11rb::protocol::xproto::ConnectionExt,
{
    use x11rb::protocol::xproto::ImageFormat;
    conn.get_image(
        ImageFormat::Z_PIXMAP,
        drawable,
        0,
        0,
        width,
        height,
        0xFFFF_FFFF,
    )
    .map_err(|e| e.to_string())?
    .reply()
    .map_err(|e| e.to_string())
}

#[cfg(not(windows))]
fn capture_screen() -> Result<RgbaImage, String> {
    use x11rb::connection::Connection;

    let (conn, idx) = x11rb::connect(None).map_err(|_| NO_DISPLAY.to_owned())?;
    let root = &conn.setup().roots[idx];
    let width = root.width_in_pixels;
    let height = root.height_in_pixels;
    let reply = match grab(&conn, root.root, width, height) {
        Ok(reply) => reply,
        // Compositors redirect the root and reject GetImage on it; the
        // composite overlay window holds the composited screen.
        Err(_) => {
            let overlay = x11rb::protocol::composite::get_overlay_window(&conn, root.root)
                .map_err(|e| e.to_string())?
                .reply()
                .map_err(|e| e.to_string())?
                .overlay_win;
            grab(&conn, overlay, width, height)?
        }
    };
    if reply.depth != 24 {
        return Err(format!("unsupported X11 screen depth {}", reply.depth));
    }
    let img = RgbaImage::from_fn(u32::from(width), u32::from(height), |x, y| {
        let i = (y as usize * usize::from(width) + x as usize) * 4;
        let b = reply.data.get(i).copied().unwrap_or(0);
        let g = reply.data.get(i + 1).copied().unwrap_or(0);
        let r = reply.data.get(i + 2).copied().unwrap_or(0);
        Rgba([r, g, b, 255])
    });
    Ok(img)
}

fn default_screenshot_path() -> Result<std::path::PathBuf, String> {
    let dir = maki_storage::paths::state_dir()
        .map_err(|e| e.to_string())?
        .join(SCREENSHOTS_DIR);
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_millis();
    Ok(dir.join(format!("maki-{stamp}.png")))
}

fn save_png(img: &RgbaImage, path: &std::path::Path) -> Result<(), String> {
    let mut file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    img.write_to(&mut file, image::ImageFormat::Png)
        .map_err(|e| e.to_string())
}

fn bgra_to_rgba(img: &RgbaImage) -> Vec<u8> {
    img.pixels()
        .flat_map(|p| [p[2], p[1], p[0], p[3]])
        .collect()
}

fn rgba_to_image(width: usize, height: usize, bytes: &[u8]) -> Option<RgbaImage> {
    if bytes.len() < width * height * 4 {
        return None;
    }
    let mut img = ImageBuffer::new(width as u32, height as u32);
    for (i, chunk) in bytes
        .as_chunks::<4>()
        .0
        .iter()
        .take(width * height)
        .enumerate()
    {
        img.put_pixel(
            (i % width) as u32,
            (i / width) as u32,
            Rgba([chunk[2], chunk[1], chunk[0], chunk[3]]),
        );
    }
    Some(img)
}

/// Capture the desktop to PNG: Windows virtual desktop, X11 root, or Unix portal.
///
/// @param opts table Options. Fields:
///   - path (string): file to write. Default: a timestamped file under the
///     maki state dir.
/// @return (table, string?) Pair: `{ path, width, height, x, y }` on success, `(nil, err)`. x/y is the desktop origin of image pixel (0,0). Add it to image coordinates for mouse input.
#[lua_fn(guard = Run, name = "screenshot")]
async fn computer_screenshot(lua: Lua, opts: Table) -> LuaResult<Pair<Table>> {
    let path: Option<String> = opts.opt("path")?;
    let target = match path {
        Some(p) => std::path::PathBuf::from(p),
        None => match default_screenshot_path() {
            Ok(p) => p,
            Err(e) => return Ok((None, Some(e))),
        },
    };
    #[cfg(windows)]
    let dims = {
        let target = target.clone();
        match smol::unblock(move || {
            let (image, left, top) = capture_screen()?;
            let (width, height) = image.dimensions();
            save_png(&image, &target)?;
            Ok::<_, String>((width, height, left, top))
        })
        .await
        {
            Ok(dimensions) => dimensions,
            Err(error) => return Ok((None, Some(error))),
        }
    };
    #[cfg(not(windows))]
    let dims = match capture_screen() {
        Ok(img) => {
            let dims = img.dimensions();
            if let Err(e) = save_png(&img, &target) {
                return Ok((None, Some(e)));
            }
            dims
        }
        Err(_) => match portal_screenshot(&target).await {
            Ok(dims) => dims,
            Err(e) => return Ok((None, Some(e))),
        },
    };
    #[cfg(windows)]
    let (width, height, x, y) = dims;
    #[cfg(not(windows))]
    let (width, height, x, y) = (dims.0, dims.1, 0, 0);
    let tbl = lua.create_table()?;
    tbl.set("path", target.to_string_lossy().into_owned())?;
    tbl.set("width", width)?;
    tbl.set("height", height)?;
    tbl.set("x", x)?;
    tbl.set("y", y)?;
    Ok((Some(tbl), None))
}

#[cfg(unix)]
async fn portal_screenshot(target: &std::path::Path) -> Result<(u32, u32), String> {
    use ashpd::desktop::screenshot::Screenshot;
    let request = async {
        let shot = Screenshot::request()
            .interactive(false)
            .send()
            .await
            .map_err(|e| format!("no X11 capture ({NO_DISPLAY}); portal screenshot failed: {e}"))?
            .response()
            .map_err(|e| format!("portal screenshot failed: {e}"))?;
        Ok::<_, String>(shot)
    };
    let shot = smol::future::or(request, async {
        smol::Timer::after(PORTAL_TIMEOUT).await;
        Err(format!(
            "no X11 capture ({NO_DISPLAY}); portal screenshot timed out after {}s",
            PORTAL_TIMEOUT.as_secs()
        ))
    })
    .await?;
    let source = shot
        .uri()
        .to_file_path()
        .map_err(|_| "portal screenshot is not a local file".to_owned())?;
    let dims = image::open(&source)
        .map(|img| image::GenericImageView::dimensions(&img))
        .unwrap_or((0, 0));
    std::fs::copy(&source, target).map_err(|e| e.to_string())?;
    Ok(dims)
}

#[cfg(not(any(unix, windows)))]
async fn portal_screenshot(_target: &std::path::Path) -> Result<(u32, u32), String> {
    Err(format!(
        "no X11 capture ({NO_DISPLAY}); no portal fallback on this platform"
    ))
}

/// Move, click, press, release, or scroll the mouse.
///
/// @param opts table Options. Fields:
///   - action (string): move | click | press | release | scroll. Required.
///   - x (integer), y (integer): absolute position for move.
///   - button (string): left | right | middle. Default left.
///   - amount (integer): scroll clicks. Default 3.
///   - axis (string): vertical | horizontal. Default vertical.
/// @return (boolean, string?) `(true, nil)` on success, `(nil, err)`.
#[lua_fn(guard = Run, name = "mouse")]
fn computer_mouse(_lua: &Lua, opts: Table) -> LuaResult<Pair<bool>> {
    let action: Option<String> = opts.opt("action")?;
    let Some(action) = action else {
        return Ok((None, Some(MOUSE_ACTION_REQUIRED.to_owned())));
    };
    let button = parse_button(opts.opt("button")?);
    let mut enigo = match enigo() {
        Ok(e) => e,
        Err(e) => return Ok((None, Some(e))),
    };
    let result = match action.as_str() {
        "move" => {
            let x: Option<i32> = opts.opt("x")?;
            let y: Option<i32> = opts.opt("y")?;
            match (x, y) {
                (Some(x), Some(y)) => enigo.move_mouse(x, y, Coordinate::Abs),
                _ => return Ok((None, Some("move needs x and y".to_owned()))),
            }
        }
        "click" => enigo.button(button, Direction::Click),
        "press" => enigo.button(button, Direction::Press),
        "release" => enigo.button(button, Direction::Release),
        "scroll" => {
            let amount: i32 = opts.opt::<i32>("amount")?.unwrap_or(3);
            let axis = match opts.opt::<String>("axis")?.as_deref() {
                Some("horizontal") => Axis::Horizontal,
                _ => Axis::Vertical,
            };
            enigo.scroll(amount, axis)
        }
        _ => return Ok((None, Some(MOUSE_ACTION_REQUIRED.to_owned()))),
    };
    match result {
        Ok(()) => Ok((Some(true), None)),
        Err(e) => Ok((None, Some(e.to_string()))),
    }
}

/// Type text, or press a key / key combo (e.g. "ctrl+c").
///
/// @param opts table Options. Fields:
///   - text (string): literal text to type.
///   - key (string): key name, optionally a "+"-joined combo like ctrl+shift+s.
/// @return (boolean, string?) `(true, nil)` on success, `(nil, err)`.
#[lua_fn(guard = Run, name = "keyboard")]
fn computer_keyboard(_lua: &Lua, opts: Table) -> LuaResult<Pair<bool>> {
    let text: Option<String> = opts.opt("text")?;
    let key: Option<String> = opts.opt("key")?;
    let mut enigo = match enigo() {
        Ok(e) => e,
        Err(e) => return Ok((None, Some(e))),
    };
    if let Some(text) = text {
        return match enigo.text(&text) {
            Ok(()) => Ok((Some(true), None)),
            Err(e) => Ok((None, Some(e.to_string()))),
        };
    }
    let Some(key) = key else {
        return Ok((None, Some("text or key is required".to_owned())));
    };
    let parts: Vec<String> = key.split('+').map(|p| p.trim().to_lowercase()).collect();
    let Some(last) = parts.last().map(String::as_str) else {
        return Ok((None, Some(KEY_UNKNOWN.to_owned())));
    };
    let Some(main) = parse_key(last) else {
        return Ok((None, Some(KEY_UNKNOWN.to_owned())));
    };
    let mut mods = Vec::new();
    for part in &parts[..parts.len() - 1] {
        match parse_key(part) {
            Some(k) => mods.push(k),
            None => return Ok((None, Some(KEY_UNKNOWN.to_owned()))),
        }
    }
    for m in &mods {
        if let Err(e) = enigo.key(*m, Direction::Press) {
            return Ok((None, Some(e.to_string())));
        }
    }
    let pressed = enigo.key(main, Direction::Click);
    for m in &mods {
        let _ = enigo.key(*m, Direction::Release);
    }
    match pressed {
        Ok(()) => Ok((Some(true), None)),
        Err(e) => Ok((None, Some(e.to_string()))),
    }
}

/// Report the mouse pointer position.
///
/// @param _opts table Unused.
/// @return (table, string?) Pair: `{ x, y }` on success, `(nil, err)`.
#[lua_fn(guard = Run, name = "position")]
fn computer_position(lua: &Lua, _opts: Table) -> LuaResult<Pair<Table>> {
    let enigo = match enigo() {
        Ok(e) => e,
        Err(e) => return Ok((None, Some(e))),
    };
    match enigo.location() {
        Ok((x, y)) => {
            let tbl = lua.create_table()?;
            tbl.set("x", x)?;
            tbl.set("y", y)?;
            Ok((Some(tbl), None))
        }
        Err(e) => Ok((None, Some(e.to_string()))),
    }
}

/// Report the main display size.
///
/// @param _opts table Unused.
/// @return (table, string?) Pair: `{ width, height }` on success, `(nil, err)`.
#[lua_fn(guard = Run, name = "display")]
fn computer_display(lua: &Lua, _opts: Table) -> LuaResult<Pair<Table>> {
    let enigo = match enigo() {
        Ok(e) => e,
        Err(e) => return Ok((None, Some(e))),
    };
    match enigo.main_display() {
        Ok((width, height)) => {
            let tbl = lua.create_table()?;
            tbl.set("width", width)?;
            tbl.set("height", height)?;
            Ok((Some(tbl), None))
        }
        Err(e) => Ok((None, Some(e.to_string()))),
    }
}

static CLIPBOARD: std::sync::OnceLock<std::sync::Mutex<Result<arboard::Clipboard, ()>>> =
    std::sync::OnceLock::new();

fn clipboard<R>(f: impl FnOnce(&mut arboard::Clipboard) -> R) -> Result<R, String> {
    let cell =
        CLIPBOARD.get_or_init(|| std::sync::Mutex::new(arboard::Clipboard::new().map_err(|_| ())));
    let mut guard = cell.lock().unwrap_or_else(|e| e.into_inner());
    let clip = guard.as_mut().map_err(|_| NO_DISPLAY.to_owned())?;
    Ok(f(clip))
}

/// Read or write the system clipboard.
///
/// @param opts table Options. Fields:
///   - text (string): set the clipboard text to this.
///   - image_path (string): set the clipboard image from this PNG file.
///   - path (string): save the clipboard image to this file (get mode).
///   - get (boolean): request text (default) or, with path, an image.
/// @return (table, string?) Pair: `{ text }` or `{ path, width, height }` on
///   get, `(true, nil)` on set, `(nil, err)` on failure.
#[lua_fn(guard = Run, name = "clipboard")]
fn computer_clipboard(lua: &Lua, opts: Table) -> LuaResult<Pair<Table>> {
    let text: Option<String> = opts.opt("text")?;
    let image_path: Option<String> = opts.opt("image_path")?;
    let get_path: Option<String> = opts.opt("path")?;
    let get: bool = opts.opt_bool("get").unwrap_or(true);

    if let Some(text) = text {
        return match clipboard(|c| c.set_text(text)) {
            Ok(Ok(())) => {
                let tbl = lua.create_table()?;
                tbl.set("set", true)?;
                Ok((Some(tbl), None))
            }
            Ok(Err(e)) => Ok((None, Some(e.to_string()))),
            Err(e) => Ok((None, Some(e))),
        };
    }
    if let Some(image_path) = image_path {
        let loaded = match image::open(&image_path) {
            Ok(img) => img.to_rgba8(),
            Err(e) => return Ok((None, Some(e.to_string()))),
        };
        let (width, height) = loaded.dimensions();
        let bytes = bgra_to_rgba(&loaded);
        let data = arboard::ImageData {
            width: width as usize,
            height: height as usize,
            bytes: std::borrow::Cow::Owned(bytes),
        };
        return match clipboard(|c| c.set_image(data)) {
            Ok(Ok(())) => {
                let tbl = lua.create_table()?;
                tbl.set("set", true)?;
                Ok((Some(tbl), None))
            }
            Ok(Err(e)) => Ok((None, Some(e.to_string()))),
            Err(e) => Ok((None, Some(e))),
        };
    }
    if !get {
        return Ok((
            None,
            Some("text, image_path, or get is required".to_owned()),
        ));
    }
    match get_path {
        Some(path) => {
            let img = match clipboard(|c| c.get_image()) {
                Ok(Ok(img)) => img,
                Ok(Err(e)) => return Ok((None, Some(e.to_string()))),
                Err(e) => return Ok((None, Some(e))),
            };
            let Some(rgba) = rgba_to_image(img.width, img.height, &img.bytes) else {
                return Ok((
                    None,
                    Some("clipboard image has a truncated buffer".to_owned()),
                ));
            };
            let path = std::path::PathBuf::from(path);
            if let Err(e) = save_png(&rgba, &path) {
                return Ok((None, Some(e)));
            }
            let tbl = lua.create_table()?;
            tbl.set("path", path.to_string_lossy().into_owned())?;
            tbl.set("width", img.width)?;
            tbl.set("height", img.height)?;
            Ok((Some(tbl), None))
        }
        None => match clipboard(|c| c.get_text()) {
            Ok(Ok(text)) => {
                let tbl = lua.create_table()?;
                tbl.set("text", text)?;
                Ok((Some(tbl), None))
            }
            Ok(Err(e)) => Ok((None, Some(e.to_string()))),
            Err(e) => Ok((None, Some(e))),
        },
    }
}

lua_table! {
    /// Desktop control: `maki.computer.screenshot` captures the screen,
    /// `mouse`/`keyboard` drive pointer and keys, `position`/`display` report
    /// state, `clipboard` reads and writes the system clipboard. Requires an
    /// interactive desktop; calls fail cleanly when its backend is unavailable.
    "maki.computer" => pub(crate) fn create_computer_table(perms: &PluginPermissions), DOCS [
        computer_screenshot(perms),
        computer_mouse(perms),
        computer_keyboard(perms),
        computer_position(perms),
        computer_display(perms),
        computer_clipboard(perms),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case("enter" => Some(Key::Return); "enter")]
    #[test_case("esc" => Some(Key::Escape); "esc")]
    #[test_case("pgup" => Some(Key::PageUp); "pgup")]
    #[test_case("ctrl" => Some(Key::Control); "ctrl")]
    #[test_case("f12" => Some(Key::F12); "f12")]
    #[test_case("nosuchkey" => None; "unknown")]
    fn key_names(name: &str) -> Option<Key> {
        parse_key(name)
    }

    #[test_case(Some("right".to_string()) => Button::Right; "right")]
    #[test_case(Some("middle".to_string()) => Button::Middle; "middle")]
    #[test_case(None => Button::Left; "default_left")]
    fn buttons(name: Option<String>) -> Button {
        parse_button(name)
    }

    #[test]
    fn bgra_roundtrip_preserves_channels() {
        let img = RgbaImage::from_pixel(1, 1, Rgba([10, 20, 30, 255]));
        let bytes = bgra_to_rgba(&img);
        assert_eq!(bytes, vec![30, 20, 10, 255]);
        let back = rgba_to_image(1, 1, &bytes).expect("roundtrip");
        assert_eq!(back.get_pixel(0, 0).0, [10, 20, 30, 255]);
    }

    #[test]
    fn rgba_to_image_rejects_short_buffers() {
        assert!(rgba_to_image(2, 2, &[0, 0, 0, 0]).is_none());
    }
}
