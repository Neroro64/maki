use super::tests::test_app;
use super::{Msg, ParsedCommand, PendingInput, Status};
use crate::components::{Action, Overlay};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use maki_agent::SubagentInbox;
use ratatui::{Terminal, backend::TestBackend};
use std::path::PathBuf;
use std::sync::Arc;
use test_case::test_case;
use tokio::sync::mpsc::unbounded_channel;

const REVIEW_PROMPT: &str = "Answer the review question about the changed branch.";
const DRAFT: &str = "preserve this unsent draft";
const PERMISSION_SCOPE: &str = "review-permission-command";
const PATH_COMMAND: &str = "/cd ./some-path";

#[test_case(KeyCode::Tab)]
#[test_case(KeyCode::Enter)]
fn path_completion_popup_owns_keys_after_the_command_token(code: KeyCode) {
    let mut app = test_app();
    app.input_box.set_input(PATH_COMMAND);
    app.input_box.buffer.move_to_end();
    let (events_tx, mut events_rx) = unbounded_channel();
    let (_commands_tx, commands_rx) = unbounded_channel();
    app.float_mgr.open(
        Arc::new(maki_agent::SharedBuf::new()),
        maki_lua::FloatConfig {
            keys: vec![maki_lua::Key::from_event(KeyEvent::from(code)).unwrap()],
            ..Default::default()
        },
        false,
        events_tx,
        commands_rx,
    );
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal
        .draw(|frame| {
            app.view(frame);
        })
        .unwrap();
    let actions = app.update(Msg::Key(KeyEvent::from(code)));
    assert!(actions.is_empty());
    let mut saw_key = false;
    while let Ok(event) = events_rx.try_recv() {
        if matches!(event, maki_lua::WinEvent::Key { .. }) {
            saw_key = true;
            break;
        }
    }
    assert!(saw_key);
    assert_eq!(app.input_box.buffer.value(), PATH_COMMAND);
}

#[test]
fn wrapped_command_tokens_keep_vertical_motion_in_the_editor() {
    let mut app = test_app();
    let text = format!("/{}", "compact".repeat(16));
    app.input_box.set_input(&text);
    app.input_box.buffer.move_to_end();
    let mut terminal = Terminal::new(TestBackend::new(40, 20)).unwrap();
    terminal
        .draw(|frame| {
            app.view(frame);
        })
        .unwrap();
    let before = app.input_box.buffer.cursor_byte();
    app.update(Msg::Key(KeyEvent::from(KeyCode::Up)));
    assert!(app.input_box.buffer.cursor_byte() < before);
    assert_eq!(app.input_box.buffer.value(), text);
}

#[test]
fn review_command_opens_a_native_modal_without_plugins() {
    let mut app = test_app();
    let actions = app.execute_command(
        ParsedCommand {
            name: "/review".into(),
            args: String::new(),
            bang: false,
        },
        0,
    );
    assert!(actions.is_empty());
    assert!(app.review.is_open());
    assert!(app.has_modal_overlay());
    assert!(!app.input_live(ratatui::layout::Rect::new(0, 0, 100, 30)));
}

#[test_case(KeyCode::Up)]
#[test_case(KeyCode::Down)]
#[test_case(KeyCode::Char('x'))]
fn review_keys_do_not_modify_the_chat_draft(code: KeyCode) {
    let mut app = test_app();
    app.input_box.set_input(DRAFT);
    app.review.open(PathBuf::from(&app.state.session.cwd));
    app.update(Msg::Key(KeyEvent::new(code, KeyModifiers::NONE)));
    assert_eq!(app.input_box.buffer.value(), DRAFT);
    assert!(app.review.is_open());
}

#[test]
fn review_paste_does_not_leak_into_the_chat_draft() {
    let mut app = test_app();
    app.input_box.set_input(DRAFT);
    app.review.open(PathBuf::from(&app.state.session.cwd));
    app.update(Msg::Paste(REVIEW_PROMPT.into()));
    assert_eq!(app.input_box.buffer.value(), DRAFT);
}

#[test]
fn review_submission_uses_the_current_session_and_preserves_its_draft() {
    let mut app = test_app();
    app.input_box.set_input(DRAFT);
    app.review.open(PathBuf::from(&app.state.session.cwd));
    let actions = app.submit_review(REVIEW_PROMPT.into());
    assert!(
        matches!(actions.as_slice(), [Action::SendMessage(message)] if message.message == REVIEW_PROMPT)
    );
    assert!(!app.review.is_open());
    assert_eq!(app.input_box.buffer.value(), DRAFT);
}

#[test]
fn review_submission_queues_while_the_agent_is_busy() {
    let mut app = test_app();
    app.status = Status::Streaming;
    app.review.open(PathBuf::from(&app.state.session.cwd));
    assert!(app.submit_review(REVIEW_PROMPT.into()).is_empty());
    assert_eq!(app.queue.len(), 1);
    assert!(!app.review.is_open());
}

#[test]
fn rejected_review_submission_keeps_the_review_open() {
    let mut app = test_app();
    app.pending_input = PendingInput::AuthRetry { subagent_id: None };
    app.review.open(PathBuf::from(&app.state.session.cwd));
    assert!(app.submit_review(REVIEW_PROMPT.into()).is_empty());
    assert!(app.review.is_open());
}

#[test]
fn review_command_keeps_the_focused_subagent() {
    let mut app = test_app();
    let info = super::tests::subagent_info("review-subagent", "Review task");
    app.resolve_or_create_chat(&info, &None);
    app.active_chat = 1;
    app.input_box.set_input("/review");
    app.update(Msg::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
    assert_eq!(app.active_chat, 1);
    assert!(app.review.is_open());
}

#[test]
fn review_submission_reaches_the_focused_subagent() {
    let mut app = test_app();
    let info = super::tests::subagent_info("review-subagent", "Review task");
    app.resolve_or_create_chat(&info, &None);
    app.active_chat = 1;
    let inbox = Arc::new(SubagentInbox::default());
    app.chats[1].inbox = Some(Arc::clone(&inbox));
    app.review.open(PathBuf::from(&app.state.session.cwd));
    assert!(app.submit_review(REVIEW_PROMPT.into()).is_empty());
    assert_eq!(inbox.texts(), [REVIEW_PROMPT]);
    assert!(app.queue.is_empty());
    assert!(!app.review.is_open());
}

#[test]
fn review_notes_cannot_be_submitted_from_another_repository() {
    let mut app = test_app();
    app.review.open(PathBuf::from("original-repository"));
    assert!(app.submit_review(REVIEW_PROMPT.into()).is_empty());
    assert!(app.review.is_open());
    assert!(app.queue.is_empty());
}

#[test]
fn review_submission_to_a_closed_subagent_keeps_the_review_open() {
    let mut app = test_app();
    let info = super::tests::subagent_info("review-subagent", "Review task");
    app.resolve_or_create_chat(&info, &None);
    app.active_chat = 1;
    app.review.open(PathBuf::from(&app.state.session.cwd));
    assert!(app.submit_review(REVIEW_PROMPT.into()).is_empty());
    assert!(app.review.is_open());
    assert!(app.queue.is_empty());
}

#[test]
fn closing_all_overlays_closes_the_native_review() {
    let mut app = test_app();
    app.review.open(PathBuf::from(&app.state.session.cwd));
    app.close_all_overlays();
    assert!(!app.review.is_open());
}

#[test]
fn permission_prompt_is_visible_and_answers_keys_above_review() {
    let mut app = test_app();
    app.review.open(PathBuf::from(&app.state.session.cwd));
    app.permission_prompt.push(
        "review-permission".into(),
        maki_config::ToolKey::native("bash"),
        vec![PERMISSION_SCOPE.into()],
        None,
        true,
        None,
    );
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    let frame = terminal
        .draw(|frame| {
            app.view(frame);
        })
        .unwrap();
    let text: String = frame
        .buffer
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect();
    assert!(text.contains(PERMISSION_SCOPE));
    app.update(Msg::Key(KeyEvent::new(
        KeyCode::Char('y'),
        KeyModifiers::NONE,
    )));
    assert!(!app.permission_prompt.is_open());
    assert!(app.review.is_open());
}

#[test]
fn review_takes_keys_over_a_plan_form() {
    let mut app = test_app();
    app.state.mode = super::Mode::Plan;
    app.plan_form.on_plan_ready();
    app.review.open(PathBuf::from(&app.state.session.cwd));
    app.update(Msg::Key(KeyEvent::from(KeyCode::Esc)));
    assert!(!app.review.is_open());
}

#[test_case(1, 1)]
#[test_case(40, 12)]
#[test_case(120, 40)]
fn native_review_loading_renders_at_terminal_sizes(width: u16, height: u16) {
    let mut app = test_app();
    app.review.open(PathBuf::from(&app.state.session.cwd));
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| {
            app.view(frame);
        })
        .unwrap();
}
