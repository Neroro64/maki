//! Persistent PowerShell sessions, one per maki session.
//!
//! Requests are single-line JSON, so blank lines and here-strings stay intact.
//! Commands are dot-sourced into the same scope to preserve variables, functions,
//! and cwd. All PowerShell streams precede a completion marker on stdout. Calls
//! sharing a session serialize command submission and response consumption. A
//! command that times out or dies is killed so the next call starts fresh.

use maki_lua_macro::{lua_fn, lua_table};
use maki_providers::strip_provider_keys;
use maki_storage::id::MakiId;
use mlua::{Function, Lua, Result as LuaResult, Table};
use serde::Serialize;
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::process::{ChildStdin, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex as AsyncMutex, mpsc};
use tokio::time::{Instant as TokioInstant, sleep_until, timeout_at};

use crate::api::util::convert::TableExt;
use crate::api::util::pair::Pair;
use crate::plugin_permissions::PluginPermissions;

const REPL_SCRIPT: &str = r#"
[Console]::InputEncoding = [Console]::OutputEncoding = $OutputEncoding = [System.Text.UTF8Encoding]::new($false)
$PSStyle.OutputRendering = 'PlainText'
while ($null -ne ($__makiLine = [Console]::In.ReadLine())) {
    $__makiRequest = ConvertFrom-Json -InputObject $__makiLine -AsHashtable
    $global:LASTEXITCODE = $null
    $__makiExitCode = 0
    try {
        . ([scriptblock]::Create($__makiRequest.command)) *>&1 | ForEach-Object {
            if ($_ -is [System.Management.Automation.ErrorRecord] -and $_.FullyQualifiedErrorId -notlike 'NativeCommandError*') {
                $__makiExitCode = 1
            }
            $_
        } | Out-String -Stream
        if (-not $?) { $__makiExitCode = 1 }
        if ($null -ne $global:LASTEXITCODE -and $global:LASTEXITCODE -ne 0) {
            $__makiExitCode = $global:LASTEXITCODE
        }
    } catch {
        $__makiExitCode = 1
        $_ | Out-String -Stream
    }
    [Console]::Out.WriteLine("`n__MAKI_PWSH_EXIT__ {0} {1}", $__makiRequest.seq, $__makiExitCode)
}
"#;
const PWSH_ARGS: [&str; 4] = ["-NoProfile", "-NonInteractive", "-Command", REPL_SCRIPT];
const MARKER: &str = "__MAKI_PWSH_EXIT__";
const DEFAULT_TIMEOUT_SECS: u64 = 120;
const READER_BUF_SIZE: usize = 8 * 1024;
const PWSH_NOT_FOUND: &str = "PowerShell (pwsh) is not installed or not on PATH";
const SESSION_REQUIRED: &str = "session is required";
const COMMAND_REQUIRED: &str = "command is required";
const SESSION_DEAD: &str = "pwsh session died between calls";
const SESSION_WAIT_TIMEOUT: &str = "pwsh timed out waiting for another command in the session";
#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

#[derive(Serialize)]
struct PwshRequest<'a> {
    command: &'a str,
    seq: u32,
}

struct PwshSession {
    stdin: Arc<AsyncMutex<ChildStdin>>,
    lines: Arc<AsyncMutex<mpsc::UnboundedReceiver<String>>>,
    pid: u32,
    started: Instant,
    seq: AtomicU32,
    reaped: Arc<AtomicBool>,
}

struct PwshCall<'a> {
    lua: &'a Lua,
    session: &'a str,
    pwsh: &'a Arc<PwshSession>,
    completed: bool,
}

impl Drop for PwshCall<'_> {
    fn drop(&mut self) {
        if !self.completed {
            with_pwsh(self.lua, |store| {
                if store.is_current(self.session, self.pwsh) {
                    store.kill(self.session);
                }
            });
        }
    }
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
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(CREATE_NO_WINDOW);
        }
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
        let stdin = Arc::new(AsyncMutex::new(child.stdin.take().expect("piped stdin")));
        let (tx, rx) = mpsc::unbounded_channel();

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
            lines: Arc::new(AsyncMutex::new(rx)),
            pid,
            started: Instant::now(),
            seq: AtomicU32::new(0),
            reaped,
        });
        self.sessions.insert(session.to_owned(), Arc::clone(&pwsh));
        Ok(pwsh)
    }

    fn is_current(&self, session: &str, pwsh: &Arc<PwshSession>) -> bool {
        self.sessions
            .get(session)
            .is_some_and(|current| Arc::ptr_eq(current, pwsh))
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

impl Drop for PwshStore {
    fn drop(&mut self) {
        for session in self.sessions.values() {
            kill_pwsh(session);
        }
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
            .status();
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
/// Calls sharing a session run one at a time. The timeout includes time spent
/// waiting for an earlier call. A timeout while waiting leaves the running
/// command alone. A running command that times out is killed: the next call
/// starts a fresh process with no leftover state.
///
/// @param opts table Options. Fields:
///   - session (string): session id owning this shell. Required.
///   - command (string): PowerShell command to run. Required.
///   - timeout (integer): seconds to wait, including queued time. Default 120.
///   - on_output (function?): synchronous callback receiving each output line,
///     including the blank separator before completion. Must not yield.
///   - collect_output (boolean): include output in the result. Default true;
///     set false when on_output already owns the capture to avoid duplicating it.
/// @return (table, string?) Pair: `{ output, exit_code, timed_out, dead }` on
///   success, `(nil, err)` on failure. `exit_code` is the native exit code,
///   or 1 for a PowerShell error and 0 for success.
#[lua_fn(guard = Run, name = "exec")]
async fn pwsh_exec(lua: Lua, opts: Table) -> LuaResult<Pair<Table>> {
    let session: Option<String> = opts.opt("session")?;
    let Some(session) = session else {
        return Ok((None, Some(SESSION_REQUIRED.to_owned())));
    };
    let command: Option<String> = opts.opt("command")?;
    let Some(command) = command else {
        return Ok((None, Some(COMMAND_REQUIRED.to_owned())));
    };
    let timeout: u64 = opts.opt::<u64>("timeout")?.unwrap_or(DEFAULT_TIMEOUT_SECS);
    let on_output: Option<Function> = opts.opt("on_output")?;
    let collect_output = opts.opt_bool("collect_output").unwrap_or(true);

    let deadline = TokioInstant::now() + Duration::from_secs(timeout);
    let prepared = timeout_at(deadline, async {
        loop {
            let pwsh = with_pwsh(&lua, |store| store.ensure(&session))?;
            let stdin = Arc::clone(&pwsh.stdin).lock_owned().await;
            // A reset or timeout may replace the process while this call waits.
            if with_pwsh(&lua, |store| store.is_current(&session, &pwsh)) {
                return Ok((pwsh, stdin));
            }
        }
    })
    .await
    .unwrap_or_else(|_| Err(SESSION_WAIT_TIMEOUT.to_owned()));
    let (pwsh, mut stdin) = match prepared {
        Ok(prepared) => prepared,
        Err(err) => return Ok((None, Some(err))),
    };
    let mut call = PwshCall {
        lua: &lua,
        session: &session,
        pwsh: &pwsh,
        completed: false,
    };
    let seq = pwsh.seq.fetch_add(1, Ordering::Relaxed);
    let rx = &pwsh.lines;
    let mut script = serde_json::to_string(&PwshRequest {
        command: &command,
        seq,
    })
    .map_err(mlua::Error::external)?;
    script.push('\n');
    let write = stdin
        .write_all(script.as_bytes())
        .and_then(|()| stdin.flush());
    if write.is_err() {
        return Ok((None, Some(SESSION_DEAD.to_owned())));
    }

    let mut rx = rx.lock().await;
    let deadline = sleep_until(deadline);
    tokio::pin!(deadline);

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
        let tick = tokio::select! {
            biased;
            maybe_line = rx.recv() => match maybe_line {
                Some(line) => Tick::Line(line),
                None => Tick::Closed,
            },
            () = &mut deadline => Tick::Timeout,
        };
        match tick {
            Tick::Line(line) => {
                if let Some(code) = parse_marker(&line, seq) {
                    call.completed = true;
                    exit_code = code;
                    // Strip the line ending and the marker's separating newline.
                    if collect_output {
                        output.pop();
                        if output.ends_with('\n') {
                            output.pop();
                        }
                    }
                    break;
                }
                if let Some(callback) = &on_output {
                    callback.call::<()>(line.as_str())?;
                }
                if collect_output {
                    output.push_str(&line);
                    output.push('\n');
                }
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

    let tbl = lua.create_table()?;
    if collect_output {
        tbl.set("output", output)?;
    }
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
    let session: Option<String> = opts.opt("session")?;
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
    let session: Option<String> = opts.opt("session")?;
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
    use std::sync::Arc;
    use super::{SESSION_WAIT_TIMEOUT, parse_marker, pwsh_exec, with_pwsh};
    use futures::FutureExt;
    use mlua::{Lua, Table};
    use test_case::test_case;

    const SEQ: u32 = 3;
    const SESSION: &str = "test-session";
    const PROBE_VALUE: &str = "42";
    const TIMEOUT_SECS: u64 = 10;

    fn command_opts(lua: &Lua, command: &str, timeout: u64) -> Table {
        let opts = lua.create_table().unwrap();
        opts.set("session", SESSION).unwrap();
        opts.set("command", command).unwrap();
        opts.set("timeout", timeout).unwrap();
        opts
    }

    #[test_case("__MAKI_PWSH_EXIT__ 3 0" => Some(Some(0)); "exit_zero")]
    #[test_case("__MAKI_PWSH_EXIT__ 3 1" => Some(Some(1)); "exit_one")]
    #[test_case("__MAKI_PWSH_EXIT__ 3 " => Some(None); "no_code")]
    #[test_case("__MAKI_PWSH_EXIT__ 4 0" => None; "other_seq")]
    #[test_case("Write-Output hi" => None; "not_a_marker")]
    fn marker_shapes(line: &str) -> Option<Option<i32>> {
        parse_marker(line, SEQ)
    }

    #[test_case(false; "cancelled_command")]
    #[test_case(true; "reset_command")]
    fn queued_call_recovers_after_interruption(reset: bool) {
        crate::test_support::runtime().block_on(async {
            let lua = Lua::new();
            let mut active = Box::pin(pwsh_exec(
                lua.clone(),
                command_opts(&lua, "[Console]::In.ReadLine()", TIMEOUT_SECS),
            ));
            assert!(active.as_mut().now_or_never().is_none());
            let mut queued = Box::pin(pwsh_exec(
                lua.clone(),
                command_opts(&lua, "$probe = 42; $probe", TIMEOUT_SECS),
            ));
            assert!(queued.as_mut().now_or_never().is_none());
            if reset {
                with_pwsh(&lua, |store| {
                    store.kill(SESSION);
                    store.ensure(SESSION).unwrap();
                });
            }
            drop(active);
            let (output, error) = queued.await.unwrap();
            assert!(error.is_none(), "{error:?}");
            let output = output.unwrap();
            assert_eq!(output.get::<String>("output").unwrap(), PROBE_VALUE);
            assert_eq!(output.get::<i32>("exit_code").unwrap(), 0);
            let (output, error) =
                pwsh_exec(lua.clone(), command_opts(&lua, "$probe", TIMEOUT_SECS))
                    .await
                    .unwrap();
            assert!(error.is_none(), "{error:?}");
            assert_eq!(
                output.unwrap().get::<String>("output").unwrap(),
                PROBE_VALUE
            );
        });
    }

    #[test_case(0; "expired_queue_deadline")]
    fn queued_timeout_preserves_the_running_session(timeout: u64) {
        crate::test_support::runtime().block_on(async {
            let lua = Lua::new();
            pwsh_exec(lua.clone(), command_opts(&lua, "$probe = 42", TIMEOUT_SECS))
                .await
                .unwrap();
            let pwsh = with_pwsh(&lua, |store| store.ensure(SESSION)).unwrap();
            let held = Arc::clone(&pwsh.stdin).lock_owned().await;
            let (output, error) = pwsh_exec(lua.clone(), command_opts(&lua, "$probe = 0", timeout))
                .await
                .unwrap();
            assert!(output.is_none());
            assert_eq!(error.as_deref(), Some(SESSION_WAIT_TIMEOUT));
            drop(held);
            let (output, error) =
                pwsh_exec(lua.clone(), command_opts(&lua, "$probe", TIMEOUT_SECS))
                    .await
                    .unwrap();
            assert!(error.is_none(), "{error:?}");
            assert_eq!(
                output.unwrap().get::<String>("output").unwrap(),
                PROBE_VALUE
            );
        });
    }
}
