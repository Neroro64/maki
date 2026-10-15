//! Persistent PowerShell sessions, one per maki session.
//!
//! pwsh runs a loop script that reads one command block per call from stdin,
//! so the process stays alive across tool calls and variables, functions, and
//! cwd persist. Each call sends the command followed by a blank line and a
//! sentinel `Write-Output` line; the reply ends when the sentinel comes back
//! with `$LASTEXITCODE`. A call that times out or dies leaves the session in
//! an unknown state, so it is killed: the next call starts a fresh process.

use maki_lua_macro::{lua_fn, lua_table};
use maki_providers::strip_provider_keys;
use maki_storage::id::MakiId;
use mlua::{Lua, Result as LuaResult, Table};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::process::{ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::api::util::pair::Pair;
use crate::plugin_permissions::PluginPermissions;

const REPL_SCRIPT: &str = r#"$PSStyle.OutputRendering='PlainText'; while ($true) { $l=[Console]::In.ReadLine(); if ($null -eq $l) { break }; $b=[System.Collections.Generic.List[string]]::new(); while ($l -ne '') { $b.Add($l); $l=[Console]::In.ReadLine(); if ($null -eq $l) { break } }; if ($b.Count -eq 0) { continue }; Invoke-Expression ($b -join "`n") | Out-String -Stream }"#;
const PWSH_ARGS: [&str; 4] = ["-NoProfile", "-NonInteractive", "-Command", REPL_SCRIPT];
const MARKER: &str = "__MAKI_PWSH_EXIT__";
const DEFAULT_TIMEOUT_SECS: u64 = 120;
const READER_BUF_SIZE: usize = 8 * 1024;
const PWSH_NOT_FOUND: &str = "PowerShell (pwsh) is not installed or not on PATH";
const SESSION_REQUIRED: &str = "session is required";
const COMMAND_REQUIRED: &str = "command is required";
const SESSION_DEAD: &str = "pwsh session died between calls";

struct PwshSession {
    stdin: Arc<Mutex<ChildStdin>>,
    lines: flume::Receiver<String>,
    pid: u32,
    started: Instant,
    seq: AtomicU32,
    reaped: Arc<AtomicBool>,
}

#[derive(Default)]
pub(crate) struct PwshStore {
    sessions: HashMap<String, Arc<PwshSession>>,
}

impl PwshStore {
    fn ensure(&mut self, session: &str) -> Result<Arc<PwshSession>, String> {
        if let Some(existing) = self.sessions.get(session) {
            return Ok(Arc::clone(existing));
        }

        let mut command = Command::new("pwsh");
        command
            .args(PWSH_ARGS)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        strip_provider_keys(&mut command);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // SAFETY: setsid is async-signal-safe, so it is sound to call in pre_exec.
            unsafe {
                command.pre_exec(|| {
                    rustix::process::setsid()?;
                    Ok(())
                });
            }
        }

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(PWSH_NOT_FOUND.into()),
            Err(e) => return Err(e.to_string()),
        };
        let pid = child.id();
        let stdin = Arc::new(Mutex::new(child.stdin.take().expect("piped stdin")));
        let (tx, rx) = flume::unbounded();

        macro_rules! spawn_reader {
            ($stream:expr, $name:expr) => {
                $stream.map(|stream| {
                    let tx = tx.clone();
                    thread::Builder::new()
                        .name($name.into())
                        .spawn(move || {
                            for line in
                                std::io::BufReader::with_capacity(READER_BUF_SIZE, stream).lines()
                            {
                                let Ok(line) = line else { break };
                                if tx.send(line).is_err() {
                                    break;
                                }
                            }
                        })
                        .ok()
                })
            };
        }
        let readers = [
            spawn_reader!(child.stdout.take(), "pwsh-stdout"),
            spawn_reader!(child.stderr.take(), "pwsh-stderr"),
        ];

        let reaped = Arc::new(AtomicBool::new(false));
        let wait_reaped = Arc::clone(&reaped);
        thread::Builder::new()
            .name("pwsh-wait".into())
            .spawn(move || {
                // The readers only return once every descendant dropped the
                // pipes, so joining them first keeps the kill target alive for
                // as long as the session is really running.
                drop(tx);
                for reader in readers.into_iter().flatten().flatten() {
                    let _ = reader.join();
                }
                let _ = child.wait();
                wait_reaped.store(true, Ordering::Relaxed);
            })
            .ok();

        let pwsh = Arc::new(PwshSession {
            stdin,
            lines: rx,
            pid,
            started: Instant::now(),
            seq: AtomicU32::new(0),
            reaped,
        });
        self.sessions.insert(session.to_owned(), Arc::clone(&pwsh));
        Ok(pwsh)
    }

    fn kill(&mut self, session: &str) {
        if let Some(pwsh) = self.sessions.remove(session) {
            kill_pwsh(&pwsh);
        }
    }

    pub(crate) fn kill_session(&mut self, session: MakiId) {
        self.kill(&session.to_string());
    }

    fn status(&self, session: &str) -> Option<(u32, u64)> {
        self.sessions
            .get(session)
            .map(|s| (s.pid, s.started.elapsed().as_secs()))
    }
}

/// Signalling a reaped pid would hit whoever the kernel handed it to next, so
/// skip sessions the wait thread already reaped. The setsid group leader keeps
/// its pgid off the free list until reaped, so the group is the right target.
fn kill_pwsh(pwsh: &PwshSession) {
    if pwsh.reaped.load(Ordering::Relaxed) {
        return;
    }
    #[cfg(unix)]
    {
        use rustix::process::{Pid, Signal, kill_process_group};
        if let Ok(raw) = i32::try_from(pwsh.pid)
            && let Some(pid) = Pid::from_raw(raw)
        {
            let _ = kill_process_group(pid, Signal::KILL);
        }
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pwsh.pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
    }
}

pub(crate) fn with_pwsh<R>(lua: &Lua, f: impl FnOnce(&mut PwshStore) -> R) -> R {
    if lua.app_data_ref::<PwshStore>().is_none() {
        lua.set_app_data(PwshStore::default());
    }
    let mut store = lua
        .app_data_mut::<PwshStore>()
        .expect("pwsh store was just installed");
    f(&mut store)
}

pub(crate) fn kill_session(lua: &Lua, session: MakiId) {
    with_pwsh(lua, |store| store.kill_session(session));
}

fn parse_marker(line: &str, seq: u32) -> Option<Option<i32>> {
    let rest = line
        .strip_prefix(MARKER)?
        .strip_prefix(&format!(" {seq} "))?;
    Some(rest.trim().parse::<i32>().ok())
}

/// Run one command in the session's persistent PowerShell process.
///
/// The process is spawned on first use and kept alive for the session, so
/// variables, functions, and the working directory carry over between calls.
/// A timeout kills the process: the next call starts a fresh one with no
/// leftover state.
///
/// @param opts table Options. Fields:
///   - session (string): session id owning this shell. Required.
///   - command (string): PowerShell command to run. Required.
///   - timeout (integer): seconds to wait for the reply. Default 120.
/// @return (table, string?) Pair: `{ output, exit_code, timed_out, dead }` on
///   success, `(nil, err)` on failure. `exit_code` is nil when PowerShell
///   tracked no native command exit code.
#[lua_fn(guard = Run, name = "exec")]
async fn pwsh_exec(lua: Lua, opts: Table) -> LuaResult<Pair<Table>> {
    let session: Option<String> = opts.get("session")?;
    let Some(session) = session else {
        return Ok((None, Some(SESSION_REQUIRED.to_owned())));
    };
    let command: Option<String> = opts.get("command")?;
    let Some(command) = command else {
        return Ok((None, Some(COMMAND_REQUIRED.to_owned())));
    };
    let timeout: u64 = opts
        .get::<Option<u64>>("timeout")?
        .unwrap_or(DEFAULT_TIMEOUT_SECS);

    let prepared = with_pwsh(&lua, |store| {
        store.ensure(&session).ok().map(|pwsh| {
            let seq = pwsh.seq.fetch_add(1, Ordering::Relaxed);
            (pwsh.stdin.clone(), pwsh.lines.clone(), seq)
        })
    });
    let Some((stdin, rx, seq)) = prepared else {
        return Ok((None, Some(PWSH_NOT_FOUND.to_owned())));
    };

    let sentinel = format!("Write-Output \"{MARKER} {seq} $LASTEXITCODE\"");
    let script = format!("{command}\n\n{sentinel}\n\n");
    let write = {
        let mut guard = stdin.lock().unwrap_or_else(|e| e.into_inner());
        guard
            .write_all(script.as_bytes())
            .and_then(|()| guard.flush())
    };
    if write.is_err() {
        with_pwsh(&lua, |store| store.kill(&session));
        return Ok((None, Some(SESSION_DEAD.to_owned())));
    }

    let deadline = smol::Timer::after(Duration::from_secs(timeout));
    futures_lite::pin!(deadline);

    let mut output = String::new();
    let mut exit_code: Option<i32> = None;
    let mut timed_out = false;
    let mut dead = false;

    enum Tick {
        Line(String),
        Closed,
        Timeout,
    }
    loop {
        let tick = futures_lite::future::or(
            async {
                match rx.recv_async().await {
                    Ok(line) => Tick::Line(line),
                    Err(_) => Tick::Closed,
                }
            },
            async {
                (&mut deadline).await;
                Tick::Timeout
            },
        )
        .await;
        match tick {
            Tick::Line(line) => {
                if let Some(code) = parse_marker(&line, seq) {
                    exit_code = code;
                    break;
                }
                if !output.is_empty() {
                    output.push('\n');
                }
                output.push_str(&line);
            }
            Tick::Closed => {
                dead = true;
                break;
            }
            Tick::Timeout => {
                timed_out = true;
                break;
            }
        }
    }

    if timed_out || dead {
        with_pwsh(&lua, |store| store.kill(&session));
    }

    let tbl = lua.create_table()?;
    tbl.set("output", output)?;
    tbl.set("exit_code", exit_code)?;
    tbl.set("timed_out", timed_out)?;
    tbl.set("dead", dead)?;
    Ok((Some(tbl), None))
}

/// Kill the session's persistent PowerShell process.
///
/// @param opts table Options. Fields:
///   - session (string): session id owning this shell. Required.
/// @return (boolean, string?) `(true, nil)` on success, `(nil, err)` on failure.
#[lua_fn(guard = Run, name = "reset")]
fn pwsh_reset(lua: &Lua, opts: Table) -> LuaResult<Pair<bool>> {
    let session: Option<String> = opts.get("session")?;
    let Some(session) = session else {
        return Ok((None, Some(SESSION_REQUIRED.to_owned())));
    };
    with_pwsh(lua, |store| store.kill(&session));
    Ok((Some(true), None))
}

/// Report whether the session's persistent PowerShell process is alive.
///
/// @param opts table Options. Fields:
///   - session (string): session id owning this shell. Required.
/// @return (table, string?) Pair: `{ alive, pid, elapsed_secs }` on success,
///   `(nil, err)` on failure.
#[lua_fn(name = "status")]
fn pwsh_status(lua: &Lua, opts: Table) -> LuaResult<Pair<Table>> {
    let session: Option<String> = opts.get("session")?;
    let Some(session) = session else {
        return Ok((None, Some(SESSION_REQUIRED.to_owned())));
    };
    let tbl = lua.create_table()?;
    match with_pwsh(lua, |store| store.status(&session)) {
        Some((pid, elapsed_secs)) => {
            tbl.set("alive", true)?;
            tbl.set("pid", pid)?;
            tbl.set("elapsed_secs", elapsed_secs)?;
        }
        None => tbl.set("alive", false)?,
    }
    Ok((Some(tbl), None))
}

lua_table! {
    /// Persistent PowerShell sessions: `maki.pwsh.exec` runs a command in a
    /// process that stays alive across tool calls, so variables and cwd
    /// persist. `reset` kills it, `status` reports it.
    "maki.pwsh" => pub(crate) fn create_pwsh_table(perms: &PluginPermissions), DOCS [
        pwsh_exec(perms),
        pwsh_reset(perms),
        pwsh_status,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const SEQ: u32 = 3;

    #[test_case("__MAKI_PWSH_EXIT__ 3 0" => Some(Some(0)); "exit_zero")]
    #[test_case("__MAKI_PWSH_EXIT__ 3 1" => Some(Some(1)); "exit_one")]
    #[test_case("__MAKI_PWSH_EXIT__ 3 " => Some(None); "no_code")]
    #[test_case("__MAKI_PWSH_EXIT__ 4 0" => None; "other_seq")]
    #[test_case("Write-Output hi" => None; "not_a_marker")]
    fn marker_shapes(line: &str) -> Option<Option<i32>> {
        parse_marker(line, SEQ)
    }

    #[test]
    fn store_kill_is_idempotent() {
        let mut store = PwshStore::default();
        store.kill("missing");
        assert!(store.sessions.is_empty());
    }

    #[test]
    fn ensure_spawns_a_live_session() {
        let mut store = PwshStore::default();
        let Ok(pwsh) = store.ensure("test-session") else {
            return;
        };
        assert_eq!(
            store.status("test-session").map(|(pid, _)| pid),
            Some(pwsh.pid)
        );
        store.kill("test-session");
        assert!(store.status("test-session").is_none());
    }
}
