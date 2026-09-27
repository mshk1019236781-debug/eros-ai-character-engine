// SPDX-License-Identifier: AGPL-3.0-only
//! Runtime half of the V1 world/entity facts.
//!
//! Two seams, both model-free:
//!
//! ```text
//! main RP completion
//!   ↓  reply  +  memory trailer {event?, world_facts[]}   (one call, unchanged)
//! write_candidates                                     ← this file
//!   ↓  engine.world_facts  (dedupe / supersede)
//! load_for_prompt
//!   ↓  scope filter → relevance filter → budget → [world_facts] block
//! EROS prompt assembly
//! ```
//!
//! Facts are not events: nothing here touches `recent_episodes`,
//! `event_edges`, the recall scorer, the recall controller or the graph
//! expansion. The write path is a second adapter beside
//! `memory_adapter::MemoryWriteAdapter`, not a change to it.

use eros_engine_core::event_memory::knowledge_scope_allows;
use eros_engine_core::world_fact::{
    render_world_facts, world_fact_is_relevant, WorldFactCandidate,
    DEFAULT_MAX_WORLD_FACTS_IN_PROMPT, MAX_WORLD_FACTS_PER_METADATA, WORLD_FACT_SCAN_LIMIT,
};
use eros_engine_store::world_fact::{WorldFactInsert, WorldFactRepo, WorldFactWriteOutcome};
use sqlx::PgPool;
use uuid::Uuid;

/// The non-model context for one write. Same shape — and the same reasoning —
/// as `MemoryWriteContext`: a model that could choose its own ids could file a
/// fact into another story.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorldFactWriteContext {
    pub user_id: Uuid,
    pub instance_id: Uuid,
    pub session_id: Uuid,
    pub source_message_id: Option<Uuid>,
    pub source_turn: Option<i32>,
}

/// What one prompt build ended up doing with the fact store.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorldFactPromptContext {
    pub block: Option<String>,
    /// Active facts in scope for this user/instance before any filtering.
    pub active: usize,
    /// Facts this viewer is not allowed to see (requirement C).
    pub scope_blocked: usize,
    /// Facts that survived scope + relevance and reached the prompt.
    pub injected: usize,
    pub fact_ids: Vec<Uuid>,
}

/// Store every fact candidate the main RP model emitted this turn.
///
/// Fail-open per fact, never per turn: a rejected candidate is logged and
/// skipped, and the event written beside it is untouched. No model and no
/// embedding call happens here — a fact has no vector by design.
pub async fn write_candidates(
    pool: &PgPool,
    context: &WorldFactWriteContext,
    candidates: &[WorldFactCandidate],
) {
    if candidates.is_empty() {
        return;
    }
    let repo = WorldFactRepo { pool };
    for candidate in candidates.iter().take(MAX_WORLD_FACTS_PER_METADATA) {
        let mut candidate = candidate.clone();
        candidate.normalize();
        if let Err(reason) = candidate.validate() {
            tracing::warn!(
                session_id = %context.session_id,
                instance_id = %context.instance_id,
                turn = ?context.source_turn,
                subject = %candidate.subject,
                predicate = %candidate.predicate,
                object = %candidate.object,
                error = %reason,
                "WORLD_FACT_REJECTED"
            );
            continue;
        }

        let insert = WorldFactInsert {
            user_id: context.user_id,
            instance_id: context.instance_id,
            subject: candidate.subject.clone(),
            predicate: candidate.predicate.clone(),
            object: candidate.object.clone(),
            fact_type: candidate.fact_type,
            statement: candidate.statement.clone(),
            knowledge_scope: candidate.knowledge_scope.clone(),
            source_message_id: context.source_message_id,
            source_turn: context.source_turn,
        };
        match repo.upsert(&insert).await {
            Ok(WorldFactWriteOutcome::Created { fact_id }) => tracing::info!(
                session_id = %context.session_id,
                instance_id = %context.instance_id,
                turn = ?context.source_turn,
                fact_id = %fact_id,
                subject = %insert.subject,
                predicate = %insert.predicate,
                object = %insert.object,
                fact_type = insert.fact_type.as_str(),
                knowledge_scope = ?insert.knowledge_scope,
                "WORLD_FACT_CREATED"
            ),
            Ok(WorldFactWriteOutcome::Deduped { fact_id }) => tracing::info!(
                session_id = %context.session_id,
                instance_id = %context.instance_id,
                turn = ?context.source_turn,
                fact_id = %fact_id,
                subject = %insert.subject,
                predicate = %insert.predicate,
                object = %insert.object,
                "WORLD_FACT_DEDUPED"
            ),
            Ok(WorldFactWriteOutcome::Updated {
                fact_id,
                replaced_fact_id,
            }) => tracing::info!(
                session_id = %context.session_id,
                instance_id = %context.instance_id,
                turn = ?context.source_turn,
                fact_id = %fact_id,
                replaced_fact_id = %replaced_fact_id,
                subject = %insert.subject,
                predicate = %insert.predicate,
                object = %insert.object,
                "WORLD_FACT_UPDATED"
            ),
            Err(error) => tracing::warn!(
                session_id = %context.session_id,
                instance_id = %context.instance_id,
                subject = %insert.subject,
                error = %error,
                "WORLD_FACT_WRITE failed"
            ),
        }
    }
}

/// Read the facts this turn is allowed to see, and render the prompt block.
///
/// Order matters and is the whole of requirement C and E:
///
/// 1. **scope** — a fact whose `knowledge_scope` does not name the viewer is
///    dropped here, before rendering. The model is never asked to keep a
///    secret; it never receives one.
/// 2. **relevance** — a fact earns prompt budget only by naming someone in the
///    current scene or by being mentioned in what the user just said.
/// 3. **budget** — at most [`DEFAULT_MAX_WORLD_FACTS_IN_PROMPT`] facts, however
///    many are stored.
///
/// Never returns an error: a database hiccup omits the block, which is the safe
/// direction (the reply simply cannot cite a fact it was not given).
pub async fn load_for_prompt(
    pool: &PgPool,
    user_id: Uuid,
    instance_id: Uuid,
    session_id: Uuid,
    viewer: Option<&str>,
    participants: &[String],
    query_text: &str,
    // Telemetry only, and `u32` because it comes straight from the recall
    // controller's turn counter. Nothing in the selection logic reads it.
    turn: u32,
) -> WorldFactPromptContext {
    let repo = WorldFactRepo { pool };
    let rows = match repo
        .list_active(user_id, instance_id, WORLD_FACT_SCAN_LIMIT)
        .await
    {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(
                error = %error,
                session_id = %session_id,
                instance_id = %instance_id,
                "WORLD_FACT_RETRIEVED failed; [world_facts] omitted"
            );
            return WorldFactPromptContext::default();
        }
    };
    let active = rows.len();

    let mut scope_blocked = Vec::new();
    let mut relevant = Vec::new();
    for row in rows {
        if !knowledge_scope_allows(&row.knowledge_scope, viewer) {
            scope_blocked.push(row);
            continue;
        }
        if !world_fact_is_relevant(&row.subject, &row.object, viewer, participants, query_text) {
            continue;
        }
        relevant.push(row);
    }

    tracing::info!(
        session_id = %session_id,
        instance_id = %instance_id,
        turn,
        viewer = viewer.unwrap_or("<public>"),
        active_count = active,
        scope_blocked_count = scope_blocked.len(),
        relevant_count = relevant.len(),
        "WORLD_FACT_RETRIEVED"
    );
    for row in &scope_blocked {
        tracing::info!(
            session_id = %session_id,
            instance_id = %instance_id,
            turn,
            viewer = viewer.unwrap_or("<public>"),
            fact_id = %row.id,
            fact_subject = %row.subject,
            fact_knowledge_scope = ?row.knowledge_scope,
            "WORLD_FACT_SCOPE_BLOCKED"
        );
    }

    let selected: Vec<_> = relevant
        .into_iter()
        .take(DEFAULT_MAX_WORLD_FACTS_IN_PROMPT)
        .collect();
    if selected.is_empty() {
        return WorldFactPromptContext {
            block: None,
            active,
            scope_blocked: scope_blocked.len(),
            injected: 0,
            fact_ids: Vec::new(),
        };
    }

    let statements: Vec<String> = selected.iter().map(|row| row.statement_text()).collect();
    let fact_ids: Vec<Uuid> = selected.iter().map(|row| row.id).collect();
    let block = render_world_facts(&statements);
    tracing::info!(
        session_id = %session_id,
        instance_id = %instance_id,
        turn,
        viewer = viewer.unwrap_or("<public>"),
        injected = fact_ids.len(),
        fact_ids = ?fact_ids,
        "WORLD_FACT_PROMPT_INJECT"
    );

    WorldFactPromptContext {
        block,
        active,
        scope_blocked: scope_blocked.len(),
        injected: fact_ids.len(),
        fact_ids,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routes::companion::testutil::seed_persona_instance;
    use eros_engine_core::world_fact::FactType;

    fn candidate(
        subject: &str,
        predicate: &str,
        object: &str,
        fact_type: FactType,
        scope: &[&str],
    ) -> WorldFactCandidate {
        WorldFactCandidate {
            subject: subject.to_string(),
            predicate: predicate.to_string(),
            object: object.to_string(),
            fact_type,
            statement: Some(format!("{subject}是{object}")),
            knowledge_scope: scope.iter().map(|value| value.to_string()).collect(),
        }
    }

    async fn fixture(pool: &PgPool) -> (Uuid, Uuid, Uuid) {
        let user_id = Uuid::new_v4();
        let instance_id = seed_persona_instance(pool, user_id).await;
        let session_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id, instance_id) VALUES ($1, $2) RETURNING id",
        )
        .bind(user_id)
        .bind(instance_id)
        .fetch_one(pool)
        .await
        .unwrap();
        (user_id, instance_id, session_id)
    }

    fn context(user_id: Uuid, instance_id: Uuid, session_id: Uuid) -> WorldFactWriteContext {
        WorldFactWriteContext {
            user_id,
            instance_id,
            session_id,
            source_message_id: None,
            source_turn: Some(3),
        }
    }

    /// Requirement: a stable relationship stated twice is stored once.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_restated_fact_is_stored_once(pool: PgPool) {
        let (user_id, instance_id, session_id) = fixture(&pool).await;
        let context = context(user_id, instance_id, session_id);
        let fact = candidate("白远舟", "的哥哥", "白芷", FactType::Relationship, &[]);

        write_candidates(&pool, &context, std::slice::from_ref(&fact)).await;
        write_candidates(&pool, &context, std::slice::from_ref(&fact)).await;

        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM engine.world_facts")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 1);
    }

    /// Requirement C: a fact scoped to 白芷/温景行 must not reach a 陆衍舟 POV,
    /// and the block it would have carried must not be rendered at all.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_scoped_fact_is_invisible_to_another_viewer(pool: PgPool) {
        let (user_id, instance_id, session_id) = fixture(&pool).await;
        let context = context(user_id, instance_id, session_id);
        write_candidates(
            &pool,
            &context,
            &[candidate(
                "白远舟",
                "的哥哥",
                "白芷",
                FactType::Relationship,
                &["白芷", "温景行"],
            )],
        )
        .await;

        let participants = vec!["陆衍舟".to_string()];
        let blocked = load_for_prompt(
            &pool,
            user_id,
            instance_id,
            session_id,
            Some("陆衍舟"),
            &participants,
            "白远舟最近怎么样？",
            4,
        )
        .await;
        assert_eq!(blocked.active, 1);
        assert_eq!(blocked.scope_blocked, 1);
        assert_eq!(blocked.injected, 0);
        assert!(blocked.block.is_none());

        let allowed = load_for_prompt(
            &pool,
            user_id,
            instance_id,
            session_id,
            Some("白芷"),
            &participants,
            "白远舟最近怎么样？",
            4,
        )
        .await;
        assert_eq!(allowed.scope_blocked, 0);
        assert_eq!(allowed.injected, 1);
        let block = allowed.block.expect("allowed viewer gets the block");
        assert!(block.starts_with("[world_facts]\n"));
        assert!(block.contains("白远舟是白芷"));
    }

    /// Requirement E: a fact unrelated to the scene and unmentioned in the turn
    /// stays out of the prompt even for a viewer who may see it.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn an_unrelated_fact_is_not_injected(pool: PgPool) {
        let (user_id, instance_id, session_id) = fixture(&pool).await;
        let context = context(user_id, instance_id, session_id);
        write_candidates(
            &pool,
            &context,
            &[candidate(
                "白远舟",
                "的哥哥",
                "白芷",
                FactType::Relationship,
                &[],
            )],
        )
        .await;

        let result = load_for_prompt(
            &pool,
            user_id,
            instance_id,
            session_id,
            Some("裴烬"),
            &["裴烬".to_string()],
            "今天天气不错",
            4,
        )
        .await;
        assert_eq!(result.active, 1);
        assert_eq!(result.injected, 0);
        assert!(result.block.is_none());
    }

    /// Requirement B, program side: a waiter is not an entity, so nothing is
    /// stored even though the model proposed it.
    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn a_transient_role_is_rejected_before_storage(pool: PgPool) {
        let (user_id, instance_id, session_id) = fixture(&pool).await;
        let context = context(user_id, instance_id, session_id);
        write_candidates(
            &pool,
            &context,
            &[candidate("服务员", "倒了", "水", FactType::Identity, &[])],
        )
        .await;

        let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM engine.world_facts")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(rows, 0);
    }
}
