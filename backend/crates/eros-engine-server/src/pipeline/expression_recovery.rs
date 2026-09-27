// SPDX-License-Identifier: AGPL-3.0-only
//! Runtime half of Expression Recovery V1.
//!
//! Two seams, one of which is on the reply path and read-only:
//!
//! ```text
//! post_process  (detached, after the reply is persisted)
//!   -> cadence: 12 eligible assistant turns since this character's last review
//!   -> expression_review::LlmExpressionReviewer   (one call, low frequency)
//!   -> apply_review -> engine.expression_exemplars / ..._recovery_state
//! handlers      (reply path, READ ONLY)
//!   -> load_for_prompt -> [expression_reference] block
//! ```
//!
//! Nothing here touches recall, the Event graph, World Facts, Agency, the
//! response contract, or the prompt's stable prefix. The write seam runs where
//! `run_semantic_review` already runs, and the read seam reads two tables and
//! renders one string.
//!
//! **Why the cadence is read from the database.** "How many assistant turns
//! have passed since this character was last reviewed" has to survive a
//! restart. An in-process counter would restart at zero, so a long session
//! spanning a deploy could simply never be reviewed again, and a seeded or
//! replayed conversation would never reach the threshold at all. The watermark
//! is `expression_recovery_state.last_reviewed_at`, which the row already has
//! to carry.

use chrono::{DateTime, Utc};
use eros_engine_core::expression_recovery::{
    recovery_transition, render_expression_reference, select_prompt_exemplars,
    select_stable_exemplar_indices, ExpressionReviewOutput, ExpressionStatus, PromptExemplar,
    RecoveryAction, EXPRESSION_REVIEW_EVERY_TURNS, EXPRESSION_REVIEW_WINDOW_TURNS,
    MAX_ACTIVE_EXEMPLARS, STABLE_REVIEWS_TO_END,
};
use eros_engine_store::expression_recovery::{
    ExpressionExemplarInsert, ExpressionExemplarRepo, ExpressionRecoveryRepo,
    ExpressionRecoveryStateRow, ExpressionRecoveryStateWrite,
};
use sqlx::PgPool;
use uuid::Uuid;

use crate::expression_review::{ExpressionReviewRequest, LlmExpressionReviewer};
use crate::state::AppState;

/// One persisted Main RP reply, as the reviewer and the exemplar pool see it.
///
/// `content` is the row's stored text with the hidden memory trailer already
/// stripped by the persistence layer -- this is the same string the user was
/// shown, which is exactly what an exemplar has to be.
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub struct AssistantSample {
    pub id: Uuid,
    pub content: String,
}

/// The identity a review and a prompt build are both scoped by. Carried as one
/// value so no call site can pass three of the four.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReviewContext {
    pub session_id: Uuid,
    pub user_id: Uuid,
    pub instance_id: Uuid,
    /// `persona_genomes.id` of the speaking character.
    pub character_id: Uuid,
    /// Assistant-turn ordinal within the session, for telemetry only.
    pub turn: i32,
}

/// What one applied review changed. Returned rather than only logged so a test
/// can assert the effect instead of grepping stdout.
#[derive(Debug, Clone, PartialEq)]
pub struct AppliedReview {
    pub action: RecoveryAction,
    pub active: bool,
    pub consecutive_stable: u32,
    /// Exemplars this review added to the pool (stable windows only).
    pub saved_exemplars: Vec<Uuid>,
    /// The exemplars the next turn will inject. Empty while inactive.
    pub injected_exemplar_ids: Vec<Uuid>,
}

/// What one prompt build found. Mirrors `WorldFactPromptContext` in shape and
/// in intent: the caller only ever asks whether there is a block.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExpressionRecoveryPromptContext {
    pub block: Option<String>,
    pub active: bool,
    /// Exemplars that reached the prompt.
    pub injected: usize,
    pub exemplar_ids: Vec<Uuid>,
    /// Active exemplars this character has, before selection.
    pub pool_size: usize,
}

/// A persisted reply is eligible to be reviewed or stored as an exemplar.
///
/// One SQL predicate, used by both the count and the window query, so the
/// cadence can never disagree with the window it triggers. Ghost replies,
/// truncations, the empty-response fallback line, other channels and blank rows
/// are all excluded: none of them is the character's expression.
const ELIGIBLE_CLAUSE: &str = "role = 'assistant' \
     AND channel IS NULL \
     AND NOT ghost_decision \
     AND NOT truncated \
     AND btrim(content) <> '' \
     AND content <> $2";

/// This session's eligible assistant turns: how many since `after`, and how
/// many in total. Both in one round trip because the `turn` telemetry field is
/// the second number and neither is worth a query of its own.
async fn assistant_turn_counts(
    pool: &PgPool,
    session_id: Uuid,
    after: Option<DateTime<Utc>>,
) -> Result<(i64, i64), sqlx::Error> {
    sqlx::query_as(&format!(
        "SELECT count(*) FILTER (WHERE $3::timestamptz IS NULL OR sent_at > $3), \
                count(*) \
         FROM engine.chat_messages \
         WHERE session_id = $1 AND {ELIGIBLE_CLAUSE}"
    ))
    .bind(session_id)
    .bind(crate::pipeline::stream::EMPTY_RESPONSE_FALLBACK_TEXT)
    .bind(after)
    .fetch_one(pool)
    .await
}

/// The newest `limit` eligible replies, returned oldest first -- the order the
/// review and the prompt both read in.
async fn recent_assistant_window(
    pool: &PgPool,
    session_id: Uuid,
    limit: i64,
) -> Result<Vec<AssistantSample>, sqlx::Error> {
    let mut rows: Vec<AssistantSample> = sqlx::query_as(&format!(
        "SELECT id, content FROM engine.chat_messages \
         WHERE session_id = $1 AND {ELIGIBLE_CLAUSE} \
         ORDER BY sent_at DESC, id DESC \
         LIMIT $3"
    ))
    .bind(session_id)
    .bind(crate::pipeline::stream::EMPTY_RESPONSE_FALLBACK_TEXT)
    .bind(limit.clamp(1, EXPRESSION_REVIEW_WINDOW_TURNS as i64))
    .fetch_all(pool)
    .await?;
    rows.reverse();
    Ok(rows)
}

/// The detached-task entry point, called from `post_process::run` beside
/// `run_semantic_review`.
///
/// Returns without touching anything when the cadence has not been reached --
/// the overwhelmingly common case -- so a normal turn pays two indexed counts
/// and no model call.
pub async fn run_review(state: &AppState, context: ReviewContext, character_name: &str) {
    let state_repo = ExpressionRecoveryRepo { pool: &state.pool };
    let current = match state_repo
        .load(context.user_id, context.instance_id, context.character_id)
        .await
    {
        Ok(row) => row,
        Err(error) => {
            tracing::warn!(
                error = %error,
                session_id = %context.session_id,
                "EXPRESSION_REVIEW_SKIP state load failed"
            );
            return;
        }
    };
    let watermark = current.as_ref().and_then(|row| row.last_reviewed_at);
    let (since_review, session_turns) =
        match assistant_turn_counts(&state.pool, context.session_id, watermark).await {
            Ok(counts) => counts,
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    session_id = %context.session_id,
                    "EXPRESSION_REVIEW_SKIP turn count failed"
                );
                return;
            }
        };
    if since_review < EXPRESSION_REVIEW_EVERY_TURNS as i64 {
        tracing::debug!(
            session_id = %context.session_id,
            instance_id = %context.instance_id,
            character_id = %context.character_id,
            assistant_turns_since_review = since_review,
            "EXPRESSION_REVIEW_SKIP cadence not reached"
        );
        return;
    }

    let window = match recent_assistant_window(
        &state.pool,
        context.session_id,
        EXPRESSION_REVIEW_WINDOW_TURNS as i64,
    )
    .await
    {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(
                error = %error,
                session_id = %context.session_id,
                "EXPRESSION_REVIEW_SKIP window load failed"
            );
            return;
        }
    };
    if window.is_empty() {
        return;
    }

    let recovery_active = current.as_ref().is_some_and(|row| row.active);
    let last_status = current
        .as_ref()
        .and_then(|row| row.status())
        .map(|status| status.as_str().to_string());
    let context = ReviewContext {
        turn: session_turns as i32,
        ..context
    };

    tracing::info!(
        session_id = %context.session_id,
        instance_id = %context.instance_id,
        character_id = %context.character_id,
        turn = context.turn,
        assistant_turns_since_review = since_review,
        samples = window.len(),
        recovery_active,
        last_status = ?last_status,
        "EXPRESSION_REVIEW_TRIGGERED"
    );

    let reviewer = LlmExpressionReviewer::new(state.openrouter.clone(), state.model_config.clone());
    let review = reviewer
        .review_or_skip(ExpressionReviewRequest {
            instance_id: context.instance_id,
            character_name: character_name.to_string(),
            expression_core: crate::prompt::expression_core_by_name(character_name).to_string(),
            samples: window.iter().map(|row| row.content.clone()).collect(),
            recovery_active,
            last_status,
        })
        .await;

    tracing::info!(
        session_id = %context.session_id,
        instance_id = %context.instance_id,
        character_id = %context.character_id,
        turn = context.turn,
        status = review
            .output
            .as_ref()
            .map(|output| output.status.as_str())
            .unwrap_or("none"),
        severity = review.output.as_ref().map(|output| output.severity),
        signals_count = review
            .output
            .as_ref()
            .map(|output| output.signals.len())
            .unwrap_or(0),
        target_tags = ?review
            .output
            .as_ref()
            .map(|output| output.target_tags.clone())
            .unwrap_or_default(),
        exemplar_picks = review
            .output
            .as_ref()
            .map(|output| output.exemplar_picks.len())
            .unwrap_or(0),
        provider = ?review.provider,
        model = ?review.model,
        latency_ms = review.latency_ms,
        error = ?review.error,
        "EXPRESSION_REVIEW_RESULT"
    );

    let Some(output) = review.output else {
        // No opinion. The attempt is recorded so the low-frequency watermark
        // advances and a provider outage cannot turn into one call per turn;
        // nothing about recovery changes.
        record_attempt(&state.pool, context, current.as_ref()).await;
        return;
    };

    let applied = apply_review(&state.pool, context, &window, &output).await;
    tracing::debug!(
        session_id = %context.session_id,
        action = ?applied.action,
        active = applied.active,
        injected = applied.injected_exemplar_ids.len(),
        "EXPRESSION_REVIEW_APPLIED"
    );
}

/// Record that a review ran and reached no conclusion.
///
/// Writes the previous verdict back unchanged (or NULL for a character that has
/// never had one) and lets `save` advance `last_reviewed_at`. The alternative,
/// writing nothing and retrying on the next turn, would mean a provider outage
/// produces one review call per turn -- the opposite of low frequency. What
/// this direction costs is one lost window, and the window's replies stay in
/// the transcript to be covered by the next one.
async fn record_attempt(
    pool: &PgPool,
    context: ReviewContext,
    current: Option<&ExpressionRecoveryStateRow>,
) {
    let repo = ExpressionRecoveryRepo { pool };
    let write = ExpressionRecoveryStateWrite {
        user_id: context.user_id,
        instance_id: context.instance_id,
        character_id: context.character_id,
        active: current.is_some_and(|row| row.active),
        consecutive_stable: current.map(|row| row.consecutive_stable()).unwrap_or(0),
        last_status: current.and_then(|row| row.status()),
        last_severity: current.and_then(|row| row.last_severity).unwrap_or(0.0),
        started_at: None,
        active_exemplar_ids: current
            .map(|row| row.active_exemplar_ids.clone())
            .unwrap_or_default(),
        target_tags: current
            .map(|row| row.target_tags.clone())
            .unwrap_or_default(),
    };
    if let Err(error) = repo.save(&write).await {
        tracing::warn!(
            error = %error,
            session_id = %context.session_id,
            "EXPRESSION_REVIEW_ATTEMPT not recorded"
        );
    }
}

/// Apply one review verdict.
///
/// Split out of [`run_review`] so the entire state effect -- the stable-window
/// capture, the recovery transition and the exemplar selection -- is testable
/// without a provider, and so the provider call has exactly one place it can
/// influence.
pub async fn apply_review(
    pool: &PgPool,
    context: ReviewContext,
    window: &[AssistantSample],
    output: &ExpressionReviewOutput,
) -> AppliedReview {
    let state_repo = ExpressionRecoveryRepo { pool };
    let exemplar_repo = ExpressionExemplarRepo { pool };
    let current = state_repo
        .load(context.user_id, context.instance_id, context.character_id)
        .await
        .ok()
        .flatten();

    // Only a stable window may teach the pool. A drifting window is evidence of
    // the problem, not of the character.
    let saved_exemplars = if output.status == ExpressionStatus::Stable {
        save_stable_exemplars(&exemplar_repo, context, window, output).await
    } else {
        Vec::new()
    };

    let transition = recovery_transition(
        current.as_ref().is_some_and(|row| row.active),
        current
            .as_ref()
            .map(|row| row.consecutive_stable())
            .unwrap_or(0),
        output.status,
    );

    // A drift/collapse re-selects for its own tags; a stable review that merely
    // keeps recovery alive leaves the current selection alone, because its
    // `target_tags` describe the window it just read, not a new need.
    let previous = current
        .as_ref()
        .map(|row| row.active_exemplar_ids.clone())
        .unwrap_or_default();
    let injected_exemplar_ids = match transition.action {
        RecoveryAction::Start => {
            select_for_recovery(&exemplar_repo, context, &output.target_tags).await
        }
        RecoveryAction::Refresh if output.status.needs_recovery() => {
            select_for_recovery(&exemplar_repo, context, &output.target_tags).await
        }
        RecoveryAction::Refresh => previous,
        RecoveryAction::Idle | RecoveryAction::End => Vec::new(),
    };

    match transition.action {
        RecoveryAction::Start => tracing::info!(
            session_id = %context.session_id,
            instance_id = %context.instance_id,
            character_id = %context.character_id,
            turn = context.turn,
            status = output.status.as_str(),
            severity = output.severity,
            signals = ?output.signals,
            exemplars = injected_exemplar_ids.len(),
            "EXPRESSION_RECOVERY_STARTED"
        ),
        RecoveryAction::End => tracing::info!(
            session_id = %context.session_id,
            instance_id = %context.instance_id,
            character_id = %context.character_id,
            turn = context.turn,
            consecutive_stable = STABLE_REVIEWS_TO_END,
            "EXPRESSION_RECOVERY_ENDED"
        ),
        RecoveryAction::Refresh if output.status == ExpressionStatus::Stable => tracing::info!(
            session_id = %context.session_id,
            instance_id = %context.instance_id,
            character_id = %context.character_id,
            turn = context.turn,
            consecutive_stable = transition.consecutive_stable,
            "EXPRESSION_RECOVERY_STABLE"
        ),
        _ => {}
    }

    let write = ExpressionRecoveryStateWrite {
        user_id: context.user_id,
        instance_id: context.instance_id,
        character_id: context.character_id,
        active: transition.active,
        consecutive_stable: transition.consecutive_stable,
        last_status: Some(output.status),
        last_severity: output.severity,
        // Only the review that starts recovery stamps a start; `save` keeps the
        // existing value on every later write.
        started_at: (transition.action == RecoveryAction::Start).then(Utc::now),
        active_exemplar_ids: injected_exemplar_ids.clone(),
        target_tags: output.target_tags.clone(),
    };
    if let Err(error) = state_repo.save(&write).await {
        tracing::warn!(
            error = %error,
            session_id = %context.session_id,
            "EXPRESSION_RECOVERY_STATE not saved"
        );
    }

    AppliedReview {
        action: transition.action,
        active: transition.active,
        consecutive_stable: transition.consecutive_stable,
        saved_exemplars,
        injected_exemplar_ids,
    }
}

/// Store the representative replies of one stable window.
///
/// The model's picks are indices, so the text stored is the persisted row
/// verbatim; the tags come from the pick, falling back to the window's
/// `target_tags` when the model tagged the window but not the sample.
async fn save_stable_exemplars(
    repo: &ExpressionExemplarRepo<'_>,
    context: ReviewContext,
    window: &[AssistantSample],
    output: &ExpressionReviewOutput,
) -> Vec<Uuid> {
    let texts: Vec<String> = window.iter().map(|row| row.content.clone()).collect();
    let chosen = select_stable_exemplar_indices(&texts, &output.exemplar_picks);
    let mut saved = Vec::new();
    for index in chosen {
        let Some(row) = window.get(index) else {
            continue;
        };
        let tags = output
            .exemplar_picks
            .iter()
            .find(|pick| pick.index == index + 1)
            .map(|pick| pick.tags.clone())
            .filter(|tags| !tags.is_empty())
            .unwrap_or_else(|| output.target_tags.clone());
        let insert = ExpressionExemplarInsert {
            user_id: context.user_id,
            instance_id: context.instance_id,
            character_id: context.character_id,
            raw_text: row.content.clone(),
            tags,
            source_session_id: Some(context.session_id),
            source_message_id: Some(row.id),
            source_turn: Some(context.turn),
        };
        match repo.insert(&insert).await {
            Ok(Some(exemplar_id)) => {
                tracing::info!(
                    session_id = %context.session_id,
                    instance_id = %context.instance_id,
                    character_id = %context.character_id,
                    turn = context.turn,
                    exemplar_id = %exemplar_id,
                    source_message_id = %row.id,
                    chars = row.content.chars().count(),
                    "EXPRESSION_EXEMPLAR_SAVED"
                );
                saved.push(exemplar_id);
            }
            Ok(None) => tracing::debug!(
                session_id = %context.session_id,
                character_id = %context.character_id,
                source_message_id = %row.id,
                "EXPRESSION_EXEMPLAR_DEDUPED"
            ),
            Err(error) => tracing::warn!(
                error = %error,
                session_id = %context.session_id,
                "EXPRESSION_EXEMPLAR_SAVE failed"
            ),
        }
    }
    // The cap is what keeps the pool a reservoir rather than an archive.
    if !saved.is_empty() {
        if let Err(error) = repo
            .trim_to_cap(context.user_id, context.instance_id, context.character_id)
            .await
        {
            tracing::warn!(error = %error, "EXPRESSION_EXEMPLAR_TRIM failed");
        }
    }
    saved
}

/// Select the 2-3 exemplars a starting or refreshing recovery will inject.
async fn select_for_recovery(
    repo: &ExpressionExemplarRepo<'_>,
    context: ReviewContext,
    target_tags: &[String],
) -> Vec<Uuid> {
    let pool_rows = match repo
        .list_pool(
            context.user_id,
            context.instance_id,
            context.character_id,
            MAX_ACTIVE_EXEMPLARS,
        )
        .await
    {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(
                error = %error,
                session_id = %context.session_id,
                "EXPRESSION_EXEMPLAR_RETRIEVED failed"
            );
            return Vec::new();
        }
    };
    let candidates: Vec<PromptExemplar> = pool_rows.iter().map(to_prompt_exemplar).collect();
    let picked = select_prompt_exemplars(&candidates, target_tags);
    let ids: Vec<Uuid> = picked.iter().map(|exemplar| exemplar.id).collect();

    tracing::info!(
        session_id = %context.session_id,
        instance_id = %context.instance_id,
        character_id = %context.character_id,
        turn = context.turn,
        pool_size = pool_rows.len(),
        target_tags = ?target_tags,
        retrieved = ids.len(),
        exemplar_ids = ?ids,
        "EXPRESSION_EXEMPLAR_RETRIEVED"
    );
    if ids.is_empty() {
        // Legitimate: a character whose first drift arrives before it has ever
        // had a stable window reviewed has nothing to be pulled back toward.
        tracing::warn!(
            session_id = %context.session_id,
            character_id = %context.character_id,
            "EXPRESSION_EXEMPLAR_RETRIEVED empty pool; recovery starts with no reference"
        );
    } else {
        if let Err(error) = repo.mark_used(&ids).await {
            tracing::warn!(error = %error, "EXPRESSION_EXEMPLAR_MARK_USED failed");
        }
        tracing::info!(
            session_id = %context.session_id,
            instance_id = %context.instance_id,
            character_id = %context.character_id,
            turn = context.turn,
            injected = ids.len(),
            exemplar_ids = ?ids,
            "EXPRESSION_RECOVERY_INJECTED"
        );
    }
    ids
}

fn to_prompt_exemplar(
    row: &eros_engine_store::expression_recovery::ExpressionExemplarRow,
) -> PromptExemplar {
    PromptExemplar {
        id: row.id,
        text: row.raw_text.clone(),
        tags: row.tags.clone(),
    }
}

/// Read side of the chain: the `[expression_reference]` block for one turn.
///
/// Strictly read-only and never fails the turn. An inactive character, an
/// unreadable state row and an empty pool all return `Default::default()`, and
/// `handlers` then leaves the prompt byte-identical to what it would have been
/// without this feature.
pub async fn load_for_prompt(
    pool: &PgPool,
    session_id: Uuid,
    user_id: Uuid,
    instance_id: Uuid,
    character_id: Uuid,
    turn: u32,
) -> ExpressionRecoveryPromptContext {
    let state_repo = ExpressionRecoveryRepo { pool };
    let state = match state_repo.load(user_id, instance_id, character_id).await {
        Ok(Some(state)) if state.active => state,
        Ok(_) => return ExpressionRecoveryPromptContext::default(),
        Err(error) => {
            tracing::warn!(
                error = %error,
                session_id = %session_id,
                "EXPRESSION_RECOVERY inactive: state load failed"
            );
            return ExpressionRecoveryPromptContext::default();
        }
    };

    let exemplar_repo = ExpressionExemplarRepo { pool };
    let pool_rows = match exemplar_repo
        .list_pool(user_id, instance_id, character_id, MAX_ACTIVE_EXEMPLARS)
        .await
    {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(
                error = %error,
                session_id = %session_id,
                "EXPRESSION_RECOVERY inactive: pool load failed"
            );
            return ExpressionRecoveryPromptContext::default();
        }
    };

    // Prefer exactly the rows this recovery selected. If they were retired by
    // the cap since, fall back to a fresh selection so an active recovery does
    // not silently stop injecting.
    let mut chosen: Vec<PromptExemplar> = state
        .active_exemplar_ids
        .iter()
        .filter_map(|id| pool_rows.iter().find(|row| row.id == *id))
        .map(to_prompt_exemplar)
        .collect();
    if chosen.is_empty() {
        let candidates: Vec<PromptExemplar> = pool_rows.iter().map(to_prompt_exemplar).collect();
        chosen = select_prompt_exemplars(&candidates, &state.target_tags)
            .into_iter()
            .cloned()
            .collect();
    }

    let refs: Vec<&PromptExemplar> = chosen.iter().collect();
    let block = render_expression_reference(&refs);
    let exemplar_ids: Vec<Uuid> = chosen.iter().map(|exemplar| exemplar.id).collect();

    tracing::info!(
        session_id = %session_id,
        instance_id = %instance_id,
        character_id = %character_id,
        turn,
        active = true,
        pool_size = pool_rows.len(),
        injected = exemplar_ids.len(),
        exemplar_ids = ?exemplar_ids,
        "EXPRESSION_RECOVERY_INJECTED_PROMPT"
    );

    ExpressionRecoveryPromptContext {
        block,
        active: true,
        injected: exemplar_ids.len(),
        exemplar_ids,
        pool_size: pool_rows.len(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use eros_engine_core::expression_recovery::ExemplarPick;
    use eros_engine_store::expression_recovery::ExpressionExemplarInsert;

    const DRIFTY: &str = "当然可以呀，我完全理解你的感受，让我慢慢跟你解释一下这背后的原因。";
    const IN_CHARACTER_ONE: &str = "啧，少逞强，手给我看看，别自己硬扛。";
    const IN_CHARACTER_TWO: &str = "这也能叫计划？你先想清楚后果再来找我。";
    const IN_CHARACTER_THREE: &str = "行，那就按你说的来，出事别怪我没提醒。";

    /// One user, one instance, one genome and one session. The genome name is
    /// randomised so a test may seed more than one character without tripping
    /// the genome/instance uniqueness.
    async fn fixture(pool: &PgPool) -> (Uuid, Uuid, Uuid, Uuid) {
        let user_id = Uuid::new_v4();
        let genome_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_genomes (name, system_prompt, art_metadata) \
             VALUES ($1, 'you are a companion', '{}'::jsonb) RETURNING id",
        )
        .bind(format!("裴烬-{}", Uuid::new_v4()))
        .fetch_one(pool)
        .await
        .unwrap();
        let instance_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_instances (genome_id, owner_uid) \
             VALUES ($1, $2) RETURNING id",
        )
        .bind(genome_id)
        .bind(user_id)
        .fetch_one(pool)
        .await
        .unwrap();
        let session_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id, instance_id) VALUES ($1, $2) RETURNING id",
        )
        .bind(user_id)
        .bind(instance_id)
        .fetch_one(pool)
        .await
        .unwrap();
        (user_id, instance_id, genome_id, session_id)
    }

    /// One assistant row, explicitly ordered. `sent_at` has to be supplied
    /// because two rows inserted in the same transaction would otherwise tie and
    /// the window order would fall back to a random uuid.
    async fn assistant_row(pool: &PgPool, session_id: Uuid, text: &str, offset: f64) -> Uuid {
        sqlx::query_scalar(
            "INSERT INTO engine.chat_messages (session_id, role, content, sent_at) \
             VALUES ($1, 'assistant', $2, now() + make_interval(secs => $3::double precision)) \
             RETURNING id",
        )
        .bind(session_id)
        .bind(text)
        .bind(offset)
        .fetch_one(pool)
        .await
        .unwrap()
    }

    async fn window_of(pool: &PgPool, session_id: Uuid, texts: &[&str]) -> Vec<AssistantSample> {
        for (index, text) in texts.iter().enumerate() {
            assistant_row(pool, session_id, text, index as f64).await;
        }
        recent_assistant_window(pool, session_id, EXPRESSION_REVIEW_WINDOW_TURNS as i64)
            .await
            .unwrap()
    }

    async fn seed_exemplar(
        pool: &PgPool,
        context: ReviewContext,
        text: &str,
        tags: &[&str],
    ) -> Uuid {
        ExpressionExemplarRepo { pool }
            .insert(&ExpressionExemplarInsert {
                user_id: context.user_id,
                instance_id: context.instance_id,
                character_id: context.character_id,
                raw_text: text.to_string(),
                tags: tags.iter().map(|tag| tag.to_string()).collect(),
                source_session_id: Some(context.session_id),
                source_message_id: None,
                source_turn: Some(1),
            })
            .await
            .unwrap()
            .expect("a fresh row")
    }

    fn review(
        status: ExpressionStatus,
        tags: &[&str],
        picks: &[ExemplarPick],
    ) -> ExpressionReviewOutput {
        ExpressionReviewOutput {
            status,
            severity: if status == ExpressionStatus::Stable {
                0.05
            } else {
                0.7
            },
            signals: vec!["recent window is repetitive".into()],
            target_tags: tags.iter().map(|tag| tag.to_string()).collect(),
            exemplar_picks: picks.to_vec(),
        }
    }

    fn pick(index: usize, tags: &[&str]) -> ExemplarPick {
        ExemplarPick {
            index,
            tags: tags.iter().map(|tag| tag.to_string()).collect(),
        }
    }

    /// TEST 1 -- Stable. A window that still sounds like the character must not
    /// start recovery and must not inject anything, but it *does* teach the
    /// pool: that is how a character ever accumulates a stable history.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn test_1_stable_windows_feed_the_pool_without_starting_recovery(pool: PgPool) {
        let (user_id, instance_id, character_id, session_id) = fixture(&pool).await;
        let context = ReviewContext {
            session_id,
            user_id,
            instance_id,
            character_id,
            turn: 12,
        };
        let window = window_of(
            &pool,
            session_id,
            &[IN_CHARACTER_ONE, IN_CHARACTER_TWO, IN_CHARACTER_THREE],
        )
        .await;

        let applied = apply_review(
            &pool,
            context,
            &window,
            &review(ExpressionStatus::Stable, &["casual"], &[pick(1, &["care"])]),
        )
        .await;

        assert_eq!(applied.action, RecoveryAction::Idle);
        assert!(!applied.active, "stable must not start recovery");
        assert!(applied.injected_exemplar_ids.is_empty());
        assert_eq!(
            applied.saved_exemplars.len(),
            3,
            "the model pick leads and the newest eligible tail fills the per-window cap"
        );
        // The pick is the one the model nominated; the rest are the newest
        // eligible replies. Nothing outside this window is ever stored.
        let stored: Vec<String> =
            sqlx::query_scalar("SELECT raw_text FROM engine.expression_exemplars")
                .fetch_all(&pool)
                .await
                .unwrap();
        assert!(stored.contains(&IN_CHARACTER_ONE.to_string()));
        assert!(stored.contains(&IN_CHARACTER_TWO.to_string()));
        assert!(stored.contains(&IN_CHARACTER_THREE.to_string()));

        let prompt =
            load_for_prompt(&pool, session_id, user_id, instance_id, character_id, 12).await;
        assert!(prompt.block.is_none(), "nothing is injected while inactive");
        assert!(!prompt.active);
    }

    /// TEST 2 -- Collapse. Same character, now monotonous: recovery starts and
    /// the character's own stable history is what gets injected.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn test_2_collapse_starts_recovery_and_injects_own_exemplars(pool: PgPool) {
        let (user_id, instance_id, character_id, session_id) = fixture(&pool).await;
        let context = ReviewContext {
            session_id,
            user_id,
            instance_id,
            character_id,
            turn: 24,
        };
        for (text, tags) in [
            (IN_CHARACTER_ONE, &["care"][..]),
            (IN_CHARACTER_TWO, &["conflict"][..]),
            (IN_CHARACTER_THREE, &["confrontation"][..]),
        ] {
            seed_exemplar(&pool, context, text, tags).await;
        }
        let collapsed = vec![
            "啧，你行不行。".to_string(),
            "啧，又来了。".to_string(),
            "啧，真麻烦。".to_string(),
        ];
        let window: Vec<AssistantSample> = collapsed
            .iter()
            .enumerate()
            .map(|(index, text)| AssistantSample {
                id: Uuid::from_u128(index as u128 + 1),
                content: text.clone(),
            })
            .collect();

        let applied = apply_review(
            &pool,
            context,
            &window,
            &review(ExpressionStatus::Collapse, &["conflict"], &[]),
        )
        .await;

        assert_eq!(applied.action, RecoveryAction::Start);
        assert!(applied.active);
        assert!(
            (2..=3).contains(&applied.injected_exemplar_ids.len()),
            "2-3 exemplars, never more"
        );
        assert!(
            applied.saved_exemplars.is_empty(),
            "a collapse teaches nothing"
        );

        let prompt =
            load_for_prompt(&pool, session_id, user_id, instance_id, character_id, 24).await;
        let block = prompt.block.expect("recovery injects a reference block");
        assert!(block.starts_with("[expression_reference]\n"));
        assert!(block.contains(IN_CHARACTER_ONE) || block.contains(IN_CHARACTER_TWO));
        // TEST 6, through the real prompt path rather than the renderer alone.
        for required in [
            "Do not copy wording.",
            "不要复制原句",
            "不要复刻原事件",
            "不要机械放大显著特征",
        ] {
            assert!(block.contains(required), "missing {required} in:\n{block}");
        }
        assert_eq!(prompt.injected, applied.injected_exemplar_ids.len());
    }

    /// TEST 3 -- Drift. Same trigger, different failure mode: a character that
    /// has turned polite and explanatory is pulled back toward its own
    /// roughness, not toward a generic personality.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn test_3_drift_starts_recovery(pool: PgPool) {
        let (user_id, instance_id, character_id, session_id) = fixture(&pool).await;
        let context = ReviewContext {
            session_id,
            user_id,
            instance_id,
            character_id,
            turn: 36,
        };
        seed_exemplar(&pool, context, IN_CHARACTER_ONE, &["care"]).await;
        seed_exemplar(&pool, context, IN_CHARACTER_TWO, &["conflict"]).await;
        let window = window_of(&pool, session_id, &[DRIFTY, DRIFTY, DRIFTY]).await;

        let applied = apply_review(
            &pool,
            context,
            &window,
            &review(ExpressionStatus::Drift, &["care"], &[]),
        )
        .await;

        assert_eq!(applied.action, RecoveryAction::Start);
        assert!(applied.active);
        assert_eq!(applied.injected_exemplar_ids.len(), 2);

        let prompt =
            load_for_prompt(&pool, session_id, user_id, instance_id, character_id, 36).await;
        let block = prompt.block.expect("drift injects a reference block");
        assert!(block.contains(IN_CHARACTER_ONE));
        assert!(
            !block.contains("当然可以呀"),
            "the drifted text is never a reference"
        );
    }

    /// TEST 4 -- Character isolation. Two characters share one relationship;
    /// only the collapsing character's own stable history may be retrieved.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn test_4_exemplars_never_cross_characters(pool: PgPool) {
        let (user_id, instance_id, peijin, session_id) = fixture(&pool).await;
        let luyanzhou = Uuid::new_v4();
        let peijin_context = ReviewContext {
            session_id,
            user_id,
            instance_id,
            character_id: peijin,
            turn: 48,
        };
        let luyanzhou_context = ReviewContext {
            character_id: luyanzhou,
            ..peijin_context
        };
        let peijin_exemplar =
            seed_exemplar(&pool, peijin_context, IN_CHARACTER_TWO, &["conflict"]).await;
        let luyanzhou_exemplar =
            seed_exemplar(&pool, luyanzhou_context, IN_CHARACTER_THREE, &["conflict"]).await;
        assert_ne!(peijin_exemplar, luyanzhou_exemplar);

        let window = window_of(&pool, session_id, &["啧，你行不行。", "啧，又来了。"]).await;
        let applied = apply_review(
            &pool,
            peijin_context,
            &window,
            &review(ExpressionStatus::Collapse, &["conflict"], &[]),
        )
        .await;

        assert!(applied.active);
        assert!(
            applied.injected_exemplar_ids.contains(&peijin_exemplar),
            "only the collapsing character's pool is reachable"
        );
        assert!(
            !applied.injected_exemplar_ids.contains(&luyanzhou_exemplar),
            "another character's exemplar must never be retrieved"
        );

        let block = load_for_prompt(&pool, session_id, user_id, instance_id, peijin, 48)
            .await
            .block
            .expect("block renders");
        assert!(block.contains(IN_CHARACTER_TWO));
        assert!(
            !block.contains(IN_CHARACTER_THREE),
            "陆衍舟's exemplar must not reach 裴烬's prompt:\n{block}"
        );
    }

    /// TEST 5 -- Recovery release. Two consecutive stable reviews end it and
    /// the next prompt carries no reference block; a single stable review does
    /// not, which is what stops the block flapping.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn test_5_two_stable_reviews_release_recovery(pool: PgPool) {
        let (user_id, instance_id, character_id, session_id) = fixture(&pool).await;
        let context = ReviewContext {
            session_id,
            user_id,
            instance_id,
            character_id,
            turn: 60,
        };
        seed_exemplar(&pool, context, IN_CHARACTER_ONE, &["care"]).await;
        seed_exemplar(&pool, context, IN_CHARACTER_TWO, &["conflict"]).await;
        let window = window_of(&pool, session_id, &[IN_CHARACTER_ONE, IN_CHARACTER_TWO]).await;

        let started = apply_review(
            &pool,
            context,
            &window,
            &review(ExpressionStatus::Collapse, &["conflict"], &[]),
        )
        .await;
        assert_eq!(started.action, RecoveryAction::Start);

        let first_stable = apply_review(
            &pool,
            context,
            &window,
            &review(ExpressionStatus::Stable, &[], &[]),
        )
        .await;
        assert_eq!(first_stable.action, RecoveryAction::Refresh);
        assert!(first_stable.active, "one stable review must not release");
        let mid = load_for_prompt(&pool, session_id, user_id, instance_id, character_id, 60).await;
        assert!(
            mid.block.is_some(),
            "injection continues across the first stable review"
        );

        let second_stable = apply_review(
            &pool,
            context,
            &window,
            &review(ExpressionStatus::Stable, &[], &[]),
        )
        .await;
        assert_eq!(second_stable.action, RecoveryAction::End);
        assert!(!second_stable.active);

        let after =
            load_for_prompt(&pool, session_id, user_id, instance_id, character_id, 60).await;
        assert!(after.block.is_none(), "recovery ended: no reference block");
        assert!(after.exemplar_ids.is_empty());
    }

    /// TEST 7, program side: the prompt read is a pure read, and a character
    /// that has never been reviewed leaves the rest of the pipeline untouched.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn test_7_unreviewed_character_is_read_only_and_untouched(pool: PgPool) {
        let (user_id, instance_id, character_id, session_id) = fixture(&pool).await;
        assistant_row(&pool, session_id, IN_CHARACTER_ONE, 0.0).await;
        let count = |table: &'static str| {
            let pool = pool.clone();
            async move {
                sqlx::query_scalar::<_, i64>(&format!("SELECT count(*) FROM {table}"))
                    .fetch_one(&pool)
                    .await
                    .unwrap()
            }
        };
        let before = (
            count("engine.expression_recovery_state").await,
            count("engine.expression_exemplars").await,
            count("engine.chat_messages").await,
            count("engine.recent_episodes").await,
            count("engine.world_facts").await,
            count("engine.event_edges").await,
        );

        let prompt =
            load_for_prompt(&pool, session_id, user_id, instance_id, character_id, 1).await;
        assert!(prompt.block.is_none());
        assert!(!prompt.active);
        assert_eq!(prompt, ExpressionRecoveryPromptContext::default());

        let after = (
            count("engine.expression_recovery_state").await,
            count("engine.expression_exemplars").await,
            count("engine.chat_messages").await,
            count("engine.recent_episodes").await,
            count("engine.world_facts").await,
            count("engine.event_edges").await,
        );
        assert_eq!(
            before, after,
            "the reply-path read writes nothing, and no neighbouring store is touched"
        );
    }

    /// The cadence sees only the character's own replies: ghosts, truncations,
    /// the engine's empty-response fallback, blank rows, other channels and the
    /// user's own turns are all invisible to it.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn the_cadence_counts_only_eligible_assistant_turns(pool: PgPool) {
        let (user_id, instance_id, _character_id, session_id) = fixture(&pool).await;
        let _ = (user_id, instance_id);
        for index in 0..12 {
            assistant_row(&pool, session_id, IN_CHARACTER_ONE, index as f64).await;
        }
        sqlx::query(
            "INSERT INTO engine.chat_messages (session_id, role, content, ghost_decision) \
             VALUES ($1, 'assistant', $2, true)",
        )
        .bind(session_id)
        .bind(IN_CHARACTER_ONE)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO engine.chat_messages (session_id, role, content, truncated) \
             VALUES ($1, 'assistant', $2, true)",
        )
        .bind(session_id)
        .bind(IN_CHARACTER_ONE)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO engine.chat_messages (session_id, role, content) \
             VALUES ($1, 'assistant', $2), ($1, 'assistant', '   '), ($1, 'user', 'hello')",
        )
        .bind(session_id)
        .bind(crate::pipeline::stream::EMPTY_RESPONSE_FALLBACK_TEXT)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO engine.chat_messages (session_id, role, content, channel) \
             VALUES ($1, 'assistant', $2, 'voice')",
        )
        .bind(session_id)
        .bind(IN_CHARACTER_TWO)
        .execute(&pool)
        .await
        .unwrap();

        assert_eq!(
            assistant_turn_counts(&pool, session_id, None)
                .await
                .unwrap(),
            (12, 12)
        );
        let window = recent_assistant_window(&pool, session_id, 12)
            .await
            .unwrap();
        assert_eq!(window.len(), 12);
        assert!(window.iter().all(|row| row.content == IN_CHARACTER_ONE));
        // Oldest first: the window is chronological, not newest-first.
        assert!(window[0].id != window[11].id);
    }

    /// The watermark is what makes the review low-frequency: only turns after
    /// the last review count toward the next one.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn the_watermark_excludes_already_reviewed_turns(pool: PgPool) {
        let (_user_id, _instance_id, _character_id, session_id) = fixture(&pool).await;
        for index in 0..12 {
            assistant_row(&pool, session_id, IN_CHARACTER_ONE, index as f64).await;
        }
        let watermark = Utc::now() + chrono::Duration::seconds(5);
        let (since, total) = assistant_turn_counts(&pool, session_id, Some(watermark))
            .await
            .unwrap();
        assert_eq!(total, 12);
        assert_eq!(since, 6, "offsets 6..12 are the only unseen turns");
    }

    /// A review that reaches no conclusion still advances the watermark, and
    /// changes nothing else -- so an outage costs one window, not one call per
    /// turn.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_failed_review_records_the_attempt_and_keeps_recovery(pool: PgPool) {
        let (user_id, instance_id, character_id, session_id) = fixture(&pool).await;
        let context = ReviewContext {
            session_id,
            user_id,
            instance_id,
            character_id,
            turn: 72,
        };
        seed_exemplar(&pool, context, IN_CHARACTER_TWO, &["conflict"]).await;
        let window = window_of(&pool, session_id, &["啧，你行不行。"]).await;
        let started = apply_review(
            &pool,
            context,
            &window,
            &review(ExpressionStatus::Collapse, &["conflict"], &[]),
        )
        .await;
        assert!(started.active);

        let repo = ExpressionRecoveryRepo { pool: &pool };
        let before = repo
            .load(user_id, instance_id, character_id)
            .await
            .unwrap()
            .unwrap();
        record_attempt(&pool, context, Some(&before)).await;
        let after = repo
            .load(user_id, instance_id, character_id)
            .await
            .unwrap()
            .unwrap();

        assert_eq!(after.active, before.active);
        assert_eq!(after.consecutive_stable, before.consecutive_stable);
        assert_eq!(after.last_status, before.last_status);
        assert_eq!(after.active_exemplar_ids, before.active_exemplar_ids);
        assert!(
            after.last_reviewed_at > before.last_reviewed_at,
            "the attempt advances the low-frequency watermark"
        );
    }
}
