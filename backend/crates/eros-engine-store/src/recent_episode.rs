// SPDX-License-Identifier: AGPL-3.0-only
//! Persistence for lightweight, relationship-scoped recent episodes.
//!
//! This module deliberately contains only storage operations. It does not
//! call an embedding service, an LLM, or any of the existing memory/prompt
//! pipelines.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::PgPool;
use uuid::Uuid;

/// A recent episode ready for callers that will decide whether to recall it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, sqlx::FromRow)]
pub struct RecentEpisode {
    pub id: Uuid,
    pub user_id: Uuid,
    pub instance_id: Uuid,
    pub session_id: Uuid,
    pub summary: String,
    pub tags: Vec<String>,
    pub source_start_message_id: Uuid,
    pub source_end_message_id: Uuid,
    pub created_at: DateTime<Utc>,
    pub created_turn: i32,
    pub salience: f64,
    pub last_recalled_at: Option<DateTime<Utc>>,
    pub recall_count: i32,
    pub expires_at: Option<DateTime<Utc>>,
    pub is_active: bool,
}

/// Values supplied when inserting an episode. `created_at`, recall state and
/// activity are database-owned defaults so callers cannot accidentally create
/// an already-recalled or inactive episode.
#[derive(Debug, Clone)]
pub struct RecentEpisodeInsert {
    pub user_id: Uuid,
    pub instance_id: Uuid,
    pub session_id: Uuid,
    pub summary: String,
    pub tags: Vec<String>,
    pub source_start_message_id: Uuid,
    pub source_end_message_id: Uuid,
    pub created_turn: i32,
    pub salience: f64,
    pub expires_at: Option<DateTime<Utc>>,
}

pub struct RecentEpisodeRepo<'a> {
    pub pool: &'a PgPool,
}

impl RecentEpisodeRepo<'_> {
    /// Insert one episode and return its generated id.
    pub async fn insert_episode(&self, episode: RecentEpisodeInsert) -> Result<Uuid, sqlx::Error> {
        sqlx::query_scalar(
            "INSERT INTO engine.recent_episodes \
                 (user_id, instance_id, session_id, summary, tags, \
                  source_start_message_id, source_end_message_id, created_turn, \
                  salience, expires_at) \
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) \
             RETURNING id",
        )
        .bind(episode.user_id)
        .bind(episode.instance_id)
        .bind(episode.session_id)
        .bind(episode.summary)
        .bind(episode.tags)
        .bind(episode.source_start_message_id)
        .bind(episode.source_end_message_id)
        .bind(episode.created_turn)
        .bind(episode.salience)
        .bind(episode.expires_at)
        .fetch_one(self.pool)
        .await
    }

    /// Return active, unexpired episodes for one user/persona relationship.
    /// Candidates are ranked by salience first, then newest turn/time.
    pub async fn recent_candidates(
        &self,
        user_id: Uuid,
        instance_id: Uuid,
        limit: i64,
    ) -> Result<Vec<RecentEpisode>, sqlx::Error> {
        sqlx::query_as::<_, RecentEpisode>(
            "SELECT id, user_id, instance_id, session_id, summary, tags, \
                    source_start_message_id, source_end_message_id, created_at, \
                    created_turn, salience, last_recalled_at, recall_count, \
                    expires_at, is_active \
             FROM engine.recent_episodes \
             WHERE user_id = $1 AND instance_id = $2 \
               AND is_active \
               AND (expires_at IS NULL OR expires_at > now()) \
             ORDER BY salience DESC, created_turn DESC, created_at DESC, id DESC \
             LIMIT $3",
        )
        .bind(user_id)
        .bind(instance_id)
        .bind(limit)
        .fetch_all(self.pool)
        .await
    }

    /// Mark an episode as recalled and increment its recall counter.
    /// Returns `false` when the row is missing, inactive, or already expired.
    pub async fn touch_recalled(&self, id: Uuid) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            "UPDATE engine.recent_episodes \
             SET last_recalled_at = now(), recall_count = recall_count + 1 \
             WHERE id = $1 AND is_active \
               AND (expires_at IS NULL OR expires_at > now())",
        )
        .bind(id)
        .execute(self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Deactivate one episode. Returns `false` when it was already inactive
    /// or does not exist.
    pub async fn expire_episode(&self, id: Uuid) -> Result<bool, sqlx::Error> {
        let result = sqlx::query(
            "UPDATE engine.recent_episodes SET is_active = false \
             WHERE id = $1 AND is_active",
        )
        .bind(id)
        .execute(self.pool)
        .await?;
        Ok(result.rows_affected() == 1)
    }

    /// Deactivate every active episode whose explicit expiry has passed.
    pub async fn cleanup_expired(&self) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            "UPDATE engine.recent_episodes SET is_active = false \
             WHERE is_active AND expires_at IS NOT NULL AND expires_at <= now()",
        )
        .execute(self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{seed_chat_message, seed_chat_session};

    async fn seed_episode_sources(pool: &PgPool, user_id: Uuid) -> (Uuid, Uuid, Uuid, Uuid) {
        let session_id = seed_chat_session(pool, user_id).await;
        let start = seed_chat_message(pool, session_id).await;
        let end = seed_chat_message(pool, session_id).await;
        let instance_id: Uuid =
            sqlx::query_scalar("SELECT instance_id FROM engine.chat_sessions WHERE id = $1")
                .bind(session_id)
                .fetch_one(pool)
                .await
                .unwrap();
        (instance_id, session_id, start, end)
    }

    fn insert(
        user_id: Uuid,
        instance_id: Uuid,
        session_id: Uuid,
        start: Uuid,
        end: Uuid,
        summary: &str,
        salience: f64,
        created_turn: i32,
        expires_at: Option<DateTime<Utc>>,
    ) -> RecentEpisodeInsert {
        RecentEpisodeInsert {
            user_id,
            instance_id,
            session_id,
            summary: summary.into(),
            tags: vec!["test".into(), "episode".into()],
            source_start_message_id: start,
            source_end_message_id: end,
            created_turn,
            salience,
            expires_at,
        }
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn insert_and_candidates_round_trip_and_scope(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (instance_id, session_id, start, end) = seed_episode_sources(&pool, user_id).await;
        let repo = RecentEpisodeRepo { pool: &pool };
        let id = repo
            .insert_episode(insert(
                user_id,
                instance_id,
                session_id,
                start,
                end,
                "important conversation",
                0.8,
                3,
                None,
            ))
            .await
            .unwrap();

        let rows = repo
            .recent_candidates(user_id, instance_id, 10)
            .await
            .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, id);
        assert_eq!(rows[0].summary, "important conversation");
        assert_eq!(rows[0].tags, vec!["test", "episode"]);
        assert_eq!(rows[0].recall_count, 0);
        assert!(rows[0].is_active);
        assert!(repo
            .recent_candidates(Uuid::new_v4(), instance_id, 10)
            .await
            .unwrap()
            .is_empty());
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn candidates_exclude_expired_and_order_by_salience(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (instance_id, session_id, start, end) = seed_episode_sources(&pool, user_id).await;
        let repo = RecentEpisodeRepo { pool: &pool };
        repo.insert_episode(insert(
            user_id,
            instance_id,
            session_id,
            start,
            end,
            "low",
            0.1,
            8,
            None,
        ))
        .await
        .unwrap();
        repo.insert_episode(insert(
            user_id,
            instance_id,
            session_id,
            start,
            end,
            "high",
            0.9,
            1,
            None,
        ))
        .await
        .unwrap();
        repo.insert_episode(insert(
            user_id,
            instance_id,
            session_id,
            start,
            end,
            "expired",
            1.0,
            99,
            Some(Utc::now() - chrono::Duration::minutes(1)),
        ))
        .await
        .unwrap();

        let rows = repo
            .recent_candidates(user_id, instance_id, 10)
            .await
            .unwrap();
        assert_eq!(
            rows.iter().map(|r| r.summary.as_str()).collect::<Vec<_>>(),
            vec!["high", "low"]
        );
    }

    #[sqlx::test(migrations = "./migrations")]
    async fn touch_expire_and_cleanup_update_only_eligible_rows(pool: PgPool) {
        let user_id = Uuid::new_v4();
        let (instance_id, session_id, start, end) = seed_episode_sources(&pool, user_id).await;
        let repo = RecentEpisodeRepo { pool: &pool };
        let live = repo
            .insert_episode(insert(
                user_id,
                instance_id,
                session_id,
                start,
                end,
                "live",
                0.5,
                1,
                None,
            ))
            .await
            .unwrap();
        let expired = repo
            .insert_episode(insert(
                user_id,
                instance_id,
                session_id,
                start,
                end,
                "expired",
                0.5,
                2,
                Some(Utc::now() - chrono::Duration::minutes(1)),
            ))
            .await
            .unwrap();

        assert!(repo.touch_recalled(live).await.unwrap());
        assert!(!repo.touch_recalled(expired).await.unwrap());
        let recalled: (i32, Option<DateTime<Utc>>) = sqlx::query_as(
            "SELECT recall_count, last_recalled_at FROM engine.recent_episodes WHERE id = $1",
        )
        .bind(live)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(recalled.0, 1);
        assert!(recalled.1.is_some());

        assert!(repo.expire_episode(live).await.unwrap());
        assert!(!repo.expire_episode(live).await.unwrap());
        assert_eq!(repo.cleanup_expired().await.unwrap(), 1);
        assert!(repo
            .recent_candidates(user_id, instance_id, 10)
            .await
            .unwrap()
            .is_empty());
    }
}
