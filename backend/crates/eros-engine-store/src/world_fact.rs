// SPDX-License-Identifier: AGPL-3.0-only
//! Persistence for the V1 world/entity facts (0065).
//!
//! One table, one writer. A world fact is a small, deduplicated statement —
//! `白远舟 是 白芷 的哥哥` — that outlives the sentence which stated it. It is
//! deliberately not an event: no embedding, no edges, no cooldown, no scorer.
//!
//! Nothing here calls a model or an embedding service. Candidates arrive
//! already parsed from the main RP completion's trailer, which is what keeps
//! the "no new LLM call" property intact.

use chrono::{DateTime, Utc};
use eros_engine_core::world_fact::FactType;
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool};
use uuid::Uuid;

/// Every column of a fact row, in the order [`WorldFactRow`] declares them.
const WORLD_FACT_COLUMNS: &str = "id, user_id, instance_id, subject, predicate, object, \
     fact_type, statement, knowledge_scope, source_message_id, source_turn, is_active, \
     created_at, updated_at";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, FromRow)]
pub struct WorldFactRow {
    pub id: Uuid,
    pub user_id: Uuid,
    pub instance_id: Uuid,
    pub subject: String,
    pub predicate: String,
    pub object: String,
    pub fact_type: String,
    pub statement: Option<String>,
    pub knowledge_scope: Vec<String>,
    pub source_message_id: Option<Uuid>,
    pub source_turn: Option<i32>,
    pub is_active: bool,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl WorldFactRow {
    pub fn fact_type(&self) -> Option<FactType> {
        FactType::parse(&self.fact_type)
    }

    /// The line this fact contributes to a prompt.
    pub fn statement_text(&self) -> String {
        match self.statement.as_ref() {
            Some(statement) if !statement.trim().is_empty() => statement.trim().to_string(),
            _ => format!("{}：{}＝{}", self.subject, self.predicate, self.object),
        }
    }
}

/// Values supplied when writing one fact. `id`, `created_at`, `updated_at` and
/// `is_active` are the database's: a caller must not be able to create an
/// already-deactivated fact, and the update path owns `updated_at`.
#[derive(Debug, Clone, PartialEq)]
pub struct WorldFactInsert {
    pub user_id: Uuid,
    pub instance_id: Uuid,
    pub subject: String,
    pub predicate: String,
    pub object: String,
    pub fact_type: FactType,
    pub statement: Option<String>,
    pub knowledge_scope: Vec<String>,
    pub source_message_id: Option<Uuid>,
    pub source_turn: Option<i32>,
}

/// What one write did. Three outcomes rather than `Result<Uuid>` because
/// "re-stated the same fact" and "replaced the fact with a changed one" are
/// different events to an operator, and both are successes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorldFactWriteOutcome {
    Created {
        fact_id: Uuid,
    },
    /// Same subject/predicate/object seen again: one row touched, none added.
    Deduped {
        fact_id: Uuid,
    },
    /// The stable fact changed (职业 医生 → 建筑师). The previous row is
    /// deactivated in the same transaction so the current fact is unambiguous,
    /// while the old value stays readable for traceability.
    Updated {
        fact_id: Uuid,
        replaced_fact_id: Uuid,
    },
}

impl WorldFactWriteOutcome {
    pub fn fact_id(&self) -> Uuid {
        match self {
            WorldFactWriteOutcome::Created { fact_id }
            | WorldFactWriteOutcome::Deduped { fact_id }
            | WorldFactWriteOutcome::Updated { fact_id, .. } => *fact_id,
        }
    }
}

pub struct WorldFactRepo<'a> {
    pub pool: &'a PgPool,
}

impl WorldFactRepo<'_> {
    /// Write one fact, deduplicating on (subject, predicate).
    ///
    /// The three-way branch is the whole update policy:
    ///
    /// * nothing active for that subject+predicate → INSERT
    /// * same object already stored → touch `updated_at` (and refresh the
    ///   rendering/scope, which may legitimately have been restated better)
    /// * different object → deactivate the old row, INSERT the new one
    ///
    /// `SELECT ... FOR UPDATE` plus the partial unique index means two
    /// concurrent turns restating the same fact cannot both insert.
    pub async fn upsert(
        &self,
        insert: &WorldFactInsert,
    ) -> Result<WorldFactWriteOutcome, sqlx::Error> {
        let mut tx = self.pool.begin().await?;

        let existing: Option<(Uuid, String, String)> = sqlx::query_as(
            "SELECT id, object, fact_type FROM engine.world_facts \
             WHERE user_id = $1 AND instance_id = $2 AND subject = $3 AND predicate = $4 \
               AND is_active \
             FOR UPDATE",
        )
        .bind(insert.user_id)
        .bind(insert.instance_id)
        .bind(&insert.subject)
        .bind(&insert.predicate)
        .fetch_optional(&mut *tx)
        .await?;

        match existing {
            Some((fact_id, object, _fact_type)) if object == insert.object => {
                sqlx::query(
                    "UPDATE engine.world_facts \
                     SET updated_at = now(), \
                         statement = COALESCE($2, statement), \
                         knowledge_scope = $3 \
                     WHERE id = $1",
                )
                .bind(fact_id)
                .bind(insert.statement.as_deref())
                .bind(&insert.knowledge_scope)
                .execute(&mut *tx)
                .await?;
                tx.commit().await?;
                Ok(WorldFactWriteOutcome::Deduped { fact_id })
            }
            Some((replaced_fact_id, _object, _fact_type)) => {
                sqlx::query(
                    "UPDATE engine.world_facts SET is_active = false, updated_at = now() \
                     WHERE id = $1",
                )
                .bind(replaced_fact_id)
                .execute(&mut *tx)
                .await?;
                let fact_id = insert_row(&mut tx, insert).await?;
                tx.commit().await?;
                Ok(WorldFactWriteOutcome::Updated {
                    fact_id,
                    replaced_fact_id,
                })
            }
            None => {
                let fact_id = insert_row(&mut tx, insert).await?;
                tx.commit().await?;
                Ok(WorldFactWriteOutcome::Created { fact_id })
            }
        }
    }

    /// Active facts for one relationship, newest first.
    ///
    /// Scope filtering is *not* done here — the caller applies
    /// `knowledge_scope_allows` so that a denied fact can be counted and logged
    /// rather than silently vanishing. The row count is bounded so a runaway
    /// writer cannot turn one prompt build into an unbounded scan.
    pub async fn list_active(
        &self,
        user_id: Uuid,
        instance_id: Uuid,
        limit: i64,
    ) -> Result<Vec<WorldFactRow>, sqlx::Error> {
        let sql = format!(
            "SELECT {WORLD_FACT_COLUMNS} FROM engine.world_facts \
             WHERE user_id = $1 AND instance_id = $2 AND is_active \
             ORDER BY updated_at DESC, id DESC \
             LIMIT $3"
        );
        sqlx::query_as::<_, WorldFactRow>(&sql)
            .bind(user_id)
            .bind(instance_id)
            .bind(limit)
            .fetch_all(self.pool)
            .await
    }
}

async fn insert_row(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    insert: &WorldFactInsert,
) -> Result<Uuid, sqlx::Error> {
    sqlx::query_scalar(
        "INSERT INTO engine.world_facts \
             (user_id, instance_id, subject, predicate, object, fact_type, statement, \
              knowledge_scope, source_message_id, source_turn) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) \
         RETURNING id",
    )
    .bind(insert.user_id)
    .bind(insert.instance_id)
    .bind(&insert.subject)
    .bind(&insert.predicate)
    .bind(&insert.object)
    .bind(insert.fact_type.as_str())
    .bind(insert.statement.as_deref())
    .bind(&insert.knowledge_scope)
    .bind(insert.source_message_id)
    .bind(insert.source_turn)
    .fetch_one(&mut **tx)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::seed_persona_instance;

    async fn owner_and_instance(pool: &PgPool) -> (Uuid, Uuid) {
        let owner = Uuid::new_v4();
        let instance = seed_persona_instance(pool, owner).await;
        (owner, instance)
    }

    fn insert(
        user_id: Uuid,
        instance_id: Uuid,
        subject: &str,
        predicate: &str,
        object: &str,
    ) -> WorldFactInsert {
        WorldFactInsert {
            user_id,
            instance_id,
            subject: subject.to_string(),
            predicate: predicate.to_string(),
            object: object.to_string(),
            fact_type: FactType::Relationship,
            statement: Some(format!("{subject}是{object}的哥哥")),
            knowledge_scope: Vec::new(),
            source_message_id: None,
            source_turn: Some(1),
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn first_write_creates_and_a_repeat_dedupes(pool: PgPool) {
        let (owner, instance) = owner_and_instance(&pool).await;
        let repo = WorldFactRepo { pool: &pool };
        let row = insert(owner, instance, "白远舟", "的哥哥", "白芷");

        let first = repo.upsert(&row).await.unwrap();
        assert!(matches!(first, WorldFactWriteOutcome::Created { .. }));

        // Requirement: repeating the same statement must not add a second row.
        let second = repo.upsert(&row).await.unwrap();
        assert_eq!(
            second,
            WorldFactWriteOutcome::Deduped {
                fact_id: first.fact_id()
            }
        );

        let active = repo.list_active(owner, instance, 10).await.unwrap();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].object, "白芷");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn a_changed_object_replaces_the_old_fact(pool: PgPool) {
        let (owner, instance) = owner_and_instance(&pool).await;
        let repo = WorldFactRepo { pool: &pool };
        let first = repo
            .upsert(&insert(owner, instance, "温景行", "职业", "医生"))
            .await
            .unwrap();
        let second = repo
            .upsert(&insert(owner, instance, "温景行", "职业", "建筑师"))
            .await
            .unwrap();

        assert_eq!(
            second,
            WorldFactWriteOutcome::Updated {
                fact_id: second.fact_id(),
                replaced_fact_id: first.fact_id()
            }
        );

        let active = repo.list_active(owner, instance, 10).await.unwrap();
        assert_eq!(active.len(), 1, "only the current fact stays active");
        assert_eq!(active[0].object, "建筑师");

        // The superseded value is still on disk for traceability.
        let all: i64 = sqlx::query_scalar("SELECT count(*) FROM engine.world_facts")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(all, 2);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn scope_and_provenance_round_trip(pool: PgPool) {
        let (owner, instance) = owner_and_instance(&pool).await;
        let repo = WorldFactRepo { pool: &pool };
        let mut row = insert(owner, instance, "白远舟", "的哥哥", "白芷");
        row.fact_type = FactType::Relationship;
        row.knowledge_scope = vec!["白芷".into(), "温景行".into()];
        repo.upsert(&row).await.unwrap();

        let stored = repo.list_active(owner, instance, 10).await.unwrap();
        assert_eq!(stored[0].knowledge_scope, vec!["白芷", "温景行"]);
        assert_eq!(stored[0].fact_type(), Some(FactType::Relationship));
        assert_eq!(stored[0].source_turn, Some(1));
    }

    /// A fact is scoped to its own relationship: another user's instance never
    /// sees it even with the same subject.
    #[sqlx::test(migrations = "./migrations")]
    async fn facts_are_isolated_per_instance(pool: PgPool) {
        let (owner, instance) = owner_and_instance(&pool).await;
        let (other_owner, other_instance) = owner_and_instance(&pool).await;
        let repo = WorldFactRepo { pool: &pool };
        repo.upsert(&insert(owner, instance, "白远舟", "的哥哥", "白芷"))
            .await
            .unwrap();

        let mine = repo.list_active(owner, instance, 10).await.unwrap();
        let theirs = repo
            .list_active(other_owner, other_instance, 10)
            .await
            .unwrap();
        assert_eq!(mine.len(), 1);
        assert!(theirs.is_empty());
    }
}
