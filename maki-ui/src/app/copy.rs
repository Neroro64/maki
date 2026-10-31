use crossterm::event::KeyEvent;

use crate::components::list_picker::{PickerAction, PickerItem};

use super::App;

pub(super) const COPY_FOOTER: &[(&str, &str)] = &[("Enter", "copy")];
const TITLE: &str = " Copy agent output ";
const PREVIEW_MAX_LEN: usize = 80;
const NO_OUTPUTS_MSG: &str = "No agent output to copy";

pub(crate) struct CopyEntry {
    index: usize,
    preview: String,
}

impl CopyEntry {
    fn new(index: usize, text: &str) -> Self {
        let first_line = text
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or_default()
            .trim();
        let end = first_line.floor_char_boundary(PREVIEW_MAX_LEN);
        let suffix = if end < first_line.len() { "..." } else { "" };
        Self {
            index,
            preview: format!("{}: {}{suffix}", index + 1, &first_line[..end]),
        }
    }
}

impl PickerItem for CopyEntry {
    fn label(&self) -> &str {
        &self.preview
    }
}

impl App {
    pub(super) fn open_copy_picker(&mut self) {
        let mut entries: Vec<CopyEntry> = self.chats[self.active_chat]
            .assistant_outputs()
            .enumerate()
            .map(|(index, text)| CopyEntry::new(index, text))
            .collect();
        if entries.is_empty() {
            self.flash(NO_OUTPUTS_MSG.to_owned());
            return;
        }
        entries.reverse();
        self.copy_picker.open(entries, TITLE);
    }

    pub(super) fn handle_copy_picker_key(&mut self, key: KeyEvent) {
        let PickerAction::Select(entry) = self.copy_picker.handle_key(key) else {
            return;
        };
        let text = self.chats[self.active_chat]
            .assistant_outputs()
            .nth(entry.index)
            .map(str::to_owned);
        if let Some(text) = text {
            self.copy(&text);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use maki_agent::AgentEvent;
    use maki_providers::{ImageMediaType, ImageSource};
    use test_case::test_case;

    use crate::app::Msg;
    use crate::app::tests::test_app;
    use crate::components::list_picker::PickerAction;
    use crate::components::{DisplayMessage, DisplayRole, Status, ToolRole, ToolStatus};

    use super::NO_OUTPUTS_MSG;

    const COMMAND: &str = "/copy";
    const OLDER: &str = "\n## Older response with a long heading that reaches beyond the preview width: 日本語の回答\n\n```rust\nlet answer = 42;\n```\n";
    const LATEST: &str = "Latest response\n\n**Keep Markdown** and trailing whitespace.  \n";
    const DRAFT: &str = "keep this draft";
    const FILTER: &str = "Older";
    const THINKING: &str = "private reasoning";
    const TOOL_OUTPUT: &str = "tool output";
    const LIVE_PREFIX: &str = "Live response";
    const LIVE_SUFFIX: &str = "\n\nMore text.";
    const LIVE_FULL: &str = "Live response\n\nMore text.";
    const IMAGE: &str = "dGVzdA==";
    const TOOL_ID: &str = "copy-test-tool";
    const TOOL_NAME: &str = "bash";

    #[test_case("", false, LATEST ; "newest_first")]
    #[test_case("", true, OLDER ; "choose_older")]
    #[test_case(FILTER, false, OLDER ; "filter_preview")]
    fn copy_picker_selects_complete_agent_output(filter: &str, older: bool, expected: &str) {
        let mut app = test_app();
        app.main_chat().load_messages(vec![
            DisplayMessage::new(DisplayRole::User, DRAFT.into()),
            DisplayMessage::new(DisplayRole::Assistant, OLDER.into()),
            DisplayMessage::new(DisplayRole::Thinking, THINKING.into()),
            DisplayMessage::new(DisplayRole::Assistant, LATEST.into()),
            DisplayMessage::new(
                DisplayRole::Tool(Box::new(ToolRole {
                    id: TOOL_ID.into(),
                    name: Arc::from(TOOL_NAME),
                    status: ToolStatus::Success,
                })),
                TOOL_OUTPUT.into(),
            ),
            DisplayMessage::new(DisplayRole::Assistant, " \n\t".into()),
            DisplayMessage::with_images(
                DisplayRole::Assistant,
                String::new(),
                vec![ImageSource::new(ImageMediaType::Png, Arc::from(IMAGE))],
            ),
            DisplayMessage::new(DisplayRole::Done, String::new()),
        ]);
        app.update(Msg::Paste(COMMAND.into()));
        app.update(Msg::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
        app.update(Msg::Paste(filter.into()));
        if older {
            app.update(Msg::Key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE)));
        }

        let PickerAction::Select(entry) = app
            .copy_picker
            .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        else {
            panic!("expected a selected response");
        };
        assert_eq!(
            app.main_chat().assistant_outputs().nth(entry.index),
            Some(expected)
        );
        assert!(app.input_box.buffer.value().is_empty());
    }

    #[test_case(KeyCode::Esc ; "escape")]
    #[test_case(KeyCode::Char('c') ; "ctrl_c")]
    fn cancelling_copy_picker_preserves_running_turn_and_draft(code: KeyCode) {
        let mut app = test_app();
        app.main_chat().load_messages(vec![DisplayMessage::new(
            DisplayRole::Assistant,
            LATEST.into(),
        )]);
        app.update(Msg::Paste(DRAFT.into()));
        app.status = Status::Streaming;
        app.open_copy_picker();
        app.update(Msg::Paste(FILTER.into()));
        let modifiers = if code == KeyCode::Esc {
            KeyModifiers::NONE
        } else {
            KeyModifiers::CONTROL
        };

        let actions = app.update(Msg::Key(KeyEvent::new(code, modifiers)));

        assert!(actions.is_empty());
        assert!(!app.copy_picker.is_open());
        assert_eq!(app.status, Status::Streaming);
        assert_eq!(app.input_box.buffer.value(), DRAFT);
    }

    #[test_case(DisplayRole::User ; "user_only")]
    #[test_case(DisplayRole::Thinking ; "thinking_only")]
    #[test_case(DisplayRole::Assistant ; "whitespace_only")]
    fn copy_without_agent_output_reports_empty_history(role: DisplayRole) {
        let mut app = test_app();
        app.main_chat()
            .load_messages(vec![DisplayMessage::new(role, " \n".into())]);

        app.open_copy_picker();

        assert!(!app.copy_picker.is_open());
        assert_eq!(app.status_bar.flash_text(), Some(NO_OUTPUTS_MSG));
    }

    #[test_case(false ; "still_streaming")]
    #[test_case(true ; "flushed_while_selecting")]
    fn copy_selection_tracks_streaming_output_across_flushes(flush: bool) {
        let mut app = test_app();
        app.main_chat().handle_event(
            AgentEvent::TextDelta {
                text: LIVE_PREFIX.into(),
            },
            None,
        );
        app.open_copy_picker();
        app.main_chat().handle_event(
            AgentEvent::ThinkingDelta {
                text: THINKING.into(),
            },
            None,
        );
        app.main_chat().handle_event(
            AgentEvent::TextDelta {
                text: LIVE_SUFFIX.into(),
            },
            None,
        );
        if flush {
            app.main_chat().flush();
            app.main_chat().handle_event(
                AgentEvent::TextDelta {
                    text: LATEST.into(),
                },
                None,
            );
        }

        let PickerAction::Select(entry) = app
            .copy_picker
            .handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        else {
            panic!("expected the streaming response");
        };
        assert_eq!(
            app.main_chat().assistant_outputs().nth(entry.index),
            Some(LIVE_FULL)
        );
    }
}
