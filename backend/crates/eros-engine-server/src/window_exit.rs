// SPDX-License-Identifier: AGPL-3.0-only
//! Pure observation of messages that have permanently left recent context.
//!
//! This module does not write checkpoints or episodes. The coordinator keeps
//! only an in-process cursor per session; the post-process review hook consumes
//! its batches after a reply has been persisted.

use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};

use chrono::{DateTime, Utc};
use eros_engine_store::chat::ChatMessage;
use uuid::Uuid;

use crate::history_window::{select_window_for_mode, ConversationMode};
use crate::repetition::Injected;

/// Default threshold before exited messages are emitted as one batch.
pub const DEFAULT_WINDOW_EXIT_BATCH_SIZE: usize = 5;

static REVIEW_COORDINATOR: OnceLock<Mutex<WindowExitCoordinator>> = OnceLock::new();

/// Process-local coordinator used by the low-frequency semantic review hook.
/// The lock covers only pure window selection and cursor advancement; no I/O
/// or model call runs while it is held.
pub fn next_review_batch(
    user_id: Uuid,
    instance_id: Uuid,
    session_id: Uuid,
    current_message_id: Uuid,
    history: &[ChatMessage],
    mode: ConversationMode,
    extra: usize,
) -> Option<WindowExitBatch> {
    let coordinator = REVIEW_COORDINATOR.get_or_init(|| {
        Mutex::new(WindowExitCoordinator::new(WindowExitConfig::new(
            DEFAULT_WINDOW_EXIT_BATCH_SIZE,
        )))
    });
    coordinator.lock().ok()?.next_batch_for_mode(
        user_id,
        instance_id,
        session_id,
        current_message_id,
        history,
        mode,
        extra,
    )
}

/// Batching limit supplied by the eventual caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WindowExitConfig {
    pub batch_size: usize,
}

impl WindowExitConfig {
    pub fn new(batch_size: usize) -> Self {
        assert!(batch_size > 0, "window-exit batch size must not be zero");
        Self { batch_size }
    }
}

/// A chronological range which is no longer eligible for recent context.
#[derive(Debug, Clone)]
pub struct WindowExitBatch {
    pub user_id: Uuid,
    pub instance_id: Uuid,
    pub session_id: Uuid,
    pub start_message_id: Uuid,
    pub end_message_id: Uuid,
    pub messages: Vec<ChatMessage>,
    pub created_at: DateTime<Utc>,
}

/// Lightweight, in-process deduplication around [`window_exit_batch`].
#[derive(Debug)]
pub struct WindowExitCoordinator {
    config: WindowExitConfig,
    checkpoints: HashMap<Uuid, Uuid>,
}

impl WindowExitCoordinator {
    pub fn new(config: WindowExitConfig) -> Self {
        Self {
            config,
            checkpoints: HashMap::new(),
        }
    }

    /// Return newly exited messages and advance this session's checkpoint.
    ///
    /// `history` must contain the complete session in chronological order.
    /// Advancing happens only when at least `batch_size` messages are ready.
    pub fn next_batch(
        &mut self,
        user_id: Uuid,
        instance_id: Uuid,
        session_id: Uuid,
        history: &[ChatMessage],
        retained_message_ids: &HashSet<Uuid>,
    ) -> Option<WindowExitBatch> {
        let checkpoint = self.checkpoints.get(&session_id).copied();
        let batch = window_exit_batch(
            self.config,
            checkpoint,
            user_id,
            instance_id,
            session_id,
            history,
            retained_message_ids,
            Utc::now(),
        )?;
        self.checkpoints.insert(session_id, batch.end_message_id);
        Some(batch)
    }

    /// Derive the retained IDs from the same mode-aware history policy used
    /// by generation, then emit only rows that policy actually excluded.
    /// `extra` must be the dynamic ladder value calculated for the same
    /// history (for example from `character_insights`).
    pub fn next_batch_for_mode(
        &mut self,
        user_id: Uuid,
        instance_id: Uuid,
        session_id: Uuid,
        current_message_id: Uuid,
        history: &[ChatMessage],
        mode: ConversationMode,
        extra: usize,
    ) -> Option<WindowExitBatch> {
        let retained: HashSet<Uuid> = select_window_for_mode(
            history
                .iter()
                .map(|message| Injected {
                    id: message.id,
                    role: message.role.clone(),
                    text: message.content.clone(),
                })
                .collect(),
            current_message_id,
            mode,
            extra,
        )
        .into_iter()
        .map(|message| message.id)
        .collect();
        self.next_batch(user_id, instance_id, session_id, history, &retained)
    }
}

/// Compute one batch without changing history or any persisted state.
#[allow(clippy::too_many_arguments)]
pub fn window_exit_batch(
    config: WindowExitConfig,
    checkpoint: Option<Uuid>,
    user_id: Uuid,
    instance_id: Uuid,
    session_id: Uuid,
    history: &[ChatMessage],
    retained_message_ids: &HashSet<Uuid>,
    created_at: DateTime<Utc>,
) -> Option<WindowExitBatch> {
    let start = match checkpoint {
        None => 0,
        Some(id) => history.iter().position(|message| message.id == id)? + 1,
    };
    let messages: Vec<ChatMessage> = history[start..]
        .iter()
        .filter(|message| !retained_message_ids.contains(&message.id))
        .cloned()
        .collect();
    if messages.len() < config.batch_size {
        return None;
    }

    Some(WindowExitBatch {
        user_id,
        instance_id,
        session_id,
        start_message_id: messages.first()?.id,
        end_message_id: messages.last()?.id,
        messages,
        created_at,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(session_id: Uuid, index: u128) -> ChatMessage {
        ChatMessage {
            id: Uuid::from_u128(index + 1),
            session_id,
            role: if index % 2 == 0 { "user" } else { "assistant" }.into(),
            content: index.to_string(),
            sent_at: DateTime::from_timestamp(index as i64, 0).unwrap(),
            client_msg_id: None,
            ghost_decision: false,
            user_message_id: None,
            continues_from_message_id: None,
            truncated: false,
            generation_id: None,
            assistant_action_type: None,
            channel: None,
            pre_filter_content: None,
            metadata: None,
            read_at: None,
        }
    }

    fn history(session_id: Uuid, len: usize) -> Vec<ChatMessage> {
        (0..len)
            .map(|index| message(session_id, index as u128))
            .collect()
    }

    fn coordinator(batch_size: usize) -> WindowExitCoordinator {
        WindowExitCoordinator::new(WindowExitConfig::new(batch_size))
    }

    fn retained(ids: &[Uuid]) -> HashSet<Uuid> {
        ids.iter().copied().collect()
    }

    fn ids(messages: &[ChatMessage]) -> Vec<Uuid> {
        messages.iter().map(|message| message.id).collect()
    }

    #[test]
    fn history_within_window_does_not_generate_a_batch() {
        let session_id = Uuid::new_v4();
        let rows = history(session_id, 4);
        assert!(coordinator(DEFAULT_WINDOW_EXIT_BATCH_SIZE)
            .next_batch(
                Uuid::new_v4(),
                Uuid::new_v4(),
                session_id,
                &rows,
                &retained(&ids(&rows)),
            )
            .is_none());
    }

    #[test]
    fn exited_rows_below_batch_size_are_held() {
        let session_id = Uuid::new_v4();
        let rows = history(session_id, 8);
        assert!(coordinator(DEFAULT_WINDOW_EXIT_BATCH_SIZE)
            .next_batch(
                Uuid::new_v4(),
                Uuid::new_v4(),
                session_id,
                &rows,
                &retained(&ids(&rows[3..])),
            )
            .is_none());
    }

    #[test]
    fn batch_threshold_emits_the_correct_range() {
        let user_id = Uuid::new_v4();
        let instance_id = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        let rows = history(session_id, 9);
        let batch = coordinator(DEFAULT_WINDOW_EXIT_BATCH_SIZE)
            .next_batch(
                user_id,
                instance_id,
                session_id,
                &rows,
                &retained(&ids(&rows[5..])),
            )
            .unwrap();

        assert_eq!(batch.user_id, user_id);
        assert_eq!(batch.instance_id, instance_id);
        assert_eq!(batch.session_id, session_id);
        assert_eq!(batch.start_message_id, rows[0].id);
        assert_eq!(batch.end_message_id, rows[4].id);
        assert_eq!(ids(&batch.messages), ids(&rows[..5]));
        assert!(batch.created_at <= Utc::now());
    }

    #[test]
    fn repeated_call_does_not_emit_the_same_range() {
        let session_id = Uuid::new_v4();
        let rows = history(session_id, 9);
        let mut coordinator = coordinator(DEFAULT_WINDOW_EXIT_BATCH_SIZE);
        let first = coordinator.next_batch(
            Uuid::new_v4(),
            Uuid::new_v4(),
            session_id,
            &rows,
            &retained(&ids(&rows[5..])),
        );
        let repeated = coordinator.next_batch(
            Uuid::new_v4(),
            Uuid::new_v4(),
            session_id,
            &rows,
            &retained(&ids(&rows[5..])),
        );

        assert!(first.is_some());
        assert!(repeated.is_none());
    }

    #[test]
    fn added_history_emits_only_the_newly_exited_range() {
        let session_id = Uuid::new_v4();
        let first_rows = history(session_id, 9);
        let mut coordinator = coordinator(DEFAULT_WINDOW_EXIT_BATCH_SIZE);
        let first = coordinator
            .next_batch(
                Uuid::new_v4(),
                Uuid::new_v4(),
                session_id,
                &first_rows,
                &retained(&ids(&first_rows[5..])),
            )
            .unwrap();

        let later_rows = history(session_id, 15);
        let second = coordinator
            .next_batch(
                Uuid::new_v4(),
                Uuid::new_v4(),
                session_id,
                &later_rows,
                &retained(&ids(&later_rows[11..])),
            )
            .unwrap();

        assert_eq!(ids(&first.messages), ids(&first_rows[..5]));
        assert_eq!(ids(&second.messages), ids(&later_rows[5..11]));
    }

    #[test]
    fn current_and_most_recent_complete_exchange_are_never_emitted() {
        let session_id = Uuid::new_v4();
        let rows = history(session_id, 7);
        let current = rows[6].id;
        let batch = coordinator(2)
            .next_batch(
                Uuid::new_v4(),
                Uuid::new_v4(),
                session_id,
                &rows,
                &retained(&ids(&rows[4..])),
            )
            .unwrap();
        let batch_ids: HashSet<Uuid> = ids(&batch.messages).into_iter().collect();

        assert!(!batch_ids.contains(&current));
        assert!(!batch_ids.contains(&rows[4].id));
        assert!(!batch_ids.contains(&rows[5].id));
        assert_eq!(ids(&batch.messages), ids(&rows[..4]));
    }

    #[test]
    fn only_messages_excluded_by_the_actual_history_window_are_emitted() {
        use crate::history_window::ConversationMode;

        let session_id = Uuid::new_v4();
        let rows = history(session_id, 15);
        let current = rows[14].id;
        let mut coordinator = coordinator(2);
        let batch = coordinator
            .next_batch_for_mode(
                Uuid::new_v4(),
                Uuid::new_v4(),
                session_id,
                current,
                &rows,
                ConversationMode::Narrative,
                0,
            )
            .unwrap();
        assert!(!batch.messages.iter().any(|message| message.id == current));

        let retained: HashSet<Uuid> =
            select_window_for_mode_for_test(&rows, current, ConversationMode::Narrative, 0);
        assert!(batch
            .messages
            .iter()
            .all(|message| !retained.contains(&message.id)));
    }

    fn select_window_for_mode_for_test(
        rows: &[ChatMessage],
        current: Uuid,
        mode: ConversationMode,
        extra: usize,
    ) -> HashSet<Uuid> {
        select_window_for_mode(
            rows.iter()
                .map(|message| Injected {
                    id: message.id,
                    role: message.role.clone(),
                    text: message.content.clone(),
                })
                .collect(),
            current,
            mode,
            extra,
        )
        .into_iter()
        .map(|message| message.id)
        .collect()
    }
}
