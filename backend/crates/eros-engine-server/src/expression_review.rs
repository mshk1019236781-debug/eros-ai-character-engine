// SPDX-License-Identifier: AGPL-3.0-only
//! Low-frequency Expression Review: has this character's recent expression
//! drifted away from its own fingerprint, or collapsed into a few mechanical
//! patterns?
//!
//! This is the project's second review model doing a second review job. It is
//! a separate module rather than a branch of [`crate::semantic_review`] because
//! the two share a transport and nothing else: semantic review arbitrates one
//! *candidate* against its own source evidence, while this one judges a
//! *window* of already-persisted replies against a fingerprint. Bending the
//! candidate types to fit would have meant teaching `ReviewCandidate` about a
//! concept it does not have.
//!
//! Three properties this module is built around:
//!
//! * **One call, low frequency.** The cadence lives in
//!   `pipeline::expression_recovery`; this module only ever serves a request it
//!   is handed.
//! * **The model selects, it never writes.** `exemplar_picks` names *indices*
//!   into samples the engine already has, and the engine stores the raw row.
//!   There is no field in the response through which a model could supply
//!   replacement text.
//! * **A failure is silence.** Every error path returns "no opinion", which can
//!   neither start nor end recovery. A provider outage must not be able to
//!   toggle the prompt.

use std::sync::Arc;
use std::time::Duration;

use eros_engine_core::expression_recovery::{ExpressionReviewOutput, EXPRESSION_TAGS};
use eros_engine_llm::model_config::ModelConfig;
use eros_engine_llm::openrouter::{ChatMessage, ChatRequest, OpenRouterClient};
use uuid::Uuid;

/// Task key resolved from `model_config.toml`. Callers depend on the task key,
/// never on a provider name.
pub const EXPRESSION_REVIEW_TASK: &str = "expression_review";

/// Bounds one review call. Same order as the semantic reviewer's 30s: both run
/// detached from the reply path, so the budget exists to release the detached
/// task, not to protect a user.
///
/// The provider measured above this budget on loaded calls (see the run notes),
/// so a timed-out review is expected in production; it costs its window by
/// design and changes no verdict. Widening it is left to the operator.
pub const EXPRESSION_REVIEW_TIMEOUT: Duration = Duration::from_secs(30);

const REVIEW_SYSTEM_PROMPT: &str = r#"你是低频表达复核器。只判断一个角色的最近一段 Main RP 原始输出是否偏离它自己的 expression_core，或者是否正在收缩成少数机械重复的表达模式。

不要总结剧情，不要评价文笔，不要改写任何一句原文，不要生成新的角色台词，不要补写样本里没有的内容，不要修改人物设定、关系数值或长期状态。

三种结论：
- stable：表达仍在正常范围内。语速、长短句比例、情绪强度的普通波动都属于 stable。
- drift：角色越来越不像原来的角色。例如原本直接粗粝，现在变成礼貌、温柔、解释型、客服型；或者角色自己的判断消失，开始顺从、泛化、模板化。
- collapse：角色仍然像这个角色，但表达坍缩成少数标签。例如句句粗话、全部变成短句、每次关心都重复同一个动作、冲突反复使用同一句式、某个显著特征异常高频。

只输出严格 JSON，不要 Markdown 或解释：
{"status":"stable|drift|collapse","severity":0.0,"signals":["简短证据"],"target_tags":["conflict|care|refusal|jealousy|casual|confrontation"],"exemplar_picks":[{"index":1,"tags":["care"]}]}

字段规则：
- severity 是 0 到 1 之间的小数，表示偏离程度；stable 必须为 0 或接近 0。
- signals 最多 6 条，每条不超过 80 字。只写观察到的表达现象，不要写修改建议，不要引用大段原文。
- target_tags 只能从 conflict、care、refusal、jealousy、casual、confrontation 里选，用于取回该角色历史稳定表达样本；不确定就留空数组。
- exemplar_picks 只在 status=stable 时填写：从上面带编号的样本里挑最多 3 条最能代表该角色稳定表达的原句，index 用样本编号，tags 从同一套标签里选。status 不是 stable 时必须为空数组。
- 除这一条 JSON 之外不要输出任何内容。"#;

/// One review input. The engine supplies everything; nothing here is
/// model-authored.
#[derive(Debug, Clone, PartialEq)]
pub struct ExpressionReviewRequest {
    pub instance_id: Uuid,
    /// The character's name, for the review's own reference only.
    pub character_name: String,
    /// The exact `[expression_core]` block the Main RP prompt carries. Read
    /// from the same source the prompt uses, so the review can never judge a
    /// character against a fingerprint the character was not actually given.
    pub expression_core: String,
    /// The window's raw assistant replies, oldest first. The review's
    /// `exemplar_picks[].index` is 1-based into this list.
    pub samples: Vec<String>,
    /// Whether recovery is currently injecting, and what the previous review
    /// concluded. Passed so a review of an already-recovering character can
    /// judge change rather than absolute state.
    pub recovery_active: bool,
    pub last_status: Option<String>,
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum ExpressionReviewError {
    #[error("[tasks.expression_review] is not configured")]
    NotConfigured,
    #[error("expression review has no samples to judge")]
    NoSamples,
    #[error("expression review provider failed: {0}")]
    Provider(String),
    #[error("expression review provider timed out after {0} seconds")]
    Timeout(u64),
    #[error("expression review returned invalid JSON: {0}")]
    InvalidJson(String),
}

/// What one review attempt produced.
///
/// `output` is `None` on every failure, and a `None` output is never a verdict:
/// callers must treat it as "carry on unchanged".
#[derive(Debug, Clone, PartialEq)]
pub struct ExpressionReview {
    pub output: Option<ExpressionReviewOutput>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub error: Option<String>,
    pub latency_ms: u64,
}

impl ExpressionReview {
    pub fn is_opinion(&self) -> bool {
        self.output.is_some()
    }
}

pub struct LlmExpressionReviewer {
    client: Arc<OpenRouterClient>,
    model_config: Arc<ModelConfig>,
    timeout: Duration,
}

impl LlmExpressionReviewer {
    pub fn new(client: Arc<OpenRouterClient>, model_config: Arc<ModelConfig>) -> Self {
        Self {
            client,
            model_config,
            timeout: EXPRESSION_REVIEW_TIMEOUT,
        }
    }

    #[cfg(test)]
    fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The configured route, or `None` when the task is absent. Resolved
    /// separately from the request so telemetry can name the model even on a
    /// call that never returned.
    fn route(&self) -> Option<(String, String, String)> {
        self.model_config
            .tasks
            .contains_key(EXPRESSION_REVIEW_TASK)
            .then(|| self.model_config.resolve(EXPRESSION_REVIEW_TASK, None))
            .map(|resolved| {
                let provider = resolved
                    .model
                    .split_once('@')
                    .map(|(_, provider)| provider.to_string())
                    .unwrap_or_else(|| "openrouter".into());
                (resolved.model, provider, resolved.fallback_model.join(","))
            })
    }

    fn request(
        &self,
        input: &ExpressionReviewRequest,
    ) -> Result<ChatRequest, ExpressionReviewError> {
        if input.samples.is_empty() {
            return Err(ExpressionReviewError::NoSamples);
        }
        if !self.model_config.tasks.contains_key(EXPRESSION_REVIEW_TASK) {
            return Err(ExpressionReviewError::NotConfigured);
        }
        let resolved = self.model_config.resolve(EXPRESSION_REVIEW_TASK, None);
        Ok(ChatRequest {
            model: resolved.model,
            fallback_model: resolved.fallback_model,
            messages: vec![
                ChatMessage {
                    role: "system".into(),
                    content: REVIEW_SYSTEM_PROMPT.into(),
                },
                ChatMessage {
                    role: "user".into(),
                    content: review_payload(input),
                },
            ],
            temperature: resolved.temperature as f32,
            sampling: resolved.sampling,
            max_tokens: resolved.max_tokens,
            reasoning: resolved.reasoning,
            response_format: Some(review_response_format()),
            task: Some(EXPRESSION_REVIEW_TASK.into()),
            ..Default::default()
        })
    }

    pub async fn review(
        &self,
        input: ExpressionReviewRequest,
    ) -> Result<ExpressionReview, ExpressionReviewError> {
        let request = self.request(&input)?;
        let (configured_model, provider, _) =
            self.route().ok_or(ExpressionReviewError::NotConfigured)?;
        let sample_count = input.samples.len();

        let started = std::time::Instant::now();
        let response = tokio::time::timeout(self.timeout, self.client.execute(request))
            .await
            .map_err(|_| ExpressionReviewError::Timeout(self.timeout.as_secs()))?
            .map_err(|error| ExpressionReviewError::Provider(error.to_string()))?;
        let latency_ms = started.elapsed().as_millis() as u64;
        let served_model = response.model.clone().unwrap_or(configured_model);

        let mut output: ExpressionReviewOutput = serde_json::from_str(response.reply.trim())
            .map_err(|error| ExpressionReviewError::InvalidJson(error.to_string()))?;
        output.normalize(sample_count);

        Ok(ExpressionReview {
            output: Some(output),
            provider: Some(provider),
            model: Some(served_model),
            error: None,
            latency_ms,
        })
    }

    /// Detached-task entry point. Every error becomes a review with no opinion,
    /// so a caller can log one shape whether or not the provider answered.
    pub async fn review_or_skip(&self, input: ExpressionReviewRequest) -> ExpressionReview {
        let (model, provider) = match self.route() {
            Some((model, provider, _)) => (Some(model), Some(provider)),
            None => (None, None),
        };
        let started = std::time::Instant::now();
        match self.review(input).await {
            Ok(review) => review,
            Err(error) => ExpressionReview {
                output: None,
                provider,
                model,
                error: Some(error.to_string()),
                latency_ms: started.elapsed().as_millis() as u64,
            },
        }
    }
}

/// The review's user message: fingerprint, the window, and nothing else.
///
/// Deliberately not a conversation and deliberately not the plot. The brief
/// forbids giving the review the whole history or the Event DB, and the reason
/// is visible here -- everything in this string is something the review could
/// otherwise mistake for evidence about what should happen next.
fn review_payload(input: &ExpressionReviewRequest) -> String {
    let mut payload = String::new();
    payload.push_str(&format!(
        "[character]\n{}\n\n[expression_core]\n{}\n",
        input.character_name, input.expression_core
    ));
    payload.push_str(
        &match (input.recovery_active, input.last_status.as_deref()) {
            (true, Some(last)) => format!(
                "\n[recovery]\n该角色当前处于 Expression Recovery 中（上一次结论：{last}）。\
             请判断这段输出相比上一次是好转、持平还是继续偏离。\n"
            ),
            (true, None) => "\n[recovery]\n该角色当前处于 Expression Recovery 中。\n".to_string(),
            (false, _) => "\n[recovery]\n未处于 Expression Recovery。\n".to_string(),
        },
    );
    payload.push_str(
        "\n[recent_assistant_outputs]\n下面是该角色最近的连续 Main RP 原始输出，按时间先后编号。\
         只判断表达方式，不总结剧情，不改写内容，也不要引用超过必要的原文。\n",
    );
    for (offset, sample) in input.samples.iter().enumerate() {
        payload.push_str(&format!("\n{}. {}\n", offset + 1, sample.trim()));
    }
    payload
}

fn review_response_format() -> serde_json::Value {
    let tag = serde_json::json!({ "type": "string", "enum": EXPRESSION_TAGS });
    serde_json::json!({
        "type": "json_schema",
        "json_schema": {
            "name": "expression_review",
            "strict": true,
            "schema": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "status": { "type": "string", "enum": ["stable", "drift", "collapse"] },
                    "severity": { "type": "number" },
                    "signals": { "type": "array", "items": { "type": "string" } },
                    "target_tags": { "type": "array", "items": tag.clone() },
                    "exemplar_picks": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "additionalProperties": false,
                            "properties": {
                                "index": { "type": "integer" },
                                "tags": { "type": "array", "items": tag.clone() }
                            },
                            "required": ["index", "tags"]
                        }
                    }
                },
                "required": ["status", "severity", "signals", "target_tags", "exemplar_picks"]
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use eros_engine_core::expression_recovery::{ExemplarPick, ExpressionStatus};
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const CONFIGURED: &str = "[tasks.expression_review]\nmodel='mock/reviewer'\nfallback=[]\n\
                              retry_depth=0\ntemperature=0.0\nmax_tokens=600\n";

    fn model_config(raw: &str) -> Arc<ModelConfig> {
        Arc::new(ModelConfig::from_toml_str(raw).unwrap())
    }

    fn request(samples: &[&str]) -> ExpressionReviewRequest {
        ExpressionReviewRequest {
            instance_id: Uuid::from_u128(7),
            character_name: "裴烬".into(),
            expression_core: crate::prompt::expression_core_by_name("裴烬").to_string(),
            samples: samples.iter().map(|sample| sample.to_string()).collect(),
            recovery_active: false,
            last_status: None,
        }
    }

    fn provider_body(reply: &str) -> serde_json::Value {
        json!({ "id": "expr-1", "model": "mock/reviewer", "choices": [
            { "message": { "content": reply }, "finish_reason": "stop" }] })
    }

    async fn reviewer_with(
        config: &str,
        response: ResponseTemplate,
    ) -> (LlmExpressionReviewer, MockServer) {
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
            LlmExpressionReviewer::new(client, model_config(config)),
            server,
        )
    }

    async fn reviewer_success(output: serde_json::Value) -> (LlmExpressionReviewer, MockServer) {
        reviewer_with(
            CONFIGURED,
            ResponseTemplate::new(200).set_body_json(provider_body(&output.to_string())),
        )
        .await
    }

    #[tokio::test]
    async fn a_stable_review_is_accepted_and_normalized() {
        let (reviewer, _server) = reviewer_success(json!({
            "status": "stable",
            "severity": 0.05,
            "signals": ["长短句比例正常", "关心时仍有自己的判断"],
            "target_tags": ["care", "not-a-tag"],
            "exemplar_picks": [{"index": 1, "tags": ["care"]}, {"index": 9, "tags": ["care"]}]
        }))
        .await;

        let review = reviewer
            .review(request(&[
                "啧，少逞强，手给我看看。",
                "这也能叫计划？你想过后果吗。",
            ]))
            .await
            .unwrap();

        assert!(review.is_opinion());
        let output = review.output.expect("a 200 with valid JSON is an opinion");
        assert_eq!(output.status, ExpressionStatus::Stable);
        assert_eq!(output.target_tags, vec!["care".to_string()]);
        // index 9 is past the end of a two-sample window.
        assert_eq!(
            output.exemplar_picks,
            vec![ExemplarPick {
                index: 1,
                tags: vec!["care".into()]
            }]
        );
        assert_eq!(review.provider.as_deref(), Some("openrouter"));
        assert_eq!(review.model.as_deref(), Some("mock/reviewer"));
        assert!(review.error.is_none());
    }

    #[tokio::test]
    async fn a_collapse_review_carries_no_exemplar_picks() {
        let (reviewer, _server) = reviewer_success(json!({
            "status": "collapse",
            "severity": 0.72,
            "signals": ["连续六条回复全部以同一句式开头"],
            "target_tags": ["conflict"],
            "exemplar_picks": []
        }))
        .await;

        let review = reviewer.review(request(&["啧，你行不行。"])).await.unwrap();
        let output = review.output.unwrap();
        assert_eq!(output.status, ExpressionStatus::Collapse);
        assert!(output.exemplar_picks.is_empty());
        assert_eq!(output.severity, 0.72);
    }

    #[tokio::test]
    async fn the_request_carries_the_fingerprint_and_the_numbered_samples_only() {
        let (reviewer, server) = reviewer_success(json!({
            "status": "stable",
            "severity": 0.0,
            "signals": [],
            "target_tags": [],
            "exemplar_picks": []
        }))
        .await;

        reviewer
            .review(request(&[
                "啧，少逞强，手给我看看。",
                "这也能叫计划？你想过后果吗。",
            ]))
            .await
            .unwrap();

        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1, "one review is one provider call");
        let wire: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        let payload = wire["messages"][1]["content"].as_str().unwrap();

        // The fingerprint the character actually receives.
        assert!(payload.contains("[expression_core]"));
        assert!(payload.contains("反应直接，情绪外显"));
        // Numbered samples, in order, each exactly once.
        assert!(payload.contains("1. 啧，少逞强，手给我看看。"));
        assert!(payload.contains("2. 这也能叫计划？你想过后果吗。"));
        assert_eq!(payload.matches("啧，少逞强，手给我看看。").count(), 1);
        // No plot history, no Event DB, no previous replies beyond the window.
        assert!(wire["messages"][1]["content"]
            .as_str()
            .unwrap()
            .contains("[recent_assistant_outputs]"));
        assert!(!payload.contains("[world_facts]"));
        assert!(!payload.contains("[reply_length]"));
    }

    #[tokio::test]
    async fn the_response_format_is_a_strict_schema_over_the_closed_tags() {
        let (reviewer, server) = reviewer_success(json!({
            "status": "stable",
            "severity": 0.0,
            "signals": [],
            "target_tags": [],
            "exemplar_picks": []
        }))
        .await;
        reviewer.review(request(&["啧，少逞强。"])).await.unwrap();

        let requests = server.received_requests().await.unwrap();
        let wire: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
        let schema = &wire["response_format"]["json_schema"];
        assert_eq!(wire["response_format"]["type"], "json_schema");
        assert_eq!(schema["strict"], true);
        assert_eq!(
            schema["schema"]["properties"]["target_tags"]["items"]["enum"],
            json!(EXPRESSION_TAGS)
        );
        assert_eq!(
            schema["schema"]["properties"]["status"]["enum"],
            json!(["stable", "drift", "collapse"])
        );
        // No field exists through which a model could supply replacement text.
        assert!(schema["schema"]["properties"]
            .get("exemplar_text")
            .is_none());
    }

    #[tokio::test]
    async fn invalid_json_is_a_review_with_no_opinion() {
        let (reviewer, _server) = reviewer_with(
            CONFIGURED,
            ResponseTemplate::new(200).set_body_json(provider_body("this is not json")),
        )
        .await;
        let review = reviewer
            .review(request(&["啧，少逞强。"]))
            .await
            .unwrap_err();
        assert!(matches!(review, ExpressionReviewError::InvalidJson(_)));
    }

    #[tokio::test]
    async fn a_provider_error_is_a_review_with_no_opinion() {
        let (reviewer, _server) = reviewer_with(
            CONFIGURED,
            ResponseTemplate::new(500).set_body_string("upstream boom"),
        )
        .await;
        let review = reviewer
            .review(request(&["啧，少逞强。"]))
            .await
            .unwrap_err();
        assert!(matches!(review, ExpressionReviewError::Provider(_)));
    }

    #[tokio::test]
    async fn a_timeout_is_a_review_with_no_opinion() {
        let (reviewer, _server) = reviewer_with(
            CONFIGURED,
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(5))
                .set_body_json(provider_body("{}")),
        )
        .await;
        let reviewer = reviewer.with_timeout(Duration::from_millis(50));
        let review = reviewer
            .review(request(&["啧，少逞强。"]))
            .await
            .unwrap_err();
        assert_eq!(review, ExpressionReviewError::Timeout(0));
    }

    #[tokio::test]
    async fn an_unconfigured_task_never_calls_the_provider() {
        let (reviewer, server) = reviewer_with(
            "[tasks.semantic_review]\nmodel='mock/reviewer'\n",
            ResponseTemplate::new(200),
        )
        .await;
        let error = reviewer
            .review(request(&["啧，少逞强。"]))
            .await
            .unwrap_err();
        assert_eq!(error, ExpressionReviewError::NotConfigured);
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn an_empty_window_never_calls_the_provider() {
        let (reviewer, server) = reviewer_success(json!({
            "status": "stable",
            "severity": 0.0,
            "signals": [],
            "target_tags": [],
            "exemplar_picks": []
        }))
        .await;
        let error = reviewer.review(request(&[])).await.unwrap_err();
        assert_eq!(error, ExpressionReviewError::NoSamples);
        assert!(server.received_requests().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn review_or_skip_turns_every_failure_into_silence() {
        let (reviewer, _server) = reviewer_with(
            CONFIGURED,
            ResponseTemplate::new(500).set_body_string("upstream boom"),
        )
        .await;
        let review = reviewer.review_or_skip(request(&["啧，少逞强。"])).await;
        assert!(!review.is_opinion());
        assert!(review.output.is_none());
        assert!(review.error.is_some());
        // The route is still named, so telemetry can say which model failed.
        assert_eq!(review.model.as_deref(), Some("mock/reviewer"));
        assert_eq!(review.provider.as_deref(), Some("openrouter"));
    }
}
