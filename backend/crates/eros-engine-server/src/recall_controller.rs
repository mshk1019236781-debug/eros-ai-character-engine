//! Lightweight process-local controller for optional spontaneous recall.
//!
//! Explicit semantic/plot recall remains authoritative.  This controller only
//! tracks turn pressure and opens a small window in which the existing vector
//! retrieval may opportunistically run; the retrieval adapter still applies
//! all scope, score and cooldown rules.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use uuid::Uuid;

const PRESSURE_THRESHOLD: u32 = 8;
const MAX_WINDOW_RECALLS: u8 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecallKind {
    Explicit(&'static str),
    Spontaneous,
}

impl RecallKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Explicit(level) => level,
            Self::Spontaneous => "spontaneous",
        }
    }

    pub const fn is_spontaneous(self) -> bool {
        matches!(self, Self::Spontaneous)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecallDecision {
    pub turn_count: u32,
    pub recall_pressure: u32,
    pub window_until: Option<u32>,
    pub window_opened: bool,
    pub budget_before_attempt: u8,
    pub budget_remaining: u8,
    pub kind: Option<RecallKind>,
}

#[derive(Debug, Clone, Copy)]
struct RecallState {
    turn_count: u32,
    last_recall_turn: Option<u32>,
    recall_pressure: u32,
    spontaneous_recall_budget: u8,
    recall_window_until: Option<u32>,
}

impl Default for RecallState {
    fn default() -> Self {
        Self {
            turn_count: 0,
            last_recall_turn: None,
            recall_pressure: 0,
            spontaneous_recall_budget: 0,
            recall_window_until: None,
        }
    }
}

#[derive(Debug, Default)]
pub struct RecallController {
    sessions: HashMap<Uuid, RecallState>,
}

impl RecallController {
    pub fn new() -> Self {
        Self::default()
    }

    /// Advance one turn and decide whether existing retrieval is eligible.
    /// Explicit triggers bypass the pressure gate.  A spontaneous decision is
    /// only an eligibility signal; vector retrieval remains the relevance gate.
    pub fn begin_turn(
        &mut self,
        session_id: Uuid,
        explicit: Option<&'static str>,
    ) -> RecallDecision {
        let state = self.sessions.entry(session_id).or_default();
        state.turn_count = state.turn_count.saturating_add(1);
        if let Some(level) = explicit {
            state.recall_pressure = 0;
            state.spontaneous_recall_budget = 0;
            state.recall_window_until = None;
            return RecallDecision {
                turn_count: state.turn_count,
                recall_pressure: state.recall_pressure,
                window_until: None,
                window_opened: false,
                budget_before_attempt: 0,
                budget_remaining: 0,
                kind: Some(RecallKind::Explicit(level)),
            };
        }

        state.recall_pressure = state.recall_pressure.saturating_add(1);
        let window_active = state
            .recall_window_until
            .is_some_and(|until| state.turn_count <= until);
        let window_opened = !window_active
            && state.spontaneous_recall_budget == 0
            && state.recall_pressure >= PRESSURE_THRESHOLD;
        if window_opened {
            state.spontaneous_recall_budget = MAX_WINDOW_RECALLS;
            state.recall_window_until =
                Some(state.turn_count.saturating_add(MAX_WINDOW_RECALLS as u32));
        }
        let in_window = state
            .recall_window_until
            .is_some_and(|until| state.turn_count <= until);
        let budget_before_attempt = state.spontaneous_recall_budget;
        let kind = if in_window && state.spontaneous_recall_budget > 0 {
            state.spontaneous_recall_budget -= 1;
            Some(RecallKind::Spontaneous)
        } else {
            None
        };
        RecallDecision {
            turn_count: state.turn_count,
            recall_pressure: state.recall_pressure,
            window_until: state.recall_window_until,
            window_opened,
            budget_before_attempt,
            budget_remaining: state.spontaneous_recall_budget,
            kind,
        }
    }

    /// Complete a turn after retrieval.  A successful recall lowers pressure;
    /// an empty/failed lookup remains fail-open and leaves normal progression.
    pub fn finish_turn(&mut self, session_id: Uuid, kind: RecallKind, recalled: bool) {
        let Some(state) = self.sessions.get_mut(&session_id) else {
            return;
        };
        if recalled {
            state.last_recall_turn = Some(state.turn_count);
            state.recall_pressure = state.recall_pressure.saturating_sub(3);
            if kind.is_spontaneous() {
                state.spontaneous_recall_budget = 0;
                state.recall_window_until = None;
            }
        }
    }

    #[cfg(test)]
    fn state(&self, session_id: Uuid) -> Option<(u32, u32, u8, Option<u32>)> {
        self.sessions.get(&session_id).map(|s| {
            (
                s.turn_count,
                s.recall_pressure,
                s.spontaneous_recall_budget,
                s.recall_window_until,
            )
        })
    }
}

static CONTROLLER: OnceLock<Mutex<RecallController>> = OnceLock::new();

pub fn begin_turn(session_id: Uuid, explicit: Option<&'static str>) -> RecallDecision {
    CONTROLLER
        .get_or_init(|| Mutex::new(RecallController::new()))
        .lock()
        .expect("recall controller mutex poisoned")
        .begin_turn(session_id, explicit)
}

pub fn finish_turn(session_id: Uuid, kind: RecallKind, recalled: bool) {
    if let Some(controller) = CONTROLLER.get() {
        if let Ok(mut guard) = controller.lock() {
            guard.finish_turn(session_id, kind, recalled);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pressure_opens_bounded_spontaneous_window() {
        let id = Uuid::new_v4();
        let mut controller = RecallController::new();
        for _ in 0..7 {
            assert_eq!(controller.begin_turn(id, None).kind, None);
        }
        let first = controller.begin_turn(id, None);
        assert_eq!(first.kind, Some(RecallKind::Spontaneous));
        assert!(first.window_opened);
        assert_eq!(
            (first.budget_before_attempt, first.budget_remaining),
            (2, 1)
        );
        let second = controller.begin_turn(id, None);
        assert_eq!(second.kind, Some(RecallKind::Spontaneous));
        assert!(!second.window_opened);
        assert_eq!(
            (second.budget_before_attempt, second.budget_remaining),
            (1, 0)
        );
        assert_eq!(controller.begin_turn(id, None).kind, None);
    }

    #[test]
    fn explicit_recall_resets_pressure() {
        let id = Uuid::new_v4();
        let mut controller = RecallController::new();
        for _ in 0..7 {
            let _ = controller.begin_turn(id, None);
        }
        assert_eq!(
            controller.begin_turn(id, Some("plot")).kind,
            Some(RecallKind::Explicit("plot"))
        );
        let (_, pressure, budget, window) = controller.state(id).unwrap();
        assert_eq!((pressure, budget, window), (0, 0, None));
    }
}
