// SPDX-License-Identifier: AGPL-3.0-only
//! Pure rules for Expression Recovery V1.
//!
//! The chain this module serves:
//!
//! recent Main RP assistant output
//!   -> low-frequency Expression Review   (drift / collapse / stable)
//!   -> exemplars from the character's own stable history
//!   -> temporary [expression_reference] block on the Main RP prompt
//!   -> 2 consecutive stable reviews end the injection
//!
//! Everything here is deterministic and I/O-free, so the whole decision
//! surface -- the closed tag vocabulary, the sample selection, the recovery
//! transition and the rendered block -- is unit-testable without a database,
//! a provider, or a running server.
//!
//! What deliberately does NOT live here: the judgement of whether a window has
//! drifted. That is the Review model's job (see `expression_review` in the
//! server crate). This module only bounds what the model may say and what the
//! engine does with it.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The closed tag vocabulary.
///
/// Small on purpose. A tag exists to route one recovery turn to a handful of
/// relevant exemplars, not to describe a scene. Anything outside this list is
/// dropped rather than stored: a tag vocabulary that grows by model fiat
/// stops being a routing key within a week.
pub const EXPRESSION_TAGS: [&str; 6] = [
    "conflict",
    "care",
    "refusal",
    "jealousy",
    "casual",
    "confrontation",
];

/// Assistant turns between two Expression Reviews.
///
/// The brief allows 10-15; 12 is the middle. It is the same order of magnitude
/// as the existing window-exit semantic review (a batch of 5 mixed messages),
/// so the engine keeps one notion of "low frequency" rather than two.
pub const EXPRESSION_REVIEW_EVERY_TURNS: usize = 12;

/// How many recent assistant turns one review actually sees. The review is
/// shown the raw text of these and nothing else -- no plot history, no Event DB.
pub const EXPRESSION_REVIEW_WINDOW_TURNS: usize = 12;

/// Exemplars one stable review window may contribute. Equal to the injection
/// ceiling, so a single window can never over-supply the prompt.
pub const EXEMPLARS_PER_STABLE_WINDOW: usize = 3;

/// Injection budget. Three is the brief's ceiling. Two is the floor the brief
/// names; one is accepted only when the character's pool is genuinely that
/// thin, because injecting nothing while recovery is active is worse than
/// injecting a single authentic sample.
pub const PROMPT_EXEMPLAR_MAX: usize = 3;

/// Consecutive stable reviews before injection stops. Two rather than one is
/// the entire anti-flapping measure: a character that is stable, drifting,
/// stable, drifting must not toggle the block on and off every review.
pub const STABLE_REVIEWS_TO_END: u32 = 2;

/// Active exemplars kept per character. The pool is a reservoir of stable
/// expression, not an archive of every reply ever sent; the oldest rows are
/// deactivated once this is exceeded.
pub const MAX_ACTIVE_EXEMPLARS: i64 = 12;

/// A reply shorter than this carries no expression worth imitating.
pub const MIN_EXEMPLAR_CHARS: usize = 12;

/// A reply longer than this is a scene, not a gesture -- it teaches plot, and
/// plot is exactly what an expression exemplar must not teach.
pub const MAX_EXEMPLAR_CHARS: usize = 600;

/// Upper bound on `signals` kept from one review output.
pub const MAX_SIGNALS: usize = 6;
/// Upper bound on one `signals` entry, in characters.
pub const MAX_SIGNAL_CHARS: usize = 80;

/// Map a raw string onto the closed vocabulary. Case-insensitive and trimmed;
/// anything unrecognised returns `None` so callers must decide to drop it.
pub fn normalize_tag(raw: &str) -> Option<&'static str> {
    let needle = raw.trim().to_ascii_lowercase();
    EXPRESSION_TAGS
        .iter()
        .copied()
        .find(|tag| *tag == needle.as_str())
}

/// Normalise a tag list: drop unknown tags, drop duplicates, keep order.
pub fn normalize_tags(raw: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for item in raw {
        let Some(tag) = normalize_tag(item) else {
            continue;
        };
        if !out.iter().any(|existing| existing == tag) {
            out.push(tag.to_string());
        }
    }
    out
}

/// What one review concluded about the recent window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExpressionStatus {
    Stable,
    Drift,
    Collapse,
}

impl ExpressionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::Drift => "drift",
            Self::Collapse => "collapse",
        }
    }

    /// Both drift and collapse start or continue recovery; only stable can end
    /// it. Callers should branch on this rather than enumerate the two.
    pub fn needs_recovery(self) -> bool {
        !matches!(self, Self::Stable)
    }
}

/// One sample the Review model nominated as representative of stable
/// expression, plus the tags it assigned to that sample.
///
/// `index` is a 1-based reference into the numbered samples the review was
/// shown. The text itself is never carried here: the engine stores the raw
/// persisted row, so a model can select an exemplar but can never rewrite one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExemplarPick {
    pub index: usize,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// The review model's entire output surface.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpressionReviewOutput {
    pub status: ExpressionStatus,
    pub severity: f64,
    #[serde(default)]
    pub signals: Vec<String>,
    #[serde(default)]
    pub target_tags: Vec<String>,
    #[serde(default)]
    pub exemplar_picks: Vec<ExemplarPick>,
}

impl ExpressionReviewOutput {
    /// Bound one model output to what this module is willing to act on.
    ///
    /// Normalising rather than rejecting is deliberate: a provider that
    /// returns a 1.4 severity or an unknown tag has still told us `status`,
    /// and throwing the whole review away over a scale overshoot would make
    /// recovery hostage to phrasing. Nothing here can turn a `stable` into a
    /// recovery; it can only narrow what a drift may do.
    pub fn normalize(&mut self, sample_count: usize) {
        if !self.severity.is_finite() {
            self.severity = 0.0;
        }
        self.severity = self.severity.clamp(0.0, 1.0);

        self.signals = self
            .signals
            .iter()
            .map(|signal| signal.trim().replace(['\n', '\r'], " "))
            .filter(|signal| !signal.is_empty())
            .map(|signal| signal.chars().take(MAX_SIGNAL_CHARS).collect::<String>())
            .take(MAX_SIGNALS)
            .collect();

        self.target_tags = normalize_tags(&self.target_tags);

        let mut seen: Vec<usize> = Vec::new();
        self.exemplar_picks = self
            .exemplar_picks
            .iter()
            .filter(|pick| pick.index >= 1 && pick.index <= sample_count)
            .filter(|pick| {
                if seen.contains(&pick.index) {
                    return false;
                }
                seen.push(pick.index);
                true
            })
            .take(EXEMPLARS_PER_STABLE_WINDOW)
            .map(|pick| ExemplarPick {
                index: pick.index,
                tags: normalize_tags(&pick.tags),
            })
            .collect();
    }
}

/// Is this persisted reply usable as an exemplar?
///
/// The program owns mechanical eligibility -- length band, trailer leakage,
/// "does it contain language at all". The Review model owns the judgement the
/// program cannot make deterministically: whether a reply is *representative*
/// expression or just plot information. Splitting it this way is what keeps
/// this function free of a second classifier.
pub fn is_eligible_exemplar_text(text: &str) -> bool {
    let trimmed = text.trim();
    let len = trimmed.chars().count();
    if len < MIN_EXEMPLAR_CHARS || len > MAX_EXEMPLAR_CHARS {
        return false;
    }
    // The hidden memory trailer must never reach the prompt, and an exemplar
    // is rendered verbatim.
    if trimmed.contains("<eros_memory") {
        return false;
    }
    // A row of punctuation or stage directions is not an expression sample.
    trimmed.chars().any(|c| c.is_alphanumeric() || is_cjk(c))
}

fn is_cjk(c: char) -> bool {
    matches!(c, '\u{3400}'..='\u{9fff}' | '\u{f900}'..='\u{faff}')
}

/// The dedupe key for one exemplar: the text with all whitespace removed.
///
/// Every byte a model wrote stays in `raw_text`; this is only the identity of
/// the reply. Whitespace is dropped rather than collapsed because collapsing
/// would not catch the case that actually recurs here: the product's replies
/// are predominantly CJK, so a reply persisted with a line break in the middle
/// has no space where the unwrapped original has one. Dropping it makes the
/// two the same row, and two genuinely different replies never differ by
/// whitespace alone.
pub fn exemplar_text_key(text: &str) -> String {
    text.chars().filter(|c| !c.is_whitespace()).collect()
}

/// Choose which of a stable window's samples become exemplars.
///
/// The model's picks win when they are mechanically eligible; the tail is
/// topped up deterministically from the newest eligible samples so a review
/// that returns no picks still leaves the pool growing. Bounded by
/// [`EXEMPLARS_PER_STABLE_WINDOW`] either way.
pub fn select_stable_exemplar_indices(samples: &[String], picks: &[ExemplarPick]) -> Vec<usize> {
    let eligible = |index: usize| -> bool {
        samples
            .get(index)
            .is_some_and(|text| is_eligible_exemplar_text(text))
    };

    let mut chosen: Vec<usize> = Vec::new();
    for pick in picks {
        let index = pick.index - 1;
        if chosen.len() == EXEMPLARS_PER_STABLE_WINDOW {
            break;
        }
        if eligible(index) && !chosen.contains(&index) {
            chosen.push(index);
        }
    }

    for index in (0..samples.len()).rev() {
        if chosen.len() == EXEMPLARS_PER_STABLE_WINDOW {
            break;
        }
        if eligible(index) && !chosen.contains(&index) {
            chosen.push(index);
        }
    }

    chosen
}

/// One exemplar as the prompt needs it: raw text plus the tags it was stored
/// under. Carries no character field because a retrieval pool is already
/// character-scoped -- there is no value of this type that could name another.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptExemplar {
    pub id: Uuid,
    pub text: String,
    pub tags: Vec<String>,
}

/// Pick the 2-3 exemplars one recovery turn actually injects.
///
/// Tag matches first, then the character's generic stable pool as fallback --
/// the brief's "no matching tag falls back to generic", with the per-character
/// cap enforced by the pool the caller passes in. Input order is preserved, so
/// a repo returning newest-first yields the most recent matches.
pub fn select_prompt_exemplars<'a>(
    pool: &'a [PromptExemplar],
    target_tags: &[String],
) -> Vec<&'a PromptExemplar> {
    let wanted = normalize_tags(target_tags);
    let mut picked: Vec<&PromptExemplar> = Vec::new();

    for exemplar in pool {
        if picked.len() == PROMPT_EXEMPLAR_MAX {
            break;
        }
        if exemplar
            .tags
            .iter()
            .any(|tag| wanted.iter().any(|want| want == tag))
        {
            picked.push(exemplar);
        }
    }
    for exemplar in pool {
        if picked.len() == PROMPT_EXEMPLAR_MAX {
            break;
        }
        if !picked.iter().any(|picked| picked.id == exemplar.id) {
            picked.push(exemplar);
        }
    }
    picked
}

/// Render the temporary reference block, or `None` when there is nothing to
/// inject.
///
/// The closing contract is load-bearing, not decoration: a raw example is the
/// most copyable thing that can reach a prompt, so the block names the three
/// failure modes the brief requires -- copying the wording, replaying the old
/// event, and amplifying a salient trait just because an example used it.
pub fn render_expression_reference(exemplars: &[&PromptExemplar]) -> Option<String> {
    if exemplars.is_empty() {
        return None;
    }
    let mut body = String::from(
        "[expression_reference]\n\
         Recent expression drift or collapse was detected for this character. The following are \n\
         authentic examples of this character's historically stable expression.\n",
    );
    for (offset, exemplar) in exemplars.iter().enumerate() {
        body.push_str(&format!(
            "\nExample {}:\n{}\n",
            offset + 1,
            exemplar.text.trim()
        ));
    }
    body.push_str(
        "\nUse them only as references for: reaction pattern, expression rhythm, the relationship \n\
         between action and dialogue, and how intensity is distributed across a turn.\n\
         Do not copy wording. Do not recreate the old event. Do not force the same action. Do not \n\
         increase the frequency of a salient trait merely because it appears in the examples.\n\
         不要复制原句，不要复刻原事件，不要机械放大显著特征（例如因为示例里出现粗口、某个句式或 \
         某个动作，就提高它们的出现频率）。这些示例只校准表达分布，不替换 expression_core， \
         不改变人物设定，也不决定当前剧情走向。保持你自己的判断和反应节奏。",
    );
    Some(body)
}

/// What the engine did to the recovery state this review.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecoveryAction {
    /// No recovery is running and none should start.
    Idle,
    /// Drift/collapse with recovery off: start injecting.
    Start,
    /// Recovery already running: re-select exemplars for the new tags and keep
    /// injecting.
    Refresh,
    /// Enough consecutive stable reviews: stop injecting.
    End,
}

/// The next recovery state, plus what changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryTransition {
    pub active: bool,
    pub consecutive_stable: u32,
    pub action: RecoveryAction,
}

/// The whole recovery state machine, in one pure function.
///
/// * stable while inactive -> nothing happens (the overwhelmingly common case)
/// * drift/collapse -> recovery starts, or refreshes if already running
/// * stable while active -> the counter advances; only the second consecutive
///   stable review ends recovery
///
/// `consecutive_stable` resets on every drift/collapse, so
/// active -> inactive -> active cannot be produced by a single alternating
/// review: leaving recovery takes two stable reviews in a row.
pub fn recovery_transition(
    active: bool,
    consecutive_stable: u32,
    status: ExpressionStatus,
) -> RecoveryTransition {
    match status {
        ExpressionStatus::Stable if !active => RecoveryTransition {
            active: false,
            consecutive_stable: 0,
            action: RecoveryAction::Idle,
        },
        ExpressionStatus::Stable => {
            let next = consecutive_stable.saturating_add(1);
            if next >= STABLE_REVIEWS_TO_END {
                RecoveryTransition {
                    active: false,
                    consecutive_stable: 0,
                    action: RecoveryAction::End,
                }
            } else {
                RecoveryTransition {
                    active: true,
                    consecutive_stable: next,
                    action: RecoveryAction::Refresh,
                }
            }
        }
        ExpressionStatus::Drift | ExpressionStatus::Collapse => RecoveryTransition {
            active: true,
            consecutive_stable: 0,
            action: if active {
                RecoveryAction::Refresh
            } else {
                RecoveryAction::Start
            },
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pick(index: usize, tags: &[&str]) -> ExemplarPick {
        ExemplarPick {
            index,
            tags: tags.iter().map(|tag| tag.to_string()).collect(),
        }
    }

    fn exemplar(text: &str, tags: &[&str]) -> PromptExemplar {
        PromptExemplar {
            id: Uuid::new_v4(),
            text: text.to_string(),
            tags: tags.iter().map(|tag| tag.to_string()).collect(),
        }
    }

    fn output(status: ExpressionStatus) -> ExpressionReviewOutput {
        ExpressionReviewOutput {
            status,
            severity: 0.5,
            signals: vec![],
            target_tags: vec![],
            exemplar_picks: vec![],
        }
    }

    #[test]
    fn tag_vocabulary_is_closed_and_deduplicated() {
        assert_eq!(normalize_tag("Conflict"), Some("conflict"));
        assert_eq!(normalize_tag("  care "), Some("care"));
        assert_eq!(normalize_tag("sadness"), None);
        assert_eq!(
            normalize_tags(&[
                "care".into(),
                "CARE".into(),
                "nonsense".into(),
                "conflict".into()
            ]),
            vec!["care".to_string(), "conflict".to_string()]
        );
    }

    #[test]
    fn normalization_bounds_severity_signals_and_picks() {
        let mut review = ExpressionReviewOutput {
            status: ExpressionStatus::Collapse,
            severity: 4.2,
            signals: vec![
                "\n短句\n".into(),
                "".into(),
                "x".repeat(400),
                "a".into(),
                "b".into(),
                "c".into(),
                "d".into(),
                "e".into(),
            ],
            target_tags: vec!["CONFLICT".into(), "unknown".into()],
            exemplar_picks: vec![
                pick(1, &["care"]),
                pick(1, &["care"]),
                pick(99, &["care"]),
                pick(2, &["care"]),
                pick(3, &["care"]),
                pick(4, &["care"]),
            ],
        };
        review.normalize(5);

        assert_eq!(review.severity, 1.0);
        assert_eq!(review.signals.len(), MAX_SIGNALS);
        assert_eq!(review.signals[0], "短句");
        assert!(review.signals[2].chars().count() <= MAX_SIGNAL_CHARS);
        assert_eq!(review.target_tags, vec!["conflict".to_string()]);
        assert_eq!(
            review
                .exemplar_picks
                .iter()
                .map(|pick| pick.index)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
    }

    #[test]
    fn out_of_range_picks_are_dropped_rather_than_clamped() {
        let mut review = output(ExpressionStatus::Stable);
        review.exemplar_picks = vec![pick(0, &[]), pick(7, &[]), pick(2, &[])];
        review.normalize(3);
        assert_eq!(
            review
                .exemplar_picks
                .iter()
                .map(|pick| pick.index)
                .collect::<Vec<_>>(),
            vec![2]
        );
    }

    #[test]
    fn eligibility_rejects_short_long_trailer_and_wordless_text() {
        assert!(!is_eligible_exemplar_text("   "));
        assert!(!is_eligible_exemplar_text("好。"));
        assert!(!is_eligible_exemplar_text(
            &"啊".repeat(MAX_EXEMPLAR_CHARS + 1)
        ));
        assert!(!is_eligible_exemplar_text(
            "啧，你先把话说清楚<eros_memory>{\"type\":\"callback\"}</eros_memory>"
        ));
        assert!(!is_eligible_exemplar_text("————————————"));
        assert!(is_eligible_exemplar_text("啧，少逞强，手给我看看。"));
    }

    #[test]
    fn text_key_ignores_whitespace_so_a_rewrapped_reply_is_one_row() {
        assert_eq!(exemplar_text_key("  你别  过来\n\n了 "), "你别过来了");
        assert_eq!(
            exemplar_text_key("啧，少逞强，手给我看看。"),
            exemplar_text_key("啧，少逞强，\n手给我看看。")
        );
        assert_ne!(
            exemplar_text_key("手给我看看。"),
            exemplar_text_key("手不用你看。")
        );
    }

    #[test]
    fn stable_window_prefers_model_picks_then_tops_up_from_the_newest() {
        let samples: Vec<String> = vec![
            "第一条，足够长的一句完整表达。".into(),
            "太短".into(),
            "第二条，也足够长的一句完整表达。".into(),
            "第三条，同样足够长的一句完整表达。".into(),
            "第四条，依然是足够长的一句完整表达。".into(),
        ];
        let chosen = select_stable_exemplar_indices(&samples, &[pick(3, &["care"])]);
        assert_eq!(chosen[0], 2, "the model pick leads");
        assert_eq!(chosen, vec![2, 4, 3]);

        let no_picks = select_stable_exemplar_indices(&samples, &[]);
        assert_eq!(no_picks, vec![4, 3, 2]);
    }

    #[test]
    fn stable_window_never_exceeds_the_per_window_cap() {
        let samples: Vec<String> = (0..9)
            .map(|index| format!("第{index}条，足够长的一句完整表达内容。"))
            .collect();
        let picks = vec![pick(1, &[]), pick(2, &[]), pick(3, &[]), pick(4, &[])];
        assert_eq!(
            select_stable_exemplar_indices(&samples, &picks).len(),
            EXEMPLARS_PER_STABLE_WINDOW
        );
    }

    #[test]
    fn prompt_selection_prefers_tag_matches_then_falls_back_to_generic() {
        let pool = vec![
            exemplar("A，一句足够长的冲突表达。", &["conflict"]),
            exemplar("B，一句足够长的关心表达。", &["care"]),
            exemplar("C，一句足够长的日常表达。", &["casual"]),
            exemplar("D，一句足够长的关心表达二。", &["care"]),
        ];
        let picked = select_prompt_exemplars(&pool, &["care".into()]);
        assert_eq!(
            picked.iter().map(|e| e.text.as_str()).collect::<Vec<_>>(),
            vec![
                "B，一句足够长的关心表达。",
                "D，一句足够长的关心表达二。",
                "A，一句足够长的冲突表达。"
            ]
        );
        assert!(picked.len() <= PROMPT_EXEMPLAR_MAX);

        let generic = select_prompt_exemplars(&pool, &["jealousy".into()]);
        assert_eq!(
            generic.iter().map(|e| e.text.as_str()).collect::<Vec<_>>(),
            vec![
                "A，一句足够长的冲突表达。",
                "B，一句足够长的关心表达。",
                "C，一句足够长的日常表达。"
            ],
            "no tag match falls back to the character's generic stable pool"
        );
    }

    #[test]
    fn prompt_selection_is_empty_for_an_empty_pool() {
        assert!(select_prompt_exemplars(&[], &["care".into()]).is_empty());
    }

    #[test]
    fn drifts_start_recovery_and_one_stable_review_does_not_end_it() {
        let start = recovery_transition(false, 0, ExpressionStatus::Collapse);
        assert_eq!(start.action, RecoveryAction::Start);
        assert!(start.active);

        let first_stable = recovery_transition(start.active, 0, ExpressionStatus::Stable);
        assert_eq!(first_stable.action, RecoveryAction::Refresh);
        assert!(
            first_stable.active,
            "one stable review must not release recovery"
        );

        let second_stable = recovery_transition(
            first_stable.active,
            first_stable.consecutive_stable,
            ExpressionStatus::Stable,
        );
        assert_eq!(second_stable.action, RecoveryAction::End);
        assert!(!second_stable.active);
    }

    #[test]
    fn alternating_reviews_cannot_flap_recovery_off() {
        let start = recovery_transition(false, 0, ExpressionStatus::Drift);
        let stable = recovery_transition(
            start.active,
            start.consecutive_stable,
            ExpressionStatus::Stable,
        );
        assert!(stable.active, "stable then drift keeps recovery on");
        let drift = recovery_transition(
            stable.active,
            stable.consecutive_stable,
            ExpressionStatus::Drift,
        );
        assert_eq!(drift.consecutive_stable, 0, "drift resets the counter");
        assert!(drift.active);

        let stable_again = recovery_transition(
            drift.active,
            drift.consecutive_stable,
            ExpressionStatus::Stable,
        );
        assert!(
            stable_again.active,
            "the counter was reset, so this is again only the first stable"
        );
    }

    #[test]
    fn stable_while_inactive_is_a_no_op() {
        let transition = recovery_transition(false, 0, ExpressionStatus::Stable);
        assert_eq!(transition.action, RecoveryAction::Idle);
        assert!(!transition.active);
        assert_eq!(transition.consecutive_stable, 0);
    }

    #[test]
    fn drift_while_active_refreshes_instead_of_restarting() {
        let transition = recovery_transition(true, 1, ExpressionStatus::Collapse);
        assert_eq!(transition.action, RecoveryAction::Refresh);
        assert!(transition.active);
        assert_eq!(transition.consecutive_stable, 0);
    }

    #[test]
    fn rendered_block_carries_the_no_copy_contract() {
        let pool = vec![
            exemplar("啧，少逞强，手给我看看。", &["care"]),
            exemplar("这也能叫计划？你想过后果吗。", &["conflict"]),
        ];
        let picked = select_prompt_exemplars(&pool, &["care".into()]);
        let block = render_expression_reference(&picked).expect("block renders");

        assert!(block.starts_with("[expression_reference]\n"));
        assert!(block.contains("Example 1:\n啧，少逞强，手给我看看。"));
        assert!(block.contains("Example 2:"));
        for required in [
            "Do not copy wording.",
            "Do not recreate the old event.",
            "Do not force the same action.",
            "不要复制原句",
            "不要复刻原事件",
            "不要机械放大显著特征",
            "不替换 expression_core",
        ] {
            assert!(block.contains(required), "missing {required} in:\n{block}");
        }
        // The exemplar text is reproduced verbatim, and exactly once.
        assert_eq!(block.matches("啧，少逞强，手给我看看。").count(), 1);
    }

    #[test]
    fn rendered_block_is_none_without_exemplars() {
        assert!(render_expression_reference(&[]).is_none());
    }
}
