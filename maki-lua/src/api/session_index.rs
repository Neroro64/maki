use std::collections::{HashMap, HashSet};
use std::fmt::Write;
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use maki_providers::{ContentBlock, Message, Role, TokenUsage};
use maki_storage::StateDir;
use maki_storage::id::MakiId;
use maki_storage::sessions::{Session, SessionFile, session_files};
use regex::RegexBuilder;
use serde::de::IgnoredAny;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

const CACHE_BYTES: usize = 8 * 1024 * 1024;
const CACHE_SESSIONS: usize = 64;
pub(super) const MAX_ROWS: usize = 100;
pub(super) const MAX_OUTPUT_BYTES: usize = 24 * 1024;
const MAX_MESSAGE_BYTES: usize = 4 * 1024;
const HIT_CHARS: usize = 120;
const TARGET_CHARS: usize = 80;
const TOOL_NAME_CHARS: usize = 80;
const ERROR_CHARS: usize = 60;
const MAX_QUERY_CHARS: usize = 1024;
const TRUNCATED: &str = "\n[session output truncated]";
const AMBIGUOUS: &str = "ambiguous session prefix; use a longer id from session_index";
const NO_SESSION: &str = "no session matching id";
const INVALID_PREFIX: &str = "id must be a nonempty session id or prefix, not a path or glob";
const NO_MATCHES: &str = "no matches";
const NO_SESSIONS: &str = "no sessions found";

#[derive(Serialize)]
struct IgnoredOutput;

impl<'de> Deserialize<'de> for IgnoredOutput {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        IgnoredAny::deserialize(deserializer)?;
        Ok(Self)
    }
}

type ReadSession = Session<Message, TokenUsage, IgnoredOutput>;

struct OutlineMessage {
    role: &'static str,
    text: String,
}

struct Outline {
    id: MakiId,
    title: String,
    cwd: String,
    messages: Vec<OutlineMessage>,
}

impl Outline {
    fn bytes(&self) -> usize {
        self.title.capacity()
            + self.cwd.capacity()
            + self.messages.capacity() * size_of::<OutlineMessage>()
            + self
                .messages
                .iter()
                .map(|message| message.text.capacity())
                .sum::<usize>()
    }
}

struct Cached {
    size: u64,
    modified: SystemTime,
    used: u64,
    outline: Arc<Outline>,
}

#[derive(Default)]
pub(super) struct SessionIndex {
    entries: HashMap<PathBuf, Cached>,
    bytes: usize,
    clock: u64,
}

fn squeeze(text: &str) -> String {
    let mut result = String::new();
    push_words(&mut result, text);
    result
}

fn push_words(result: &mut String, text: &str) {
    for word in text
        .split(|c: char| c.is_whitespace() || c.is_control())
        .filter(|word| !word.is_empty())
    {
        if !result.is_empty() {
            result.push(' ');
        }
        result.push_str(word);
    }
}

fn compact(text: &str, chars: usize) -> String {
    let mut result = String::new();
    let mut remaining = chars;
    let mut space = false;
    for ch in text.chars() {
        if remaining == 0 {
            break;
        }
        if ch.is_whitespace() || ch.is_control() {
            space = !result.is_empty();
        } else {
            if space {
                result.push(' ');
                remaining -= 1;
                space = false;
                if remaining == 0 {
                    break;
                }
            }
            result.push(ch);
            remaining -= 1;
        }
    }
    result
}

fn normalize(message: &Message) -> OutlineMessage {
    let mut text = String::new();
    for block in &message.content {
        match block {
            ContentBlock::Text { text: content } => push_words(&mut text, content),
            ContentBlock::ToolUse { name, input, .. } => {
                let target = [
                    "path",
                    "file_path",
                    "pattern",
                    "command",
                    "query",
                    "url",
                    "id",
                    "session",
                ]
                .iter()
                .find_map(|key| input.get(key).and_then(Value::as_str))
                .map(|target| compact(target, TARGET_CHARS))
                .unwrap_or_default();
                if !text.is_empty() {
                    text.push(' ');
                }
                text.push('[');
                text.push_str(&compact(name, TOOL_NAME_CHARS));
                if !target.is_empty() {
                    text.push(' ');
                    text.push_str(&target);
                }
                text.push(']');
            }
            ContentBlock::ToolResult {
                content, is_error, ..
            } => {
                if !text.is_empty() {
                    text.push(' ');
                }
                if *is_error {
                    text.push_str("[err: ");
                    text.push_str(&compact(content, ERROR_CHARS));
                    text.push(']');
                } else {
                    text.push_str("[ok]");
                }
            }
            ContentBlock::Thinking { .. }
            | ContentBlock::RedactedThinking { .. }
            | ContentBlock::Image { .. }
            | ContentBlock::Unknown(_) => {}
        }
    }
    OutlineMessage {
        role: match message.role {
            Role::User => "user",
            Role::Assistant => "assistant",
        },
        text,
    }
}

fn prefix_bytes(text: &str, max_bytes: usize) -> &str {
    let mut end = max_bytes.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn capped(text: &str, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text.to_owned();
    }
    let mut result = prefix_bytes(text, max_bytes.saturating_sub(TRUNCATED.len())).to_owned();
    result.push_str(TRUNCATED);
    result
}

fn window(text: &str, start: usize, end: usize) -> String {
    let before = text[..start].chars().rev().take(HIT_CHARS / 3).count();
    let from = text[..start]
        .char_indices()
        .rev()
        .nth(before.saturating_sub(1))
        .map_or(start, |(at, _)| at);
    let to = text[from..]
        .char_indices()
        .nth(HIT_CHARS)
        .map_or(text.len(), |(at, _)| from + at);
    let mut result = String::new();
    if from != 0 {
        result.push('…');
    }
    result.push_str(&text[from..to]);
    if to < text.len() || end > to {
        result.push('…');
    }
    result
}

fn resolve<'a>(files: &'a [SessionFile], prefix: &str) -> Result<&'a SessionFile, String> {
    if prefix.is_empty()
        || !prefix
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
    {
        return Err(INVALID_PREFIX.to_owned());
    }
    let exact = prefix.parse::<MakiId>().ok();
    if let Some(file) = files.iter().find(|file| Some(file.summary.id) == exact) {
        return Ok(file);
    }
    let mut matches = files
        .iter()
        .filter(|file| file.summary.id.to_string().starts_with(prefix));
    let first = matches.next().ok_or_else(|| NO_SESSION.to_owned())?;
    if matches.next().is_some() {
        return Err(AMBIGUOUS.to_owned());
    }
    Ok(first)
}

impl SessionIndex {
    fn files(&mut self, storage: &StateDir) -> Result<Vec<SessionFile>, String> {
        let files = session_files(storage).map_err(|error| error.to_string())?;
        let present: HashSet<_> = files.iter().map(|file| file.path.as_path()).collect();
        self.entries.retain(|path, entry| {
            if present.contains(path.as_path()) {
                true
            } else {
                self.bytes -= entry.outline.bytes();
                false
            }
        });
        Ok(files)
    }

    fn load(&mut self, file: &SessionFile) -> Result<Arc<Outline>, String> {
        self.clock = self.clock.wrapping_add(1);
        if let Some(entry) = self.entries.get_mut(&file.path)
            && entry.size == file.size
            && entry.modified == file.modified
        {
            entry.used = self.clock;
            return Ok(Arc::clone(&entry.outline));
        }
        if let Some(old) = self.entries.remove(&file.path) {
            self.bytes -= old.outline.bytes();
        }
        let stored: ReadSession = file.read().map_err(|error| error.to_string())?;
        let messages = stored.messages().iter().map(normalize).collect();
        let outline = Arc::new(Outline {
            id: stored.id,
            title: stored.title,
            cwd: squeeze(&stored.cwd),
            messages,
        });
        let bytes = outline.bytes();
        if bytes <= CACHE_BYTES
            && fs::metadata(&file.path).is_ok_and(|metadata| {
                metadata.len() == file.size && metadata.modified().ok() == Some(file.modified)
            })
        {
            while self.bytes + bytes > CACHE_BYTES || self.entries.len() >= CACHE_SESSIONS {
                let Some(oldest) = self
                    .entries
                    .iter()
                    .min_by_key(|(_, entry)| entry.used)
                    .map(|(path, _)| path.clone())
                else {
                    break;
                };
                if let Some(old) = self.entries.remove(&oldest) {
                    self.bytes -= old.outline.bytes();
                }
            }
            self.bytes += bytes;
            self.entries.insert(
                file.path.clone(),
                Cached {
                    size: file.size,
                    modified: file.modified,
                    used: self.clock,
                    outline: Arc::clone(&outline),
                },
            );
        }
        Ok(outline)
    }

    pub(super) fn search(
        &mut self,
        storage: &StateDir,
        cwd: Option<&str>,
        query: &str,
        limit: usize,
    ) -> Result<String, String> {
        let query = squeeze(query);
        if query.chars().count() > MAX_QUERY_CHARS {
            return Err(format!("query exceeds {MAX_QUERY_CHARS} characters"));
        }
        let matcher = if query.is_empty() {
            None
        } else {
            Some(
                RegexBuilder::new(&regex::escape(&query))
                    .case_insensitive(true)
                    .build()
                    .map_err(|error| error.to_string())?,
            )
        };
        let mut rows = Vec::new();
        let mut unavailable = 0;
        'sessions: for file in self.files(storage)? {
            if cwd.is_some_and(|cwd| file.summary.cwd != cwd) {
                continue;
            }
            let outline = match self.load(&file) {
                Ok(outline) => outline,
                Err(_) => {
                    unavailable += 1;
                    continue;
                }
            };
            if let Some(matcher) = &matcher {
                for (index, message) in outline.messages.iter().enumerate() {
                    if let Some(hit) = matcher.find(&message.text) {
                        rows.push(format!(
                            "{} {}#{} | {}",
                            outline.id,
                            message.role,
                            index + 1,
                            window(&message.text, hit.start(), hit.end())
                        ));
                        if rows.len() >= limit {
                            break 'sessions;
                        }
                    }
                }
            } else {
                rows.push(format!(
                    "{}  {}  ({}, {} msgs)",
                    outline.id,
                    outline.title,
                    outline.cwd,
                    outline.messages.len()
                ));
                if rows.len() >= limit {
                    break;
                }
            }
        }
        if rows.is_empty() {
            rows.push(
                if matcher.is_none() {
                    NO_SESSIONS
                } else {
                    NO_MATCHES
                }
                .to_owned(),
            );
        }
        if unavailable > 0 {
            rows.push(format!("[{unavailable} unreadable sessions skipped]"));
        }
        Ok(capped(&rows.join("\n"), MAX_OUTPUT_BYTES))
    }

    pub(super) fn read(
        &mut self,
        storage: &StateDir,
        id: &str,
        last: usize,
    ) -> Result<String, String> {
        let files = self.files(storage)?;
        let outline = self.load(resolve(&files, id)?)?;
        let mut header = format!("{}  {}  ({})", outline.id, outline.title, outline.cwd);
        header = capped(&header, MAX_MESSAGE_BYTES);
        let start = outline.messages.len().saturating_sub(last);
        let mut rows = Vec::new();
        let mut bytes = header.len() + TRUNCATED.len();
        for (index, message) in outline.messages.iter().enumerate().skip(start).rev() {
            let mut row = format!("{}#{}  {}\n", outline.id, index + 1, message.role);
            row.push_str(&capped(&message.text, MAX_MESSAGE_BYTES));
            if bytes + row.len() + 1 > MAX_OUTPUT_BYTES {
                header.push_str(TRUNCATED);
                break;
            }
            bytes += row.len() + 1;
            rows.push(row);
        }
        for row in rows.iter().rev() {
            write!(header, "\n{row}").map_err(|error| error.to_string())?;
        }
        Ok(header)
    }
}

#[cfg(test)]
mod tests {
    use std::fs::{self, OpenOptions};
    use std::io::Write;
    use std::sync::Arc;
    use std::time::{Duration, UNIX_EPOCH};

    use maki_agent::session::StoredSession;
    use maki_providers::{ContentBlock, Message, MessageKind, Role};
    use maki_storage::StateDir;
    use maki_storage::sessions::{SESSIONS_DIR, SessionClaim};
    use serde_json::json;
    use tempfile::TempDir;
    use test_case::test_case;

    use super::{
        AMBIGUOUS, CACHE_BYTES, ERROR_CHARS, HIT_CHARS, INVALID_PREFIX, MAX_MESSAGE_BYTES,
        MAX_OUTPUT_BYTES, MAX_ROWS, NO_MATCHES, NO_SESSION, NO_SESSIONS, SessionIndex,
        TARGET_CHARS, TRUNCATED, normalize, resolve, session_files,
    };

    const ID_A: &str = "550e8400-e29b-41d4-a716-446655440000";
    const ID_B: &str = "550e8400-e29b-41d4-a716-446655440001";
    const PROJECT: &str = "/project";
    const OTHER_PROJECT: &str = "/other";
    const TEXT: &str = "cursor [bug].* fixed";
    const PRIVATE_THOUGHT: &str = "private reasoning";
    const PRIVATE_OUTPUT: &str = "private tool output";
    const PARTIAL_RECORD: &[u8] = b"{\"t\":\"msg\",\"d\":";
    const NEWEST: &str = "newest message";

    fn storage() -> (TempDir, StateDir) {
        let temp = tempfile::tempdir().unwrap();
        let storage = StateDir::from_path(temp.path().join("state"));
        (temp, storage)
    }

    fn save(storage: &StateDir, id: &str, cwd: &str, texts: &[&str]) -> StoredSession {
        let mut stored = StoredSession::new("test/model", cwd);
        stored.id = id.parse().unwrap();
        for text in texts {
            stored.push_message(Message::user((*text).to_owned()));
        }
        let claim = SessionClaim::acquire(stored.id, storage).unwrap();
        stored.save(&claim, storage).unwrap();
        stored
    }

    #[test]
    fn normalizes_current_message_shape_without_thinking_or_result_bodies() {
        let message = Message {
            role: Role::Assistant,
            kind: MessageKind::Turn,
            display_text: Some("display only".to_owned()),
            content: vec![
                ContentBlock::Thinking {
                    thinking: PRIVATE_THOUGHT.to_owned(),
                    signature: None,
                },
                ContentBlock::RedactedThinking {
                    data: PRIVATE_THOUGHT.to_owned(),
                },
                ContentBlock::Text {
                    text: " full\n\ttext \u{1b} tail ".to_owned(),
                },
                ContentBlock::tool_use(
                    "call",
                    "edit",
                    json!({ "path": "src/x.rs", "content": PRIVATE_OUTPUT }),
                ),
                ContentBlock::tool_result("call", PRIVATE_OUTPUT, false),
                ContentBlock::tool_result("error", " file\nnot found ", true),
                ContentBlock::Unknown(json!({ "type": "future", "text": PRIVATE_OUTPUT })),
            ],
        };
        let normalized = normalize(&message);
        assert_eq!(normalized.role, "assistant");
        assert_eq!(
            normalized.text,
            "full text tail [edit src/x.rs] [ok] [err: file not found]"
        );
        assert!(!normalized.text.contains(PRIVATE_THOUGHT));
        assert!(!normalized.text.contains(PRIVATE_OUTPUT));
    }

    #[test]
    fn clips_unicode_tool_targets_and_error_markers() {
        let text = "é".repeat(TARGET_CHARS * 4);
        let mut message = Message::user(String::new());
        message.content = vec![
            ContentBlock::tool_use("call", "bash", json!({ "command": text })),
            ContentBlock::tool_result("call", "界".repeat(ERROR_CHARS * 4), true),
        ];
        assert_eq!(
            normalize(&message).text,
            format!(
                "[bash {}] [err: {}]",
                "é".repeat(TARGET_CHARS),
                "界".repeat(ERROR_CHARS)
            )
        );
    }

    #[test]
    fn searches_full_text_literally_case_insensitively_in_bounded_windows() {
        let (_temp, storage) = storage();
        let text = format!(
            "{} {TEXT} {}",
            "preface ".repeat(200),
            "ending ".repeat(200)
        );
        save(&storage, ID_A, PROJECT, &[&text]);
        let mut index = SessionIndex::default();
        let result = index
            .search(&storage, Some(PROJECT), "CURSOR [BUG].*", 20)
            .unwrap();
        assert!(result.contains(TEXT));
        assert!(result.contains("user#1"));
        assert!(result.chars().count() < HIT_CHARS + 100);
        assert!(!result.contains(&"preface ".repeat(30)));
        assert_eq!(
            index
                .search(&storage, Some(PROJECT), "cursor .+", 20)
                .unwrap(),
            NO_MATCHES
        );
    }

    #[test]
    fn supports_current_directory_and_all_project_scopes() {
        let (_temp, storage) = storage();
        let local = save(&storage, ID_A, PROJECT, &[TEXT]);
        let other = save(&storage, ID_B, OTHER_PROJECT, &[TEXT]);
        let mut index = SessionIndex::default();
        let scoped = index.search(&storage, Some(PROJECT), TEXT, 20).unwrap();
        assert!(scoped.contains(&local.id.to_string()));
        assert!(!scoped.contains(&other.id.to_string()));
        let all = index.search(&storage, None, TEXT, 20).unwrap();
        assert!(all.contains(&local.id.to_string()));
        assert!(all.contains(&other.id.to_string()));
        assert_eq!(
            index.search(&storage, None, "", 1).unwrap().lines().count(),
            1
        );
    }

    #[test_case("" ; "empty")]
    #[test_case("*" ; "glob")]
    #[test_case("../session" ; "path")]
    fn refuses_unsafe_session_prefixes(prefix: &str) {
        assert_eq!(resolve(&[], prefix).err().as_deref(), Some(INVALID_PREFIX));
    }

    #[test]
    fn resolves_only_exact_or_unique_ids_and_rejects_ambiguity() {
        let (_temp, storage) = storage();
        let first = save(&storage, ID_A, PROJECT, &[TEXT]);
        save(&storage, ID_B, PROJECT, &[TEXT]);
        let files = session_files(&storage).unwrap();
        let canonical = first.id.to_string();
        assert_eq!(resolve(&files, &canonical).unwrap().summary.id, first.id);
        assert_eq!(resolve(&files, ID_A).unwrap().summary.id, first.id);
        assert_eq!(
            resolve(&files, &canonical[..1]).err().as_deref(),
            Some(AMBIGUOUS)
        );
        assert_eq!(
            resolve(&files, "missing").err().as_deref(),
            Some(NO_SESSION)
        );
    }

    #[test]
    fn reads_newest_message_window_and_bounds_large_answers() {
        let (_temp, storage) = storage();
        let huge = "界".repeat(MAX_MESSAGE_BYTES * 2);
        let stored = save(
            &storage,
            ID_A,
            PROJECT,
            &[TEXT, &huge, &huge, &huge, &huge, &huge, &huge, NEWEST],
        );
        let mut index = SessionIndex::default();
        let last = index.read(&storage, &stored.id.to_string(), 1).unwrap();
        assert!(last.contains(NEWEST));
        assert!(!last.contains(TEXT));
        assert!(last.contains("#8"));
        let all = index
            .read(&storage, &stored.id.to_string(), MAX_ROWS)
            .unwrap();
        assert!(all.len() <= MAX_OUTPUT_BYTES);
        assert!(all.contains(NEWEST));
        assert!(all.contains(TRUNCATED));
    }

    #[test]
    fn caches_only_unchanged_files_and_follows_rewrites_not_archives() {
        let (_temp, storage) = storage();
        let mut stored = save(&storage, ID_A, PROJECT, &[TEXT, "retired branch"]);
        let mut index = SessionIndex::default();
        let files = index.files(&storage).unwrap();
        let original = index.load(&files[0]).unwrap();
        assert!(Arc::ptr_eq(&original, &index.load(&files[0]).unwrap()));
        stored.replace_messages(vec![Message::user(NEWEST.to_owned())]);
        let claim = SessionClaim::acquire(stored.id, &storage).unwrap();
        stored.save(&claim, &storage).unwrap();
        let path = storage
            .path()
            .join(SESSIONS_DIR)
            .join(format!("{}.jsonl", stored.id));
        OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(UNIX_EPOCH + Duration::from_secs(2_000_000_000))
            .unwrap();
        OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(PARTIAL_RECORD)
            .unwrap();
        let files = index.files(&storage).unwrap();
        let rewritten = index.load(&files[0]).unwrap();
        assert!(!Arc::ptr_eq(&original, &rewritten));
        assert_eq!(rewritten.messages.len(), 1);
        assert_eq!(rewritten.messages[0].text, NEWEST);
        assert_eq!(
            index.search(&storage, None, "retired branch", 20).unwrap(),
            NO_MATCHES
        );
    }

    #[test]
    fn keeps_cache_bounded_and_drops_deleted_sessions() {
        let (_temp, storage) = storage();
        save(&storage, ID_A, PROJECT, &[&"x".repeat(CACHE_BYTES + 1)]);
        let mut index = SessionIndex::default();
        index.search(&storage, None, "x", 1).unwrap();
        assert!(index.entries.is_empty());
        assert_eq!(index.bytes, 0);
        save(&storage, ID_B, PROJECT, &[TEXT]);
        let files = index.files(&storage).unwrap();
        let file = files
            .iter()
            .find(|file| file.summary.id == ID_B.parse().unwrap())
            .unwrap();
        index.load(file).unwrap();
        fs::remove_file(&file.path).unwrap();
        index.files(&storage).unwrap();
        assert!(index.entries.is_empty());
        assert_eq!(index.bytes, 0);
    }

    #[test]
    fn missing_store_stays_missing_and_legacy_reads_never_migrate() {
        let (_temp, storage) = storage();
        let mut index = SessionIndex::default();
        assert_eq!(index.search(&storage, None, "", 30).unwrap(), NO_SESSIONS);
        assert!(!storage.path().exists());
        let mut stored = StoredSession::new("test/model", PROJECT);
        stored.id = ID_A.parse().unwrap();
        stored.push_message(Message::user(TEXT.to_owned()));
        let dir = storage.ensure_subdir(SESSIONS_DIR).unwrap();
        let legacy = dir.join(format!("{ID_A}.json"));
        fs::write(&legacy, serde_json::to_vec(&stored).unwrap()).unwrap();
        assert!(index.read(&storage, ID_A, 4).unwrap().contains(TEXT));
        assert!(legacy.exists());
        assert_eq!(fs::read_dir(&dir).unwrap().count(), 1);
    }
}
