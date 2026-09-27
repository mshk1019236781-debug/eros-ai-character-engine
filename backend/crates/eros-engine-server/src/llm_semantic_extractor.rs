// SPDX-License-Identifier: AGPL-3.0-only
//! LLM-backed semantic extraction using the engine's shared model router.

use std::sync::Arc;
use std::time::Duration;

use eros_engine_llm::model_config::ModelConfig;
use eros_engine_llm::openrouter::{ChatMessage, ChatRequest, OpenRouterClient};
use serde::Serialize;

use crate::history_window::ConversationMode;
use crate::semantic_extractor::{
    mark_review_flags, validate_result, CurrentActiveState, SemanticExtractionError,
    SemanticExtractionInput, SemanticExtractionResult, SemanticExtractor,
};

pub const SEMANTIC_EXTRACTION_TASK: &str = "semantic_extraction";
const SEMANTIC_EXTRACTION_TIMEOUT: Duration = Duration::from_secs(20);

const SYSTEM_PROMPT: &str = r#"你是近期内容候选整理器。只分析输入 JSON 的 messages 原文，并且只用其中给出的 previous_semantic_summary、current_active_states 和 conversation_mode 作为上下文。不要猜测批次外事实。

只输出严格符合 JSON Schema 的 JSON，不要 Markdown、代码围栏或说明。

输出规则：
1. candidate_events 是 0 到 N 条候选事件，不是最终事实。仅在删除后可能造成后续剧情理解错误、产生决定/承诺/拒绝、重要信息得失、未来约束、场景或计划变化，或独特可复用互动时输出。proposed_importance 只能是 light、normal、important、major，表示影响范围，不是关系数值。
2. candidate_recent_episode 是 0 或 1 条候选小互动，不是最终事实。普通问候、哈哈、嗯、普通吃饭喝水和普通互怼都必须为 null。
3. candidate_active_state_changes 只提出可能仍影响后续的持续状态 add/update/remove，不执行它们。必须结合 current_active_states、previous_semantic_summary 和本批原文决定状态语义是否结束；程序只做结构校验，后续 review 再裁决。
4. next_semantic_summary 是覆盖旧摘要的 1 到 2 句短摘要，只保留当前主要持续话题。话题结束或完全转移时为 null；已经作为结束事件保留的内容不要堆积进它。
5. source message id 必须直接复制 messages 中覆盖事实的最小连续范围。不要生成时间或轮次。confidence 只能是 low、medium、high，按原文支持程度选择。若你发现不确定、字段冲突或疑似原文不支持的信息，将 needs_review 设为 true 并简短填写 review_reasons；程序还会追加确定性的强制 review 规则。
6. participants 只能使用输入 participants 提供的名字；没有明确参与者时为空数组。
7. chat 必须严格过滤短消息的寒暄、哈哈和普通互怼，不能因消息多制造 light 事件。narrative 应重视决定、结果、信息揭露、场景变化、人物加入/离开和持续状态变化，但不要保存纯文学动作或环境描写。

严格禁止：人格变化判断、关系数值或阶段判断、心理成长推断、Agency/Recall 决策、跨 batch 合并、pending/open thread、文学化改写、相对时间词（曾经、之前、很久以前）、编造原文不存在的事实、story_time、Prompt 或长期记忆建议。候选绝不能直接写入长期记忆、关系状态或 active state。"#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SemanticParticipantNames {
    pub user: String,
    pub character: String,
}

pub struct LlmSemanticExtractor {
    client: Arc<OpenRouterClient>,
    model_config: Arc<ModelConfig>,
    participants: SemanticParticipantNames,
    timeout: Duration,
}

impl LlmSemanticExtractor {
    pub fn new(
        client: Arc<OpenRouterClient>,
        model_config: Arc<ModelConfig>,
        participants: SemanticParticipantNames,
    ) -> Self {
        Self {
            client,
            model_config,
            participants,
            timeout: SEMANTIC_EXTRACTION_TIMEOUT,
        }
    }

    #[cfg(test)]
    fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    fn request(
        &self,
        input: SemanticExtractionInput<'_>,
    ) -> Result<ChatRequest, SemanticExtractionError> {
        if !self
            .model_config
            .tasks
            .contains_key(SEMANTIC_EXTRACTION_TASK)
        {
            return Err(SemanticExtractionError::Extractor(
                "[tasks.semantic_extraction] is not configured".into(),
            ));
        }
        let resolved = self.model_config.resolve(SEMANTIC_EXTRACTION_TASK, None);
        let payload = serde_json::to_string(&ExtractionInput::new(input, &self.participants))
            .map_err(|error| SemanticExtractionError::Extractor(error.to_string()))?;
        Ok(ChatRequest {
            model: resolved.model,
            fallback_model: resolved.fallback_model,
            messages: vec![
                ChatMessage {
                    role: "system".into(),
                    content: SYSTEM_PROMPT.into(),
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
            response_format: Some(response_format()),
            task: Some(SEMANTIC_EXTRACTION_TASK.into()),
            ..Default::default()
        })
    }
}

#[async_trait::async_trait]
impl SemanticExtractor for LlmSemanticExtractor {
    async fn extract(
        &self,
        input: SemanticExtractionInput<'_>,
    ) -> Result<SemanticExtractionResult, SemanticExtractionError> {
        let request = self.request(input)?;
        let response = tokio::time::timeout(self.timeout, self.client.execute(request))
            .await
            .map_err(|_| {
                SemanticExtractionError::Extractor(format!(
                    "provider timed out after {} seconds",
                    self.timeout.as_secs_f64()
                ))
            })?
            .map_err(|error| {
                SemanticExtractionError::Extractor(format!("provider failed: {error}"))
            })?;
        let mut result: SemanticExtractionResult = serde_json::from_str(response.reply.trim())
            .map_err(|error| {
                SemanticExtractionError::Extractor(format!("invalid JSON output: {error}"))
            })?;
        mark_review_flags(&mut result);
        mark_unexpected_participants(&mut result, &self.participants);
        validate_result(input.batch, &result)?;
        Ok(result)
    }
}

fn mark_unexpected_participants(
    result: &mut SemanticExtractionResult,
    allowed: &SemanticParticipantNames,
) {
    let unexpected = |participants: &[String]| {
        participants
            .iter()
            .any(|name| name != &allowed.user && name != &allowed.character)
    };
    for candidate in &mut result.candidate_events {
        if unexpected(&candidate.participants) {
            candidate.needs_review = true;
            if !candidate
                .review_reasons
                .iter()
                .any(|reason| reason == "participant is not in the supplied names")
            {
                candidate
                    .review_reasons
                    .push("participant is not in the supplied names".into());
            }
        }
    }
    if let Some(candidate) = &mut result.candidate_recent_episode {
        if unexpected(&candidate.participants) {
            candidate.needs_review = true;
            if !candidate
                .review_reasons
                .iter()
                .any(|reason| reason == "participant is not in the supplied names")
            {
                candidate
                    .review_reasons
                    .push("participant is not in the supplied names".into());
            }
        }
    }
}

#[derive(Serialize)]
struct ExtractionInput<'a> {
    participants: ParticipantsInput<'a>,
    conversation_mode: &'static str,
    previous_semantic_summary: Option<&'a str>,
    current_active_states: &'a [CurrentActiveState],
    messages: Vec<MessageInput<'a>>,
}

impl<'a> ExtractionInput<'a> {
    fn new(input: SemanticExtractionInput<'a>, names: &'a SemanticParticipantNames) -> Self {
        Self {
            participants: ParticipantsInput {
                user: &names.user,
                character: &names.character,
            },
            conversation_mode: match input.conversation_mode {
                ConversationMode::Chat => "chat",
                ConversationMode::Narrative => "narrative",
            },
            previous_semantic_summary: input.previous_semantic_summary,
            current_active_states: input.current_active_states,
            messages: input
                .batch
                .messages
                .iter()
                .map(|message| MessageInput {
                    id: message.id,
                    role: &message.role,
                    content: &message.content,
                })
                .collect(),
        }
    }
}

#[derive(Serialize)]
struct ParticipantsInput<'a> {
    user: &'a str,
    character: &'a str,
}
#[derive(Serialize)]
struct MessageInput<'a> {
    id: uuid::Uuid,
    role: &'a str,
    content: &'a str,
}

fn response_format() -> serde_json::Value {
    let source = serde_json::json!({ "source_start_message_id": { "type": "string" }, "source_end_message_id": { "type": "string" } });
    let event = serde_json::json!({
        "type": "object", "additionalProperties": false,
        "properties": {
            "summary": { "type": "string" }, "proposed_importance": { "type": "string", "enum": ["light", "normal", "important", "major"] },
            "participants": { "type": "array", "items": { "type": "string" } }, "location": { "type": ["string", "null"] },
            "source_start_message_id": source["source_start_message_id"].clone(), "source_end_message_id": source["source_end_message_id"].clone(),
            "proposed_relationship_relevant": { "type": "boolean" }, "proposed_relationship_direction": { "type": "string", "enum": ["positive", "negative", "mixed", "uncertain"] },
            "confidence": { "type": "string", "enum": ["low", "medium", "high"] }, "needs_review": { "type": "boolean" }, "review_reasons": { "type": "array", "items": { "type": "string" } }
        },
        "required": ["summary", "proposed_importance", "participants", "location", "source_start_message_id", "source_end_message_id", "proposed_relationship_relevant", "proposed_relationship_direction", "confidence", "needs_review", "review_reasons"]
    });
    let episode = serde_json::json!({
        "type": "object", "additionalProperties": false,
        "properties": { "summary": { "type": "string" }, "participants": { "type": "array", "items": { "type": "string" } }, "location": { "type": ["string", "null"] }, "source_start_message_id": source["source_start_message_id"].clone(), "source_end_message_id": source["source_end_message_id"].clone(), "proposed_importance": { "type": "string", "enum": ["light", "normal", "important", "major"] }, "proposed_relationship_relevant": { "type": "boolean" }, "confidence": { "type": "string", "enum": ["low", "medium", "high"] }, "needs_review": { "type": "boolean" }, "review_reasons": { "type": "array", "items": { "type": "string" } } },
        "required": ["summary", "participants", "location", "source_start_message_id", "source_end_message_id", "proposed_importance", "proposed_relationship_relevant", "confidence", "needs_review", "review_reasons"]
    });
    let active_state = serde_json::json!({
        "type": "object", "additionalProperties": false,
        "properties": { "operation": { "type": "string", "enum": ["add", "update", "remove"] }, "state_key": { "type": "string" }, "proposed_value": {}, "confidence": { "type": "string", "enum": ["low", "medium", "high"] }, "needs_review": { "type": "boolean" }, "review_reasons": { "type": "array", "items": { "type": "string" } }, "source_start_message_id": source["source_start_message_id"].clone(), "source_end_message_id": source["source_end_message_id"].clone() },
        "required": ["operation", "state_key", "proposed_value", "confidence", "needs_review", "review_reasons", "source_start_message_id", "source_end_message_id"]
    });
    serde_json::json!({
        "type": "json_schema", "json_schema": { "name": "semantic_extraction", "strict": true,
        "schema": { "type": "object", "additionalProperties": false,
            "properties": {
                "candidate_events": { "type": "array", "items": event },
                "candidate_recent_episode": { "anyOf": [episode, { "type": "null" }] },
                "candidate_active_state_changes": { "type": "array", "items": active_state },
                "next_semantic_summary": { "type": ["string", "null"] }
            },
            "required": ["candidate_events", "candidate_recent_episode", "candidate_active_state_changes", "next_semantic_summary"]
        }
    }} )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::semantic_extractor::{ActiveStateOperation, EventImportance};
    use crate::window_exit::WindowExitBatch;
    use chrono::{DateTime, Utc};
    use eros_engine_store::chat::ChatMessage as StoredChatMessage;
    use serde_json::{json, Value};
    use uuid::Uuid;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn message(session_id: Uuid, index: u128, role: &str, content: &str) -> StoredChatMessage {
        StoredChatMessage {
            id: Uuid::from_u128(index + 1),
            session_id,
            role: role.into(),
            content: content.into(),
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
            metadata: Some(json!({"must_not_reach_extractor": true})),
            read_at: None,
        }
    }
    fn batch() -> WindowExitBatch {
        let session_id = Uuid::new_v4();
        let messages = vec![
            message(session_id, 0, "user", "Please schedule the checkup."),
            message(session_id, 1, "assistant", "I agree to go this afternoon."),
            message(session_id, 2, "user", "Look at this cat image."),
        ];
        WindowExitBatch {
            user_id: Uuid::new_v4(),
            instance_id: Uuid::new_v4(),
            session_id,
            start_message_id: messages[0].id,
            end_message_id: messages[2].id,
            messages,
            created_at: Utc::now(),
        }
    }
    fn model_config() -> Arc<ModelConfig> {
        Arc::new(ModelConfig::from_toml_str("[tasks.semantic_extraction]\nmodel='mock/cheap'\nfallback=[]\nretry_depth=0\ntemperature=0.1\nmax_tokens=500\n").unwrap())
    }
    fn provider_body(reply: &str) -> Value {
        json!({ "id": "gen-semantic", "model": "mock/cheap", "choices": [{ "message": { "content": reply }, "finish_reason": "stop" }] })
    }
    fn empty_output() -> Value {
        json!({"candidate_events": [], "candidate_recent_episode": null, "candidate_active_state_changes": [], "next_semantic_summary": null})
    }
    fn event(batch: &WindowExitBatch, importance: &str) -> Value {
        json!({ "summary": "They agreed to a checkup.", "proposed_importance": importance, "participants": ["Bai Zhi", "Pei Jin"], "location": null, "source_start_message_id": batch.messages[0].id, "source_end_message_id": batch.messages[1].id, "proposed_relationship_relevant": false, "proposed_relationship_direction": "uncertain", "confidence": "high", "needs_review": false, "review_reasons": [] })
    }
    fn episode(batch: &WindowExitBatch) -> Value {
        json!({ "summary": "They traded a cat-image joke.", "participants": ["Bai Zhi", "Pei Jin"], "location": null, "source_start_message_id": batch.messages[1].id, "source_end_message_id": batch.messages[2].id, "proposed_importance": "light", "proposed_relationship_relevant": false, "confidence": "high", "needs_review": false, "review_reasons": [] })
    }
    fn state(batch: &WindowExitBatch, operation: &str) -> Value {
        json!({ "state_key": "afternoon_checkup", "proposed_value": true, "operation": operation, "confidence": "high", "needs_review": false, "review_reasons": [], "source_start_message_id": batch.messages[0].id, "source_end_message_id": batch.messages[1].id })
    }
    async fn extractor_with(response: ResponseTemplate) -> (LlmSemanticExtractor, MockServer) {
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
        (
            LlmSemanticExtractor::new(
                client,
                model_config(),
                SemanticParticipantNames {
                    user: "Bai Zhi".into(),
                    character: "Pei Jin".into(),
                },
            ),
            server,
        )
    }
    async fn success(reply: Value) -> (LlmSemanticExtractor, MockServer) {
        extractor_with(ResponseTemplate::new(200).set_body_json(provider_body(&reply.to_string())))
            .await
    }
    fn input<'a>(
        batch: &'a WindowExitBatch,
        previous: Option<&'a str>,
        states: &'a [CurrentActiveState],
        mode: ConversationMode,
    ) -> SemanticExtractionInput<'a> {
        SemanticExtractionInput {
            batch,
            previous_semantic_summary: previous,
            current_active_states: states,
            conversation_mode: mode,
        }
    }

    #[tokio::test]
    async fn ordinary_chatter_returns_no_events() {
        let batch = batch();
        let (extractor, _) = success(empty_output()).await;
        assert_eq!(
            extractor
                .extract(input(&batch, None, &[], ConversationMode::Chat))
                .await
                .unwrap(),
            SemanticExtractionResult::default()
        );
    }
    #[tokio::test]
    async fn normal_major_and_multiple_events_are_parsed() {
        let batch = batch();
        let output = json!({ "candidate_events": [event(&batch, "normal"), event(&batch, "major")], "candidate_recent_episode": null, "candidate_active_state_changes": [], "next_semantic_summary": null });
        let (extractor, _) = success(output).await;
        let result = extractor
            .extract(input(&batch, None, &[], ConversationMode::Narrative))
            .await
            .unwrap();
        assert_eq!(result.candidate_events.len(), 2);
        assert_eq!(
            result.candidate_events[0].proposed_importance,
            EventImportance::Normal
        );
        assert_eq!(
            result.candidate_events[1].proposed_importance,
            EventImportance::Major
        );
    }
    #[tokio::test]
    async fn a_recent_episode_is_parsed() {
        let batch = batch();
        let (extractor, _) = success(json!({"candidate_events": [], "candidate_recent_episode": episode(&batch), "candidate_active_state_changes": [], "next_semantic_summary": null})).await;
        assert!(extractor
            .extract(input(&batch, None, &[], ConversationMode::Chat))
            .await
            .unwrap()
            .candidate_recent_episode
            .is_some());
    }
    #[tokio::test]
    async fn participant_outside_supplied_names_requires_review() {
        let batch = batch();
        let mut candidate = event(&batch, "light");
        candidate["participants"] = json!(["Unknown Person"]);
        let (extractor, _) = success(json!({
            "candidate_events": [candidate],
            "candidate_recent_episode": null,
            "candidate_active_state_changes": [],
            "next_semantic_summary": null
        }))
        .await;
        let result = extractor
            .extract(input(&batch, None, &[], ConversationMode::Chat))
            .await
            .unwrap();
        assert!(result.candidate_events[0].needs_review);
        assert!(result.candidate_events[0]
            .review_reasons
            .iter()
            .any(|reason| reason == "participant is not in the supplied names"));
    }
    #[tokio::test]
    async fn add_update_and_remove_active_states_are_parsed() {
        let batch = batch();
        let output = json!({"candidate_events": [], "candidate_recent_episode": null, "candidate_active_state_changes": [state(&batch, "add"), state(&batch, "update"), state(&batch, "remove")], "next_semantic_summary": null});
        let (extractor, _) = success(output).await;
        let result = extractor
            .extract(input(&batch, None, &[], ConversationMode::Narrative))
            .await
            .unwrap();
        assert_eq!(
            result
                .candidate_active_state_changes
                .iter()
                .map(|change| change.operation)
                .collect::<Vec<_>>(),
            vec![
                ActiveStateOperation::Add,
                ActiveStateOperation::Update,
                ActiveStateOperation::Remove
            ]
        );
    }
    #[tokio::test]
    async fn previous_summary_states_and_mode_are_sent_and_summary_is_replaced() {
        let batch = batch();
        let states = [CurrentActiveState {
            key: "in_treatment".into(),
            value: json!(true),
        }];
        let output = json!({"candidate_events": [], "candidate_recent_episode": null, "candidate_active_state_changes": [], "next_semantic_summary": "They are arranging the afternoon checkup."});
        let (extractor, server) = success(output).await;
        let result = extractor
            .extract(input(
                &batch,
                Some("They were discussing treatment."),
                &states,
                ConversationMode::Narrative,
            ))
            .await
            .unwrap();
        assert_eq!(
            result.next_semantic_summary.as_deref(),
            Some("They are arranging the afternoon checkup.")
        );
        let requests = server.received_requests().await.unwrap();
        let wire: Value = serde_json::from_slice(&requests[0].body).unwrap();
        let payload: Value =
            serde_json::from_str(wire["messages"][1]["content"].as_str().unwrap()).unwrap();
        assert_eq!(payload["conversation_mode"], "narrative");
        assert_eq!(
            payload["previous_semantic_summary"],
            "They were discussing treatment."
        );
        assert_eq!(payload["current_active_states"][0]["key"], "in_treatment");
        assert!(payload["messages"][0].get("metadata").is_none());
        assert!(payload.get("persona").is_none());
        assert!(payload.get("memory").is_none());
    }
    #[tokio::test]
    async fn changed_topic_can_clear_the_rolling_summary() {
        let batch = batch();
        let (extractor, _) = success(empty_output()).await;
        assert!(extractor
            .extract(input(
                &batch,
                Some("A finished topic."),
                &[],
                ConversationMode::Chat
            ))
            .await
            .unwrap()
            .next_semantic_summary
            .is_none());
    }
    #[tokio::test]
    async fn chat_and_narrative_rules_are_in_the_prompt() {
        let batch = batch();
        let (extractor, server) = success(empty_output()).await;
        extractor
            .extract(input(&batch, None, &[], ConversationMode::Chat))
            .await
            .unwrap();
        let requests = server.received_requests().await.unwrap();
        let wire: Value = serde_json::from_slice(&requests[0].body).unwrap();
        let prompt = wire["messages"][0]["content"].as_str().unwrap();
        assert!(prompt.contains("chat 必须严格过滤"));
        assert!(prompt.contains("narrative 应重视决定"));
    }
    #[tokio::test]
    async fn rejects_multiple_recent_episodes_invalid_enums_and_invalid_json() {
        let batch = batch();
        for output in [
            json!({"candidate_events": [], "candidate_recent_episode": [episode(&batch), episode(&batch)], "candidate_active_state_changes": [], "next_semantic_summary": null}),
            json!({"candidate_events": [event(&batch, "critical")], "candidate_recent_episode": null, "candidate_active_state_changes": [], "next_semantic_summary": null}),
            json!({"candidate_events": [{"summary":"x","proposed_importance":"normal","participants":[],"location":null,"source_start_message_id":batch.messages[0].id,"source_end_message_id":batch.messages[0].id,"proposed_relationship_relevant":false,"proposed_relationship_direction":"up","confidence":"high","needs_review":false,"review_reasons":[]}], "candidate_recent_episode": null, "candidate_active_state_changes": [], "next_semantic_summary": null}),
        ] {
            let (extractor, _) = success(output).await;
            assert!(extractor
                .extract(input(&batch, None, &[], ConversationMode::Chat))
                .await
                .unwrap_err()
                .to_string()
                .contains("invalid JSON output"));
        }
        let (extractor, _) =
            extractor_with(ResponseTemplate::new(200).set_body_json(provider_body("not-json")))
                .await;
        assert!(extractor
            .extract(input(&batch, None, &[], ConversationMode::Chat))
            .await
            .unwrap_err()
            .to_string()
            .contains("invalid JSON output"));
    }
    #[tokio::test]
    async fn rejects_out_of_batch_and_invalid_summary() {
        let batch = batch();
        let mut invalid = event(&batch, "normal");
        invalid["source_end_message_id"] = json!(Uuid::new_v4());
        let (extractor, _) = success(json!({"candidate_events": [invalid], "candidate_recent_episode": null, "candidate_active_state_changes": [], "next_semantic_summary": null})).await;
        assert!(matches!(
            extractor
                .extract(input(&batch, None, &[], ConversationMode::Chat))
                .await
                .unwrap_err(),
            SemanticExtractionError::SourceOutsideBatch { .. }
        ));
        let mut long = event(&batch, "light");
        long["summary"] =
            json!("x".repeat(crate::semantic_extractor::MAX_CANDIDATE_SUMMARY_CHARS + 1));
        let (extractor, _) = success(json!({"candidate_events": [long], "candidate_recent_episode": null, "candidate_active_state_changes": [], "next_semantic_summary": null})).await;
        assert!(matches!(
            extractor
                .extract(input(&batch, None, &[], ConversationMode::Chat))
                .await
                .unwrap_err(),
            SemanticExtractionError::SummaryTooLong { .. }
        ));
    }
    #[tokio::test]
    async fn provider_failure_and_timeout_return_errors() {
        let batch = batch();
        let (extractor, _) =
            extractor_with(ResponseTemplate::new(503).set_body_string("unavailable")).await;
        assert!(extractor
            .extract(input(&batch, None, &[], ConversationMode::Chat))
            .await
            .unwrap_err()
            .to_string()
            .contains("provider failed"));
        let (extractor, _) = extractor_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(100))
                .set_body_json(provider_body(&empty_output().to_string())),
        )
        .await;
        assert!(extractor
            .with_timeout(Duration::from_millis(1))
            .extract(input(&batch, None, &[], ConversationMode::Chat))
            .await
            .unwrap_err()
            .to_string()
            .contains("provider timed out"));
    }

    /// Opt-in local quality/latency evaluation. This uses the shipped model
    /// config and production provider router without touching AppState, the
    /// chat pipeline, or the database.
    #[tokio::test]
    #[ignore = "requires the local Ollama model configured for semantic_extraction"]
    async fn live_ollama_semantic_extraction_matrix() {
        #[derive(Clone)]
        struct LiveCase {
            name: &'static str,
            mode: ConversationMode,
            messages: Vec<(&'static str, &'static str)>,
            previous: Option<&'static str>,
            states: Vec<CurrentActiveState>,
        }

        fn live_batch(case_index: usize, rows: &[(&str, &str)]) -> WindowExitBatch {
            let session_id = Uuid::from_u128(10_000 + case_index as u128);
            let messages = rows
                .iter()
                .enumerate()
                .map(|(index, (role, content))| {
                    message(
                        session_id,
                        20_000 + case_index as u128 * 100 + index as u128,
                        role,
                        content,
                    )
                })
                .collect::<Vec<_>>();
            WindowExitBatch {
                user_id: Uuid::from_u128(30_001),
                instance_id: Uuid::from_u128(30_002),
                session_id,
                start_message_id: messages.first().unwrap().id,
                end_message_id: messages.last().unwrap().id,
                messages,
                created_at: Utc::now(),
            }
        }

        let config_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/model_config.toml");
        let config = Arc::new(ModelConfig::from_toml_file(&config_path).unwrap());
        let resolved = config.resolve(SEMANTIC_EXTRACTION_TASK, None);
        assert_eq!(resolved.model, "qwen3:4b@ollama");
        let client = Arc::new(
            OpenRouterClient::new("unused-by-ollama".into())
                .with_providers(config.build_providers()),
        );
        let extractor = LlmSemanticExtractor::new(
            client,
            config,
            SemanticParticipantNames {
                user: "裴烬".into(),
                character: "白芷".into(),
            },
        );
        let cases = vec![
            LiveCase {
                name: "A_chatter",
                mode: ConversationMode::Chat,
                messages: vec![
                    ("user", "嗯。"),
                    ("assistant", "哈哈哈。"),
                    ("user", "知道了。"),
                ],
                previous: None,
                states: vec![],
            },
            LiveCase {
                name: "B_normal_event",
                mode: ConversationMode::Chat,
                messages: vec![
                    ("user", "下午陪我去医院吧。"),
                    ("assistant", "行，我答应下午陪你去医院。"),
                ],
                previous: None,
                states: vec![],
            },
            LiveCase {
                name: "C_shared_joke",
                mode: ConversationMode::Chat,
                messages: vec![
                    ("user", "我把你上次那张丑照做成表情包了。"),
                    (
                        "assistant",
                        "你敢发出去试试，我就把你的打呼照也做成表情包。",
                    ),
                    ("user", "成交，以后这就是我们的专属表情包大战。"),
                ],
                previous: None,
                states: vec![],
            },
            LiveCase {
                name: "D_important_injury",
                mode: ConversationMode::Narrative,
                messages: vec![
                    ("user", "白芷在战斗中身受重伤，被送进疗养院治疗。"),
                    ("assistant", "医生安排白芷住院观察，短期内不能离开疗养院。"),
                ],
                previous: None,
                states: vec![],
            },
            LiveCase {
                name: "E_major_sacrifice",
                mode: ConversationMode::Narrative,
                messages: vec![
                    ("user", "裴烬冲到白芷身前替她挡下刀，腹部中刀后严重受伤。"),
                    ("assistant", "白芷扶住失血倒下的裴烬，立刻呼救送医。"),
                ],
                previous: None,
                states: vec![],
            },
            LiveCase {
                name: "F_state_remove",
                mode: ConversationMode::Narrative,
                messages: vec![
                    ("user", "医生说可以出院了，东西已经收拾好了。"),
                    ("assistant", "白芷办完出院手续，已经离开疗养院。"),
                ],
                previous: Some("白芷仍在疗养院接受治疗。"),
                states: vec![CurrentActiveState {
                    key: "character_treatment".into(),
                    value: json!("白芷正在疗养院治疗"),
                }],
            },
            LiveCase {
                name: "G_rolling_summary",
                mode: ConversationMode::Chat,
                messages: vec![
                    ("user", "医院那边我约到下午三点了。"),
                    ("assistant", "好，我下午两点半来接你，我们一起过去。"),
                ],
                previous: Some("裴烬和白芷正在商量下午去医院检查。"),
                states: vec![CurrentActiveState {
                    key: "hospital_visit".into(),
                    value: json!("下午去医院检查"),
                }],
            },
        ];

        for round in 1..=2 {
            for (case_index, case) in cases.iter().enumerate() {
                let batch = live_batch(case_index, &case.messages);
                let input = SemanticExtractionInput {
                    batch: &batch,
                    previous_semantic_summary: case.previous,
                    current_active_states: &case.states,
                    conversation_mode: case.mode,
                };
                let request = extractor.request(input).unwrap();
                let started = std::time::Instant::now();
                let response = tokio::time::timeout(
                    Duration::from_secs(120),
                    extractor.client.execute(request),
                )
                .await;
                let elapsed_ms = started.elapsed().as_millis();

                let mut record = json!({
                    "round": round,
                    "case": case.name,
                    "elapsed_ms": elapsed_ms,
                    "json_valid": false,
                    "validation_valid": false,
                    "output": null,
                    "error": null,
                });
                match response {
                    Err(_) => record["error"] = json!("timeout"),
                    Ok(Err(error)) => {
                        record["error"] = json!(format!("provider: {error}"));
                    }
                    Ok(Ok(response)) => match serde_json::from_str::<SemanticExtractionResult>(
                        response.reply.trim(),
                    ) {
                        Err(error) => {
                            record["error"] = json!(format!("json: {error}"));
                            record["output"] = json!(response.reply);
                        }
                        Ok(result) => {
                            record["json_valid"] = json!(true);
                            record["output"] = serde_json::to_value(&result).unwrap();
                            match validate_result(&batch, &result) {
                                Ok(()) => record["validation_valid"] = json!(true),
                                Err(error) => {
                                    record["error"] = json!(format!("validation: {error}"));
                                }
                            }
                        }
                    },
                }
                println!("LIVE_SEMANTIC_RESULT {}", record);
            }
        }
    }
}
