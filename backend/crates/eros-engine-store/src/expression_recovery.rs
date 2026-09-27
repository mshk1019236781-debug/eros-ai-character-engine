// SPDX-License-Identifier: AGPL-3.0-only
//! Persistence for Expression Recovery V1 (0066).
//!
//! Two repos over two tables, both model-free:
//!
//! * [`ExpressionExemplarRepo`] -- the per-character reservoir of stable
//!   expression. Rows are raw persisted replies, never summaries.
//! * [`ExpressionRecoveryRepo`] -- the thin per-relationship flag saying
//!   whether that reservoir is currently being injected.
//!
//! Nothing here calls a model, an embedding service, or the Event layer. A
//! candidate arrives already selected by the Review model, and the text is
//! copied out of `engine.chat_messages` unmodified.

use chrono::{DateTime, Utc};
use eros_engine_core::expression_recovery::{
    exemplar_text_key, ExpressionStatus, MAX_ACTIVE_EXEMPLARS,
};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

const EXEMPLAR_COLUMNS: &str = "id, user_id, instance_id, character_id, raw_text, tags, \
     source_session_id, source_message_id, source_turn, is_active, used_count, last_used_at, \
     created_at";

/// One exemplar as stored. `raw_text` is what the prompt renders.
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct ExpressionExemplarRow {
    pub id: Uuid,
    pub user_id: Uuid,
    pub instance_id: Uuid,
    pub character_id: Uuid,
    pub raw_text: String,
    pub tags: Vec<String>,
    pub source_session_id: Option<Uuid>,
    pub source_message_id: Option<Uuid>,
    pub source_turn: Option<i32>,
    pub is_active: bool,
    pub used_count: i32,
    pub last_used_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

/// One candidate on its way into the pool. `user_id`/`instance_id` are supplied
/// by the engine, never by a model: a candidate that chose its own tenancy
/// could file a sample into someone else's story.
#[derive(Debug, Clone, PartialEq)]
pub struct ExpressionExemplarInsert {
    pub user_id: Uuid,
    pub instance_id: Uuid,
    pub character_id: Uuid,
    pub raw_text: String,
    pub tags: Vec<String>,
    pub source_session_id: Option<Uuid>,
    pub source_message_id: Option<Uuid>,
    pub source_turn: Option<i32>,
}

pub struct ExpressionExemplarRepo<'a> {
    pub pool: &'a PgPool,
}

impl ExpressionExemplarRepo<'_> {
    /// Insert one candidate. Returns `Some(id)` when a row was created and
    /// `None` when this exact reply is already active for the character.
    ///
    /// The dedupe key is `exemplar_text_key(raw_text)` -- the whitespace-
    /// collapsed copy -- so re-observing the same reply with different line
    /// breaks touches nothing. `raw_text` itself is stored untouched, because
    /// the prompt must render exactly what the model wrote.
    pub async fn insert(
        &self,
        insert: &ExpressionExemplarInsert,
    ) -> Result<Option<Uuid>, sqlx::Error> {
        let key = exemplar_text_key(&insert.raw_text);
        sqlx::query_scalar(
            "INSERT INTO engine.expression_exemplars \
             (user_id, instance_id, character_id, raw_text, text_key, tags, \
              source_session_id, source_message_id, source_turn) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9) \
             ON CONFLICT (user_id, instance_id, character_id, text_key) WHERE is_active \
             DO NOTHING \
             RETURNING id",
        )
        .bind(insert.user_id)
        .bind(insert.instance_id)
        .bind(insert.character_id)
        .bind(&insert.raw_text)
        .bind(&key)
        .bind(&insert.tags)
        .bind(insert.source_session_id)
        .bind(insert.source_message_id)
        .bind(insert.source_turn)
        .fetch_optional(self.pool)
        .await
    }

    /// This character's active pool, newest first.
    ///
    /// Every filter is applied in SQL: a caller cannot accidentally widen the
    /// scope by passing a narrower tag list, because tenancy and identity are
    /// not parameters a caller can omit.
    pub async fn list_pool(
        &self,
        user_id: Uuid,
        instance_id: Uuid,
        character_id: Uuid,
        limit: i64,
    ) -> Result<Vec<ExpressionExemplarRow>, sqlx::Error> {
        sqlx::query_as(&format!(
            "SELECT {EXEMPLAR_COLUMNS} FROM engine.expression_exemplars \
             WHERE user_id = $1 AND instance_id = $2 AND character_id = $3 AND is_active \
             ORDER BY created_at DESC, id DESC \
             LIMIT $4"
        ))
        .bind(user_id)
        .bind(instance_id)
        .bind(character_id)
        .bind(limit.clamp(1, MAX_ACTIVE_EXEMPLARS * 4))
        .fetch_all(self.pool)
        .await
    }

    pub async fn count_active(
        &self,
        user_id: Uuid,
        instance_id: Uuid,
        character_id: Uuid,
    ) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT count(*) FROM engine.expression_exemplars \
             WHERE user_id = $1 AND instance_id = $2 AND character_id = $3 AND is_active",
        )
        .bind(user_id)
        .bind(instance_id)
        .bind(character_id)
        .fetch_one(self.pool)
        .await
    }

    /// Deactivate everything past the newest `MAX_ACTIVE_EXEMPLARS` rows.
    ///
    /// This is what keeps the pool a reservoir rather than an archive, and it
    /// is the reason a long-running character never accumulates an unbounded
    /// number of injectable rows. Returns how many rows were deactivated.
    pub async fn trim_to_cap(
        &self,
        user_id: Uuid,
        instance_id: Uuid,
        character_id: Uuid,
    ) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            "UPDATE engine.expression_exemplars SET is_active = false \
             WHERE id IN ( \
                 SELECT id FROM engine.expression_exemplars \
                 WHERE user_id = $1 AND instance_id = $2 AND character_id = $3 AND is_active \
                 ORDER BY created_at DESC, id DESC \
                 OFFSET $4 \
             )",
        )
        .bind(user_id)
        .bind(instance_id)
        .bind(character_id)
        .bind(MAX_ACTIVE_EXEMPLARS)
        .execute(self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Read-side bookkeeping: which exemplars a recovery turn actually used.
    /// Nothing in selection reads these back today; they exist so an operator
    /// can tell a stale pool from an unused one.
    pub async fn mark_used(&self, ids: &[Uuid]) -> Result<(), sqlx::Error> {
        if ids.is_empty() {
            return Ok(());
        }
        sqlx::query(
            "UPDATE engine.expression_exemplars \
             SET used_count = used_count + 1, last_used_at = now() \
             WHERE id = ANY($1)",
        )
        .bind(ids)
        .execute(self.pool)
        .await?;
        Ok(())
    }
}

/// The recovery flag for one (relationship, character).
#[derive(Debug, Clone, PartialEq, FromRow)]
pub struct ExpressionRecoveryStateRow {
    pub user_id: Uuid,
    pub instance_id: Uuid,
    pub character_id: Uuid,
    pub active: bool,
    pub consecutive_stable: i32,
    pub last_status: Option<String>,
    pub last_severity: Option<f64>,
    pub last_reviewed_at: Option<DateTime<Utc>>,
    pub started_at: Option<DateTime<Utc>>,
    pub active_exemplar_ids: Vec<Uuid>,
    pub target_tags: Vec<String>,
    pub updated_at: DateTime<Utc>,
}

impl ExpressionRecoveryStateRow {
    pub fn status(&self) -> Option<ExpressionStatus> {
        match self.last_status.as_deref() {
            Some("stable") => Some(ExpressionStatus::Stable),
            Some("drift") => Some(ExpressionStatus::Drift),
            Some("collapse") => Some(ExpressionStatus::Collapse),
            _ => None,
        }
    }

    pub fn consecutive_stable(&self) -> u32 {
        self.consecutive_stable.max(0) as u32
    }
}

/// One full write of the recovery row. `last_reviewed_at` and `updated_at` are
/// the database's; everything that decides behaviour is here.
#[derive(Debug, Clone, PartialEq)]
pub struct ExpressionRecoveryStateWrite {
    pub user_id: Uuid,
    pub instance_id: Uuid,
    pub character_id: Uuid,
    pub active: bool,
    pub consecutive_stable: u32,
    /// `None` means "reviewed, no conclusion was reached" -- a provider error,
    /// a timeout or unparseable output. The row is still written so the
    /// low-frequency watermark advances; the previous verdict is not
    /// overwritten by the absence of a new one.
    pub last_status: Option<ExpressionStatus>,
    pub last_severity: f64,
    /// `Some` only on the review that started the current recovery; preserved
    /// across refreshes by the upsert.
    pub started_at: Option<DateTime<Utc>>,
    pub active_exemplar_ids: Vec<Uuid>,
    pub target_tags: Vec<String>,
}

pub struct ExpressionRecoveryRepo<'a> {
    pub pool: &'a PgPool,
}

impl ExpressionRecoveryRepo<'_> {
    pub async fn load(
        &self,
        user_id: Uuid,
        instance_id: Uuid,
        character_id: Uuid,
    ) -> Result<Option<ExpressionRecoveryStateRow>, sqlx::Error> {
        sqlx::query_as(
            "SELECT user_id, instance_id, character_id, active, consecutive_stable, \
                    last_status, last_severity, last_reviewed_at, started_at, \
                    active_exemplar_ids, target_tags, updated_at \
             FROM engine.expression_recovery_state \
             WHERE user_id = $1 AND instance_id = $2 AND character_id = $3",
        )
        .bind(user_id)
        .bind(instance_id)
        .bind(character_id)
        .fetch_optional(self.pool)
        .await
    }

    /// Write the row, creating it if this is the character's first review.
    ///
    /// `started_at` is `COALESCE`-ed with the existing value rather than
    /// overwritten: a refresh mid-recovery must not make it look like recovery
    /// restarted, which would defeat the point of tracking it at all.
    pub async fn save(
        &self,
        write: &ExpressionRecoveryStateWrite,
    ) -> Result<ExpressionRecoveryStateRow, sqlx::Error> {
        sqlx::query_as(
            "INSERT INTO engine.expression_recovery_state \
             (user_id, instance_id, character_id, active, consecutive_stable, last_status, \
              last_severity, last_reviewed_at, started_at, active_exemplar_ids, target_tags, \
              updated_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, now(), $8, $9, $10, now()) \
             ON CONFLICT (user_id, instance_id, character_id) DO UPDATE SET \
                 active = EXCLUDED.active, \
                 consecutive_stable = EXCLUDED.consecutive_stable, \
                 last_status = EXCLUDED.last_status, \
                 last_severity = EXCLUDED.last_severity, \
                 last_reviewed_at = now(), \
                 started_at = COALESCE(EXCLUDED.started_at, \
                                       engine.expression_recovery_state.started_at), \
                 active_exemplar_ids = EXCLUDED.active_exemplar_ids, \
                 target_tags = EXCLUDED.target_tags, \
                 updated_at = now() \
             RETURNING user_id, instance_id, character_id, active, consecutive_stable, \
                       last_status, last_severity, last_reviewed_at, started_at, \
                       active_exemplar_ids, target_tags, updated_at",
        )
        .bind(write.user_id)
        .bind(write.instance_id)
        .bind(write.character_id)
        .bind(write.active)
        .bind(write.consecutive_stable as i32)
        .bind(write.last_status.map(ExpressionStatus::as_str))
        .bind(write.last_severity)
        .bind(write.started_at)
        .bind(&write.active_exemplar_ids)
        .bind(&write.target_tags)
        .fetch_one(self.pool)
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::seed_persona_instance;

    /// One relationship plus the genome id its instance was cloned from. The
    /// genome id is what `character_id` carries in production.
    async fn relationship(pool: &PgPool) -> (Uuid, Uuid, Uuid) {
        let owner = Uuid::new_v4();
        let instance = seed_persona_instance(pool, owner).await;
        let genome: Uuid =
            sqlx::query_scalar("SELECT genome_id FROM engine.persona_instances WHERE id = $1")
                .bind(instance)
                .fetch_one(pool)
                .await
                .unwrap();
        (owner, instance, genome)
    }

    fn exemplar(
        user_id: Uuid,
        instance_id: Uuid,
        character_id: Uuid,
        text: &str,
        tags: &[&str],
    ) -> ExpressionExemplarInsert {
        ExpressionExemplarInsert {
            user_id,
            instance_id,
            character_id,
            raw_text: text.to_string(),
            tags: tags.iter().map(|tag| tag.to_string()).collect(),
            source_session_id: None,
            source_message_id: None,
            source_turn: Some(7),
        }
    }

    /// Requirement: a stable reply observed twice is one exemplar, not two.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_repeated_reply_is_stored_once(pool: PgPool) {
        let (owner, instance, genome) = relationship(&pool).await;
        let repo = ExpressionExemplarRepo { pool: &pool };
        let row = exemplar(
            owner,
            instance,
            genome,
            "啧，少逞强，手给我看看。",
            &["care"],
        );

        let first = repo.insert(&row).await.unwrap();
        assert!(first.is_some());

        // Same reply, different line breaks: the whitespace-collapsed key is
        // what dedupes, so this must not add a row either.
        let mut repeated = row.clone();
        repeated.raw_text = "啧，少逞强，\n手给我看看。".to_string();
        assert!(repo.insert(&repeated).await.unwrap().is_none());

        assert_eq!(repo.count_active(owner, instance, genome).await.unwrap(), 1);
    }

    /// Requirement: 裴烬's exemplars must never be retrievable for 陆衍舟 --
    /// even inside one relationship, and even for the same user.
    #[sqlx::test(migrations = "./migrations")]
    async fn the_pool_is_isolated_per_character(pool: PgPool) {
        let (owner, instance, peijin) = relationship(&pool).await;
        let luyanzhou = Uuid::new_v4();
        let repo = ExpressionExemplarRepo { pool: &pool };
        repo.insert(&exemplar(
            owner,
            instance,
            peijin,
            "这也能叫计划？你想过后果吗。",
            &["conflict"],
        ))
        .await
        .unwrap();
        repo.insert(&exemplar(
            owner,
            instance,
            luyanzhou,
            "先坐下，把今天的事从头说一遍。",
            &["care"],
        ))
        .await
        .unwrap();

        let mine = repo.list_pool(owner, instance, peijin, 12).await.unwrap();
        assert_eq!(mine.len(), 1);
        assert!(mine[0].raw_text.contains("这也能叫计划"));
        assert!(mine.iter().all(|row| row.character_id == peijin));
    }

    /// The same characters in two relationships never share a pool either.
    #[sqlx::test(migrations = "./migrations")]
    async fn the_pool_is_isolated_per_relationship(pool: PgPool) {
        let (owner, instance, genome) = relationship(&pool).await;
        let (other_owner, other_instance, _) = relationship(&pool).await;
        let repo = ExpressionExemplarRepo { pool: &pool };
        repo.insert(&exemplar(
            owner,
            instance,
            genome,
            "你先别急着下结论。",
            &[],
        ))
        .await
        .unwrap();

        let mine = repo.list_pool(owner, instance, genome, 12).await.unwrap();
        let theirs = repo
            .list_pool(other_owner, other_instance, genome, 12)
            .await
            .unwrap();
        assert_eq!(mine.len(), 1);
        assert!(theirs.is_empty());
    }

    /// Requirement: the pool is bounded, and the oldest rows are the ones that
    /// leave it.
    #[sqlx::test(migrations = "./migrations")]
    async fn the_cap_retires_the_oldest_rows(pool: PgPool) {
        let (owner, instance, genome) = relationship(&pool).await;
        let repo = ExpressionExemplarRepo { pool: &pool };
        for index in 0..(MAX_ACTIVE_EXEMPLARS + 3) {
            repo.insert(&exemplar(
                owner,
                instance,
                genome,
                &format!("第{index}条稳定表达样本，长度足够成为 exemplar。"),
                &[],
            ))
            .await
            .unwrap();
        }
        assert_eq!(
            repo.count_active(owner, instance, genome).await.unwrap(),
            MAX_ACTIVE_EXEMPLARS + 3
        );

        let retired = repo.trim_to_cap(owner, instance, genome).await.unwrap();
        assert_eq!(retired, 3);
        assert_eq!(
            repo.count_active(owner, instance, genome).await.unwrap(),
            MAX_ACTIVE_EXEMPLARS
        );

        // The retired rows stay on disk, deactivated rather than deleted.
        let all: i64 = sqlx::query_scalar("SELECT count(*) FROM engine.expression_exemplars")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(all, MAX_ACTIVE_EXEMPLARS + 3);
    }

    /// Requirement: re-adopting a reply that was retired mid-history is
    /// possible -- the unique index is partial on `is_active`.
    #[sqlx::test(migrations = "./migrations")]
    async fn a_retired_reply_can_be_adopted_again(pool: PgPool) {
        let (owner, instance, genome) = relationship(&pool).await;
        let repo = ExpressionExemplarRepo { pool: &pool };
        let row = exemplar(owner, instance, genome, "行，那就按你说的来。", &[]);
        let first = repo.insert(&row).await.unwrap().unwrap();

        sqlx::query("UPDATE engine.expression_exemplars SET is_active = false WHERE id = $1")
            .bind(first)
            .execute(&pool)
            .await
            .unwrap();

        assert!(repo.insert(&row).await.unwrap().is_some());
        assert_eq!(repo.count_active(owner, instance, genome).await.unwrap(), 1);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn recovery_state_round_trips_and_keeps_the_first_start(pool: PgPool) {
        let (owner, instance, genome) = relationship(&pool).await;
        let repo = ExpressionRecoveryRepo { pool: &pool };
        assert!(repo.load(owner, instance, genome).await.unwrap().is_none());

        let exemplar_id = Uuid::new_v4();
        let started = repo
            .save(&ExpressionRecoveryStateWrite {
                user_id: owner,
                instance_id: instance,
                character_id: genome,
                active: true,
                consecutive_stable: 0,
                last_status: Some(ExpressionStatus::Collapse),
                last_severity: 0.7,
                started_at: Some(Utc::now()),
                active_exemplar_ids: vec![exemplar_id],
                target_tags: vec!["conflict".into()],
            })
            .await
            .unwrap();
        assert!(started.active);
        assert_eq!(started.status(), Some(ExpressionStatus::Collapse));
        assert_eq!(started.active_exemplar_ids, vec![exemplar_id]);

        // A later review that does not restate `started_at` must not clear it.
        let refreshed = repo
            .save(&ExpressionRecoveryStateWrite {
                user_id: owner,
                instance_id: instance,
                character_id: genome,
                active: true,
                consecutive_stable: 1,
                last_status: Some(ExpressionStatus::Stable),
                last_severity: 0.1,
                started_at: None,
                active_exemplar_ids: vec![exemplar_id],
                target_tags: vec!["care".into()],
            })
            .await
            .unwrap();
        assert_eq!(refreshed.consecutive_stable(), 1);
        assert_eq!(refreshed.status(), Some(ExpressionStatus::Stable));
        assert_eq!(
            refreshed.started_at, started.started_at,
            "a refresh mid-recovery must not look like a restart"
        );

        let ended = repo
            .save(&ExpressionRecoveryStateWrite {
                user_id: owner,
                instance_id: instance,
                character_id: genome,
                active: false,
                consecutive_stable: 0,
                last_status: Some(ExpressionStatus::Stable),
                last_severity: 0.05,
                started_at: None,
                active_exemplar_ids: vec![],
                target_tags: vec![],
            })
            .await
            .unwrap();
        assert!(!ended.active);
        assert!(ended.active_exemplar_ids.is_empty());
    }
}
