//! Low-frequency review of semantic-extraction candidates.
//!
//! This module is deliberately separate from extraction and generation. It
//! only reviews candidates already marked for review; failures leave them
//! pending and never turn a proposal into a confirmed fact.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use eros_engine_llm::model_config::ModelConfig;
use eros_engine_llm::openrouter::{ChatMessage, ChatRequest, OpenRouterClient};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::semantic_extractor::{
    ActiveStateCandidate, ActiveStateOperation, CandidateConfidence, CurrentActiveState,
    EventCandidate, EventImportance, RecentEpisodeCandidate, RelationshipDirection,
    SemanticExtractionResult,
};

pub const SEMANTIC_REVIEW_TASK: &str = "semantic_review";
const REVIEW_TIMEOUT: Duration = Duration::from_secs(30);

const REVIEW_SYSTEM_PROMPT: &str = r#"你是低频语义候选复核器。只复核输入中的一个候选及其必要原文证据。不要重新分析整个对话，不要创建候选之外的新事件，不要补写原文没有的人物、地点或事实，不要修改人格、关系数值、Affinity 或长期状态。
事件只返回 keep、final_summary、final_importance、final_relationship_relevant、final_relationship_direction、final_participants、final_location、confidence。
active state 只返回 keep、final_operation、state_key、final_value、confidence。
recent episode 只返回 keep、final_summary、final_importance、final_relationship_relevant、confidence。
keep=false 或证据不足时必须降低 confidence。只输出严格 JSON，不要 Markdown 或解释。"#;

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value")]
pub enum ReviewCandidate {
    Event(EventCandidate),
    RecentEpisode(RecentEpisodeCandidate),
    ActiveState(ActiveStateCandidate),
}

impl ReviewCandidate {
    pub fn needs_review(&self) -> bool {
        match self {
            Self::Event(candidate) => candidate.needs_review,
            Self::RecentEpisode(candidate) => candidate.needs_review,
            Self::ActiveState(candidate) => candidate.needs_review,
        }
    }

    pub fn source_range(&self) -> (Uuid, Uuid) {
        match self {
            Self::Event(candidate) => (
                candidate.source_start_message_id,
                candidate.source_end_message_id,
            ),
            Self::RecentEpisode(candidate) => (
                candidate.source_start_message_id,
                candidate.source_end_message_id,
            ),
            Self::ActiveState(candidate) => (
                candidate.source_start_message_id,
                candidate.source_end_message_id,
            ),
        }
    }

    fn review_reasons(&self) -> &[String] {
        match self {
            Self::Event(candidate) => &candidate.review_reasons,
            Self::RecentEpisode(candidate) => &candidate.review_reasons,
            Self::ActiveState(candidate) => &candidate.review_reasons,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReviewRequest {
    pub candidate: ReviewCandidate,
    pub evidence: Vec<ReviewEvidence>,
    pub current_active_state: Option<CurrentActiveState>,
    pub previous_semantic_summary: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ReviewEvidence {
    pub message_id: Uuid,
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReviewQueues {
    pub no_review_candidates: Vec<ReviewCandidate>,
    pub review_candidates: Vec<ReviewCandidate>,
}

pub fn split_review_candidates(result: &SemanticExtractionResult) -> ReviewQueues {
    let mut queues = ReviewQueues {
        no_review_candidates: Vec::new(),
        review_candidates: Vec::new(),
    };
    for candidate in result
        .candidate_events
        .iter()
        .cloned()
        .map(ReviewCandidate::Event)
        .chain(
            result
                .candidate_recent_episode
                .iter()
                .cloned()
                .map(ReviewCandidate::RecentEpisode),
        )
        .chain(
            result
                .candidate_active_state_changes
                .iter()
                .cloned()
                .map(ReviewCandidate::ActiveState),
        )
    {
        if candidate.needs_review() {
            queues.review_candidates.push(candidate);
        } else {
            queues.no_review_candidates.push(candidate);
        }
    }
    queues
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FinalSemanticResult {
    pub confirmed_events: Vec<ConfirmedEvent>,
    pub confirmed_recent_episode: Option<ConfirmedRecentEpisode>,
    pub confirmed_active_state_changes: Vec<ConfirmedActiveStateChange>,
    pub next_semantic_summary: Option<String>,
}

impl FinalSemanticResult {
    pub fn from_local(result: &SemanticExtractionResult) -> Self {
        let final_result = Self {
            confirmed_events: result
                .candidate_events
                .iter()
                .filter(|candidate| !candidate.needs_review)
                .map(|candidate| ConfirmedEvent {
                    summary: candidate.summary.clone(),
                    importance: candidate.proposed_importance,
                    participants: candidate.participants.clone(),
                    location: candidate.location.clone(),
                    source_start_message_id: candidate.source_start_message_id,
                    source_end_message_id: candidate.source_end_message_id,
                    relationship_relevant: candidate.proposed_relationship_relevant,
                    relationship_direction: candidate.proposed_relationship_direction,
                    confidence: candidate.confidence,
                })
                .collect(),
            confirmed_recent_episode: result
                .candidate_recent_episode
                .as_ref()
                .filter(|candidate| !candidate.needs_review)
                .map(|candidate| ConfirmedRecentEpisode {
                    summary: candidate.summary.clone(),
                    importance: candidate.proposed_importance,
                    relationship_relevant: candidate.proposed_relationship_relevant,
                    confidence: candidate.confidence,
                    source_start_message_id: candidate.source_start_message_id,
                    source_end_message_id: candidate.source_end_message_id,
                }),
            confirmed_active_state_changes: result
                .candidate_active_state_changes
                .iter()
                .filter(|candidate| !candidate.needs_review)
                .map(|candidate| ConfirmedActiveStateChange {
                    operation: candidate.operation,
                    state_key: candidate.state_key.clone(),
                    value: candidate.proposed_value.clone(),
                    confidence: candidate.confidence,
                    source_start_message_id: candidate.source_start_message_id,
                    source_end_message_id: candidate.source_end_message_id,
                })
                .collect(),
            next_semantic_summary: result.next_semantic_summary.clone(),
        };
        final_result
    }

    /// Applies only a successful review outcome. Pending/failed outcomes have
    /// no path into this final structure.
    pub fn accept(&mut self, outcome: &ReviewOutcome) {
        if outcome.status != ReviewStatus::Confirmed {
            return;
        }
        match outcome.reviewed.as_ref() {
            Some(ReviewedCandidate::Event(output)) if output.keep => {
                self.confirmed_events.push(ConfirmedEvent {
                    summary: output.final_summary.clone(),
                    importance: output.final_importance,
                    participants: output.final_participants.clone(),
                    location: output.final_location.clone(),
                    source_start_message_id: outcome.audit.candidate_source_start_message_id,
                    source_end_message_id: outcome.audit.candidate_source_end_message_id,
                    relationship_relevant: output.final_relationship_relevant,
                    relationship_direction: output.final_relationship_direction,
                    confidence: output.confidence,
                });
            }
            Some(ReviewedCandidate::RecentEpisode(output)) if output.keep => {
                self.confirmed_recent_episode = Some(ConfirmedRecentEpisode {
                    summary: output.final_summary.clone(),
                    importance: output.final_importance,
                    relationship_relevant: output.final_relationship_relevant,
                    confidence: output.confidence,
                    source_start_message_id: outcome.audit.candidate_source_start_message_id,
                    source_end_message_id: outcome.audit.candidate_source_end_message_id,
                });
            }
            Some(ReviewedCandidate::ActiveState(output)) if output.keep => {
                self.confirmed_active_state_changes
                    .push(ConfirmedActiveStateChange {
                        operation: output.final_operation,
                        state_key: output.state_key.clone(),
                        value: output.final_value.clone(),
                        confidence: output.confidence,
                        source_start_message_id: outcome.audit.candidate_source_start_message_id,
                        source_end_message_id: outcome.audit.candidate_source_end_message_id,
                    });
            }
            _ => {}
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ConfirmedEvent {
    pub summary: String,
    pub importance: EventImportance,
    pub participants: Vec<String>,
    pub location: Option<String>,
    pub source_start_message_id: Uuid,
    pub source_end_message_id: Uuid,
    pub relationship_relevant: bool,
    pub relationship_direction: RelationshipDirection,
    pub confidence: CandidateConfidence,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ConfirmedRecentEpisode {
    pub summary: String,
    pub importance: EventImportance,
    pub relationship_relevant: bool,
    pub confidence: CandidateConfidence,
    pub source_start_message_id: Uuid,
    pub source_end_message_id: Uuid,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ConfirmedActiveStateChange {
    pub operation: ActiveStateOperation,
    pub state_key: String,
    pub value: serde_json::Value,
    pub confidence: CandidateConfidence,
    pub source_start_message_id: Uuid,
    pub source_end_message_id: Uuid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewStatus {
    Confirmed,
    Rejected,
    Pending,
    Failed,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReviewAudit {
    pub candidate_source_start_message_id: Uuid,
    pub candidate_source_end_message_id: Uuid,
    pub original_candidate: ReviewCandidate,
    pub reviewed_result: Option<ReviewedCandidate>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub review_reason: String,
    pub reviewed_at: DateTime<Utc>,
    pub review_status: ReviewStatus,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ReviewedCandidate {
    Event(EventReviewOutput),
    RecentEpisode(RecentEpisodeReviewOutput),
    ActiveState(ActiveStateReviewOutput),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EventReviewOutput {
    pub keep: bool,
    pub final_summary: String,
    pub final_importance: EventImportance,
    pub final_relationship_relevant: bool,
    pub final_relationship_direction: RelationshipDirection,
    pub final_participants: Vec<String>,
    pub final_location: Option<String>,
    pub confidence: CandidateConfidence,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecentEpisodeReviewOutput {
    pub keep: bool,
    pub final_summary: String,
    pub final_importance: EventImportance,
    pub final_relationship_relevant: bool,
    pub confidence: CandidateConfidence,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActiveStateReviewOutput {
    pub keep: bool,
    pub final_operation: ActiveStateOperation,
    pub state_key: String,
    pub final_value: serde_json::Value,
    pub confidence: CandidateConfidence,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum SemanticReviewError {
    #[error("[tasks.semantic_review] is not configured")]
    NotConfigured,
    #[error("semantic review candidate does not require review")]
    CandidateDoesNotRequireReview,
    #[error("semantic review input has no source evidence")]
    MissingEvidence,
    #[error("semantic review provider failed: {0}")]
    Provider(String),
    #[error("semantic review provider timed out after {0} seconds")]
    Timeout(u64),
    #[error("semantic review returned invalid JSON: {0}")]
    InvalidJson(String),
    #[error("semantic review validation failed: {0}")]
    Validation(String),
}

pub struct LlmSemanticReviewer {
    client: Arc<OpenRouterClient>,
    model_config: Arc<ModelConfig>,
    timeout: Duration,
}

impl LlmSemanticReviewer {
    pub fn new(client: Arc<OpenRouterClient>, model_config: Arc<ModelConfig>) -> Self {
        Self {
            client,
            model_config,
            timeout: REVIEW_TIMEOUT,
        }
    }

    #[cfg(test)]
    fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    fn request(
        &self,
        input: &ReviewRequest,
    ) -> Result<(ChatRequest, CandidateKind), SemanticReviewError> {
        if !input.candidate.needs_review() {
            return Err(SemanticReviewError::CandidateDoesNotRequireReview);
        }
        if input.evidence.is_empty() {
            return Err(SemanticReviewError::MissingEvidence);
        }
        if !self.model_config.tasks.contains_key(SEMANTIC_REVIEW_TASK) {
            return Err(SemanticReviewError::NotConfigured);
        }
        let kind = CandidateKind::of(&input.candidate);
        let payload = serde_json::to_string(&ReviewPayload::new(input, kind))
            .map_err(|error| SemanticReviewError::InvalidJson(error.to_string()))?;
        let resolved = self.model_config.resolve(SEMANTIC_REVIEW_TASK, None);
        Ok((
            ChatRequest {
                model: resolved.model,
                fallback_model: resolved.fallback_model,
                messages: vec![
                    ChatMessage {
                        role: "system".into(),
                        content: REVIEW_SYSTEM_PROMPT.into(),
                    },
                    ChatMessage {
                        role: "user".into(),
                        content: payload,
                    },
                ],
                temperature: resolved.temperature as f32,
                sampling: resolved.sampling,
                max_tokens: resolved.max_tokens,
                reasoning: resolved.reasoning,
                response_format: Some(review_response_format(kind)),
                task: Some(SEMANTIC_REVIEW_TASK.into()),
                ..Default::default()
            },
            kind,
        ))
    }

    pub async fn review(&self, input: ReviewRequest) -> Result<ReviewOutcome, SemanticReviewError> {
        let (request, kind) = self.request(&input)?;
        let resolved = self.model_config.resolve(SEMANTIC_REVIEW_TASK, None);
        let configured_model = resolved.model.clone();
        let provider = configured_model
            .split_once('@')
            .map(|(_, provider)| provider.to_string())
            .or_else(|| Some("openrouter".into()));
        let response = tokio::time::timeout(self.timeout, self.client.execute(request))
            .await
            .map_err(|_| SemanticReviewError::Timeout(self.timeout.as_secs()))?
            .map_err(|error| SemanticReviewError::Provider(error.to_string()))?;
        let served_model = response.model.clone().unwrap_or(configured_model);
        let reviewed = parse_review_output(response.reply.trim(), kind)?;
        validate_review_output(&input.candidate, &input.evidence, &reviewed)?;
        let status = if reviewed.keep() {
            ReviewStatus::Confirmed
        } else {
            ReviewStatus::Rejected
        };
        let review_reason = if input.candidate.review_reasons().is_empty() {
            "candidate requested review".into()
        } else {
            input.candidate.review_reasons().join("; ")
        };
        Ok(ReviewOutcome {
            reviewed: Some(reviewed.clone()),
            status,
            audit: ReviewAudit {
                candidate_source_start_message_id: input.candidate.source_range().0,
                candidate_source_end_message_id: input.candidate.source_range().1,
                original_candidate: input.candidate,
                reviewed_result: Some(reviewed.clone()),
                provider,
                model: Some(served_model),
                review_reason,
                reviewed_at: Utc::now(),
                review_status: status,
            },
        })
    }

    /// Background-safe entry point. Any transport, parse, or validation error
    /// leaves the original candidate pending for a later retry.
    pub async fn review_or_pending(&self, input: ReviewRequest) -> ReviewOutcome {
        let candidate = input.candidate.clone();
        match self.review(input).await {
            Ok(outcome) => outcome,
            Err(error) => self.failure_audit(candidate, &error),
        }
    }

    pub fn failure_audit(
        &self,
        candidate: ReviewCandidate,
        error: &SemanticReviewError,
    ) -> ReviewOutcome {
        let (start, end) = candidate.source_range();
        let configured_model = self
            .model_config
            .tasks
            .contains_key(SEMANTIC_REVIEW_TASK)
            .then(|| self.model_config.resolve(SEMANTIC_REVIEW_TASK, None).model);
        let provider = configured_model.as_deref().map(|model| {
            model
                .split_once('@')
                .map(|(_, provider)| provider.to_string())
                .unwrap_or_else(|| "openrouter".into())
        });
        ReviewOutcome {
            reviewed: None,
            status: ReviewStatus::Pending,
            audit: ReviewAudit {
                candidate_source_start_message_id: start,
                candidate_source_end_message_id: end,
                original_candidate: candidate,
                reviewed_result: None,
                provider,
                model: configured_model,
                review_reason: error.to_string(),
                reviewed_at: Utc::now(),
                review_status: ReviewStatus::Failed,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReviewOutcome {
    pub reviewed: Option<ReviewedCandidate>,
    pub status: ReviewStatus,
    pub audit: ReviewAudit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CandidateKind {
    Event,
    RecentEpisode,
    ActiveState,
}

impl CandidateKind {
    fn of(candidate: &ReviewCandidate) -> Self {
        match candidate {
            ReviewCandidate::Event(_) => Self::Event,
            ReviewCandidate::RecentEpisode(_) => Self::RecentEpisode,
            ReviewCandidate::ActiveState(_) => Self::ActiveState,
        }
    }
}

#[derive(Serialize)]
struct ReviewPayload<'a> {
    candidate_type: &'static str,
    candidate: &'a ReviewCandidate,
    evidence: &'a [ReviewEvidence],
    current_active_state: &'a Option<CurrentActiveState>,
    previous_semantic_summary: &'a Option<String>,
}

impl<'a> ReviewPayload<'a> {
    fn new(input: &'a ReviewRequest, kind: CandidateKind) -> Self {
        Self {
            candidate_type: match kind {
                CandidateKind::Event => "event",
                CandidateKind::RecentEpisode => "recent_episode",
                CandidateKind::ActiveState => "active_state",
            },
            candidate: &input.candidate,
            evidence: &input.evidence,
            current_active_state: &input.current_active_state,
            previous_semantic_summary: &input.previous_semantic_summary,
        }
    }
}

fn parse_review_output(
    value: &str,
    kind: CandidateKind,
) -> Result<ReviewedCandidate, SemanticReviewError> {
    match kind {
        CandidateKind::Event => serde_json::from_str(value)
            .map(ReviewedCandidate::Event)
            .map_err(|error| SemanticReviewError::InvalidJson(error.to_string())),
        CandidateKind::RecentEpisode => serde_json::from_str(value)
            .map(ReviewedCandidate::RecentEpisode)
            .map_err(|error| SemanticReviewError::InvalidJson(error.to_string())),
        CandidateKind::ActiveState => serde_json::from_str(value)
            .map(ReviewedCandidate::ActiveState)
            .map_err(|error| SemanticReviewError::InvalidJson(error.to_string())),
    }
}

fn validate_review_output(
    candidate: &ReviewCandidate,
    evidence: &[ReviewEvidence],
    output: &ReviewedCandidate,
) -> Result<(), SemanticReviewError> {
    if evidence.is_empty() {
        return Err(SemanticReviewError::MissingEvidence);
    }
    match (candidate, output) {
        (ReviewCandidate::Event(_original), ReviewedCandidate::Event(reviewed)) => {
            if reviewed.final_summary.trim().is_empty() {
                return Err(SemanticReviewError::Validation(
                    "final_summary is empty".into(),
                ));
            }
            if reviewed
                .final_participants
                .iter()
                .any(|participant| participant.trim().is_empty())
            {
                return Err(SemanticReviewError::Validation("empty participant".into()));
            }
            if reviewed.final_summary.chars().count() > 280 {
                return Err(SemanticReviewError::Validation(
                    "final_summary is too long".into(),
                ));
            }
            if !reviewed.keep {
                return Ok(());
            }
            if !evidence_covers(candidate, evidence) {
                return Err(SemanticReviewError::Validation(
                    "source evidence does not cover candidate".into(),
                ));
            }
            Ok(())
        }
        (ReviewCandidate::RecentEpisode(_original), ReviewedCandidate::RecentEpisode(reviewed)) => {
            if reviewed.final_summary.trim().is_empty()
                || reviewed.final_summary.chars().count() > 280
            {
                return Err(SemanticReviewError::Validation(
                    "invalid final_summary".into(),
                ));
            }
            if !evidence_covers(candidate, evidence) {
                return Err(SemanticReviewError::Validation(
                    "source evidence does not cover candidate".into(),
                ));
            }
            Ok(())
        }
        (ReviewCandidate::ActiveState(original), ReviewedCandidate::ActiveState(reviewed)) => {
            if reviewed.state_key.trim().is_empty() {
                return Err(SemanticReviewError::Validation("state_key is empty".into()));
            }
            if reviewed.state_key != original.state_key {
                return Err(SemanticReviewError::Validation(
                    "state_key changed during review".into(),
                ));
            }
            if !evidence_covers(candidate, evidence) {
                return Err(SemanticReviewError::Validation(
                    "source evidence does not cover candidate".into(),
                ));
            }
            Ok(())
        }
        _ => Err(SemanticReviewError::Validation(
            "review output type does not match candidate".into(),
        )),
    }
}

fn evidence_covers(candidate: &ReviewCandidate, evidence: &[ReviewEvidence]) -> bool {
    let (start, end) = candidate.source_range();
    evidence.iter().any(|item| item.message_id == start)
        && evidence.iter().any(|item| item.message_id == end)
}

impl ReviewedCandidate {
    fn keep(&self) -> bool {
        match self {
            Self::Event(output) => output.keep,
            Self::RecentEpisode(output) => output.keep,
            Self::ActiveState(output) => output.keep,
        }
    }
}

fn review_response_format(kind: CandidateKind) -> serde_json::Value {
    let string_enum = |values: &[&str]| serde_json::json!({ "type": "string", "enum": values });
    let schema = match kind {
        CandidateKind::Event => serde_json::json!({
            "type":"object", "additionalProperties":false, "properties": {
                "keep":{"type":"boolean"}, "final_summary":{"type":"string"}, "final_importance":string_enum(&["light","normal","important","major"]),
                "final_relationship_relevant":{"type":"boolean"}, "final_relationship_direction":string_enum(&["positive","negative","mixed","uncertain"]), "final_participants":{"type":"array","items":{"type":"string"}}, "final_location":{"type":["string","null"]}, "confidence":string_enum(&["low","medium","high"])
            }, "required":["keep","final_summary","final_importance","final_relationship_relevant","final_relationship_direction","final_participants","final_location","confidence"]
        }),
        CandidateKind::RecentEpisode => serde_json::json!({
            "type":"object", "additionalProperties":false, "properties": {
                "keep":{"type":"boolean"}, "final_summary":{"type":"string"}, "final_importance":string_enum(&["light","normal","important","major"]), "final_relationship_relevant":{"type":"boolean"}, "confidence":string_enum(&["low","medium","high"])
            }, "required":["keep","final_summary","final_importance","final_relationship_relevant","confidence"]
        }),
        CandidateKind::ActiveState => serde_json::json!({
            "type":"object", "additionalProperties":false, "properties": {
                "keep":{"type":"boolean"}, "final_operation":string_enum(&["add","update","remove"]), "state_key":{"type":"string"}, "final_value":{}, "confidence":string_enum(&["low","medium","high"])
            }, "required":["keep","final_operation","state_key","final_value","confidence"]
        }),
    };
    serde_json::json!({ "type":"json_schema", "json_schema": { "name":"semantic_review", "strict":true, "schema":schema } })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::semantic_extractor::{CandidateConfidence, EventImportance};
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn ids() -> (Uuid, Uuid) {
        (Uuid::from_u128(1), Uuid::from_u128(2))
    }
    fn event(needs_review: bool) -> EventCandidate {
        let (start, end) = ids();
        EventCandidate {
            summary: "A proposal".into(),
            proposed_importance: EventImportance::Important,
            participants: vec!["A".into()],
            location: None,
            source_start_message_id: start,
            source_end_message_id: end,
            proposed_relationship_relevant: false,
            proposed_relationship_direction: RelationshipDirection::Uncertain,
            confidence: CandidateConfidence::High,
            needs_review,
            review_reasons: vec![],
        }
    }
    fn evidence() -> Vec<ReviewEvidence> {
        let (start, end) = ids();
        vec![
            ReviewEvidence {
                message_id: start,
                role: "user".into(),
                content: "A proposal".into(),
            },
            ReviewEvidence {
                message_id: end,
                role: "assistant".into(),
                content: "Confirmed".into(),
            },
        ]
    }
    fn request(candidate: ReviewCandidate) -> ReviewRequest {
        ReviewRequest {
            candidate,
            evidence: evidence(),
            current_active_state: None,
            previous_semantic_summary: Some("Current topic".into()),
        }
    }
    fn event_output(
        importance: &str,
        relationship_relevant: bool,
        keep: bool,
    ) -> serde_json::Value {
        json!({
            "keep": keep,
            "final_summary": "Reviewed proposal",
            "final_importance": importance,
            "final_relationship_relevant": relationship_relevant,
            "final_relationship_direction": if relationship_relevant { "positive" } else { "uncertain" },
            "final_participants": ["A"],
            "final_location": null,
            "confidence": "high"
        })
    }
    fn state_candidate() -> ActiveStateCandidate {
        let (start, end) = ids();
        ActiveStateCandidate {
            state_key: "treatment".into(),
            proposed_value: json!(null),
            operation: ActiveStateOperation::Remove,
            confidence: CandidateConfidence::High,
            needs_review: true,
            review_reasons: vec!["active-state change requires review".into()],
            source_start_message_id: start,
            source_end_message_id: end,
        }
    }
    fn episode_candidate() -> RecentEpisodeCandidate {
        let (start, end) = ids();
        RecentEpisodeCandidate {
            summary: "Shared joke".into(),
            participants: vec!["A".into()],
            location: None,
            source_start_message_id: start,
            source_end_message_id: end,
            proposed_importance: EventImportance::Important,
            proposed_relationship_relevant: true,
            confidence: CandidateConfidence::Medium,
            needs_review: true,
            review_reasons: vec!["high impact episode proposal".into()],
        }
    }
    fn model_config() -> Arc<ModelConfig> {
        Arc::new(
            ModelConfig::from_toml_str(
                "[tasks.semantic_review]\nmodel='mock/reviewer'\nfallback=[]\nretry_depth=0\ntemperature=0.0\nmax_tokens=500\n",
            )
            .unwrap(),
        )
    }
    fn provider_body(reply: &str) -> serde_json::Value {
        json!({ "id":"review-1", "model":"mock/reviewer", "choices":[{ "message":{ "content":reply }, "finish_reason":"stop" }] })
    }
    async fn reviewer_with(response: ResponseTemplate) -> (LlmSemanticReviewer, MockServer) {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(response)
            .mount(&server)
            .await;
        let client = Arc::new(OpenRouterClient::with_base_url(
            "test-key".into(),
            format!("{}/chat/completions", server.uri()),
        ));
        (LlmSemanticReviewer::new(client, model_config()), server)
    }
    async fn reviewer_success(output: serde_json::Value) -> (LlmSemanticReviewer, MockServer) {
        reviewer_with(ResponseTemplate::new(200).set_body_json(provider_body(&output.to_string())))
            .await
    }
    #[test]
    fn split_routes_only_review_candidates() {
        let mut reviewed = event(true);
        let plain = event(false);
        reviewed.review_reasons = vec!["high impact".into()];
        let queues = split_review_candidates(&SemanticExtractionResult {
            candidate_events: vec![reviewed, plain],
            ..Default::default()
        });
        assert_eq!(queues.review_candidates.len(), 1);
        assert_eq!(queues.no_review_candidates.len(), 1);
    }
    #[test]
    fn final_result_keeps_local_summary_only() {
        let result = SemanticExtractionResult {
            candidate_events: vec![event(false)],
            next_semantic_summary: Some("Current topic".into()),
            ..Default::default()
        };
        let final_result = FinalSemanticResult::from_local(&result);
        assert_eq!(
            final_result.next_semantic_summary.as_deref(),
            Some("Current topic")
        );
        assert_eq!(final_result.confirmed_events.len(), 1);
    }
    #[test]
    fn invalid_review_output_is_rejected() {
        let original = ReviewCandidate::Event(event(true));
        let output = ReviewedCandidate::Event(EventReviewOutput {
            keep: true,
            final_summary: "".into(),
            final_importance: EventImportance::Normal,
            final_relationship_relevant: false,
            final_relationship_direction: RelationshipDirection::Uncertain,
            final_participants: vec![],
            final_location: None,
            confidence: CandidateConfidence::High,
        });
        assert!(validate_review_output(&original, &evidence(), &output).is_err());
    }
    #[test]
    fn active_state_review_keeps_key_stable() {
        let (start, end) = ids();
        let original = ReviewCandidate::ActiveState(ActiveStateCandidate {
            state_key: "treatment".into(),
            proposed_value: json!(true),
            operation: ActiveStateOperation::Remove,
            confidence: CandidateConfidence::High,
            needs_review: true,
            review_reasons: vec![],
            source_start_message_id: start,
            source_end_message_id: end,
        });
        let output = ReviewedCandidate::ActiveState(ActiveStateReviewOutput {
            keep: true,
            final_operation: ActiveStateOperation::Remove,
            state_key: "other".into(),
            final_value: json!(null),
            confidence: CandidateConfidence::High,
        });
        assert!(validate_review_output(&original, &evidence(), &output).is_err());
    }
    #[tokio::test]
    async fn important_candidate_is_confirmed() {
        let (reviewer, _) = reviewer_success(event_output("important", false, true)).await;
        let outcome = reviewer
            .review(request(ReviewCandidate::Event(event(true))))
            .await
            .unwrap();
        assert_eq!(outcome.status, ReviewStatus::Confirmed);
        assert_eq!(outcome.audit.provider.as_deref(), Some("openrouter"));
        assert_eq!(outcome.audit.model.as_deref(), Some("mock/reviewer"));
    }
    #[tokio::test]
    async fn major_can_be_downgraded_to_important() {
        let mut candidate = event(true);
        candidate.proposed_importance = EventImportance::Major;
        let (reviewer, _) = reviewer_success(event_output("important", true, true)).await;
        let outcome = reviewer
            .review(request(ReviewCandidate::Event(candidate)))
            .await
            .unwrap();
        let Some(ReviewedCandidate::Event(output)) = outcome.reviewed else {
            panic!("expected event review")
        };
        assert_eq!(output.final_importance, EventImportance::Important);
    }
    #[tokio::test]
    async fn upgraded_normal_can_be_corrected() {
        let (reviewer, _) = reviewer_success(event_output("normal", false, true)).await;
        let outcome = reviewer
            .review(request(ReviewCandidate::Event(event(true))))
            .await
            .unwrap();
        let Some(ReviewedCandidate::Event(output)) = outcome.reviewed else {
            panic!("expected event review")
        };
        assert_eq!(output.final_importance, EventImportance::Normal);
    }
    #[tokio::test]
    async fn relationship_flag_can_be_corrected_both_directions() {
        for expected in [false, true] {
            let mut candidate = event(true);
            candidate.proposed_relationship_relevant = !expected;
            let (reviewer, _) = reviewer_success(event_output("normal", expected, true)).await;
            let outcome = reviewer
                .review(request(ReviewCandidate::Event(candidate)))
                .await
                .unwrap();
            let Some(ReviewedCandidate::Event(output)) = outcome.reviewed else {
                panic!("expected event review")
            };
            assert_eq!(output.final_relationship_relevant, expected);
        }
    }
    #[tokio::test]
    async fn active_state_remove_can_be_confirmed_or_rejected() {
        for keep in [true, false] {
            let output = json!({ "keep":keep, "final_operation":"remove", "state_key":"treatment", "final_value":null, "confidence":"high" });
            let (reviewer, _) = reviewer_success(output).await;
            let outcome = reviewer
                .review(request(ReviewCandidate::ActiveState(state_candidate())))
                .await
                .unwrap();
            assert_eq!(
                outcome.status,
                if keep {
                    ReviewStatus::Confirmed
                } else {
                    ReviewStatus::Rejected
                }
            );
        }
    }
    #[tokio::test]
    async fn recent_episode_can_be_reviewed() {
        let output = json!({ "keep":true, "final_summary":"Shared joke", "final_importance":"light", "final_relationship_relevant":false, "confidence":"high" });
        let (reviewer, _) = reviewer_success(output).await;
        let outcome = reviewer
            .review(request(ReviewCandidate::RecentEpisode(episode_candidate())))
            .await
            .unwrap();
        assert_eq!(outcome.status, ReviewStatus::Confirmed);
    }
    #[tokio::test]
    async fn keep_false_never_enters_final_result() {
        let (reviewer, _) = reviewer_success(event_output("light", false, false)).await;
        let outcome = reviewer
            .review(request(ReviewCandidate::Event(event(true))))
            .await
            .unwrap();
        let mut final_result =
            FinalSemanticResult::from_local(&SemanticExtractionResult::default());
        final_result.accept(&outcome);
        assert!(final_result.confirmed_events.is_empty());
    }
    #[tokio::test]
    async fn invalid_json_remains_pending() {
        let (reviewer, _) =
            reviewer_with(ResponseTemplate::new(200).set_body_json(provider_body("not-json")))
                .await;
        let outcome = reviewer
            .review_or_pending(request(ReviewCandidate::Event(event(true))))
            .await;
        assert_eq!(outcome.status, ReviewStatus::Pending);
        assert_eq!(outcome.audit.review_status, ReviewStatus::Failed);
        assert!(outcome.reviewed.is_none());
    }
    #[tokio::test]
    async fn timeout_remains_pending() {
        let (reviewer, _) = reviewer_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(100))
                .set_body_json(provider_body(
                    &event_output("normal", false, true).to_string(),
                )),
        )
        .await;
        let outcome = reviewer
            .with_timeout(Duration::from_millis(1))
            .review_or_pending(request(ReviewCandidate::Event(event(true))))
            .await;
        assert_eq!(outcome.status, ReviewStatus::Pending);
    }
    #[tokio::test]
    async fn provider_error_remains_pending() {
        let (reviewer, _) =
            reviewer_with(ResponseTemplate::new(503).set_body_string("unavailable")).await;
        let outcome = reviewer
            .review_or_pending(request(ReviewCandidate::Event(event(true))))
            .await;
        assert_eq!(outcome.status, ReviewStatus::Pending);
        assert!(outcome.audit.review_reason.contains("provider failed"));
    }
    #[tokio::test]
    async fn validation_failure_remains_pending() {
        let mut output = event_output("normal", false, true);
        output["final_summary"] = json!("");
        let (reviewer, _) = reviewer_success(output).await;
        let outcome = reviewer
            .review_or_pending(request(ReviewCandidate::Event(event(true))))
            .await;
        assert_eq!(outcome.status, ReviewStatus::Pending);
        assert!(outcome.audit.review_reason.contains("validation failed"));
    }
    #[tokio::test]
    async fn no_review_candidate_does_not_call_provider() {
        let (reviewer, server) = reviewer_success(event_output("normal", false, true)).await;
        let error = reviewer
            .review(request(ReviewCandidate::Event(event(false))))
            .await
            .unwrap_err();
        assert_eq!(error, SemanticReviewError::CandidateDoesNotRequireReview);
        assert!(server.received_requests().await.unwrap().is_empty());
    }
    #[tokio::test]
    async fn review_request_contains_only_candidate_evidence_and_short_context() {
        let (reviewer, server) = reviewer_success(event_output("normal", false, true)).await;
        reviewer
            .review(request(ReviewCandidate::Event(event(true))))
            .await
            .unwrap();
        let requests = server.received_requests().await.unwrap();
        let wire: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        let payload: serde_json::Value =
            serde_json::from_str(wire["messages"][1]["content"].as_str().unwrap()).unwrap();
        assert_eq!(payload["evidence"].as_array().unwrap().len(), 2);
        assert!(payload.get("window_exit_batch").is_none());
        assert!(payload.get("persona").is_none());
        assert_eq!(payload["previous_semantic_summary"], "Current topic");
    }
}
