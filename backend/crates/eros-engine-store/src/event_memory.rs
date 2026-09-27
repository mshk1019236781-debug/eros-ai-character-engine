// SPDX-License-Identifier: AGPL-3.0-only
//! Persistence for the V1 graph-enhanced episodic memory.
//!
//! Two tables, both owned here:
//!
//! * `engine.recent_episodes` — the event node. 0063 created it for short-lived
//!   summaries; 0064 added the semantic columns (`event_type`, `participants`,
//!   `location`, `importance`, `knowledge_scope`, `story_time`, `embedding`).
//!   An event is written with `expires_at = NULL`, so it never expires on its
//!   own and only an explicit deactivation flips `is_active`.
//! * `engine.event_edges` — event → event edges, computed by
//!   `eros_engine_core::event_memory::derive_edges`.
//!
//! Nothing in this module calls a model, an embedding service or a generation
//! path. Embeddings arrive as arguments and edges arrive as values, which is
//! what keeps the two-model ceiling intact.

use chrono::{DateTime, Utc};
use eros_engine_core::event_memory::{EventImportance, EventRelation, MemoryType};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, PgPool, Row};
use uuid::Uuid;

use crate::memory::format_vector;

/// Every column of an event node, in the order [`EventRow`] declares them.
///
/// One constant rather than the same 21-name list in five queries: adding a
/// column becomes a one-line change with a single place to get wrong.
const EVENT_COLUMNS: &str = "id, user_id, instance_id, session_id, summary, event_type, \
     participants, location, tags, importance, knowledge_scope, relationship_relevant, \
     story_time, source_start_message_id, source_end_message_id, created_at, created_turn, \
     salience, recall_count, last_recalled_at, is_active";

/// One stored event. The two vocabularies stay `String` at this layer so an
/// unexpected value is surfaced by [`EventRow::memory_type`] rather than
/// failing the whole row decode.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, FromRow)]
pub struct EventRow {
    pub id: Uuid,
    pub user_id: Uuid,
    pub instance_id: Uuid,
    pub session_id: Uuid,
    pub summary: String,
    pub event_type: String,
    pub participants: Vec<String>,
    pub location: Option<String>,
    pub tags: Vec<String>,
    pub importance: String,
    pub knowledge_scope: Vec<String>,
    pub relationship_relevant: bool,
    pub story_time: Option<DateTime<Utc>>,
    pub source_start_message_id: Uuid,
    pub source_end_message_id: Uuid,
    pub created_at: DateTime<Utc>,
    pub created_turn: i32,
    pub salience: f64,
    pub recall_count: i32,
    pub last_recalled_at: Option<DateTime<Utc>>,
    pub is_active: bool,
}

impl EventRow {
    pub fn memory_type(&self) -> Option<MemoryType> {
        MemoryType::parse(&self.event_type)
    }

    pub fn importance(&self) -> Option<EventImportance> {
        EventImportance::parse(&self.importance)
    }
}

/// A vector anchor plus the cosine similarity that put it there.
///
/// `similarity` is computed in SQL (`1 - (embedding <=> query)`) rather than
/// read back as a vector: the extra column costs one float, while decoding 512
/// of them per row would cost the whole candidate list.
#[derive(Debug, Clone, PartialEq)]
pub struct AnchorHit {
    pub event: EventRow,
    pub similarity: f64,
}

/// Values supplied when writing one event.
///
/// `created_at`, `salience`, `recall_count`, `last_recalled_at`, `is_active`
/// and `expires_at` are not fields here: the database owns the first, the
/// importance tier determines the second, and the last four must start at
/// "never recalled, active, never expires" so a caller cannot create an
/// already-decayed or already-dead event.
#[derive(Debug, Clone)]
pub struct EventInsert {
    pub user_id: Uuid,
    pub instance_id: Uuid,
    pub session_id: Uuid,
    pub summary: String,
    pub memory_type: MemoryType,
    pub participants: Vec<String>,
    pub location: Option<String>,
    pub tags: Vec<String>,
    pub importance: EventImportance,
    pub knowledge_scope: Vec<String>,
    pub relationship_relevant: bool,
    pub story_time: Option<DateTime<Utc>>,
    pub source_start_message_id: Uuid,
    pub source_end_message_id: Uuid,
    pub created_turn: i32,
    /// `None` stores a NULL vector. Such an event is unreachable by anchor
    /// search and still reachable through its edges — a deliberate degradation
    /// for a turn whose embedding call failed, not an error.
    pub embedding: Option<Vec<f32>>,
}

impl EventInsert {
    /// The stored `salience` column is the importance tier folded to `0..=1`,
    /// so 0063's `ORDER BY salience DESC` candidate query keeps meaning
    /// "most important first".
    pub fn salience(&self) -> f64 {
        self.importance.weight()
    }
}

/// One edge to persist, `source_event_id` → `target_event_id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventEdgeInsert {
    pub source_event_id: Uuid,
    pub target_event_id: Uuid,
    pub relation: EventRelation,
}

pub struct EventMemoryRepo<'a> {
    pub pool: &'a PgPool,
}

impl EventMemoryRepo<'_> {
    /// Insert one event and return its generated id.
    ///
    /// `expires_at` is written as an explicit NULL: this row is a durable
    /// experience, not one of 0063's short-lived summaries, and 0064's comment
    /// records that the difference lives in this one value.
    pub async fn insert_event(&self, event: EventInsert) -> Result<Uuid, sqlx::Error> {
        let vector = event.embedding.as_ref().map(|values| format_vector(values));
        // Read before the bind chain consumes `event` field by field.
        let salience = event.salience();
        sqlx::query_scalar(
            "INSERT INTO engine.recent_episodes \
                 (user_id, instance_id, session_id, summary, event_type, participants, \
                  location, tags, importance, knowledge_scope, relationship_relevant, \
                  story_time, source_start_message_id, source_end_message_id, created_turn, \
                  salience, embedding, expires_at) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13,$14,$15,$16,$17::vector,NULL) \
             RETURNING id",
        )
        .bind(event.user_id)
        .bind(event.instance_id)
        .bind(event.session_id)
        .bind(event.summary)
        .bind(event.memory_type.as_str())
        .bind(event.participants)
        .bind(event.location)
        .bind(event.tags)
        .bind(event.importance.as_str())
        .bind(event.knowledge_scope)
        .bind(event.relationship_relevant)
        .bind(event.story_time)
        .bind(event.source_start_message_id)
        .bind(event.source_end_message_id)
        .bind(event.created_turn)
        .bind(salience)
        .bind(vector)
        .fetch_one(self.pool)
        .await
    }

    /// Insert edges in one round trip.
    ///
    /// `ON CONFLICT DO NOTHING` against `event_edges_unique`: re-deriving the
    /// same edge — a retried turn, a replayed batch — must be a no-op, because
    /// a duplicate edge would make the neighbour appear twice in expansion and
    /// silently double its weight in the merge.
    ///
    /// Returns how many rows were actually written, which is what a caller
    /// wants to log; `0` is a legitimate outcome.
    pub async fn insert_edges(&self, edges: &[EventEdgeInsert]) -> Result<u64, sqlx::Error> {
        if edges.is_empty() {
            return Ok(0);
        }
        let sources: Vec<Uuid> = edges.iter().map(|edge| edge.source_event_id).collect();
        let targets: Vec<Uuid> = edges.iter().map(|edge| edge.target_event_id).collect();
        let relations: Vec<&str> = edges.iter().map(|edge| edge.relation.as_str()).collect();

        let result = sqlx::query(
            "INSERT INTO engine.event_edges (source_event_id, target_event_id, relation_type) \
             SELECT * FROM UNNEST($1::uuid[], $2::uuid[], $3::text[]) \
             ON CONFLICT (source_event_id, target_event_id, relation_type) DO NOTHING",
        )
        .bind(sources)
        .bind(targets)
        .bind(relations)
        .execute(self.pool)
        .await?;

        Ok(result.rows_affected())
    }

    /// Top-K vector anchors for one relationship, already filtered by the
    /// viewer's knowledge scope.
    ///
    /// The scope predicate is in SQL, not in Rust: filtering after the LIMIT
    /// would let three private events consume the whole anchor budget and leave
    /// the viewer with nothing, and filtering after the scan would mean the
    /// caller briefly holds events it may not see.
    ///
    /// Two literal query strings rather than one with an optional parameter,
    /// because Postgres cannot infer the type of a bind variable it never sees
    /// used.
    pub async fn anchor_search(
        &self,
        user_id: Uuid,
        instance_id: Uuid,
        viewer: Option<&str>,
        query_embedding: &[f32],
        limit: i64,
    ) -> Result<Vec<AnchorHit>, sqlx::Error> {
        let vector = format_vector(query_embedding);
        let viewer = viewer.map(str::trim).filter(|name| !name.is_empty());

        let rows = match viewer {
            Some(name) => {
                let sql = format!(
                    "SELECT {EVENT_COLUMNS}, \
                            (1 - (embedding <=> $4::vector))::float8 AS similarity \
                     FROM engine.recent_episodes \
                     WHERE user_id = $1 AND instance_id = $2 \
                       AND is_active \
                       AND (expires_at IS NULL OR expires_at > now()) \
                       AND embedding IS NOT NULL \
                       AND (knowledge_scope = '{{}}'::text[] \
                            OR $3::text = ANY(knowledge_scope)) \
                     ORDER BY embedding <=> $4::vector \
                     LIMIT $5"
                );
                sqlx::query(&sql)
                    .bind(user_id)
                    .bind(instance_id)
                    .bind(name)
                    .bind(&vector)
                    .bind(limit)
                    .fetch_all(self.pool)
                    .await?
            }
            None => {
                let sql = format!(
                    "SELECT {EVENT_COLUMNS}, \
                            (1 - (embedding <=> $3::vector))::float8 AS similarity \
                     FROM engine.recent_episodes \
                     WHERE user_id = $1 AND instance_id = $2 \
                       AND is_active \
                       AND (expires_at IS NULL OR expires_at > now()) \
                       AND embedding IS NOT NULL \
                       AND knowledge_scope = '{{}}'::text[] \
                     ORDER BY embedding <=> $3::vector \
                     LIMIT $4"
                );
                sqlx::query(&sql)
                    .bind(user_id)
                    .bind(instance_id)
                    .bind(&vector)
                    .bind(limit)
                    .fetch_all(self.pool)
                    .await?
            }
        };

        let mut hits = Vec::with_capacity(rows.len());
        for row in &rows {
            hits.push(AnchorHit {
                event: EventRow::from_row(row)?,
                similarity: row.try_get("similarity")?,
            });
        }
        Ok(hits)
    }

    /// Every distinct neighbour of `anchor_ids` reachable in one hop, in either
    /// direction, still filtered by scope and relationship.
    ///
    /// Both directions in one query: `followed_by` points forward and `related_to`
    /// points from the older event to the newer one, so a caller asking "what
    /// else belongs with this anchor" needs the union, not the outgoing edges.
    /// The anchors themselves are excluded so the caller does not score the
    /// same event at two distances.
    pub async fn expand_one_hop(
        &self,
        anchor_ids: &[Uuid],
        user_id: Uuid,
        instance_id: Uuid,
        viewer: Option<&str>,
        limit: i64,
    ) -> Result<Vec<EventRow>, sqlx::Error> {
        if anchor_ids.is_empty() {
            return Ok(Vec::new());
        }
        let viewer = viewer.map(str::trim).filter(|name| !name.is_empty());

        let sql = match viewer {
            Some(_) => format!(
                "SELECT {EVENT_COLUMNS} FROM engine.recent_episodes \
                 WHERE id IN ( \
                     SELECT target_event_id FROM engine.event_edges \
                     WHERE source_event_id = ANY($1::uuid[]) \
                     UNION \
                     SELECT source_event_id FROM engine.event_edges \
                     WHERE target_event_id = ANY($1::uuid[]) \
                 ) \
                   AND user_id = $2 AND instance_id = $3 \
                   AND is_active \
                   AND (expires_at IS NULL OR expires_at > now()) \
                   AND NOT (id = ANY($1::uuid[])) \
                   AND (knowledge_scope = '{{}}'::text[] \
                        OR $4::text = ANY(knowledge_scope)) \
                 ORDER BY created_at DESC, id DESC \
                 LIMIT $5"
            ),
            None => format!(
                "SELECT {EVENT_COLUMNS} FROM engine.recent_episodes \
                 WHERE id IN ( \
                     SELECT target_event_id FROM engine.event_edges \
                     WHERE source_event_id = ANY($1::uuid[]) \
                     UNION \
                     SELECT source_event_id FROM engine.event_edges \
                     WHERE target_event_id = ANY($1::uuid[]) \
                 ) \
                   AND user_id = $2 AND instance_id = $3 \
                   AND is_active \
                   AND (expires_at IS NULL OR expires_at > now()) \
                   AND NOT (id = ANY($1::uuid[])) \
                   AND knowledge_scope = '{{}}'::text[] \
                 ORDER BY created_at DESC, id DESC \
                 LIMIT $4"
            ),
        };

        let mut query = sqlx::query_as::<_, EventRow>(&sql)
            .bind(anchor_ids)
            .bind(user_id)
            .bind(instance_id);
        query = match viewer {
            Some(name) => query.bind(name).bind(limit),
            None => query.bind(limit),
        };
        query.fetch_all(self.pool).await
    }

    /// Load events by id, in no particular order.
    pub async fn events_by_ids(&self, ids: &[Uuid]) -> Result<Vec<EventRow>, sqlx::Error> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let sql = format!(
            "SELECT {EVENT_COLUMNS} FROM engine.recent_episodes WHERE id = ANY($1::uuid[])"
        );
        sqlx::query_as::<_, EventRow>(&sql)
            .bind(ids)
            .fetch_all(self.pool)
            .await
    }

    /// The most recently written event in one session, which is the source of
    /// the `followed_by` edge for the next one.
    pub async fn latest_event_in_session(
        &self,
        session_id: Uuid,
    ) -> Result<Option<EventRow>, sqlx::Error> {
        let sql = format!(
            "SELECT {EVENT_COLUMNS} FROM engine.recent_episodes \
             WHERE session_id = $1 AND is_active \
             ORDER BY created_at DESC, id DESC \
             LIMIT 1"
        );
        sqlx::query_as::<_, EventRow>(&sql)
            .bind(session_id)
            .fetch_optional(self.pool)
            .await
    }

    /// Recent events in one session, newest first, for the `related_to` rules.
    pub async fn session_events(
        &self,
        session_id: Uuid,
        limit: i64,
    ) -> Result<Vec<EventRow>, sqlx::Error> {
        let sql = format!(
            "SELECT {EVENT_COLUMNS} FROM engine.recent_episodes \
             WHERE session_id = $1 AND is_active \
             ORDER BY created_at DESC, id DESC \
             LIMIT $2"
        );
        sqlx::query_as::<_, EventRow>(&sql)
            .bind(session_id)
            .bind(limit)
            .fetch_all(self.pool)
            .await
    }

    /// Record that these events were injected into a prompt.
    ///
    /// One statement for the whole set so the cooldown state cannot end up
    /// half-updated, and scoped to active rows so a recalled-then-deactivated
    /// event does not come back to life. Returns the number of rows touched.
    pub async fn mark_recalled(&self, ids: &[Uuid]) -> Result<u64, sqlx::Error> {
        if ids.is_empty() {
            return Ok(0);
        }
        let result = sqlx::query(
            "UPDATE engine.recent_episodes \
             SET last_recalled_at = now(), recall_count = recall_count + 1 \
             WHERE id = ANY($1::uuid[]) AND is_active",
        )
        .bind(ids)
        .execute(self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{seed_chat_message, seed_chat_session};

    /// A deterministic 512-dim unit vector: cosine similarity between two of
    /// these is 1.0 when they share a hot index and 0.0 when they do not, so a
    /// test can assert an ordering without knowing anything about the embedder.
    fn unit_embedding(hot: usize) -> Vec<f32> {
        let mut values = vec![0.0_f32; 512];
        values[hot % 512] = 1.0;
        values
    }

    struct Fixture {
        user_id: Uuid,
        instance_id: Uuid,
        session_id: Uuid,
        first_message_id: Uuid,
        last_message_id: Uuid,
    }

    async fn fixture(pool: &PgPool) -> Fixture {
        let user_id = Uuid::new_v4();
        let session_id = seed_chat_session(pool, user_id).await;
        let instance_id: Uuid =
            sqlx::query_scalar("SELECT instance_id FROM engine.chat_sessions WHERE id = $1")
                .bind(session_id)
                .fetch_one(pool)
                .await
                .unwrap();
        Fixture {
            user_id,
            instance_id,
            session_id,
            first_message_id: seed_chat_message(pool, session_id).await,
            last_message_id: seed_chat_message(pool, session_id).await,
        }
    }

    fn event(fixture: &Fixture, summary: &str, hot: Option<usize>) -> EventInsert {
        EventInsert {
            user_id: fixture.user_id,
            instance_id: fixture.instance_id,
            session_id: fixture.session_id,
            summary: summary.to_string(),
            memory_type: MemoryType::Callback,
            participants: vec!["白芷".into(), "裴烬".into()],
            location: Some("便利店".into()),
            tags: vec!["饮料".into()],
            importance: EventImportance::Normal,
            knowledge_scope: Vec::new(),
            relationship_relevant: true,
            story_time: None,
            source_start_message_id: fixture.first_message_id,
            source_end_message_id: fixture.last_message_id,
            created_turn: 1,
            embedding: hot.map(unit_embedding),
        }
    }

    async fn edge_count(pool: &PgPool) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM engine.event_edges")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    // ── Schema ────────────────────────────────────────────────────────────

    #[sqlx::test(migrations = "./migrations")]
    async fn migration_0064_adds_the_semantic_columns_and_the_edge_table(pool: PgPool) {
        let columns: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM information_schema.columns \
             WHERE table_schema = 'engine' AND table_name = 'recent_episodes' \
               AND column_name IN ('event_type', 'participants', 'location', 'importance', \
                                   'knowledge_scope', 'relationship_relevant', 'story_time', \
                                   'embedding')",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(columns, 8, "0064 must add all eight semantic columns");

        let tables: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM information_schema.tables \
             WHERE table_schema = 'engine' AND table_name = 'event_edges'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(tables, 1);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn the_database_rejects_unknown_vocabulary_values(pool: PgPool) {
        let fixture = fixture(&pool).await;
        let repo = EventMemoryRepo { pool: &pool };
        let id = repo
            .insert_event(event(&fixture, "白芷买饮料", Some(1)))
            .await
            .unwrap();

        // The model can hallucinate a tier; the column cannot store one.
        let bad_type =
            sqlx::query("UPDATE engine.recent_episodes SET event_type = 'bogus' WHERE id = $1")
                .bind(id)
                .execute(&pool)
                .await;
        assert!(bad_type.is_err(), "event_type is a closed vocabulary");

        let bad_importance =
            sqlx::query("UPDATE engine.recent_episodes SET importance = 'critical' WHERE id = $1")
                .bind(id)
                .execute(&pool)
                .await;
        assert!(bad_importance.is_err(), "importance is a closed vocabulary");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn event_edges_reject_self_loops(pool: PgPool) {
        let fixture = fixture(&pool).await;
        let repo = EventMemoryRepo { pool: &pool };
        let id = repo
            .insert_event(event(&fixture, "白芷买饮料", Some(1)))
            .await
            .unwrap();

        let out = repo
            .insert_edges(&[EventEdgeInsert {
                source_event_id: id,
                target_event_id: id,
                relation: EventRelation::RelatedTo,
            }])
            .await;
        assert!(out.is_err(), "a self loop would duplicate the anchor");
    }

    // ── Write ─────────────────────────────────────────────────────────────

    #[sqlx::test(migrations = "./migrations")]
    async fn insert_event_round_trips_every_semantic_column(pool: PgPool) {
        let fixture = fixture(&pool).await;
        let repo = EventMemoryRepo { pool: &pool };
        let story_time = DateTime::from_timestamp(1_780_000_000, 0).unwrap();

        let mut insert = event(&fixture, "裴烬替白芷挡刀后重伤住院", Some(9));
        insert.memory_type = MemoryType::Plot;
        insert.importance = EventImportance::Major;
        insert.knowledge_scope = vec!["裴烬".into()];
        insert.story_time = Some(story_time);
        insert.location = None;
        let id = repo.insert_event(insert).await.unwrap();

        let rows = repo.events_by_ids(&[id]).await.unwrap();
        assert_eq!(rows.len(), 1);
        let row = &rows[0];
        assert_eq!(row.summary, "裴烬替白芷挡刀后重伤住院");
        assert_eq!(row.memory_type(), Some(MemoryType::Plot));
        assert_eq!(row.importance(), Some(EventImportance::Major));
        assert_eq!(row.knowledge_scope, vec!["裴烬"]);
        assert_eq!(row.location, None);
        assert_eq!(row.story_time, Some(story_time));
        assert!(
            (row.salience - 1.0).abs() < 1e-9,
            "salience mirrors importance"
        );
        assert_eq!(row.recall_count, 0);
        assert!(row.last_recalled_at.is_none());
        assert!(row.is_active);
    }

    /// 0063's rows expire; an event must not. The whole difference is one NULL,
    /// and a default would silently turn every event back into a short-lived
    /// summary.
    #[sqlx::test(migrations = "./migrations")]
    async fn inserted_events_never_expire_on_their_own(pool: PgPool) {
        let fixture = fixture(&pool).await;
        let repo = EventMemoryRepo { pool: &pool };
        let id = repo
            .insert_event(event(&fixture, "两人约好周日去公园", Some(1)))
            .await
            .unwrap();

        let expires_at: Option<DateTime<Utc>> =
            sqlx::query_scalar("SELECT expires_at FROM engine.recent_episodes WHERE id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!(expires_at.is_none());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn insert_edges_is_idempotent(pool: PgPool) {
        let fixture = fixture(&pool).await;
        let repo = EventMemoryRepo { pool: &pool };
        let first = repo
            .insert_event(event(&fixture, "first", Some(1)))
            .await
            .unwrap();
        let second = repo
            .insert_event(event(&fixture, "second", Some(2)))
            .await
            .unwrap();
        let edge = EventEdgeInsert {
            source_event_id: first,
            target_event_id: second,
            relation: EventRelation::FollowedBy,
        };

        assert_eq!(repo.insert_edges(&[edge]).await.unwrap(), 1);
        assert_eq!(
            repo.insert_edges(&[edge]).await.unwrap(),
            0,
            "a retried turn must not double the edge"
        );
        assert_eq!(edge_count(&pool).await, 1);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn insert_edges_of_an_empty_slice_touches_nothing(pool: PgPool) {
        let repo = EventMemoryRepo { pool: &pool };
        assert_eq!(repo.insert_edges(&[]).await.unwrap(), 0);
    }

    // ── Anchor retrieval ──────────────────────────────────────────────────

    #[sqlx::test(migrations = "./migrations")]
    async fn anchor_search_orders_by_cosine_similarity(pool: PgPool) {
        let fixture = fixture(&pool).await;
        let repo = EventMemoryRepo { pool: &pool };
        let near = repo
            .insert_event(event(&fixture, "near", Some(3)))
            .await
            .unwrap();
        let far = repo
            .insert_event(event(&fixture, "far", Some(200)))
            .await
            .unwrap();

        let hits = repo
            .anchor_search(
                fixture.user_id,
                fixture.instance_id,
                None,
                &unit_embedding(3),
                10,
            )
            .await
            .unwrap();
        assert_eq!(
            hits.len(),
            2,
            "both events are in scope for a public viewer"
        );
        assert_eq!(hits[0].event.id, near);
        assert_eq!(hits[1].event.id, far);
        assert!((hits[0].similarity - 1.0).abs() < 1e-5);
        assert!(hits[0].similarity > hits[1].similarity);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn anchor_search_hides_scoped_events_from_every_other_viewer(pool: PgPool) {
        let fixture = fixture(&pool).await;
        let repo = EventMemoryRepo { pool: &pool };
        let public = repo
            .insert_event(event(&fixture, "两人去公园", Some(3)))
            .await
            .unwrap();
        let mut secret = event(&fixture, "裴烬独自去处理的事", Some(3));
        secret.knowledge_scope = vec!["裴烬".into()];
        let secret_id = repo.insert_event(secret).await.unwrap();

        let as_peijin = repo
            .anchor_search(
                fixture.user_id,
                fixture.instance_id,
                Some("裴烬"),
                &unit_embedding(3),
                10,
            )
            .await
            .unwrap();
        let mut peijin_ids: Vec<Uuid> = as_peijin.iter().map(|hit| hit.event.id).collect();
        peijin_ids.sort();
        let mut expected = vec![public, secret_id];
        expected.sort();
        assert_eq!(peijin_ids, expected, "裴烬 sees both");

        let as_baizhi = repo
            .anchor_search(
                fixture.user_id,
                fixture.instance_id,
                Some("白芷"),
                &unit_embedding(3),
                10,
            )
            .await
            .unwrap();
        assert_eq!(
            as_baizhi.iter().map(|hit| hit.event.id).collect::<Vec<_>>(),
            vec![public],
            "白芷 must not see 裴烬's scoped event"
        );

        let no_viewer = repo
            .anchor_search(
                fixture.user_id,
                fixture.instance_id,
                None,
                &unit_embedding(3),
                10,
            )
            .await
            .unwrap();
        assert_eq!(
            no_viewer.iter().map(|hit| hit.event.id).collect::<Vec<_>>(),
            vec![public],
            "an unresolved POV fails closed"
        );

        let blank_viewer = repo
            .anchor_search(
                fixture.user_id,
                fixture.instance_id,
                Some("   "),
                &unit_embedding(3),
                10,
            )
            .await
            .unwrap();
        assert_eq!(blank_viewer.len(), 1, "a blank POV is not a viewer");
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn anchor_search_never_crosses_a_relationship(pool: PgPool) {
        let mine = fixture(&pool).await;
        let theirs = fixture(&pool).await;
        let repo = EventMemoryRepo { pool: &pool };
        repo.insert_event(event(&mine, "mine", Some(3)))
            .await
            .unwrap();
        repo.insert_event(event(&theirs, "theirs", Some(3)))
            .await
            .unwrap();

        let hits = repo
            .anchor_search(mine.user_id, mine.instance_id, None, &unit_embedding(3), 10)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].event.summary, "mine");
    }

    /// An event whose embedding call failed is still a node. It cannot be an
    /// anchor, and it is still reachable through its edges — the degradation
    /// the `embedding IS NOT NULL` guard is there to express.
    #[sqlx::test(migrations = "./migrations")]
    async fn an_event_without_an_embedding_is_reachable_only_by_graph(pool: PgPool) {
        let fixture = fixture(&pool).await;
        let repo = EventMemoryRepo { pool: &pool };
        let anchored = repo
            .insert_event(event(&fixture, "anchored", Some(3)))
            .await
            .unwrap();
        let unembedded = repo
            .insert_event(event(&fixture, "unembedded", None))
            .await
            .unwrap();
        repo.insert_edges(&[EventEdgeInsert {
            source_event_id: anchored,
            target_event_id: unembedded,
            relation: EventRelation::FollowedBy,
        }])
        .await
        .unwrap();

        let hits = repo
            .anchor_search(
                fixture.user_id,
                fixture.instance_id,
                None,
                &unit_embedding(3),
                10,
            )
            .await
            .unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].event.id, anchored);

        let neighbours = repo
            .expand_one_hop(&[anchored], fixture.user_id, fixture.instance_id, None, 10)
            .await
            .unwrap();
        assert_eq!(neighbours.len(), 1);
        assert_eq!(neighbours[0].id, unembedded);
    }

    // ── Graph expansion ───────────────────────────────────────────────────

    #[sqlx::test(migrations = "./migrations")]
    async fn expand_one_hop_walks_edges_in_both_directions(pool: PgPool) {
        let fixture = fixture(&pool).await;
        let repo = EventMemoryRepo { pool: &pool };
        let earlier = repo
            .insert_event(event(&fixture, "earlier", Some(1)))
            .await
            .unwrap();
        let later = repo
            .insert_event(event(&fixture, "later", Some(2)))
            .await
            .unwrap();
        repo.insert_edges(&[EventEdgeInsert {
            source_event_id: earlier,
            target_event_id: later,
            relation: EventRelation::FollowedBy,
        }])
        .await
        .unwrap();

        let forward = repo
            .expand_one_hop(&[earlier], fixture.user_id, fixture.instance_id, None, 10)
            .await
            .unwrap();
        assert_eq!(
            forward.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![later]
        );

        let backward = repo
            .expand_one_hop(&[later], fixture.user_id, fixture.instance_id, None, 10)
            .await
            .unwrap();
        assert_eq!(
            backward.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![earlier]
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn expand_one_hop_excludes_the_anchors_themselves(pool: PgPool) {
        let fixture = fixture(&pool).await;
        let repo = EventMemoryRepo { pool: &pool };
        let first = repo
            .insert_event(event(&fixture, "first", Some(1)))
            .await
            .unwrap();
        let second = repo
            .insert_event(event(&fixture, "second", Some(2)))
            .await
            .unwrap();
        repo.insert_edges(&[EventEdgeInsert {
            source_event_id: first,
            target_event_id: second,
            relation: EventRelation::FollowedBy,
        }])
        .await
        .unwrap();

        let neighbours = repo
            .expand_one_hop(
                &[first, second],
                fixture.user_id,
                fixture.instance_id,
                None,
                10,
            )
            .await
            .unwrap();
        assert!(
            neighbours.is_empty(),
            "both endpoints are anchors, so their mutual edge yields nothing"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn expand_one_hop_applies_the_same_scope_and_relationship_filters(pool: PgPool) {
        let mine = fixture(&pool).await;
        let theirs = fixture(&pool).await;
        let repo = EventMemoryRepo { pool: &pool };
        let anchor = repo
            .insert_event(event(&mine, "anchor", Some(1)))
            .await
            .unwrap();
        let mut secret = event(&mine, "secret", Some(2));
        secret.knowledge_scope = vec!["裴烬".into()];
        let secret_id = repo.insert_event(secret).await.unwrap();
        let foreign = repo
            .insert_event(event(&theirs, "foreign", Some(3)))
            .await
            .unwrap();
        repo.insert_edges(&[
            EventEdgeInsert {
                source_event_id: anchor,
                target_event_id: secret_id,
                relation: EventRelation::RelatedTo,
            },
            EventEdgeInsert {
                source_event_id: anchor,
                target_event_id: foreign,
                relation: EventRelation::RelatedTo,
            },
        ])
        .await
        .unwrap();

        let as_baizhi = repo
            .expand_one_hop(&[anchor], mine.user_id, mine.instance_id, Some("白芷"), 10)
            .await
            .unwrap();
        assert!(
            as_baizhi.is_empty(),
            "a scoped neighbour is hidden and another relationship's event never joins"
        );

        let as_peijin = repo
            .expand_one_hop(&[anchor], mine.user_id, mine.instance_id, Some("裴烬"), 10)
            .await
            .unwrap();
        assert_eq!(
            as_peijin.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![secret_id]
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn expand_one_hop_with_no_anchors_is_empty(pool: PgPool) {
        let repo = EventMemoryRepo { pool: &pool };
        assert!(repo
            .expand_one_hop(&[], Uuid::new_v4(), Uuid::new_v4(), None, 10)
            .await
            .unwrap()
            .is_empty());
    }

    // ── Cooldown bookkeeping ──────────────────────────────────────────────

    #[sqlx::test(migrations = "./migrations")]
    async fn mark_recalled_increments_the_counter_and_stamps_the_time(pool: PgPool) {
        let fixture = fixture(&pool).await;
        let repo = EventMemoryRepo { pool: &pool };
        let id = repo
            .insert_event(event(&fixture, "难喝的饮料", Some(1)))
            .await
            .unwrap();

        assert_eq!(repo.mark_recalled(&[id]).await.unwrap(), 1);
        assert_eq!(repo.mark_recalled(&[id]).await.unwrap(), 1);

        let row = &repo.events_by_ids(&[id]).await.unwrap()[0];
        assert_eq!(row.recall_count, 2);
        assert!(row.last_recalled_at.is_some());
        assert_eq!(repo.mark_recalled(&[]).await.unwrap(), 0);
        assert_eq!(repo.mark_recalled(&[Uuid::new_v4()]).await.unwrap(), 0);
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn latest_event_in_session_returns_the_newest(pool: PgPool) {
        let fixture = fixture(&pool).await;
        let repo = EventMemoryRepo { pool: &pool };
        let first = repo
            .insert_event(event(&fixture, "first", Some(1)))
            .await
            .unwrap();
        let second = repo
            .insert_event(event(&fixture, "second", Some(2)))
            .await
            .unwrap();
        // Backdate rather than sleep: `now()` is the transaction clock, and two
        // inserts can share a timestamp, which would make `id DESC` decide the
        // answer and the test flaky.
        sqlx::query(
            "UPDATE engine.recent_episodes SET created_at = now() - interval '1 hour' WHERE id = $1",
        )
        .bind(first)
        .execute(&pool)
        .await
        .unwrap();

        let latest = repo
            .latest_event_in_session(fixture.session_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(latest.id, second);

        let all = repo.session_events(fixture.session_id, 10).await.unwrap();
        assert_eq!(
            all.iter().map(|row| row.id).collect::<Vec<_>>(),
            vec![second, first],
            "newest first"
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn latest_event_in_session_is_none_on_a_fresh_session(pool: PgPool) {
        let fixture = fixture(&pool).await;
        let repo = EventMemoryRepo { pool: &pool };
        assert!(repo
            .latest_event_in_session(fixture.session_id)
            .await
            .unwrap()
            .is_none());
    }
}
