//! The context-notes notebook: written by the model while the transcript it
//! summarizes is still in context, read back after that transcript is gone.

use std::sync::PoisonError;

use maki_config::AgentConfig;
use maki_providers::{Message, Model};

use super::compaction;
use super::history::History;
use crate::NotesCell;

/// Lands once per compaction cycle, in the last stretch before the transcript
/// is summarized away: the notebook is worth writing while everything it has
/// to say is still in context.
const REMINDER: &str = "Context notes reminder: the context window is nearing its compaction threshold. Use context_notes to save a compact durable notebook (task state, decisions, changed files, blockers, next steps — one line per item) before compaction.";
/// What the notebook is worth is that the model reads it again after the
/// transcript it wrote it from is gone.
const INJECTED: &str = "Context notes (persisted, current):";
const CHANGED: &str = "Context notes now read:";
/// The reminder lands in the last fifth of the room before the threshold.
const REMINDER_LEAD_DIVISOR: u32 = 5;

/// The notebook state one [`super::run::Agent`] run keeps: the shared cell the
/// frontend and plugins write, plus what this run has already told the model
/// about it.
pub(super) struct NotesState {
    cell: NotesCell,
    reminded: bool,
    told: Option<String>,
}

impl NotesState {
    pub(super) fn new(cell: NotesCell) -> Self {
        Self {
            cell,
            reminded: false,
            told: None,
        }
    }

    fn text(&self) -> Option<String> {
        self.cell
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// The notebook only pays for itself when it is written while everything
    /// it has to say is still in context, so the model hears about it in the
    /// last stretch before the threshold, once per compaction cycle.
    pub(super) fn maybe_reminder(
        &mut self,
        config: &AgentConfig,
        model: &Model,
        context_size: u32,
        history: &mut History,
    ) {
        if !config.context_notes || self.reminded {
            return;
        }
        let usable = compaction::usable(model, config);
        if context_size >= usable - usable / REMINDER_LEAD_DIVISOR && context_size < usable {
            history.push(Message::synthetic(REMINDER.to_owned()));
            self.reminded = true;
        }
    }

    /// The notebook the model last read, told to it as it was then.
    pub(super) fn tell_change(&mut self, config: &AgentConfig, history: &mut History) {
        if !config.context_notes {
            return;
        }
        let current = self.text();
        if self.told == current {
            return;
        }
        self.told = current.clone();
        if let Some(text) = current.filter(|text| !text.is_empty()) {
            history.push(Message::synthetic(format!("{CHANGED}\n{text}")));
        }
    }

    /// Re-injects the notebook after its transcript was summarized away.
    pub(super) fn inject(&mut self, config: &AgentConfig, history: &mut History) {
        if !config.context_notes {
            return;
        }
        let Some(text) = self.text().filter(|text| !text.is_empty()) else {
            return;
        };
        self.told = Some(text.clone());
        history.push(Message::synthetic(format!("{INJECTED}\n{text}")));
    }

    /// The next compaction cycle gets its own reminder.
    pub(super) fn cycle_done(&mut self) {
        self.reminded = false;
    }
}

#[cfg(test)]
mod tests {
    use test_case::test_case;

    use super::*;

    const NOTE_BODY: &str = "task: ship the parser";
    /// `usable` depends on the config's compaction buffer, so the test sizes
    /// itself against the real value instead of hard-coding a window.
    #[derive(Clone, Copy)]
    enum Segment {
        Below,
        Lead,
        Past,
    }

    fn model() -> Model {
        let mut model = Model::from_spec("anthropic/claude-sonnet-4-20250514").unwrap();
        model.context_window = 200_000;
        model.max_output_tokens = Some(8_192);
        model
    }

    fn segment_size(segment: Segment, usable: u32) -> u32 {
        match segment {
            Segment::Below => usable / 2,
            Segment::Lead => usable - usable / (REMINDER_LEAD_DIVISOR * 2),
            Segment::Past => usable,
        }
    }

    fn synthetic_texts(history: &History) -> Vec<String> {
        history
            .as_slice()
            .iter()
            .filter(|m| m.display_text.as_deref() == Some(""))
            .filter_map(Message::first_text_content)
            .map(str::to_owned)
            .collect()
    }

    #[test_case(true, Segment::Below, false ; "below_the_lead")]
    #[test_case(true, Segment::Lead, true ; "in_the_lead")]
    #[test_case(true, Segment::Past, false ; "past_the_threshold")]
    #[test_case(false, Segment::Lead, false ; "disabled_in_the_lead")]
    fn the_reminder_lands_once_and_only_in_the_lead(
        enabled: bool,
        segment: Segment,
        expected: bool,
    ) {
        let config = AgentConfig {
            context_notes: enabled,
            ..AgentConfig::default()
        };
        let usable = compaction::usable(&model(), &config);
        let mut history = History::new(vec![Message::user("go".into())]);
        let mut notes = NotesState::new(NotesCell::default());
        notes.maybe_reminder(
            &config,
            &model(),
            segment_size(segment, usable),
            &mut history,
        );
        notes.maybe_reminder(
            &config,
            &model(),
            segment_size(segment, usable),
            &mut history,
        );
        let landed = synthetic_texts(&history)
            .iter()
            .filter(|text| text.as_str() == REMINDER)
            .count();
        assert_eq!(landed, usize::from(expected));
    }

    #[test]
    fn notes_reach_the_model_on_a_write_and_never_again_untouched() {
        let config = AgentConfig {
            context_notes: true,
            ..AgentConfig::default()
        };
        let mut history = History::new(vec![Message::user("go".into())]);
        let mut notes = NotesState::new(NotesCell::default());
        notes.tell_change(&config, &mut history);

        *notes.cell.lock().unwrap() = Some(NOTE_BODY.into());
        notes.tell_change(&config, &mut history);
        notes.tell_change(&config, &mut history);
        *notes.cell.lock().unwrap() = None;
        notes.tell_change(&config, &mut history);
        assert_eq!(
            synthetic_texts(&history),
            [format!("{CHANGED}\n{NOTE_BODY}")]
        );
    }

    #[test]
    fn notes_are_re_injected_after_compaction_and_told_as_read() {
        let config = AgentConfig {
            context_notes: true,
            ..AgentConfig::default()
        };
        let mut history = History::new(vec![Message::user("go".into())]);
        let mut notes = NotesState::new(NotesCell::default());
        *notes.cell.lock().unwrap() = Some(NOTE_BODY.into());
        notes.inject(&config, &mut history);
        notes.tell_change(&config, &mut history);
        assert_eq!(
            synthetic_texts(&history),
            [format!("{INJECTED}\n{NOTE_BODY}")]
        );
    }

    #[test_case(true, 2 ; "cycle_resets_the_reminder")]
    #[test_case(false, 0 ; "disabled_never_reminds")]
    fn a_new_cycle_gets_a_new_reminder(enabled: bool, expected: usize) {
        let config = AgentConfig {
            context_notes: enabled,
            ..AgentConfig::default()
        };
        let usable = compaction::usable(&model(), &config);
        let size = segment_size(Segment::Lead, usable);
        let mut history = History::new(vec![Message::user("go".into())]);
        let mut notes = NotesState::new(NotesCell::default());
        notes.maybe_reminder(&config, &model(), size, &mut history);
        notes.cycle_done();
        notes.maybe_reminder(&config, &model(), size, &mut history);
        let landed = synthetic_texts(&history)
            .iter()
            .filter(|text| text.as_str() == REMINDER)
            .count();
        assert_eq!(landed, expected);
    }
}
