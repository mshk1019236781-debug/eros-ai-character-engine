// SPDX-License-Identifier: AGPL-3.0-only
//! Provider-neutral semantic extraction from one window-exit batch.
//!
//! Extraction is stateless beyond the caller-supplied rolling summary and
//! active states. This module neither writes storage nor affects generation.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::history_window::ConversationMode;
use crate::window_exit::WindowExitBatch;

/// Hard cap for factual event and episode summaries returned by an extractor.
pub const MAX_CANDIDATE_SUMMARY_CHARS: usize = 280;
/// The rolling summary is deliberately smaller than a durable event.
pub const MAX_NEXT_SEMANTIC_SUMMARY_CHARS: usize = 240;
pub const MAX_CANDIDATE_EVENTS: usize = 32;
pub const MAX_ACTIVE_STATE_CANDIDATES: usize = 16;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CurrentActiveState {
    pub key: String,
    pub value: serde_json::Value,
}

/// Complete, caller-supplied context for one stateless extraction pass.
#[derive(Debug, Clone, Copy)]
pub struct SemanticExtractionInput<'a> {
    pub batch: &'a WindowExitBatch,
    pub previous_semantic_summary: Option<&'a str>,
    pub current_active_states: &'a [CurrentActiveState],
    pub conversation_mode: ConversationMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventImportance {
    Light,
    Normal,
    Important,
    Major,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationshipDirection {
    Positive,
    Negative,
    Mixed,
    Uncertain,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CandidateConfidence {
    Low,
    Medium,
    High,
}

/// A local-model proposal. It is never a durable fact until a later review.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventCandidate {
    pub summary: String,
    pub proposed_importance: EventImportance,
    pub participants: Vec<String>,
    pub location: Option<String>,
    pub source_start_message_id: Uuid,
    pub source_end_message_id: Uuid,
    pub proposed_relationship_relevant: bool,
    pub proposed_relationship_direction: RelationshipDirection,
    pub confidence: CandidateConfidence,
    pub needs_review: bool,
    pub review_reasons: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecentEpisodeCandidate {
    pub summary: String,
    pub participants: Vec<String>,
    pub location: Option<String>,
    pub source_start_message_id: Uuid,
    pub source_end_message_id: Uuid,
    pub proposed_importance: EventImportance,
    pub proposed_relationship_relevant: bool,
    pub confidence: CandidateConfidence,
    pub needs_review: bool,
    pub review_reasons: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActiveStateOperation {
    Add,
    Update,
    Remove,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActiveStateCandidate {
    pub state_key: String,
    pub proposed_value: serde_json::Value,
    pub operation: ActiveStateOperation,
    pub confidence: CandidateConfidence,
    pub needs_review: bool,
    pub review_reasons: Vec<String>,
    pub source_start_message_id: Uuid,
    pub source_end_message_id: Uuid,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SemanticExtractionResult {
    pub candidate_events: Vec<EventCandidate>,
    pub candidate_recent_episode: Option<RecentEpisodeCandidate>,
    pub candidate_active_state_changes: Vec<ActiveStateCandidate>,
    pub next_semantic_summary: Option<String>,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum SemanticExtractionError {
    #[error("semantic extractor failed: {0}")]
    Extractor(String),
    #[error("{kind}[{index}] summary must not be empty")]
    EmptySummary { kind: &'static str, index: usize },
    #[error("{kind}[{index}] summary exceeds {limit} characters")]
    SummaryTooLong {
        kind: &'static str,
        index: usize,
        limit: usize,
    },
    #[error("{kind}[{index}] contains a forbidden relative-time phrase")]
    ForbiddenRelativeTime { kind: &'static str, index: usize },
    #[error("{kind}[{index}] source message {message_id} is outside the batch")]
    SourceOutsideBatch {
        kind: &'static str,
        index: usize,
        message_id: Uuid,
    },
    #[error("{kind}[{index}] source range is reversed")]
    ReversedSourceRange { kind: &'static str, index: usize },
    #[error("active_state_changes[{index}] key must not be empty")]
    EmptyActiveStateKey { index: usize },
    #[error("candidate_events exceeds the limit of {limit}")]
    TooManyEvents { limit: usize },
    #[error("candidate_active_state_changes exceeds the limit of {limit}")]
    TooManyActiveStateChanges { limit: usize },
}

/// Interface implemented by a concrete extraction provider.
#[async_trait::async_trait]
pub trait SemanticExtractor {
    async fn extract(
        &self,
        input: SemanticExtractionInput<'_>,
    ) -> Result<SemanticExtractionResult, SemanticExtractionError>;
}

/// The only supported entry point: extract one batch, then validate its result.
pub async fn extract_and_validate(
    extractor: &impl SemanticExtractor,
    input: SemanticExtractionInput<'_>,
) -> Result<SemanticExtractionResult, SemanticExtractionError> {
    let mut result = extractor.extract(input).await?;
    mark_review_flags(&mut result);
    validate_result(input.batch, &result)?;
    Ok(result)
}

pub fn validate_result(
    batch: &WindowExitBatch,
    result: &SemanticExtractionResult,
) -> Result<(), SemanticExtractionError> {
    let positions: HashMap<Uuid, usize> = batch
        .messages
        .iter()
        .enumerate()
        .map(|(position, message)| (message.id, position))
        .collect();
    if result.candidate_events.len() > MAX_CANDIDATE_EVENTS {
        return Err(SemanticExtractionError::TooManyEvents {
            limit: MAX_CANDIDATE_EVENTS,
        });
    }
    if result.candidate_active_state_changes.len() > MAX_ACTIVE_STATE_CANDIDATES {
        return Err(SemanticExtractionError::TooManyActiveStateChanges {
            limit: MAX_ACTIVE_STATE_CANDIDATES,
        });
    }
    for (index, candidate) in result.candidate_events.iter().enumerate() {
        validate_summary(
            "candidate_events",
            index,
            &candidate.summary,
            MAX_CANDIDATE_SUMMARY_CHARS,
        )?;
        validate_source_range(
            &positions,
            "candidate_events",
            index,
            candidate.source_start_message_id,
            candidate.source_end_message_id,
        )?;
    }
    if let Some(candidate) = &result.candidate_recent_episode {
        validate_summary(
            "candidate_recent_episode",
            0,
            &candidate.summary,
            MAX_CANDIDATE_SUMMARY_CHARS,
        )?;
        validate_source_range(
            &positions,
            "candidate_recent_episode",
            0,
            candidate.source_start_message_id,
            candidate.source_end_message_id,
        )?;
    }
    for (index, candidate) in result.candidate_active_state_changes.iter().enumerate() {
        if candidate.state_key.trim().is_empty() {
            return Err(SemanticExtractionError::EmptyActiveStateKey { index });
        }
        validate_source_range(
            &positions,
            "candidate_active_state_changes",
            index,
            candidate.source_start_message_id,
            candidate.source_end_message_id,
        )?;
    }
    if let Some(summary) = &result.next_semantic_summary {
        validate_summary(
            "next_semantic_summary",
            0,
            summary,
            MAX_NEXT_SEMANTIC_SUMMARY_CHARS,
        )?;
    }
    Ok(())
}

/// Replaces model-supplied review flags with conservative, structural flags.
/// This function deliberately does not decide whether a proposal is true.
pub fn mark_review_flags(result: &mut SemanticExtractionResult) {
    for candidate in &mut result.candidate_events {
        preserve_model_review(&mut candidate.needs_review, &mut candidate.review_reasons);
        if matches!(
            candidate.proposed_importance,
            EventImportance::Important | EventImportance::Major
        ) {
            require_review(candidate, "high impact proposal");
        }
        if candidate.proposed_relationship_relevant
            && candidate.proposed_importance != EventImportance::Light
        {
            require_review(candidate, "relationship-relevant proposal");
        }
        if candidate.confidence == CandidateConfidence::Low {
            require_review(candidate, "low confidence");
        }
        if !candidate.proposed_relationship_relevant
            && candidate.proposed_relationship_direction != RelationshipDirection::Uncertain
        {
            require_review(candidate, "relationship fields are inconsistent");
        }
    }
    if let Some(candidate) = &mut result.candidate_recent_episode {
        preserve_model_review(&mut candidate.needs_review, &mut candidate.review_reasons);
        if matches!(
            candidate.proposed_importance,
            EventImportance::Important | EventImportance::Major
        ) {
            require_episode_review(candidate, "high impact episode proposal");
        }
        if candidate.confidence == CandidateConfidence::Low {
            require_episode_review(candidate, "low confidence");
        }
    }
    for candidate in &mut result.candidate_active_state_changes {
        preserve_model_review(&mut candidate.needs_review, &mut candidate.review_reasons);
        require_active_state_review(candidate, "active-state change requires review");
        if candidate.confidence == CandidateConfidence::Low {
            require_active_state_review(candidate, "low confidence");
        }
    }

    for left in 0..result.candidate_events.len() {
        for right in (left + 1)..result.candidate_events.len() {
            let a = &result.candidate_events[left];
            let b = &result.candidate_events[right];
            if a.source_start_message_id == b.source_start_message_id
                && a.source_end_message_id == b.source_end_message_id
                && (a.proposed_importance != b.proposed_importance
                    || a.proposed_relationship_direction != b.proposed_relationship_direction)
            {
                for index in [left, right] {
                    require_review(
                        &mut result.candidate_events[index],
                        "conflicting candidates in batch",
                    );
                }
            }
        }
    }
    for left in 0..result.candidate_active_state_changes.len() {
        for right in (left + 1)..result.candidate_active_state_changes.len() {
            let a = &result.candidate_active_state_changes[left];
            let b = &result.candidate_active_state_changes[right];
            if a.state_key == b.state_key && a.operation != b.operation {
                for index in [left, right] {
                    require_active_state_review(
                        &mut result.candidate_active_state_changes[index],
                        "conflicting state operations in batch",
                    );
                }
            }
        }
    }
}

/// Build conservative candidates for the production window-exit hook without
/// invoking the retired local extractor model.  This is intentionally a small
/// structural classifier: it only proposes review-worthy plot/state changes
/// when the exited text contains an unambiguous state or plot marker.  The
/// low-frequency reviewer remains the authority for keep/reject and final
/// wording.
pub fn candidates_from_window(batch: &WindowExitBatch) -> SemanticExtractionResult {
    let Some(first) = batch.messages.first() else {
        return SemanticExtractionResult::default();
    };
    let Some(last) = batch.messages.last() else {
        return SemanticExtractionResult::default();
    };
    let summary: String = batch
        .messages
        .iter()
        .map(|message| message.content.trim())
        .filter(|content| !content.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(MAX_CANDIDATE_SUMMARY_CHARS)
        .collect();
    if summary.is_empty() {
        return SemanticExtractionResult::default();
    }
    let joined = batch
        .messages
        .iter()
        .map(|message| message.content.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    let plot_marker = [
        "受伤",
        "住院",
        "手术",
        "出院",
        "失踪",
        "被找到",
        "分手",
        "和好",
        "承诺",
        "秘密",
        "任务阶段",
        "昏迷",
        "死亡",
    ]
    .iter()
    .any(|marker| joined.contains(marker));
    let state_marker = [
        "目前",
        "现在",
        "仍然",
        "还在",
        "正在",
        "持续",
        "住院",
        "治疗中",
        "医院",
    ]
    .iter()
    .any(|marker| joined.contains(marker));
    let relationship_relevant = ["喜欢", "爱", "讨厌", "分手", "和好", "关系", "承诺"]
        .iter()
        .any(|marker| joined.contains(marker));

    let mut result = SemanticExtractionResult::default();
    if plot_marker {
        result.candidate_events.push(EventCandidate {
            summary: summary.clone(),
            proposed_importance: if relationship_relevant {
                EventImportance::Major
            } else {
                EventImportance::Important
            },
            participants: vec![],
            location: None,
            source_start_message_id: first.id,
            source_end_message_id: last.id,
            proposed_relationship_relevant: relationship_relevant,
            proposed_relationship_direction: if relationship_relevant {
                RelationshipDirection::Mixed
            } else {
                RelationshipDirection::Uncertain
            },
            confidence: CandidateConfidence::Medium,
            needs_review: true,
            review_reasons: vec!["window-exit plot/state marker".into()],
        });
    }
    if state_marker {
        let state_key = if joined.contains("医院") || joined.contains("住院") {
            "location"
        } else if joined.contains("受伤") || joined.contains("治疗") {
            "physical"
        } else if relationship_relevant {
            "relationship"
        } else {
            "ongoing_plot"
        };
        result
            .candidate_active_state_changes
            .push(ActiveStateCandidate {
                state_key: state_key.into(),
                proposed_value: serde_json::Value::String(summary.clone()),
                operation: ActiveStateOperation::Add,
                confidence: CandidateConfidence::Medium,
                needs_review: true,
                review_reasons: vec!["window-exit active-state marker".into()],
                source_start_message_id: first.id,
                source_end_message_id: last.id,
            });
    }
    result
}

fn preserve_model_review(needs_review: &mut bool, reasons: &mut Vec<String>) {
    reasons.retain(|reason| !reason.trim().is_empty());
    if !reasons.is_empty() {
        *needs_review = true;
    } else if *needs_review {
        reasons.push("model requested review".into());
    }
}

fn require_review(candidate: &mut EventCandidate, reason: &str) {
    candidate.needs_review = true;
    push_unique(&mut candidate.review_reasons, reason);
}

fn require_episode_review(candidate: &mut RecentEpisodeCandidate, reason: &str) {
    candidate.needs_review = true;
    push_unique(&mut candidate.review_reasons, reason);
}

fn require_active_state_review(candidate: &mut ActiveStateCandidate, reason: &str) {
    candidate.needs_review = true;
    push_unique(&mut candidate.review_reasons, reason);
}

fn push_unique(reasons: &mut Vec<String>, reason: &str) {
    if !reasons.iter().any(|existing| existing == reason) {
        reasons.push(reason.into());
    }
}

fn validate_summary(
    kind: &'static str,
    index: usize,
    summary: &str,
    limit: usize,
) -> Result<(), SemanticExtractionError> {
    if summary.trim().is_empty() {
        return Err(SemanticExtractionError::EmptySummary { kind, index });
    }
    if summary.chars().count() > limit {
        return Err(SemanticExtractionError::SummaryTooLong { kind, index, limit });
    }
    if ["曾经", "之前", "很久以前"]
        .iter()
        .any(|phrase| summary.contains(phrase))
    {
        return Err(SemanticExtractionError::ForbiddenRelativeTime { kind, index });
    }
    Ok(())
}

fn validate_source_range(
    positions: &HashMap<Uuid, usize>,
    kind: &'static str,
    index: usize,
    start: Uuid,
    end: Uuid,
) -> Result<(), SemanticExtractionError> {
    let start_position =
        positions
            .get(&start)
            .copied()
            .ok_or(SemanticExtractionError::SourceOutsideBatch {
                kind,
                index,
                message_id: start,
            })?;
    let end_position =
        positions
            .get(&end)
            .copied()
            .ok_or(SemanticExtractionError::SourceOutsideBatch {
                kind,
                index,
                message_id: end,
            })?;
    if start_position > end_position {
        return Err(SemanticExtractionError::ReversedSourceRange { kind, index });
    }
    Ok(())
}

/// Scripted extractor for domain tests and future orchestration tests.
#[derive(Debug, Clone, Default)]
pub struct StubSemanticExtractor {
    result: SemanticExtractionResult,
}

impl StubSemanticExtractor {
    pub fn returning(result: SemanticExtractionResult) -> Self {
        Self { result }
    }
}

#[async_trait::async_trait]
impl SemanticExtractor for StubSemanticExtractor {
    async fn extract(
        &self,
        _input: SemanticExtractionInput<'_>,
    ) -> Result<SemanticExtractionResult, SemanticExtractionError> {
        Ok(self.result.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{DateTime, Utc};
    use eros_engine_store::chat::ChatMessage;

    fn message(session_id: Uuid, index: u128) -> ChatMessage {
        ChatMessage {
            id: Uuid::from_u128(index + 1),
            session_id,
            role: if index % 2 == 0 { "user" } else { "assistant" }.into(),
            content: format!("message {index}"),
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
    fn batch() -> WindowExitBatch {
        let session_id = Uuid::new_v4();
        let messages: Vec<_> = (0..5).map(|index| message(session_id, index)).collect();
        WindowExitBatch {
            user_id: Uuid::new_v4(),
            instance_id: Uuid::new_v4(),
            session_id,
            start_message_id: messages[0].id,
            end_message_id: messages[4].id,
            messages,
            created_at: Utc::now(),
        }
    }
    fn input(batch: &WindowExitBatch) -> SemanticExtractionInput<'_> {
        SemanticExtractionInput {
            batch,
            previous_semantic_summary: None,
            current_active_states: &[],
            conversation_mode: ConversationMode::Chat,
        }
    }
    fn event(
        batch: &WindowExitBatch,
        importance: EventImportance,
        summary: &str,
    ) -> EventCandidate {
        EventCandidate {
            summary: summary.into(),
            proposed_importance: importance,
            participants: vec!["Bai Zhi".into(), "Pei Jin".into()],
            location: None,
            source_start_message_id: batch.messages[0].id,
            source_end_message_id: batch.messages[1].id,
            proposed_relationship_relevant: false,
            proposed_relationship_direction: RelationshipDirection::Uncertain,
            confidence: CandidateConfidence::High,
            needs_review: false,
            review_reasons: vec![],
        }
    }
    fn episode(batch: &WindowExitBatch) -> RecentEpisodeCandidate {
        RecentEpisodeCandidate {
            summary: "They made a cat reaction-image joke.".into(),
            participants: vec!["Bai Zhi".into()],
            location: None,
            source_start_message_id: batch.messages[2].id,
            source_end_message_id: batch.messages[3].id,
            proposed_importance: EventImportance::Light,
            proposed_relationship_relevant: false,
            confidence: CandidateConfidence::High,
            needs_review: false,
            review_reasons: vec![],
        }
    }
    fn state(batch: &WindowExitBatch, operation: ActiveStateOperation) -> ActiveStateCandidate {
        ActiveStateCandidate {
            state_key: "treatment".into(),
            proposed_value: serde_json::json!("in treatment"),
            operation,
            confidence: CandidateConfidence::High,
            needs_review: false,
            review_reasons: vec![],
            source_start_message_id: batch.messages[0].id,
            source_end_message_id: batch.messages[1].id,
        }
    }
    async fn extract(
        batch: &WindowExitBatch,
        result: SemanticExtractionResult,
    ) -> Result<SemanticExtractionResult, SemanticExtractionError> {
        extract_and_validate(&StubSemanticExtractor::returning(result), input(batch)).await
    }

    #[tokio::test]
    async fn ordinary_chatter_has_no_events() {
        let batch = batch();
        assert_eq!(
            extract(&batch, SemanticExtractionResult::default())
                .await
                .unwrap(),
            SemanticExtractionResult::default()
        );
    }
    #[tokio::test]
    async fn normal_and_major_events_are_supported() {
        let batch = batch();
        let normal = extract(
            &batch,
            SemanticExtractionResult {
                candidate_events: vec![event(
                    &batch,
                    EventImportance::Normal,
                    "They agreed to a checkup.",
                )],
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let major = extract(
            &batch,
            SemanticExtractionResult {
                candidate_events: vec![event(
                    &batch,
                    EventImportance::Major,
                    "They formally ended their relationship.",
                )],
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(
            normal.candidate_events[0].proposed_importance,
            EventImportance::Normal
        );
        assert_eq!(
            major.candidate_events[0].proposed_importance,
            EventImportance::Major
        );
        assert!(!normal.candidate_events[0].needs_review);
        assert!(major.candidate_events[0].needs_review);
    }
    #[tokio::test]
    async fn light_and_normal_low_risk_events_do_not_require_review() {
        let batch = batch();
        for importance in [EventImportance::Light, EventImportance::Normal] {
            let result = extract(
                &batch,
                SemanticExtractionResult {
                    candidate_events: vec![event(&batch, importance, "A low-risk fact.")],
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            assert!(!result.candidate_events[0].needs_review);
            assert!(result.candidate_events[0].review_reasons.is_empty());
        }
    }
    #[tokio::test]
    async fn important_and_major_events_require_review() {
        let batch = batch();
        for importance in [EventImportance::Important, EventImportance::Major] {
            let result = extract(
                &batch,
                SemanticExtractionResult {
                    candidate_events: vec![event(&batch, importance, "A high-impact fact.")],
                    ..Default::default()
                },
            )
            .await
            .unwrap();
            assert!(result.candidate_events[0].needs_review);
            assert!(result.candidate_events[0]
                .review_reasons
                .iter()
                .any(|reason| reason == "high impact proposal"));
        }
    }
    #[tokio::test]
    async fn low_confidence_and_non_light_relationship_events_require_review() {
        let batch = batch();
        let mut low = event(&batch, EventImportance::Light, "An uncertain fact.");
        low.confidence = CandidateConfidence::Low;
        let mut relationship = event(
            &batch,
            EventImportance::Normal,
            "A relationship-relevant fact.",
        );
        relationship.proposed_relationship_relevant = true;
        relationship.proposed_relationship_direction = RelationshipDirection::Positive;
        let result = extract(
            &batch,
            SemanticExtractionResult {
                candidate_events: vec![low, relationship],
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(result
            .candidate_events
            .iter()
            .all(|event| event.needs_review));
        assert!(result.candidate_events[0]
            .review_reasons
            .iter()
            .any(|reason| reason == "low confidence"));
        assert!(result.candidate_events[1]
            .review_reasons
            .iter()
            .any(|reason| reason == "relationship-relevant proposal"));
    }
    #[tokio::test]
    async fn model_requested_review_is_preserved() {
        let batch = batch();
        let mut candidate = event(
            &batch,
            EventImportance::Light,
            "A possibly unsupported fact.",
        );
        candidate.needs_review = true;
        candidate.review_reasons = vec!["possible unsupported fact".into()];
        let result = extract(
            &batch,
            SemanticExtractionResult {
                candidate_events: vec![candidate],
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(result.candidate_events[0].needs_review);
        assert_eq!(
            result.candidate_events[0].review_reasons,
            ["possible unsupported fact"]
        );
    }
    #[tokio::test]
    async fn multiple_events_and_one_recent_episode_are_supported() {
        let batch = batch();
        let result = extract(
            &batch,
            SemanticExtractionResult {
                candidate_events: vec![
                    event(&batch, EventImportance::Normal, "They agreed to a checkup."),
                    event(
                        &batch,
                        EventImportance::Important,
                        "They learned treatment requires hospitalization.",
                    ),
                ],
                candidate_recent_episode: Some(episode(&batch)),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(result.candidate_events.len(), 2);
        assert!(result.candidate_recent_episode.is_some());
    }
    #[tokio::test]
    async fn all_active_state_operations_are_supported() {
        let batch = batch();
        let changes = [
            ActiveStateOperation::Add,
            ActiveStateOperation::Update,
            ActiveStateOperation::Remove,
        ]
        .into_iter()
        .map(|operation| state(&batch, operation))
        .collect();
        let result = extract(
            &batch,
            SemanticExtractionResult {
                candidate_active_state_changes: changes,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert_eq!(result.candidate_active_state_changes.len(), 3);
        assert!(result
            .candidate_active_state_changes
            .iter()
            .all(|candidate| candidate.needs_review));
    }
    #[tokio::test]
    async fn important_active_state_remove_requires_review() {
        let batch = batch();
        let result = extract(
            &batch,
            SemanticExtractionResult {
                candidate_active_state_changes: vec![state(&batch, ActiveStateOperation::Remove)],
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(result.candidate_active_state_changes[0].needs_review);
    }
    #[tokio::test]
    async fn conflicting_candidates_require_review() {
        let batch = batch();
        let first = event(&batch, EventImportance::Light, "One interpretation.");
        let second = event(&batch, EventImportance::Normal, "Another interpretation.");
        let result = extract(
            &batch,
            SemanticExtractionResult {
                candidate_events: vec![first, second],
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(result
            .candidate_events
            .iter()
            .all(|event| event.needs_review));
        assert!(result.candidate_events.iter().all(|event| event
            .review_reasons
            .iter()
            .any(|reason| reason == "conflicting candidates in batch")));
    }
    #[tokio::test]
    async fn rolling_summary_and_light_episode_do_not_require_review() {
        let batch = batch();
        let result = extract(
            &batch,
            SemanticExtractionResult {
                candidate_recent_episode: Some(episode(&batch)),
                next_semantic_summary: Some("They are still discussing the checkup.".into()),
                ..Default::default()
            },
        )
        .await
        .unwrap();
        assert!(!result.candidate_recent_episode.unwrap().needs_review);
        assert_eq!(
            result.next_semantic_summary.as_deref(),
            Some("They are still discussing the checkup.")
        );
    }
    #[tokio::test]
    async fn source_range_outside_batch_is_rejected() {
        let batch = batch();
        let mut candidate = event(&batch, EventImportance::Normal, "They agreed to a checkup.");
        candidate.source_end_message_id = Uuid::new_v4();
        assert!(matches!(
            extract(
                &batch,
                SemanticExtractionResult {
                    candidate_events: vec![candidate],
                    ..Default::default()
                }
            )
            .await
            .unwrap_err(),
            SemanticExtractionError::SourceOutsideBatch { .. }
        ));
    }
    #[tokio::test]
    async fn empty_or_overlong_summaries_are_rejected() {
        let batch = batch();
        for summary in [
            "  ".to_string(),
            "x".repeat(MAX_CANDIDATE_SUMMARY_CHARS + 1),
        ] {
            assert!(matches!(
                extract(
                    &batch,
                    SemanticExtractionResult {
                        candidate_events: vec![event(&batch, EventImportance::Light, &summary)],
                        ..Default::default()
                    }
                )
                .await
                .unwrap_err(),
                SemanticExtractionError::EmptySummary { .. }
                    | SemanticExtractionError::SummaryTooLong { .. }
            ));
        }
    }
    #[test]
    fn output_enums_reject_unknown_values() {
        assert!(serde_json::from_str::<EventImportance>("\"critical\"").is_err());
        assert!(serde_json::from_str::<RelationshipDirection>("\"up\"").is_err());
        assert!(serde_json::from_str::<ActiveStateOperation>("\"replace\"").is_err());
        assert!(serde_json::from_str::<CandidateConfidence>("\"certain\"").is_err());
    }
}
