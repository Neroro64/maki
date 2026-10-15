//! Git checkpoints of the working tree, taken on the tab's turn ends.
//!
//! The commit shells out to git, so it runs on a blocking thread and reports
//! back over a channel: the UI thread never waits on git. A rewind is the one
//! place that does wait, because the files have to be back before the agent
//! restarts on the truncated history.

use std::env;
use std::path::PathBuf;

use maki_storage::checkpoint::{Checkpoints, StoredCheckpoint, session_subdir};

use super::App;

pub(super) const FLASH_ON: &str = "Checkpoints on";
pub(super) const FLASH_OFF: &str = "Checkpoints off";
const FLASH_RESTORE_FAILED: &str = "checkpoint restore failed";
const FLASH_FAILED: &str = "checkpoint failed";

/// What a background commit reports to the tab that asked for it.
pub(crate) enum CheckpointEvent {
    Committed { messages_len: usize, sha: String },
    Failed { message: String },
}

impl App {
    /// The session's own toggle wins; the config default covers the sessions
    /// that never had one set.
    pub(crate) fn checkpoint_enabled(&self) -> bool {
        self.state
            .session
            .meta
            .checkpoint
            .unwrap_or(self.checkpoint_default)
    }

    fn checkpoints(&self) -> Checkpoints {
        let cwd = env::current_dir().unwrap_or_else(|_| PathBuf::from(&self.state.session.cwd));
        let session_id = self.state.session.id.to_string();
        Checkpoints::new(&session_subdir(self.storage.path(), &session_id), &cwd)
    }

    /// The transcript length the next checkpoint has to record: the agent's
    /// mirror is ahead of the session's copy while a turn is running.
    fn live_messages_len(&self) -> usize {
        self.shared_history
            .as_ref()
            .map(|history| history.load_full().messages.len())
            .unwrap_or_else(|| self.state.session.messages().len())
    }

    fn spawn_checkpoint(&self, messages_len: usize) {
        let mut checkpoints = self.checkpoints();
        let tx = self.checkpoint_tx.clone();
        smol::spawn(async move {
            let event = match smol::unblock(move || checkpoints.commit(messages_len)).await {
                Ok(Some(sha)) => CheckpointEvent::Committed { messages_len, sha },
                Ok(None) => return,
                Err(e) => CheckpointEvent::Failed {
                    message: e.to_string(),
                },
            };
            let _ = tx.send(event);
        })
        .detach();
    }

    /// The baseline a rewind needs: with checkpoints on and nothing recorded,
    /// the state the session starts from is the one to come back to.
    pub(crate) fn ensure_initial_checkpoint(&self) {
        if self.checkpoint_enabled() && self.state.session.meta.checkpoints.is_empty() {
            self.spawn_checkpoint(self.state.session.messages().len());
        }
    }

    pub(crate) fn handle_checkpoint_event(&mut self, event: CheckpointEvent) {
        match event {
            CheckpointEvent::Committed { messages_len, sha } => {
                self.state
                    .session_mut()
                    .meta
                    .checkpoints
                    .push(StoredCheckpoint { messages_len, sha });
                self.checkpoint_now();
            }
            CheckpointEvent::Failed { message } => {
                self.flash(format!("{FLASH_FAILED}: {message}"));
            }
        }
    }

    /// The last checkpoint at or before the point the rewind truncates to.
    pub(super) fn restore_checkpoint(&mut self, messages_len: usize) {
        if !self.checkpoint_enabled() {
            return;
        }
        let Some(record) = self
            .state
            .session
            .meta
            .checkpoints
            .iter()
            .rfind(|record| record.messages_len <= messages_len)
            .cloned()
        else {
            return;
        };
        if let Err(e) = self.checkpoints().restore(&record.sha) {
            self.flash(format!("{FLASH_RESTORE_FAILED}: {e}"));
        }
    }

    pub(super) fn toggle_checkpoint(&mut self) {
        let enabled = !self.checkpoint_enabled();
        self.state.session_mut().meta.checkpoint = Some(enabled);
        self.checkpoint_now();
        self.flash((if enabled { FLASH_ON } else { FLASH_OFF }).to_owned());
        if enabled {
            self.spawn_checkpoint(self.live_messages_len());
        }
    }

    pub(crate) fn checkpoint_turn(&mut self) {
        if self.checkpoint_enabled() {
            self.spawn_checkpoint(self.live_messages_len());
        }
    }
}
