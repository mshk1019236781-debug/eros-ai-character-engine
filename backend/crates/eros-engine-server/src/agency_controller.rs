//! Minimal deterministic Agency pressure and eligibility gate.

use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use uuid::Uuid;

const PRESSURE_TRIGGER: u32 = 3;
const COOLDOWN_TURNS: u32 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgencyDecision {
    pub turn_count: u32,
    pub pressure_before: u32,
    pub passive_turns: u32,
    pub agency_pressure: u32,
    pub pressure_after: u32,
    pub triggered: bool,
    pub blocked_reason: Option<&'static str>,
    pub cooldown_until: Option<u32>,
}

#[derive(Debug, Clone, Copy, Default)]
struct AgencyState {
    turn_count: u32,
    passive_turns: u32,
    agency_pressure: u32,
    cooldown_until: Option<u32>,
}

#[derive(Debug, Default)]
pub struct AgencyController {
    sessions: HashMap<Uuid, AgencyState>,
}

impl AgencyController {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn begin_turn(&mut self, session_id: Uuid) -> AgencyDecision {
        self.begin_turn_with_gate(session_id, None)
    }

    /// Blocked thresholds remain armed; only an eligible trigger resets state.
    pub fn begin_turn_with_gate(
        &mut self,
        session_id: Uuid,
        blocked_reason: Option<&'static str>,
    ) -> AgencyDecision {
        let state = self.sessions.entry(session_id).or_default();
        state.turn_count = state.turn_count.saturating_add(1);
        let in_cooldown = state
            .cooldown_until
            .is_some_and(|until| state.turn_count <= until);
        if in_cooldown {
            return AgencyDecision {
                turn_count: state.turn_count,
                pressure_before: state.agency_pressure,
                passive_turns: state.passive_turns,
                agency_pressure: state.agency_pressure,
                pressure_after: state.agency_pressure,
                triggered: false,
                blocked_reason: None,
                cooldown_until: state.cooldown_until,
            };
        }
        let pressure_before = state.agency_pressure;
        state.passive_turns = state.passive_turns.saturating_add(1);
        state.agency_pressure = state.agency_pressure.saturating_add(1);
        if state.agency_pressure < PRESSURE_TRIGGER {
            return AgencyDecision {
                turn_count: state.turn_count,
                pressure_before,
                passive_turns: state.passive_turns,
                agency_pressure: state.agency_pressure,
                pressure_after: state.agency_pressure,
                triggered: false,
                blocked_reason: None,
                cooldown_until: state.cooldown_until,
            };
        }
        if let Some(reason) = blocked_reason {
            return AgencyDecision {
                turn_count: state.turn_count,
                pressure_before,
                passive_turns: state.passive_turns,
                agency_pressure: state.agency_pressure,
                pressure_after: state.agency_pressure,
                triggered: false,
                blocked_reason: Some(reason),
                cooldown_until: state.cooldown_until,
            };
        }
        state.passive_turns = 0;
        state.agency_pressure = 0;
        state.cooldown_until = Some(state.turn_count.saturating_add(COOLDOWN_TURNS));
        AgencyDecision {
            turn_count: state.turn_count,
            pressure_before,
            passive_turns: 0,
            agency_pressure: 0,
            pressure_after: 0,
            triggered: true,
            blocked_reason: None,
            cooldown_until: state.cooldown_until,
        }
    }
}

static CONTROLLER: OnceLock<Mutex<AgencyController>> = OnceLock::new();

pub fn begin_turn_with_gate(
    session_id: Uuid,
    blocked_reason: Option<&'static str>,
) -> AgencyDecision {
    CONTROLLER
        .get_or_init(|| Mutex::new(AgencyController::new()))
        .lock()
        .expect("agency controller mutex poisoned")
        .begin_turn_with_gate(session_id, blocked_reason)
}

/// Explicit, explainable blocks only. Ambiguous state does not block Agency.
pub fn blocked_reason(current_situation: Option<&str>, user_text: &str) -> Option<&'static str> {
    let state = current_situation.unwrap_or_default().to_ascii_lowercase();
    let user = user_text.to_ascii_lowercase();
    if contains_any(&state, &["昏迷", "coma", "unconscious"]) {
        return Some("active_state_unconscious");
    }
    if contains_any(&state, &["住院", "hospitalized", "hospital treatment"]) {
        return Some("active_state_hospitalized");
    }
    if contains_any(&state, &["手术", "surgery", "operation"]) {
        return Some("active_state_surgery");
    }
    if contains_any(
        &state,
        &["限制行动", "行动受限", "拘留", "restrained", "immobilized"],
    ) {
        return Some("active_state_restricted");
    }
    if contains_any(&state, &["不可联系", "离线", "offline", "unreachable"]) {
        return Some("active_state_uncontactable");
    }
    if contains_any(
        &user,
        &[
            "别来找我",
            "不要联系我",
            "不要联系",
            "不要打扰",
            "暂时别联系",
            "这几天别",
            "do not contact",
            "don't contact",
            "leave me alone",
            "don't bother",
        ],
    ) {
        return Some("user_requested_no_contact");
    }
    if contains_any(
        &state,
        &[
            "任务执行中",
            "连续任务",
            "不可打断",
            "进行中且不能打断",
            "uninterruptible task",
        ],
    ) {
        return Some("active_state_uninterruptible_task");
    }
    None
}

fn contains_any(haystack: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| haystack.contains(needle))
}

pub const GUIDANCE: &str = "[agency_guidance]\n本轮需要一次实质性的主动推进：在符合当前关系、Current State、剧情上下文和 expression_core 的前提下，由你决定一个具体的主动行为（例如提出问题、发出要求/邀请、改变你自己的行动、推进当前冲突，或自然提起相关经历）。不要只表达情绪、描述想法或把选择丢回给用户；也不要替用户写台词、行动、思想、感受或重大选择。你只控制自己的角色、NPC、环境及合理后果，并继续遵守 [response_contract]。";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pressure_trigger_and_cooldown() {
        let id = Uuid::new_v4();
        let mut c = AgencyController::new();
        assert!(!c.begin_turn(id).triggered);
        assert!(!c.begin_turn(id).triggered);
        let t = c.begin_turn(id);
        assert!(t.triggered);
        assert_eq!(
            (t.pressure_before, t.pressure_after, t.cooldown_until),
            (2, 0, Some(5))
        );
        assert!(!c.begin_turn(id).triggered);
        assert!(!c.begin_turn(id).triggered);
        assert!(c.begin_turn(id).agency_pressure > 0);
    }

    #[test]
    fn blocked_threshold_preserves_pressure_then_recovers() {
        let id = Uuid::new_v4();
        let mut c = AgencyController::new();
        c.begin_turn(id);
        c.begin_turn(id);
        let b = c.begin_turn_with_gate(id, Some("active_state_hospitalized"));
        assert_eq!(b.blocked_reason, Some("active_state_hospitalized"));
        assert_eq!((b.pressure_before, b.pressure_after), (2, 3));
        assert!(c.begin_turn_with_gate(id, None).triggered);
    }

    #[test]
    fn gate_recognizes_state_and_user_boundaries() {
        assert_eq!(
            blocked_reason(Some("住院治疗中"), "继续聊"),
            Some("active_state_hospitalized")
        );
        assert_eq!(
            blocked_reason(Some("正常"), "这几天别来找我"),
            Some("user_requested_no_contact")
        );
        assert_eq!(
            blocked_reason(Some("连续任务执行中，不可打断"), "你好"),
            Some("active_state_uninterruptible_task")
        );
        assert_eq!(blocked_reason(Some("正常"), "我们聊聊"), None);
    }

    #[test]
    fn six_gate_cases_pass() {
        // A: threshold with no block.
        let a = Uuid::new_v4();
        let mut c = AgencyController::new();
        c.begin_turn_with_gate(a, None);
        c.begin_turn_with_gate(a, None);
        assert!(
            c.begin_turn_with_gate(a, blocked_reason(Some("正常"), "继续聊"))
                .triggered
        );
        // B/C: explicit state and user blocks preserve the armed pressure.
        let b = Uuid::new_v4();
        let mut c = AgencyController::new();
        c.begin_turn_with_gate(b, None);
        c.begin_turn_with_gate(b, None);
        assert!(
            !c.begin_turn_with_gate(b, blocked_reason(Some("hospitalized"), "继续聊"))
                .triggered
        );
        let d = Uuid::new_v4();
        let mut c = AgencyController::new();
        c.begin_turn_with_gate(d, None);
        c.begin_turn_with_gate(d, None);
        assert!(
            !c.begin_turn_with_gate(d, blocked_reason(Some("正常"), "这几天别来找我"))
                .triggered
        );
        // D: cooldown suppresses an immediate repeat.
        let e = Uuid::new_v4();
        let mut c = AgencyController::new();
        c.begin_turn_with_gate(e, None);
        c.begin_turn_with_gate(e, None);
        assert!(c.begin_turn_with_gate(e, None).triggered);
        assert!(!c.begin_turn_with_gate(e, None).triggered);
        // E/F: recovery triggers; guidance remains a single Main RP nudge.
        let f = Uuid::new_v4();
        let mut c = AgencyController::new();
        c.begin_turn_with_gate(f, None);
        c.begin_turn_with_gate(f, None);
        assert!(
            !c.begin_turn_with_gate(f, Some("active_state_surgery"))
                .triggered
        );
        assert!(c.begin_turn_with_gate(f, None).triggered);
        assert!(GUIDANCE.contains("[response_contract]"));
    }
}
