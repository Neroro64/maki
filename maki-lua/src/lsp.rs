use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use flume::{Receiver, TryRecvError};
use futures_lite::future;
use futures_lite::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use maki_agent::ChildGuard;
use maki_providers::strip_provider_keys;
use serde::Deserialize;
use serde_json::{Number, Value, json};
use smol::lock::Mutex;
use smol::process::{ChildStdin, Command as AsyncCommand};
use url::Url;

const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;
const MAX_HEADER_BYTES: usize = 8 * 1024;
const MAX_PENDING_MESSAGES: usize = 16;
const MAX_TIMEOUT: Duration = Duration::from_secs(300);
const DIAGNOSTICS_TIMEOUT: &str =
    "LSP diagnostics timed out before a current document report arrived";
const SPECULATION_TIMEOUT: &str =
    "LSP speculation timed out before diagnostics and restoration completed";
const APPLY_EDIT_REJECTED: &str = "Speculation never applies workspace edits";
const INVALID_LINE_RANGE: &str =
    "start_line and end_line must be inclusive, 1-based lines within the source file";
const INVALID_FRAME: &str = "Invalid LSP Content-Length framing";
const INVALID_DIAGNOSTIC: &str = "LSP server returned an invalid diagnostic range or severity";

type Reports = HashMap<String, Vec<Diagnostic>>;
type ClientSlot = Arc<Mutex<Option<Client>>>;

#[derive(Default)]
pub(crate) struct LspManager {
    clients: Mutex<HashMap<Arc<ClientKey>, ClientSlot>>,
}

pub(crate) struct Speculation {
    pub file_path: PathBuf,
    pub root: PathBuf,
    pub language_id: String,
    pub command: Vec<String>,
    pub start_line: usize,
    pub end_line: usize,
    pub replacement_text: String,
    pub timeout: Duration,
}

#[derive(Eq, Hash, PartialEq)]
struct ClientKey {
    root: PathBuf,
    command: Vec<String>,
}

struct Patch {
    text: String,
    start: usize,
    removed: usize,
    added: usize,
}

struct Client {
    child: ChildGuard,
    stdin: ChildStdin,
    messages: Receiver<Result<Value, String>>,
    _reader: smol::Task<()>,
    root_uri: String,
    root_name: String,
    pull_diagnostics: bool,
    diagnostic_identifier: Option<String>,
    next_request: u64,
    version: i64,
    document: Option<String>,
    publications: HashMap<String, Publication>,
    progress: HashSet<String>,
    quiescent: Option<bool>,
}

struct Publication {
    version: Option<i64>,
    diagnostics: Vec<Diagnostic>,
}

#[derive(Deserialize)]
struct PublishedDiagnostics {
    uri: String,
    version: Option<i64>,
    diagnostics: Vec<Diagnostic>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct DiagnosticReport {
    kind: String,
    items: Option<Vec<Diagnostic>>,
    #[serde(default)]
    related_documents: HashMap<String, DiagnosticReport>,
}

#[derive(Deserialize)]
struct Diagnostic {
    range: DiagnosticRange,
    #[serde(default = "error_severity")]
    severity: u64,
    message: String,
    source: Option<String>,
    code: Option<DiagnosticCode>,
}

#[derive(Deserialize)]
struct DiagnosticRange {
    start: Position,
    end: Position,
}

#[derive(Deserialize)]
struct Position {
    line: usize,
    character: usize,
}

#[derive(Deserialize, Eq, Hash, PartialEq)]
#[serde(untagged)]
enum DiagnosticCode {
    String(String),
    Number(Number),
}

#[derive(Eq, Hash, PartialEq)]
struct DiagnosticKey<'a> {
    uri: &'a str,
    start_line: usize,
    start_character: usize,
    end_line: usize,
    end_character: usize,
    severity: u64,
    message: &'a str,
    source: Option<&'a str>,
    code: Option<&'a DiagnosticCode>,
}

impl LspManager {
    pub(crate) async fn speculate(&self, input: Speculation) -> Result<Value, String> {
        if input.timeout.is_zero() {
            return Err("LSP timeout must be greater than zero".into());
        }
        let timeout = input.timeout.min(MAX_TIMEOUT);
        bounded(
            timeout,
            self.speculate_inner(input, timeout),
            SPECULATION_TIMEOUT,
        )
        .await
    }

    async fn speculate_inner(
        &self,
        input: Speculation,
        timeout: Duration,
    ) -> Result<Value, String> {
        if input.command.first().is_none_or(String::is_empty) {
            return Err("LSP server command must not be empty".into());
        }
        let root = smol::fs::canonicalize(&input.root).await.map_err(|error| {
            format!(
                "Cannot resolve LSP workspace {}: {error}",
                input.root.display()
            )
        })?;
        if !smol::fs::metadata(&root)
            .await
            .map_err(|error| format!("Cannot inspect LSP workspace {}: {error}", root.display()))?
            .is_dir()
        {
            return Err(format!(
                "LSP workspace is not a directory: {}",
                root.display()
            ));
        }
        let file_path = smol::fs::canonicalize(&input.file_path)
            .await
            .map_err(|error| {
                format!(
                    "Cannot resolve LSP source {}: {error}",
                    input.file_path.display()
                )
            })?;
        if !file_path.starts_with(&root) {
            return Err(format!(
                "LSP source is outside workspace {}",
                root.display()
            ));
        }
        let uri = Url::from_file_path(&file_path)
            .map_err(|()| {
                format!(
                    "Cannot convert LSP source to a file URI: {}",
                    file_path.display()
                )
            })?
            .to_string();
        let key = Arc::new(ClientKey {
            root,
            command: input.command,
        });
        let slot = {
            let mut clients = self.clients.lock().await;
            Arc::clone(clients.entry(Arc::clone(&key)).or_default())
        };
        let mut pooled = slot.lock().await;
        let source = smol::fs::read_to_string(&file_path)
            .await
            .map_err(|error| format!("Cannot read LSP source {}: {error}", file_path.display()))?;
        let patch = patch_lines(
            &source,
            input.start_line,
            input.end_line,
            &input.replacement_text,
        )?;
        // An in-flight client is owned here, never in the pool: cancellation drops its process guard.
        let mut client = match pooled.take() {
            Some(client) => client,
            None => {
                Client::start(
                    &file_path,
                    &key.root,
                    &input.language_id,
                    &key.command,
                    timeout,
                )
                .await?
            }
        };
        let result = client
            .trial(
                &file_path,
                &uri,
                &input.language_id,
                &source,
                &patch,
                timeout,
            )
            .await;
        match result {
            Ok(diagnostics) => {
                *pooled = Some(client);
                let clean = diagnostics.is_empty();
                Ok(json!({"file_path": file_path, "diagnostics": diagnostics, "clean": clean}))
            }
            Err(error) => {
                client.child.kill_and_reap().await;
                Err(error)
            }
        }
    }
}

impl Client {
    async fn start(
        file_path: &Path,
        root: &Path,
        language_id: &str,
        command: &[String],
        timeout: Duration,
    ) -> Result<Self, String> {
        let executable = command
            .first()
            .ok_or("LSP server command must not be empty")?;
        let mut process = Command::new(executable);
        process.args(&command[1..]).current_dir(root);
        strip_provider_keys(&mut process);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // SAFETY: setsid is async-signal-safe and gives ChildGuard an isolated process group.
            unsafe {
                process.pre_exec(|| {
                    rustix::process::setsid()?;
                    Ok(())
                });
            }
        }
        let mut process: AsyncCommand = process.into();
        process
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = process
            .spawn()
            .map_err(|error| format!("LSP server unavailable ({executable}): {error}"))?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take();
        let guard = ChildGuard::new(child);
        let stdin = stdin.ok_or("LSP server stdin is unavailable")?;
        let stdout = stdout.ok_or("LSP server stdout is unavailable")?;
        let root_uri = Url::from_directory_path(root)
            .map_err(|()| {
                format!(
                    "Cannot convert LSP workspace to a file URI: {}",
                    root.display()
                )
            })?
            .to_string();
        let root_name = root
            .file_name()
            .unwrap_or(root.as_os_str())
            .to_string_lossy()
            .into_owned();
        let rust_analyzer = Path::new(executable)
            .file_stem()
            .is_some_and(|name| name == "rust-analyzer");
        let (sender, messages) = flume::bounded(MAX_PENDING_MESSAGES);
        let reader = smol::spawn(async move {
            let mut stdout = BufReader::new(stdout);
            loop {
                let message = read_frame(&mut stdout).await;
                let failed = message.is_err();
                if sender.send_async(message).await.is_err() || failed {
                    break;
                }
            }
        });
        let mut client = Self {
            child: guard,
            stdin,
            messages,
            _reader: reader,
            root_uri,
            root_name,
            pull_diagnostics: false,
            diagnostic_identifier: None,
            next_request: 1,
            version: 0,
            document: None,
            publications: HashMap::new(),
            progress: HashSet::new(),
            quiescent: rust_analyzer.then_some(false),
        };
        let response = client.request("initialize", json!({
            "processId": std::process::id(),
            "clientInfo": {"name": "maki-speculate"},
            "rootUri": client.root_uri,
            "workspaceFolders": [{"uri": client.root_uri, "name": client.root_name}],
            "capabilities": {
                "workspace": {"configuration": true, "workspaceFolders": true, "applyEdit": false},
                "textDocument": {
                    "synchronization": {"dynamicRegistration": false, "didSave": false, "willSave": false, "willSaveWaitUntil": false},
                    "publishDiagnostics": {"versionSupport": true, "relatedInformation": true},
                    "diagnostic": {"dynamicRegistration": false, "relatedDocumentSupport": true}
                },
                "window": {"workDoneProgress": true},
                "experimental": {"serverStatusNotification": true}
            },
            "initializationOptions": if rust_analyzer { rust_analyzer_settings() } else { Value::Null },
            "trace": "off"
        }), timeout).await?;
        let capabilities = response
            .get("capabilities")
            .ok_or("LSP initialize response omitted capabilities")?;
        let sync = capabilities.get("textDocumentSync").and_then(|sync| {
            sync.as_u64()
                .or_else(|| sync.get("change").and_then(Value::as_u64))
        });
        if !matches!(sync, Some(1 | 2)) {
            return Err(format!(
                "LSP server does not support virtual document changes for {language_id}: {}",
                file_path.display()
            ));
        }
        if let Some(provider) = capabilities.get("diagnosticProvider") {
            client.pull_diagnostics = provider.is_object() || provider.as_bool() == Some(true);
            client.diagnostic_identifier = provider
                .get("identifier")
                .and_then(Value::as_str)
                .map(str::to_owned);
        }
        if response
            .get("serverInfo")
            .and_then(|info| info.get("name"))
            .and_then(Value::as_str)
            .is_some_and(|name| name.contains("rust-analyzer"))
        {
            client.quiescent.get_or_insert(false);
        }
        client.notify("initialized", json!({})).await?;
        if client.quiescent.is_some() {
            client
                .notify(
                    "workspace/didChangeConfiguration",
                    json!({"settings": {"rust-analyzer": rust_analyzer_settings()}}),
                )
                .await?;
        }
        client.wait_ready(timeout).await?;
        Ok(client)
    }

    async fn trial(
        &mut self,
        file_path: &Path,
        uri: &str,
        language_id: &str,
        source: &str,
        patch: &Patch,
        timeout: Duration,
    ) -> Result<Vec<Value>, String> {
        self.publications.clear();
        self.document = Some(uri.to_owned());
        self.bump_version()?;
        self.notify(
            "textDocument/didOpen",
            json!({"textDocument": {
                "uri": uri, "languageId": language_id, "version": self.version, "text": source
            }}),
        )
        .await?;
        let baseline = self.diagnostics(uri, timeout).await?;
        self.change(uri, &patch.text).await?;
        let candidate = self.diagnostics(uri, timeout).await?;
        let diagnostics = filter_diagnostics(&baseline, candidate, uri, patch)?;
        let disk_source = smol::fs::read_to_string(file_path).await.map_err(|error| {
            format!(
                "Cannot read LSP source for restoration {}: {error}",
                file_path.display()
            )
        })?;
        self.change(uri, &disk_source).await?;
        self.diagnostics(uri, timeout).await?;
        self.notify(
            "textDocument/didClose",
            json!({"textDocument": {"uri": uri}}),
        )
        .await?;
        self.document = None;
        self.publications.clear();
        Ok(diagnostics)
    }

    fn bump_version(&mut self) -> Result<(), String> {
        self.version = self
            .version
            .checked_add(1)
            .ok_or("LSP document version exhausted")?;
        Ok(())
    }

    async fn change(&mut self, uri: &str, text: &str) -> Result<(), String> {
        self.bump_version()?;
        self.publications.clear();
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": {"uri": uri, "version": self.version},
                "contentChanges": [{"text": text}]
            }),
        )
        .await
    }

    async fn diagnostics(&mut self, uri: &str, timeout: Duration) -> Result<Reports, String> {
        bounded(
            timeout,
            self.collect_diagnostics(uri, timeout),
            DIAGNOSTICS_TIMEOUT,
        )
        .await
    }

    async fn collect_diagnostics(
        &mut self,
        uri: &str,
        timeout: Duration,
    ) -> Result<Reports, String> {
        if self.pull_diagnostics {
            let mut params = json!({"textDocument": {"uri": uri}});
            if let Some(identifier) = &self.diagnostic_identifier {
                params["identifier"] = json!(identifier);
            }
            let response = self
                .request("textDocument/diagnostic", params, timeout)
                .await?;
            let report: DiagnosticReport = serde_json::from_value(response)
                .map_err(|error| format!("Invalid LSP diagnostic report: {error}"))?;
            let mut reports = HashMap::new();
            report.add_to(uri.to_owned(), &mut reports)?;
            self.wait_ready(timeout).await?;
            self.drain_messages().await?;
            for (related_uri, publication) in self.publications.drain() {
                if related_uri != uri && !reports.contains_key(&related_uri) {
                    reports.insert(related_uri, publication.diagnostics);
                }
            }
            return Ok(reports);
        }
        loop {
            self.drain_messages().await?;
            if self.ready()
                && self
                    .publications
                    .get(uri)
                    .is_some_and(|publication| publication.version == Some(self.version))
            {
                return Ok(self
                    .publications
                    .drain()
                    .map(|(uri, publication)| (uri, publication.diagnostics))
                    .collect());
            }
            let message = self.receive(timeout, DIAGNOSTICS_TIMEOUT).await?;
            self.dispatch(message).await?;
        }
    }

    fn ready(&self) -> bool {
        self.quiescent != Some(false) && self.progress.is_empty()
    }

    async fn wait_ready(&mut self, timeout: Duration) -> Result<(), String> {
        self.drain_messages().await?;
        while !self.ready() {
            let message = self.receive(timeout, DIAGNOSTICS_TIMEOUT).await?;
            self.dispatch(message).await?;
        }
        Ok(())
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<(), String> {
        self.send(&json!({"jsonrpc": "2.0", "method": method, "params": params}))
            .await
    }

    async fn request(
        &mut self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> Result<Value, String> {
        let id = self.next_request;
        self.next_request = id.checked_add(1).ok_or("LSP request id exhausted")?;
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await?;
        loop {
            let message = self.receive(timeout, DIAGNOSTICS_TIMEOUT).await?;
            if let Some(mut response) = self.dispatch(message).await? {
                if response.get("id").and_then(Value::as_u64) != Some(id) {
                    continue;
                }
                if let Some(error) = response.get("error") {
                    return Err(format!("LSP {method} failed: {error}"));
                }
                return response
                    .as_object_mut()
                    .and_then(|object| object.remove("result"))
                    .ok_or_else(|| format!("LSP {method} response omitted result"));
            }
        }
    }

    async fn receive(&self, timeout: Duration, timeout_message: &str) -> Result<Value, String> {
        bounded(
            timeout,
            async {
                self.messages
                    .recv_async()
                    .await
                    .map_err(|error| format!("LSP server unavailable: {error}"))?
            },
            timeout_message,
        )
        .await
    }

    async fn drain_messages(&mut self) -> Result<(), String> {
        loop {
            match self.messages.try_recv() {
                Ok(message) => {
                    self.dispatch(message?).await?;
                }
                Err(TryRecvError::Empty) => return Ok(()),
                Err(TryRecvError::Disconnected) => {
                    return Err("LSP server unavailable: stdout reader stopped".into());
                }
            }
        }
    }

    async fn dispatch(&mut self, mut message: Value) -> Result<Option<Value>, String> {
        future::yield_now().await;
        if message.get("jsonrpc").and_then(Value::as_str) != Some("2.0") {
            return Err("LSP server returned an invalid JSON-RPC envelope".into());
        }
        if message.get("method").is_none() {
            return Ok(Some(message));
        }
        let object = message
            .as_object_mut()
            .ok_or("LSP server returned a non-object message")?;
        let method = match object.remove("method") {
            Some(Value::String(method)) => method,
            _ => return Err("LSP server returned an invalid method".into()),
        };
        let id = object.remove("id");
        let params = object.remove("params").unwrap_or(Value::Null);
        if let Some(id) = id {
            let reply = server_reply(&method, &params, &self.root_uri, &self.root_name);
            let mut response = json!({"jsonrpc": "2.0", "id": id});
            match reply {
                Ok(result) => response["result"] = result,
                Err(error) => response["error"] = error,
            }
            self.send(&response).await?;
            return Ok(None);
        }
        match method.as_str() {
            "textDocument/publishDiagnostics" => {
                let report: PublishedDiagnostics = serde_json::from_value(params)
                    .map_err(|error| format!("Invalid LSP published diagnostics: {error}"))?;
                validate_diagnostics(&report.diagnostics)?;
                if publication_is_current(
                    self.document.as_deref(),
                    self.version,
                    &report.uri,
                    report.version,
                ) {
                    self.publications.insert(
                        report.uri,
                        Publication {
                            version: report.version,
                            diagnostics: report.diagnostics,
                        },
                    );
                }
            }
            "experimental/serverStatus" => {
                self.quiescent = params.get("quiescent").and_then(Value::as_bool);
                if params.get("health").and_then(Value::as_str) == Some("error") {
                    return Err(format!(
                        "LSP server unavailable: {}",
                        params
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("workspace loading failed")
                    ));
                }
            }
            "$/progress" => {
                if let Some(token) = params.get("token") {
                    let token = token.to_string();
                    match params
                        .get("value")
                        .and_then(|value| value.get("kind"))
                        .and_then(Value::as_str)
                    {
                        Some("begin") => {
                            self.progress.insert(token);
                        }
                        Some("end") => {
                            self.progress.remove(&token);
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
        Ok(None)
    }

    async fn send(&mut self, message: &Value) -> Result<(), String> {
        let body = serde_json::to_vec(message)
            .map_err(|error| format!("Cannot encode LSP message: {error}"))?;
        if body.len() > MAX_FRAME_BYTES {
            return Err(format!("LSP message exceeds {MAX_FRAME_BYTES} bytes"));
        }
        let header = format!("Content-Length: {}\r\n\r\n", body.len());
        self.stdin
            .write_all(header.as_bytes())
            .await
            .map_err(|error| format!("Cannot write LSP header: {error}"))?;
        self.stdin
            .write_all(&body)
            .await
            .map_err(|error| format!("Cannot write LSP message: {error}"))?;
        self.stdin
            .flush()
            .await
            .map_err(|error| format!("Cannot flush LSP message: {error}"))
    }
}

impl DiagnosticReport {
    fn add_to(self, uri: String, reports: &mut Reports) -> Result<(), String> {
        if self.kind != "full" {
            return Err(
                "LSP returned an unchanged diagnostic report without a previous result id".into(),
            );
        }
        let diagnostics = self
            .items
            .ok_or("LSP full diagnostic report omitted items")?;
        validate_diagnostics(&diagnostics)?;
        reports.insert(uri, diagnostics);
        for (related_uri, report) in self.related_documents {
            report.add_to(related_uri, reports)?;
        }
        Ok(())
    }
}

fn rust_analyzer_settings() -> Value {
    json!({"checkOnSave": false, "cargo": {"buildScripts": {"enable": false}}, "procMacro": {"enable": false}})
}

fn server_reply(
    method: &str,
    params: &Value,
    root_uri: &str,
    root_name: &str,
) -> Result<Value, Value> {
    match method {
        "workspace/configuration" => {
            let settings = rust_analyzer_settings();
            Ok(Value::Array(
                params
                    .get("items")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .map(|item| {
                                let section =
                                    item.get("section").and_then(Value::as_str).unwrap_or("");
                                if section.is_empty() {
                                    return json!({"rust-analyzer": settings});
                                }
                                if section == "rust-analyzer" {
                                    return settings.clone();
                                }
                                section
                                    .strip_prefix("rust-analyzer.")
                                    .and_then(|path| {
                                        path.split('.')
                                            .try_fold(&settings, |value, part| value.get(part))
                                    })
                                    .cloned()
                                    .unwrap_or(Value::Null)
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            ))
        }
        "workspace/workspaceFolders" => Ok(json!([{"uri": root_uri, "name": root_name}])),
        "workspace/applyEdit" => {
            Ok(json!({"applied": false, "failureReason": APPLY_EDIT_REJECTED}))
        }
        "client/registerCapability"
        | "client/unregisterCapability"
        | "window/workDoneProgress/create"
        | "window/showMessageRequest"
        | "workspace/diagnostic/refresh"
        | "workspace/semanticTokens/refresh"
        | "workspace/inlayHint/refresh"
        | "workspace/codeLens/refresh"
        | "workspace/inlineValue/refresh" => Ok(Value::Null),
        _ => Err(
            json!({"code": -32601, "message": format!("Unsupported LSP server request: {method}")}),
        ),
    }
}

async fn bounded<T>(
    timeout: Duration,
    operation: impl Future<Output = Result<T, String>>,
    message: &str,
) -> Result<T, String> {
    future::race(operation, async {
        smol::Timer::after(timeout).await;
        Err(message.to_owned())
    })
    .await
}

async fn read_frame(reader: &mut (impl AsyncRead + Unpin)) -> Result<Value, String> {
    let mut header = [0; MAX_HEADER_BYTES];
    let mut header_length = 0;
    loop {
        if header_length == MAX_HEADER_BYTES {
            return Err(INVALID_FRAME.into());
        }
        reader
            .read_exact(&mut header[header_length..header_length + 1])
            .await
            .map_err(|error| format!("LSP server unavailable while reading header: {error}"))?;
        header_length += 1;
        if header[..header_length].ends_with(b"\r\n\r\n") {
            break;
        }
    }
    let length = content_length(&header[..header_length])?;
    let mut body = vec![0; length];
    reader
        .read_exact(&mut body)
        .await
        .map_err(|error| format!("LSP server unavailable while reading message: {error}"))?;
    serde_json::from_slice(&body).map_err(|error| format!("Invalid LSP JSON message: {error}"))
}

fn content_length(header: &[u8]) -> Result<usize, String> {
    let header = std::str::from_utf8(header).map_err(|_| INVALID_FRAME.to_owned())?;
    let mut length = None;
    for line in header.split("\r\n").filter(|line| !line.is_empty()) {
        let (name, value) = line.split_once(':').ok_or(INVALID_FRAME)?;
        if name.eq_ignore_ascii_case("Content-Length") {
            if length.is_some() {
                return Err(INVALID_FRAME.into());
            }
            let parsed = value
                .trim()
                .parse::<usize>()
                .map_err(|_| INVALID_FRAME.to_owned())?;
            if parsed > MAX_FRAME_BYTES {
                return Err(format!("LSP frame exceeds {MAX_FRAME_BYTES} bytes"));
            }
            length = Some(parsed);
        }
    }
    length.ok_or_else(|| INVALID_FRAME.into())
}

fn patch_lines(
    source: &str,
    start_line: usize,
    end_line: usize,
    replacement: &str,
) -> Result<Patch, String> {
    let mut starts = vec![0];
    starts.extend(source.bytes().enumerate().filter_map(|(index, byte)| {
        (byte == b'\n' && index + 1 < source.len()).then_some(index + 1)
    }));
    if start_line == 0 || end_line < start_line || end_line > starts.len() {
        return Err(INVALID_LINE_RANGE.into());
    }
    let start = starts[start_line - 1];
    let end = starts.get(end_line).copied().unwrap_or(source.len());
    let separator = if !replacement.is_empty()
        && !replacement.ends_with('\n')
        && source[start..end].ends_with('\n')
    {
        if source[start..end].ends_with("\r\n") {
            "\r\n"
        } else {
            "\n"
        }
    } else {
        ""
    };
    let mut text =
        String::with_capacity(source.len() - (end - start) + replacement.len() + separator.len());
    text.push_str(&source[..start]);
    text.push_str(replacement);
    text.push_str(separator);
    text.push_str(&source[end..]);
    let added = replacement.bytes().filter(|byte| *byte == b'\n').count()
        + usize::from(!replacement.is_empty() && !replacement.ends_with('\n'));
    Ok(Patch {
        text,
        start: start_line - 1,
        removed: end_line - start_line + 1,
        added,
    })
}

fn error_severity() -> u64 {
    1
}

fn publication_is_current(
    document: Option<&str>,
    version: i64,
    uri: &str,
    publication_version: Option<i64>,
) -> bool {
    document != Some(uri) || publication_version == Some(version)
}

fn validate_diagnostics(diagnostics: &[Diagnostic]) -> Result<(), String> {
    if diagnostics.iter().any(|diagnostic| {
        !(1..=4).contains(&diagnostic.severity)
            || (diagnostic.range.end.line, diagnostic.range.end.character)
                < (
                    diagnostic.range.start.line,
                    diagnostic.range.start.character,
                )
            || diagnostic.range.end.line == usize::MAX
    }) {
        return Err(INVALID_DIAGNOSTIC.into());
    }
    Ok(())
}

fn diagnostic_key<'a>(
    uri: &'a str,
    diagnostic: &'a Diagnostic,
    start_line: usize,
    end_line: usize,
) -> DiagnosticKey<'a> {
    DiagnosticKey {
        uri,
        start_line,
        start_character: diagnostic.range.start.character,
        end_line,
        end_character: diagnostic.range.end.character,
        severity: diagnostic.severity,
        message: &diagnostic.message,
        source: diagnostic.source.as_deref(),
        code: diagnostic.code.as_ref(),
    }
}

fn original_line(line: usize, patch: &Patch) -> Option<usize> {
    if line < patch.start {
        Some(line)
    } else if line < patch.start.checked_add(patch.added)? {
        None
    } else {
        line.checked_sub(patch.added)?.checked_add(patch.removed)
    }
}

fn filter_diagnostics(
    baseline: &Reports,
    candidate: Reports,
    edited_uri: &str,
    patch: &Patch,
) -> Result<Vec<Value>, String> {
    let baseline: HashSet<_> = baseline
        .iter()
        .flat_map(|(uri, diagnostics)| {
            diagnostics.iter().map(move |diagnostic| {
                diagnostic_key(
                    uri,
                    diagnostic,
                    diagnostic.range.start.line,
                    diagnostic.range.end.line,
                )
            })
        })
        .collect();
    let mut output = Vec::new();
    for (uri, diagnostics) in candidate {
        let file_path = Url::parse(&uri)
            .map_err(|error| format!("Invalid LSP diagnostic URI: {error}"))?
            .to_file_path()
            .map_err(|()| format!("LSP diagnostic is not a file URI: {uri}"))?;
        for diagnostic in diagnostics {
            if diagnostic.severity > 2 {
                continue;
            }
            let original = if uri == edited_uri {
                let end = if diagnostic.range.start.line < patch.start
                    && diagnostic.range.end.line == patch.start
                    && diagnostic.range.end.character == 0
                {
                    Some(patch.start)
                } else {
                    original_line(diagnostic.range.end.line, patch)
                };
                original_line(diagnostic.range.start.line, patch)
                    .zip(end)
                    .filter(|(start, end)| {
                        !(*start < patch.start
                            && (*end > patch.start
                                || (*end == patch.start && diagnostic.range.end.character > 0)))
                    })
            } else {
                Some((diagnostic.range.start.line, diagnostic.range.end.line))
            };
            if original.is_some_and(|(start, end)| {
                baseline.contains(&diagnostic_key(&uri, &diagnostic, start, end))
            }) {
                continue;
            }
            let mut item = json!({
                "file_path": file_path,
                "line": diagnostic.range.start.line + 1,
                "end_line": diagnostic.range.end.line + 1,
                "severity": if diagnostic.severity == 1 { "error" } else { "warning" }
            });
            item["message"] = Value::String(diagnostic.message);
            if let Some(source) = diagnostic.source {
                item["source"] = Value::String(source);
            }
            if let Some(code) = diagnostic.code {
                item["code"] = match code {
                    DiagnosticCode::String(code) => Value::String(code),
                    DiagnosticCode::Number(code) => Value::Number(code),
                };
            }
            output.push(item);
        }
    }
    output.sort_by(|left, right| {
        left["file_path"]
            .as_str()
            .cmp(&right["file_path"].as_str())
            .then_with(|| left["line"].as_u64().cmp(&right["line"].as_u64()))
            .then_with(|| left["message"].as_str().cmp(&right["message"].as_str()))
    });
    Ok(output)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

    use futures_lite::io::Cursor;
    use serde_json::{Value, json};
    use test_case::test_case;
    use url::Url;

    use super::{
        APPLY_EDIT_REJECTED, Diagnostic, DiagnosticReport, INVALID_DIAGNOSTIC, INVALID_FRAME,
        INVALID_LINE_RANGE, MAX_FRAME_BYTES, MAX_HEADER_BYTES, Reports, SPECULATION_TIMEOUT,
        bounded, content_length, filter_diagnostics, patch_lines, publication_is_current,
        read_frame, server_reply, validate_diagnostics,
    };

    const MAIN_URI: &str = "file:///C:/workspace/main.rs";
    const RELATED_URI: &str = "file:///C:/workspace/related.rs";
    const TYPE_ERROR: &str = "expected i32, found &str";
    const BASELINE_ERROR: &str = "unchanged unrelated error";
    const NEW_WARNING: &str = "new warning outside edited lines";

    #[test_case("a\nb\nc\n", 2, 2, "changed", "a\nchanged\nc\n", 1; "middle_line")]
    #[test_case("a\r\nb\r\nc\r\n", 2, 2, "changed", "a\r\nchanged\r\nc\r\n", 1; "crlf_boundary")]
    #[test_case("α\nβ\nγ", 1, 2, "λ\nμ", "λ\nμ\nγ", 2; "unicode_multiple_lines")]
    #[test_case("a\nb\nc\n", 2, 2, "", "a\nc\n", 0; "deleting_line")]
    #[test_case("a\nb", 2, 2, "changed", "a\nchanged", 1; "unterminated_last_line")]
    #[test_case("a\nb\n", 1, 2, "changed\n", "changed\n", 1; "whole_file")]
    #[test_case("", 1, 1, "first", "first", 1; "empty_file")]
    fn replacing_inclusive_lines_preserves_boundaries(
        source: &str,
        start: usize,
        end: usize,
        replacement: &str,
        expected: &str,
        added: usize,
    ) {
        let patch = patch_lines(source, start, end, replacement).expect("valid patch");
        assert_eq!(patch.text, expected);
        assert_eq!(patch.added, added);
    }

    #[test_case("a\n", 0, 1; "zero_based_start")]
    #[test_case("a\nb", 2, 1; "reversed_range")]
    #[test_case("a\n", 2, 2; "terminator_is_not_an_extra_line")]
    #[test_case("", 1, 2; "past_empty_file")]
    fn invalid_patch_ranges_are_errors(source: &str, start: usize, end: usize) {
        assert_eq!(
            patch_lines(source, start, end, "candidate")
                .err()
                .as_deref(),
            Some(INVALID_LINE_RANGE)
        );
    }

    #[test_case("Content-Length: 3\r\n\r\n", Some(3); "standard_header")]
    #[test_case("content-length: 9\r\nContent-Type: application/vscode-jsonrpc; charset=utf-8\r\n\r\n", Some(9); "case_insensitive_header")]
    #[test_case("Content-Type: application/json\r\n\r\n", None; "missing_length")]
    #[test_case("Content-Length: -1\r\n\r\n", None; "negative_length")]
    #[test_case("Content-Length: 1\r\nContent-Length: 2\r\n\r\n", None; "ambiguous_length")]
    #[test_case("Content-Length: no\r\n\r\n", None; "invalid_length")]
    fn parsing_content_length_rejects_ambiguous_frames(header: &str, expected: Option<usize>) {
        match expected {
            Some(length) => assert_eq!(
                content_length(header.as_bytes()).expect("valid header"),
                length
            ),
            None => assert_eq!(
                content_length(header.as_bytes()).err().as_deref(),
                Some(INVALID_FRAME)
            ),
        }
    }

    #[test_case("日本語"; "multibyte_payload")]
    #[test_case("embedded\r\nnewlines"; "payload_newlines")]
    fn framed_messages_use_byte_lengths_and_leave_the_next_frame(message: &str) {
        smol::block_on(async {
            let expected =
                json!({"jsonrpc": "2.0", "method": "example", "params": {"message": message}});
            let body = serde_json::to_vec(&expected).expect("JSON payload");
            let header = format!("Content-Length: {}\r\n\r\n", body.len());
            let mut bytes = Vec::new();
            for _ in 0..2 {
                bytes.extend_from_slice(header.as_bytes());
                bytes.extend_from_slice(&body);
            }
            let mut reader = Cursor::new(bytes);
            assert_eq!(
                read_frame(&mut reader).await.expect("first frame"),
                expected
            );
            assert_eq!(
                read_frame(&mut reader).await.expect("second frame"),
                expected
            );
        });
    }

    #[test_case(b"Content-Length: 10\r\n\r\n{}".as_slice(), "LSP server unavailable while reading message:"; "truncated_body")]
    #[test_case(b"Content-Length: 1\r\n\r\n!".as_slice(), "Invalid LSP JSON message:"; "invalid_json")]
    #[test_case(b"Content-Length: 1\r\n".as_slice(), "LSP server unavailable while reading header:"; "truncated_header")]
    fn incomplete_protocol_messages_are_errors(bytes: &[u8], prefix: &str) {
        let error = smol::block_on(read_frame(&mut Cursor::new(bytes))).expect_err("invalid frame");
        assert!(error.starts_with(prefix), "{error}");
    }

    #[test_case(true; "oversized_header")]
    #[test_case(false; "oversized_body")]
    fn frame_limits_are_enforced_before_reading_the_body(header_limit: bool) {
        let bytes = if header_limit {
            vec![b'x'; MAX_HEADER_BYTES + 1]
        } else {
            format!("Content-Length: {}\r\n\r\n", MAX_FRAME_BYTES + 1).into_bytes()
        };
        assert!(smol::block_on(read_frame(&mut Cursor::new(bytes))).is_err());
    }

    #[test_case(Some(7), true; "current_version")]
    #[test_case(Some(6), false; "stale_version")]
    #[test_case(Some(8), false; "future_version")]
    #[test_case(None, false; "unversioned_open_document")]
    fn open_document_publications_require_the_exact_version(version: Option<i64>, expected: bool) {
        assert_eq!(
            publication_is_current(Some(MAIN_URI), 7, MAIN_URI, version),
            expected
        );
        assert!(publication_is_current(
            Some(MAIN_URI),
            7,
            RELATED_URI,
            version
        ));
    }

    #[test_case("client/registerCapability"; "capability_registration")]
    #[test_case("client/unregisterCapability"; "capability_unregistration")]
    #[test_case("window/workDoneProgress/create"; "progress_creation")]
    #[test_case("workspace/diagnostic/refresh"; "diagnostic_refresh")]
    fn server_requests_are_answered_without_waiting_for_client_requests(method: &str) {
        assert_eq!(
            server_reply(method, &json!({}), MAIN_URI, "workspace"),
            Ok(Value::Null)
        );
    }

    #[test_case("workspace/applyEdit", true; "refusing_workspace_changes")]
    #[test_case("unknown/request", false; "unsupported_request")]
    fn consequential_or_unknown_server_requests_cannot_mutate_files(
        method: &str,
        apply_edit: bool,
    ) {
        let response = server_reply(
            method,
            &json!({"edit": {"changes": {}}}),
            MAIN_URI,
            "workspace",
        );
        if apply_edit {
            assert_eq!(
                response.expect("applyEdit rejection"),
                json!({"applied": false, "failureReason": APPLY_EDIT_REJECTED})
            );
        } else {
            assert_eq!(response.expect_err("unsupported request")["code"], -32601);
        }
    }

    #[test_case("rust-analyzer.checkOnSave"; "check_on_save")]
    #[test_case("rust-analyzer.cargo.buildScripts.enable"; "build_scripts")]
    #[test_case("rust-analyzer.procMacro.enable"; "procedural_macros")]
    fn analyzer_configuration_disables_disk_driven_checks(section: &str) {
        let response = server_reply(
            "workspace/configuration",
            &json!({"items": [{"section": section}, {"section": "unknown"}]}),
            MAIN_URI,
            "workspace",
        )
        .expect("configuration reply");
        assert_eq!(response, json!([false, null]));
    }

    fn diagnostic(line: usize, severity: u64, message: &str) -> Diagnostic {
        serde_json::from_value(json!({
            "range": {"start": {"line": line, "character": 4}, "end": {"line": line, "character": 8}},
            "severity": severity, "message": message, "source": "rust-analyzer", "code": "E0308"
        })).expect("diagnostic fixture")
    }

    #[test_case("b\nextra", 3, 4; "inserting_lines_shifts_unrelated_baseline")]
    #[test_case("", 3, 2; "deleting_lines_shifts_unrelated_baseline")]
    #[test_case("replacement", 3, 3; "same_line_count")]
    fn filtering_preserves_new_errors_and_excludes_shifted_unrelated_diagnostics(
        replacement: &str,
        old_line: usize,
        new_line: usize,
    ) {
        let patch = patch_lines("a\nb\nc\nd\ne\n", 2, 2, replacement).expect("patch");
        let baseline = Reports::from([
            (
                MAIN_URI.into(),
                vec![
                    diagnostic(0, 1, BASELINE_ERROR),
                    diagnostic(old_line, 1, BASELINE_ERROR),
                ],
            ),
            (RELATED_URI.into(), vec![diagnostic(0, 1, BASELINE_ERROR)]),
        ]);
        let candidate = Reports::from([
            (
                MAIN_URI.into(),
                vec![
                    diagnostic(0, 1, BASELINE_ERROR),
                    diagnostic(new_line, 1, BASELINE_ERROR),
                    diagnostic(new_line, 1, TYPE_ERROR),
                ],
            ),
            (
                RELATED_URI.into(),
                vec![
                    diagnostic(0, 1, BASELINE_ERROR),
                    diagnostic(4, 2, NEW_WARNING),
                ],
            ),
        ]);
        let output = filter_diagnostics(&baseline, candidate, MAIN_URI, &patch)
            .expect("diagnostic filtering");
        assert_eq!(output.len(), 2);
        let error = output
            .iter()
            .find(|diagnostic| diagnostic["message"] == TYPE_ERROR)
            .expect("new type mismatch");
        assert_eq!(error["line"], new_line + 1);
        assert_eq!(error["end_line"], new_line + 1);
        assert_eq!(error["message"], TYPE_ERROR);
        assert_eq!(error["source"], "rust-analyzer");
        assert_eq!(error["code"], "E0308");
        assert_eq!(error["severity"], "error");
        let warning = output
            .iter()
            .find(|diagnostic| diagnostic["message"] == NEW_WARNING)
            .expect("cross-file warning");
        assert_eq!(warning["severity"], "warning");
        assert_eq!(
            warning["file_path"],
            json!(
                Url::parse(RELATED_URI)
                    .expect("URI")
                    .to_file_path()
                    .expect("file path")
            )
        );
    }

    #[test_case(1, 1; "related_error")]
    #[test_case(2, 1; "related_warning")]
    #[test_case(3, 0; "information_is_not_an_error")]
    #[test_case(4, 0; "hint_is_not_an_error")]
    fn diagnostics_in_replaced_lines_remain_visible(severity: u64, count: usize) {
        let patch = patch_lines("a\nb\nc\n", 2, 2, "replacement").expect("patch");
        let baseline =
            Reports::from([(MAIN_URI.into(), vec![diagnostic(1, severity, TYPE_ERROR)])]);
        let candidate =
            Reports::from([(MAIN_URI.into(), vec![diagnostic(1, severity, TYPE_ERROR)])]);
        assert_eq!(
            filter_diagnostics(&baseline, candidate, MAIN_URI, &patch)
                .expect("filtering")
                .len(),
            count
        );
    }

    #[test_case(0, 0; "exclusive_range_ending_at_edit_is_unrelated")]
    #[test_case(1, 1; "range_crossing_into_edit_is_related")]
    fn filtering_respects_half_open_diagnostic_ranges(end_character: usize, expected: usize) {
        let patch = patch_lines("a\nb\nc\n", 2, 2, "replacement").expect("patch");
        let make_report = || {
            let mut item = diagnostic(0, 1, BASELINE_ERROR);
            item.range.end.line = 1;
            item.range.end.character = end_character;
            Reports::from([(MAIN_URI.into(), vec![item])])
        };
        assert_eq!(
            filter_diagnostics(&make_report(), make_report(), MAIN_URI, &patch)
                .expect("filtering")
                .len(),
            expected
        );
    }

    #[test_case("full", true, true; "full_related_report")]
    #[test_case("unchanged", false, false; "unchanged_without_previous_result")]
    #[test_case("full", false, false; "full_without_items")]
    fn pull_reports_require_real_items_and_include_related_documents(
        kind: &str,
        items: bool,
        valid: bool,
    ) {
        let mut value = json!({"kind": kind, "relatedDocuments": {(RELATED_URI): {"kind": "full", "items": []}}});
        if items {
            value["items"] = json!([]);
        }
        let report: DiagnosticReport = serde_json::from_value(value).expect("report fixture");
        let mut reports = Reports::new();
        let result = report.add_to(MAIN_URI.into(), &mut reports);
        assert_eq!(result.is_ok(), valid);
        if valid {
            assert!(reports.contains_key(MAIN_URI));
            assert!(reports.contains_key(RELATED_URI));
        }
    }

    #[test_case(0, 0, 0; "invalid_severity")]
    #[test_case(1, 2, 1; "reversed_lines")]
    #[test_case(1, 0, usize::MAX; "unrepresentable_one_based_line")]
    fn malformed_diagnostics_are_errors_not_clean_results(severity: u64, start: usize, end: usize) {
        let mut item = diagnostic(start, severity, TYPE_ERROR);
        item.range.end.line = end;
        assert_eq!(
            validate_diagnostics(&[item]).err().as_deref(),
            Some(INVALID_DIAGNOSTIC)
        );
    }

    struct CancellationProbe(Arc<AtomicBool>);

    impl Drop for CancellationProbe {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[test_case(Duration::ZERO; "timeout_discards_the_in_flight_operation")]
    fn timing_out_returns_an_error_and_drops_owned_state(timeout: Duration) {
        let cancelled = Arc::new(AtomicBool::new(false));
        let guard = CancellationProbe(Arc::clone(&cancelled));
        let operation = async move {
            let _guard = guard;
            futures_lite::future::pending::<Result<(), String>>().await
        };
        let result = smol::block_on(bounded(timeout, operation, SPECULATION_TIMEOUT));
        assert_eq!(result.err().as_deref(), Some(SPECULATION_TIMEOUT));
        assert!(cancelled.load(Ordering::SeqCst));
    }
}
