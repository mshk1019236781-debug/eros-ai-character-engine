// SPDX-License-Identifier: AGPL-3.0-only
//! The three seams between EROS generation and the event store.
//!
//! ```text
//! main RP model
//!   ↓  reply + optional MemoryMetadata   (same call, no second model)
//! MemoryWriteAdapter                     ← this file
//!   ↓
//! events + event_edges
//!   ↓
//! anchor search (pgvector) + 1-hop expansion
//!   ↓
//! MemoryRetrievalAdapter                 ← this file
//!   ↓  top 1..3 summaries
//! render_experiences
//!   ↓
//! EROS prompt assembly
//! ```
//!
//! ## What is deliberately not here
//!
//! * No model call. The write adapter is handed an embedding; it never asks for
//!   one, and neither adapter can reach an LLM. That is the whole point of
//!   putting the rules in `eros_engine_core::event_memory`.
//! * No prompt assembly. [`render_experiences`] returns the block text; where
//!   it is inserted into the system prompt stays `prompt.rs`'s decision.
//! * No wiring. Nothing in the chat pipeline calls these yet — the skeletons
//!   exist so the call sites are a two-line change tomorrow rather than a
//!   design question.
//!
//! Because of that last point the module is a skeleton the compiler would
//! otherwise flag item by item; the allowance is the price of landing the seams
//! and the call sites as two separate, reviewable changes.
#![allow(dead_code)]

use chrono::{DateTime, Utc};
use eros_engine_core::event_memory::{
    dedupe_against_known, derive_edges, knowledge_scope_allows, normalize_for_dedup,
    participant_overlap, recall_score, render_past_experiences, EventImportance, MemoryMetadata,
    MemoryMetadataError, NewEventFacts, PriorEvent, RecallSignals, DEFAULT_MIN_RECALL_SCORE,
    MIN_DEDUP_CHARS,
};
use eros_engine_store::event_memory::{EventEdgeInsert, EventInsert, EventMemoryRepo, EventRow};
use sqlx::PgPool;
use uuid::Uuid;

/// How many recent events one session contributes to the `related_to` rules.
/// Bigger than [`MAX_RELATED_EDGES`] on purpose: the cap is applied after the
/// newest-first sort, so the query has to fetch a few more than it will link.
const RELATED_CANDIDATE_LIMIT: i64 = 32;
const DEDUPE_ANCHOR_LIMIT: i64 = 12;
const DEDUPE_HIGH_SIMILARITY: f64 = 0.88;
const DEDUPE_CONDITIONAL_SIMILARITY: f64 = 0.82;
const CALLBACK_DEDUPE_LEXICAL_OVERLAP: f64 = 0.20;
const CALLBACK_DEDUPE_MAX_AGE_HOURS: i64 = 24;

#[derive(Debug)]
pub enum MemoryAdapterError {
    Store(sqlx::Error),
}

impl std::fmt::Display for MemoryAdapterError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MemoryAdapterError::Store(error) => write!(formatter, "event store: {error}"),
        }
    }
}

impl std::error::Error for MemoryAdapterError {}

impl From<sqlx::Error> for MemoryAdapterError {
    fn from(error: sqlx::Error) -> Self {
        MemoryAdapterError::Store(error)
    }
}

/// The non-model context for one write. Everything here comes from the turn,
/// never from the model: a model that could choose its own `session_id` could
/// file an event into another story.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryWriteContext {
    pub user_id: Uuid,
    pub instance_id: Uuid,
    pub session_id: Uuid,
    pub source_start_message_id: Uuid,
    pub source_end_message_id: Uuid,
    pub created_turn: i32,
}

/// What happened to one write attempt.
///
/// Outcomes rather than `Result<Option<Uuid>>`: a rejected metadata block is
/// neither an error (the turn succeeded) nor a success (nothing was stored),
/// and the caller needs to tell it apart from `memory: null` when reading logs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryWriteOutcome {
    Written {
        event_id: Uuid,
        edges: usize,
    },
    /// The model returned `memory: null`.
    NoMetadata,
    /// The model returned a block the contract refuses. Fail-open: the reply
    /// still ships, the event is simply not remembered.
    Rejected(MemoryMetadataError),
    /// The candidate states a fact already represented by a recent event in
    /// the same user/instance scope.
    Duplicate {
        existing_event_id: Uuid,
    },
}

pub struct MemoryWriteAdapter<'a> {
    repo: EventMemoryRepo<'a>,
}

impl<'a> MemoryWriteAdapter<'a> {
    pub fn new(pool: &'a PgPool) -> Self {
        Self {
            repo: EventMemoryRepo { pool },
        }
    }

    /// Store one event and the edges the rules derive for it.
    ///
    /// `embedding` is the caller's, obtained from the same embedding router the
    /// recall path already uses. `None` is legal and stores a NULL vector — the
    /// event stays reachable through its edges.
    pub async fn write(
        &self,
        context: &MemoryWriteContext,
        metadata: Option<&MemoryMetadata>,
        embedding: Option<&[f32]>,
    ) -> Result<MemoryWriteOutcome, MemoryAdapterError> {
        let Some(raw) = metadata else {
            return Ok(MemoryWriteOutcome::NoMetadata);
        };

        let mut metadata = raw.clone();
        metadata.normalize();
        if let Err(reason) = metadata.validate() {
            tracing::warn!(error = %reason, "memory metadata rejected; event not stored");
            return Ok(MemoryWriteOutcome::Rejected(reason));
        }

        let session_events = self
            .repo
            .session_events(context.session_id, RELATED_CANDIDATE_LIMIT)
            .await?;

        // Event dedupe is deliberately multi-signal. Vector similarity is
        // only the candidate generator; type, participant/scope, recency and a
        // lexical fact signal must also agree. This suppresses a repeated plot
        // description while preserving real stage changes (injury -> hospital
        // -> discharge), whose summaries do not describe the same fact.
        if matches!(
            metadata.memory_type,
            eros_engine_core::event_memory::MemoryType::Callback
                | eros_engine_core::event_memory::MemoryType::Plot
        ) {
            if let Some(existing_event_id) = self
                .event_duplicate(context, &metadata, embedding, &session_events)
                .await?
            {
                tracing::info!(
                    existing_event_id = %existing_event_id,
                    summary = %metadata.summary,
                    "MEMORY_DEDUPE_HIT"
                );
                return Ok(MemoryWriteOutcome::Duplicate { existing_event_id });
            }
        }

        tracing::info!(
            event_type = metadata.memory_type.as_str(),
            summary = %metadata.summary,
            "MEMORY_DEDUPE_NEW"
        );

        // Only a genuine plot progression receives a chronological edge.
        // Callback events are reusable facts, not story stages, and linking
        // every adjacent callback made unrelated memories graph neighbours.
        let previous = if metadata.memory_type == eros_engine_core::event_memory::MemoryType::Plot {
            session_events
                .iter()
                .find(|event| is_plot_progression(event, &metadata, context.created_turn))
                .cloned()
        } else {
            None
        };

        let event_id = self
            .repo
            .insert_event(EventInsert {
                user_id: context.user_id,
                instance_id: context.instance_id,
                session_id: context.session_id,
                summary: metadata.summary.clone(),
                memory_type: metadata.memory_type,
                participants: metadata.participants.clone(),
                location: metadata.location.clone(),
                tags: metadata.tags.clone(),
                importance: metadata.importance,
                knowledge_scope: metadata.knowledge_scope.clone(),
                relationship_relevant: metadata.relationship_relevant,
                story_time: metadata.story_time_utc(),
                source_start_message_id: context.source_start_message_id,
                source_end_message_id: context.source_end_message_id,
                created_turn: context.created_turn,
                embedding: embedding.map(|values| values.to_vec()),
            })
            .await?;

        let candidates = session_events;
        let new_event = NewEventFacts {
            id: event_id,
            session_id: context.session_id,
            participants: metadata.participants.clone(),
            tags: metadata.tags.clone(),
        };
        let previous_prior = previous.as_ref().map(prior_event);
        let candidate_priors: Vec<PriorEvent> = candidates.iter().map(prior_event).collect();

        // `explicit_caused_by` is None for all of V1, and that is a design
        // decision rather than a placeholder: `MemoryMetadata` has no such
        // field, so the only way a `caused` edge can appear is a future change
        // to this call — never an inference from co-occurrence.
        let derived = derive_edges(&new_event, previous_prior.as_ref(), &candidate_priors, None);
        let edges: Vec<EventEdgeInsert> = derived
            .iter()
            .map(|edge| EventEdgeInsert {
                source_event_id: edge.source_event_id,
                target_event_id: edge.target_event_id,
                relation: edge.relation,
            })
            .collect();
        let written = self.repo.insert_edges(&edges).await?;

        if let Some(previous) = previous.as_ref() {
            tracing::info!(
                source_event_id = %previous.id,
                target_event_id = %event_id,
                "MEMORY_PLOT_PROGRESS"
            );
        }

        Ok(MemoryWriteOutcome::Written {
            event_id,
            edges: written as usize,
        })
    }

    /// Store one user-authored memory, skipping it when this user/instance
    /// recently stored the same fact.
    ///
    /// The duplicate check is the containment rule the recall path already uses
    /// to keep `[shared_memories]` from repeating `[Relevant Past Experiences]`
    /// ([`dedupe_against_known`]), pointed at the session's own events instead.
    /// That is the case the two entry points create between them: the main model
    /// trails a callback *and* the user submits the same fact by hand in one
    /// turn, so the manual write runs after the auto one and sees its row.
    ///
    /// Cross-session matching uses the same bounded, multi-signal rule as an
    /// automatic memory write; it never creates a second memory namespace.
    pub async fn write_manual(
        &self,
        context: &MemoryWriteContext,
        metadata: &MemoryMetadata,
        embedding: Option<&[f32]>,
    ) -> Result<MemoryWriteOutcome, MemoryAdapterError> {
        let mut metadata = metadata.clone();
        metadata.normalize();
        if let Err(reason) = metadata.validate() {
            tracing::warn!(error = %reason, "manual memory rejected; event not stored");
            return Ok(MemoryWriteOutcome::Rejected(reason));
        }

        self.write(context, Some(&metadata), embedding).await
    }

    async fn event_duplicate(
        &self,
        context: &MemoryWriteContext,
        metadata: &MemoryMetadata,
        embedding: Option<&[f32]>,
        session_events: &[EventRow],
    ) -> Result<Option<Uuid>, MemoryAdapterError> {
        if let Some(existing) = covering_compatible_event(metadata, session_events) {
            return Ok(Some(existing));
        }

        let Some(embedding) = embedding else {
            return Ok(None);
        };
        let viewer = metadata.knowledge_scope.first().map(String::as_str);
        let hits = self
            .repo
            .anchor_search(
                context.user_id,
                context.instance_id,
                viewer,
                embedding,
                DEDUPE_ANCHOR_LIMIT,
            )
            .await?;
        Ok(hits.into_iter().find_map(|hit| {
            event_duplicate_matches(metadata, &hit.event, hit.similarity).then_some(hit.event.id)
        }))
    }
}

fn covering_compatible_event(metadata: &MemoryMetadata, events: &[EventRow]) -> Option<Uuid> {
    let key = normalize_for_dedup(&metadata.summary);
    if key.is_empty() {
        return None;
    }
    events.iter().find_map(|event| {
        if !compatible_event_scope(metadata, event)
            || !event_within_dedupe_window(event)
            || is_distinct_plot_stage_or_occurrence(metadata, event)
        {
            return None;
        }
        let known = normalize_for_dedup(&event.summary);
        (known.chars().count() >= MIN_DEDUP_CHARS
            && (key.contains(known.as_str()) || known.contains(&key)))
        .then_some(event.id)
    })
}

fn event_duplicate_matches(metadata: &MemoryMetadata, event: &EventRow, similarity: f64) -> bool {
    if similarity < DEDUPE_CONDITIONAL_SIMILARITY
        || !compatible_event_scope(metadata, event)
        || !event_within_dedupe_window(event)
        || is_distinct_plot_stage_or_occurrence(metadata, event)
    {
        return false;
    }

    let left = normalize_for_dedup(&metadata.summary);
    let right = normalize_for_dedup(&event.summary);
    let containment = left.contains(&right) || right.contains(&left);
    let lexical_overlap = char_bigram_jaccard(&left, &right) >= CALLBACK_DEDUPE_LEXICAL_OVERLAP;
    let same_plot_stage = metadata.memory_type == eros_engine_core::event_memory::MemoryType::Plot
        && plot_stage_mask(&left) != 0
        && plot_stage_mask(&left) == plot_stage_mask(&right);
    if !containment && !lexical_overlap && !same_plot_stage {
        return false;
    }

    similarity >= DEDUPE_HIGH_SIMILARITY
        || explicit_participant_match(metadata, event, &left, &right)
}

fn event_within_dedupe_window(event: &EventRow) -> bool {
    Utc::now().signed_duration_since(event.created_at)
        <= chrono::Duration::hours(CALLBACK_DEDUPE_MAX_AGE_HOURS)
}

fn compatible_event_scope(metadata: &MemoryMetadata, event: &EventRow) -> bool {
    if event.memory_type() != Some(metadata.memory_type) {
        return false;
    }
    let mut candidate_scope: Vec<String> = metadata
        .knowledge_scope
        .iter()
        .map(|value| normalize_for_dedup(value))
        .collect();
    let mut known_scope: Vec<String> = event
        .knowledge_scope
        .iter()
        .map(|value| normalize_for_dedup(value))
        .collect();
    candidate_scope.sort();
    known_scope.sort();
    if candidate_scope != known_scope {
        return false;
    }

    // Older metadata can legitimately omit participants. Treat an omitted
    // side as unknown rather than a conflicting participant set; two explicit,
    // disjoint participant sets still prevent a merge.
    metadata.participants.is_empty()
        || event.participants.is_empty()
        || metadata.participants.iter().any(|participant| {
            event
                .participants
                .iter()
                .any(|known| normalize_for_dedup(participant) == normalize_for_dedup(known))
        })
}

/// The conditional similarity band needs positive participant evidence. An
/// empty participants array is not evidence by itself. When older metadata
/// omitted that array, an explicit participant label in both summaries can
/// supply the evidence without guessing an actor from the surrounding session.
fn explicit_participant_match(
    metadata: &MemoryMetadata,
    event: &EventRow,
    candidate_summary: &str,
    known_summary: &str,
) -> bool {
    if !metadata.participants.is_empty() && !event.participants.is_empty() {
        return metadata.participants.iter().any(|participant| {
            event
                .participants
                .iter()
                .any(|known| normalize_for_dedup(participant) == normalize_for_dedup(known))
        });
    }

    if !metadata.participants.is_empty() {
        return metadata.participants.iter().any(|participant| {
            let participant = normalize_for_dedup(participant);
            participant.chars().count() >= 2 && known_summary.contains(&participant)
        });
    }
    if !event.participants.is_empty() {
        return event.participants.iter().any(|participant| {
            let participant = normalize_for_dedup(participant);
            participant.chars().count() >= 2 && candidate_summary.contains(&participant)
        });
    }

    // These are explicit role labels, not inferred participants. Pronouns such
    // as "我"/"他" are intentionally excluded because they are too weak to
    // distinguish two otherwise similar events.
    const SUMMARY_PARTICIPANT_LABELS: [&str; 4] = ["用户", "助手", "角色", "对方"];
    SUMMARY_PARTICIPANT_LABELS
        .iter()
        .any(|label| candidate_summary.contains(label) && known_summary.contains(label))
}

/// Plot nodes represent state transitions. Even a high vector similarity must
/// not collapse injury -> worsening -> hospitalization -> discharge, or a
/// clearly numbered later occurrence of the same kind of event.
fn is_distinct_plot_stage_or_occurrence(metadata: &MemoryMetadata, event: &EventRow) -> bool {
    if metadata.memory_type != eros_engine_core::event_memory::MemoryType::Plot
        || event.memory_type() != Some(eros_engine_core::event_memory::MemoryType::Plot)
    {
        return false;
    }

    let candidate = normalize_for_dedup(&metadata.summary);
    let known = normalize_for_dedup(&event.summary);
    let candidate_stages = plot_stage_mask(&candidate);
    let known_stages = plot_stage_mask(&known);
    if candidate_stages != 0 && known_stages != 0 && candidate_stages != known_stages {
        return true;
    }

    occurrence_marker(&candidate) != occurrence_marker(&known)
        && occurrence_marker(&candidate).is_some()
}

fn plot_stage_mask(summary: &str) -> u16 {
    const STAGES: [(u16, &[&str]); 11] = [
        (1 << 0, &["受伤", "划伤", "重伤", "伤口"]),
        (1 << 1, &["恶化", "加重", "再次出血", "感染"]),
        (1 << 2, &["住院", "入院", "留院"]),
        (1 << 3, &["手术", "缝合", "治疗"]),
        (1 << 4, &["出院", "离院"]),
        (1 << 5, &["康复", "恢复", "痊愈"]),
        (1 << 6, &["争吵", "吵架", "冲突"]),
        (1 << 7, &["冷战", "僵持"]),
        (1 << 8, &["道歉", "和解", "缓和", "好好沟通", "好好说开"]),
        (1 << 9, &["失踪", "失联"]),
        (1 << 10, &["被找到", "找回", "寻回"]),
    ];

    STAGES.iter().fold(0, |mask, (bit, terms)| {
        if terms.iter().any(|term| summary.contains(term)) {
            mask | bit
        } else {
            mask
        }
    })
}

fn occurrence_marker(summary: &str) -> Option<u8> {
    if summary.contains("第一次") || summary.contains("首次") {
        Some(1)
    } else if summary.contains("第二次") {
        Some(2)
    } else if summary.contains("第三次") {
        Some(3)
    } else if ["又一次", "再次发生", "新一轮", "另一场", "另一次"]
        .iter()
        .any(|marker| summary.contains(marker))
    {
        Some(255)
    } else {
        None
    }
}

fn char_bigram_jaccard(left: &str, right: &str) -> f64 {
    use std::collections::HashSet;

    fn bigrams(value: &str) -> HashSet<(char, char)> {
        let chars: Vec<char> = value.chars().collect();
        chars.windows(2).map(|pair| (pair[0], pair[1])).collect()
    }

    let left = bigrams(left);
    let right = bigrams(right);
    if left.is_empty() || right.is_empty() {
        return 0.0;
    }
    let intersection = left.intersection(&right).count() as f64;
    let union = left.union(&right).count() as f64;
    intersection / union
}

fn is_plot_progression(event: &EventRow, metadata: &MemoryMetadata, created_turn: i32) -> bool {
    if event.memory_type() != Some(eros_engine_core::event_memory::MemoryType::Plot)
        || (created_turn - event.created_turn).abs() > 6
    {
        return false;
    }
    let participant_match = metadata.participants.iter().any(|participant| {
        event
            .participants
            .iter()
            .any(|known| normalize_for_dedup(participant) == normalize_for_dedup(known))
    });
    if !participant_match && !(metadata.participants.is_empty() && event.participants.is_empty()) {
        return false;
    }
    let tag_match = metadata.tags.iter().any(|tag| {
        event
            .tags
            .iter()
            .any(|known| normalize_for_dedup(tag) == normalize_for_dedup(known))
    });
    let location_match = metadata.location.as_ref().is_some_and(|location| {
        event
            .location
            .as_ref()
            .is_some_and(|known| normalize_for_dedup(location) == normalize_for_dedup(known))
    });
    tag_match || location_match || (created_turn - event.created_turn).abs() <= 3
}

/// The store row, flattened to the shape the edge rules take.
fn prior_event(row: &EventRow) -> PriorEvent {
    PriorEvent {
        id: row.id,
        session_id: row.session_id,
        participants: row.participants.clone(),
        tags: row.tags.clone(),
        created_at: row.created_at,
    }
}

/// How many vector anchors one recall may start from. Eight is enough to reach
/// every event a normal scene could callback to, and small enough that the
/// 1-hop expansion below stays a constant amount of work.
pub const DEFAULT_ANCHOR_LIMIT: i64 = 8;
/// Ceiling on the 1-hop candidate pool, before scoring.
pub const DEFAULT_EXPAND_LIMIT: i64 = 24;
/// The prompt gets at most this many experiences. Three is the point where a
/// list stops reading as "a memory surfaced" and starts reading as a summary.
pub const DEFAULT_MAX_RESULTS: usize = 3;
/// A 1-hop neighbour has no similarity of its own — only the anchor that led to
/// it does. Inheriting the best anchor's similarity, discounted, is the cheap
/// honest version of "as relevant as the thing that pointed at it"; a per-anchor
/// expansion would attribute it exactly but costs one query per anchor.
pub const NEIGHBOUR_SIMILARITY_DISCOUNT: f64 = 0.8;

/// One recall, fully specified. `Default` is not implemented because five of
/// these fields have no safe default — the caller must say whose memory this is.
#[derive(Debug, Clone, PartialEq)]
pub struct RetrievalQuery {
    pub user_id: Uuid,
    pub instance_id: Uuid,
    /// Session that issued this recall. Telemetry only: the selection logic
    /// never reads it. It exists so every recall log line emitted below can be
    /// attributed to one session after the fact.
    pub session_id: Option<Uuid>,
    /// Point-of-view character for knowledge-scope filtering. `None` means no
    /// POV was resolved, which sees public events only.
    pub viewer: Option<String>,
    /// Participants present in the current scene, for overlap weighting.
    pub participants: Vec<String>,
    pub query_embedding: Vec<f32>,
    /// Recall-time clock. Passed in rather than read from `Utc::now()` inside
    /// the adapter so a test can pin it — and so the caller can guarantee the
    /// write and read halves of one turn agree on an instant.
    pub now: DateTime<Utc>,
    /// Texts the prompt injects anyway (`[shared_memories]`), so the same
    /// history is not told twice in one turn.
    pub known_memories: Vec<String>,
    pub anchor_limit: i64,
    pub expand_limit: i64,
    pub max_results: usize,
    pub min_score: f64,
}

impl RetrievalQuery {
    pub fn new(user_id: Uuid, instance_id: Uuid, query_embedding: Vec<f32>) -> Self {
        Self {
            user_id,
            instance_id,
            session_id: None,
            viewer: None,
            participants: Vec::new(),
            query_embedding,
            now: Utc::now(),
            known_memories: Vec::new(),
            anchor_limit: DEFAULT_ANCHOR_LIMIT,
            expand_limit: DEFAULT_EXPAND_LIMIT,
            max_results: DEFAULT_MAX_RESULTS,
            min_score: DEFAULT_MIN_RECALL_SCORE,
        }
    }
}

/// One event chosen for injection. Carries the score and the distance for logs
/// and tests; neither reaches the prompt.
#[derive(Debug, Clone, PartialEq)]
pub struct RecalledExperience {
    pub event_id: Uuid,
    pub summary: String,
    pub importance: EventImportance,
    pub graph_distance: u8,
    pub score: f64,
}

pub struct MemoryRetrievalAdapter<'a> {
    repo: EventMemoryRepo<'a>,
}

impl<'a> MemoryRetrievalAdapter<'a> {
    pub fn new(pool: &'a PgPool) -> Self {
        Self {
            repo: EventMemoryRepo { pool },
        }
    }

    /// Anchor → expand → score → filter → take. Six steps, no model.
    ///
    /// Selection mutates state: the events that come back are marked recalled,
    /// which is what feeds the cooldown that keeps "难喝饮料" from surfacing at
    /// every convenience store. That write is why this is `&self` rather than a
    /// free function, and why a caller must not call it speculatively.
    pub async fn retrieve(
        &self,
        query: &RetrievalQuery,
    ) -> Result<Vec<RecalledExperience>, MemoryAdapterError> {
        let retrieval_started = std::time::Instant::now();
        let viewer = query.viewer.as_deref();

        let anchor_started = std::time::Instant::now();
        let anchors = self
            .repo
            .anchor_search(
                query.user_id,
                query.instance_id,
                viewer,
                &query.query_embedding,
                query.anchor_limit,
            )
            .await?;
        tracing::info!(
            session_id = ?query.session_id,
            latency_ms = anchor_started.elapsed().as_millis() as u64,
            candidate_count = anchors.len(),
            best_anchor_similarity = anchors.first().map(|hit| hit.similarity).unwrap_or(0.0),
            "VECTOR_ANCHOR"
        );
        // `anchor_search` orders by similarity descending, so the first hit is
        // the best one and the neighbours below can inherit its relevance.
        let best_anchor_similarity = anchors.first().map(|hit| hit.similarity).unwrap_or(0.0);

        let anchor_ids: Vec<Uuid> = anchors.iter().map(|hit| hit.event.id).collect();
        let anchor_similarities: Vec<f64> = anchors.iter().map(|hit| hit.similarity).collect();
        tracing::info!(
            session_id = ?query.session_id,
            anchor_event_ids = ?anchor_ids,
            anchor_similarities = ?anchor_similarities,
            "MEMORY_RECALL anchors"
        );
        let graph_started = std::time::Instant::now();
        let neighbours = if anchor_ids.is_empty() {
            Vec::new()
        } else {
            self.repo
                .expand_one_hop(
                    &anchor_ids,
                    query.user_id,
                    query.instance_id,
                    viewer,
                    query.expand_limit,
                )
                .await?
        };
        tracing::info!(
            session_id = ?query.session_id,
            latency_ms = graph_started.elapsed().as_millis() as u64,
            anchor_count = anchor_ids.len(),
            expanded_count = neighbours.len(),
            "GRAPH_EXPANSION"
        );
        let expanded_ids: Vec<Uuid> = neighbours.iter().map(|row| row.id).collect();
        tracing::info!(
            session_id = ?query.session_id,
            expanded_event_ids = ?expanded_ids,
            "MEMORY_RECALL graph expanded"
        );

        let mut scored: Vec<RecalledExperience> =
            Vec::with_capacity(anchors.len() + neighbours.len());
        let mut scope_denied_ids = Vec::new();
        let mut cooldown_applied_ids = Vec::new();
        for hit in &anchors {
            if !knowledge_scope_allows(&hit.event.knowledge_scope, viewer) {
                scope_denied_ids.push(hit.event.id);
                continue;
            }
            if hit.event.recall_count > 0 && hit.event.last_recalled_at.is_some() {
                cooldown_applied_ids.push(hit.event.id);
            }
            if let Some(experience) = score_row(&hit.event, 0, hit.similarity, query) {
                scored.push(experience);
            }
        }
        let neighbour_similarity = best_anchor_similarity * NEIGHBOUR_SIMILARITY_DISCOUNT;
        for row in &neighbours {
            if !knowledge_scope_allows(&row.knowledge_scope, viewer) {
                scope_denied_ids.push(row.id);
                continue;
            }
            if row.recall_count > 0 && row.last_recalled_at.is_some() {
                cooldown_applied_ids.push(row.id);
            }
            if let Some(experience) = score_row(row, 1, neighbour_similarity, query) {
                scored.push(experience);
            }
        }
        tracing::info!(
            session_id = ?query.session_id,
            denied_event_ids = ?scope_denied_ids,
            viewer = viewer.unwrap_or("<public>"),
            "KNOWLEDGE_SCOPE filtered"
        );

        scored.sort_by(|left, right| {
            right
                .score
                .partial_cmp(&left.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                // A total order: two events with the same score must not swap
                // places between two runs of the same turn.
                .then(right.event_id.cmp(&left.event_id))
        });

        // Dedup before truncating, not after: dropping a duplicate should let
        // the next-best experience through rather than shrink the list.
        let summaries: Vec<String> = scored
            .iter()
            .map(|experience| experience.summary.clone())
            .collect();
        let mut available = dedupe_against_known(&summaries, &query.known_memories);

        let mut results: Vec<RecalledExperience> = Vec::with_capacity(query.max_results);
        for experience in scored {
            if let Some(index) = available
                .iter()
                .position(|summary| summary == &experience.summary)
            {
                available.remove(index);
                results.push(experience);
                if results.len() == query.max_results {
                    break;
                }
            }
        }

        let recalled: Vec<Uuid> = results
            .iter()
            .map(|experience| experience.event_id)
            .collect();
        if !recalled.is_empty() {
            self.repo.mark_recalled(&recalled).await?;
        }
        tracing::info!(
            session_id = ?query.session_id,
            applied_count = cooldown_applied_ids.len(),
            event_ids = ?cooldown_applied_ids,
            "RECALL_COOLDOWN"
        );

        let selected: std::collections::HashSet<Uuid> = recalled.iter().copied().collect();
        let filtered_ids: Vec<Uuid> = anchor_ids
            .iter()
            .chain(expanded_ids.iter())
            .copied()
            .filter(|id| !selected.contains(id))
            .collect();
        let final_scores: Vec<(Uuid, f64)> = results
            .iter()
            .map(|experience| (experience.event_id, experience.score))
            .collect();
        tracing::info!(
            session_id = ?query.session_id,
            filtered_event_ids = ?filtered_ids,
            final_event_ids = ?recalled,
            final_scores = ?final_scores,
            cooldown_applied_event_ids = ?cooldown_applied_ids,
            "MEMORY_RECALL selected"
        );
        for (rank, experience) in results.iter().enumerate() {
            tracing::info!(
                session_id = ?query.session_id,
                rank = rank + 1,
                event_id = %experience.event_id,
                score = experience.score,
                graph_distance = experience.graph_distance,
                importance = ?experience.importance,
                "RECALL_TOP_K"
            );
        }
        tracing::info!(
            session_id = ?query.session_id,
            latency_ms = retrieval_started.elapsed().as_millis() as u64,
            selected_count = results.len(),
            "RECALL_RETRIEVAL_TOTAL"
        );

        Ok(results)
    }
}

/// Score one candidate, or reject it.
///
/// The scope check is repeated here even though every query already filters on
/// it. That is not redundancy for its own sake: this is the last function before
/// text reaches a prompt, and a future caller that reaches the store through a
/// different query must still not be able to leak 裴烬's secret to 白芷.
fn score_row(
    row: &EventRow,
    graph_distance: u8,
    vector_similarity: f64,
    query: &RetrievalQuery,
) -> Option<RecalledExperience> {
    if !knowledge_scope_allows(&row.knowledge_scope, query.viewer.as_deref()) {
        return None;
    }
    let importance = row.importance()?;
    let (participant_overlap, participant_union) =
        participant_overlap(&query.participants, &row.participants);
    let age_days = (query.now - row.created_at).num_seconds().max(0) as f64 / 86_400.0;
    let days_since_recall = row
        .last_recalled_at
        .map(|last_recalled| (query.now - last_recalled).num_seconds().max(0) as f64 / 86_400.0);

    let score = recall_score(&RecallSignals {
        vector_similarity,
        importance,
        participant_overlap,
        participant_union,
        graph_distance,
        age_days,
        recall_count: row.recall_count,
        days_since_recall,
    });
    if score < query.min_score {
        return None;
    }

    Some(RecalledExperience {
        event_id: row.id,
        summary: row.summary.clone(),
        importance,
        graph_distance,
        score,
    })
}

/// The `[Relevant Past Experiences]` block, or `None` when there is nothing to
/// inject — so a turn with no recall leaves the prompt byte-identical.
///
/// Summaries only. Importance, score, graph distance and every database field
/// stay in the runtime.
pub fn render_experiences(experiences: &[RecalledExperience]) -> Option<String> {
    let summaries: Vec<String> = experiences
        .iter()
        .map(|experience| experience.summary.clone())
        .collect();
    render_past_experiences(&summaries)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A deterministic 512-dim unit vector. Similarity is 1.0 between two of
    /// these sharing a hot index and 0.0 otherwise, so ordering can be asserted
    /// without knowing anything about the real embedder.
    fn unit_embedding(hot: usize) -> Vec<f32> {
        let mut values = vec![0.0_f32; 512];
        values[hot % 512] = 1.0;
        values
    }

    struct Fixture {
        context: MemoryWriteContext,
        session_id: Uuid,
    }

    async fn fixture(pool: &PgPool) -> Fixture {
        let user_id = Uuid::new_v4();
        let genome_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.persona_genomes (name, system_prompt, art_metadata) \
             VALUES ($1, 'you are a companion', '{}'::jsonb) RETURNING id",
        )
        .bind(format!("seed-{user_id}"))
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
            "INSERT INTO engine.chat_sessions (user_id, instance_id) \
             VALUES ($1, $2) RETURNING id",
        )
        .bind(user_id)
        .bind(instance_id)
        .fetch_one(pool)
        .await
        .unwrap();
        let start: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_messages (session_id, role, content) \
             VALUES ($1, 'user', 'seed') RETURNING id",
        )
        .bind(session_id)
        .fetch_one(pool)
        .await
        .unwrap();
        let end: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_messages (session_id, role, content) \
             VALUES ($1, 'assistant', 'seed') RETURNING id",
        )
        .bind(session_id)
        .fetch_one(pool)
        .await
        .unwrap();

        Fixture {
            context: MemoryWriteContext {
                user_id,
                instance_id,
                session_id,
                source_start_message_id: start,
                source_end_message_id: end,
                created_turn: 1,
            },
            session_id,
        }
    }

    fn metadata(summary: &str) -> MemoryMetadata {
        MemoryMetadata {
            memory_type: eros_engine_core::event_memory::MemoryType::Callback,
            summary: summary.to_string(),
            participants: vec!["白芷".into(), "裴烬".into()],
            location: Some("便利店".into()),
            tags: vec!["饮料".into()],
            importance: EventImportance::Normal,
            knowledge_scope: Vec::new(),
            relationship_relevant: true,
            story_time: None,
            world_facts: Vec::new(),
        }
    }

    fn event_row(
        summary: &str,
        memory_type: eros_engine_core::event_memory::MemoryType,
        participants: &[&str],
    ) -> EventRow {
        EventRow {
            id: Uuid::new_v4(),
            user_id: Uuid::new_v4(),
            instance_id: Uuid::new_v4(),
            session_id: Uuid::new_v4(),
            summary: summary.to_string(),
            event_type: memory_type.as_str().to_string(),
            participants: participants.iter().map(|value| value.to_string()).collect(),
            location: None,
            tags: Vec::new(),
            importance: EventImportance::Normal.as_str().to_string(),
            knowledge_scope: Vec::new(),
            relationship_relevant: false,
            story_time: None,
            source_start_message_id: Uuid::new_v4(),
            source_end_message_id: Uuid::new_v4(),
            created_at: Utc::now(),
            created_turn: 1,
            salience: EventImportance::Normal.weight(),
            recall_count: 0,
            last_recalled_at: None,
            is_active: true,
        }
    }

    fn dedupe_candidate(
        summary: &str,
        memory_type: eros_engine_core::event_memory::MemoryType,
        participants: &[&str],
    ) -> MemoryMetadata {
        let mut value = metadata(summary);
        value.memory_type = memory_type;
        value.participants = participants.iter().map(|value| value.to_string()).collect();
        value.location = None;
        value.tags.clear();
        value.relationship_relevant = false;
        value
    }

    #[test]
    fn conditional_band_dedupes_the_three_observed_cross_session_facts() {
        let cases = [
            (
                "用户在回家路上遭遇冲突，我为护住用户右手被严重划伤，伤口仍在流血",
                "回家路上发生冲突，Aria为保护用户右手被划伤，伤口严重流血",
                eros_engine_core::event_memory::MemoryType::Plot,
                0.8223,
            ),
            (
                "用户走楼梯踩空一阶，Aria及时拉住，用户未受伤但心跳很快",
                "用户在楼梯踩空一阶，我及时拉住他，他没有受伤但心跳很快",
                eros_engine_core::event_memory::MemoryType::Callback,
                0.8701,
            ),
            (
                "用户主动道歉，双方决定停止僵持并好好沟通，关系开始缓和",
                "用户为之前伤人的话道歉，表示想结束僵持，好好说开",
                eros_engine_core::event_memory::MemoryType::Plot,
                0.8220,
            ),
        ];

        for (candidate, known, memory_type, similarity) in cases {
            let metadata = dedupe_candidate(candidate, memory_type, &[]);
            let event = event_row(known, memory_type, &[]);
            assert!(
                event_duplicate_matches(&metadata, &event, similarity),
                "observed duplicate was missed: {candidate}"
            );
        }
    }

    #[test]
    fn plot_progression_and_participant_boundaries_prevent_false_merges() {
        let plot = eros_engine_core::event_memory::MemoryType::Plot;
        let callback = eros_engine_core::event_memory::MemoryType::Callback;
        let cases = [
            (
                dedupe_candidate("同一处伤口的伤势随后明显恶化", plot, &["用户", "Aria"]),
                event_row("用户手部在冲突中受伤", plot, &["用户", "Aria"]),
                "injury -> worsening",
            ),
            (
                dedupe_candidate("治疗数日后用户伤势稳定并出院", plot, &["用户", "Aria"]),
                event_row("用户因伤住院接受治疗", plot, &["用户", "Aria"]),
                "hospitalization -> discharge",
            ),
            (
                dedupe_candidate("用户与Aria发生第二次新的争吵", plot, &["用户", "Aria"]),
                event_row("用户与Aria发生第一次争吵", plot, &["用户", "Aria"]),
                "first quarrel -> second quarrel",
            ),
            (
                dedupe_candidate("白芷走楼梯踩空后被裴烬拉住", callback, &["白芷", "裴烬"]),
                event_row("林默走楼梯踩空后被苏禾拉住", callback, &["林默", "苏禾"]),
                "different participants",
            ),
        ];

        for (candidate, known, label) in cases {
            assert!(
                !event_duplicate_matches(&candidate, &known, 0.95),
                "distinct event was merged: {label}"
            );
        }

        let prior_hospital = event_row("用户因伤住院接受治疗", plot, &["用户", "Aria"]);
        let discharge = dedupe_candidate(
            "用户因伤住院治疗数日后伤势稳定并出院",
            plot,
            &["用户", "Aria"],
        );
        assert_eq!(
            covering_compatible_event(&discharge, &[prior_hospital]),
            None,
            "the containment fast path must preserve hospitalization -> discharge"
        );
    }

    #[test]
    fn conditional_band_rejects_low_similarity_stale_and_unknown_participant_matches() {
        let callback = eros_engine_core::event_memory::MemoryType::Callback;
        let explicit = dedupe_candidate(
            "用户走楼梯踩空一阶，Aria及时拉住",
            callback,
            &["用户", "Aria"],
        );
        let recent = event_row(
            "用户在楼梯踩空一阶，被Aria及时拉住",
            callback,
            &["用户", "Aria"],
        );
        assert!(!event_duplicate_matches(&explicit, &recent, 0.8199));

        let mut stale = recent.clone();
        stale.created_at = Utc::now() - chrono::Duration::hours(25);
        assert!(!event_duplicate_matches(&explicit, &stale, 0.95));

        let unknown = dedupe_candidate("雨夜在旧桥边找到遗失的纸袋", callback, &[]);
        let unknown_prior = event_row("在旧桥边的雨夜找回了那个纸袋", callback, &[]);
        assert!(!event_duplicate_matches(&unknown, &unknown_prior, 0.87));
    }

    async fn stored_events(pool: &PgPool) -> i64 {
        sqlx::query_scalar("SELECT count(*) FROM engine.recent_episodes")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn recall_state(pool: &PgPool, id: Uuid) -> (i32, bool) {
        let row: (i32, Option<DateTime<Utc>>) = sqlx::query_as(
            "SELECT recall_count, last_recalled_at FROM engine.recent_episodes WHERE id = $1",
        )
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap();
        (row.0, row.1.is_some())
    }

    // ── Write ─────────────────────────────────────────────────────────────

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn write_stores_the_event_and_leaves_the_first_one_unlinked(pool: PgPool) {
        let fixture = fixture(&pool).await;
        let adapter = MemoryWriteAdapter::new(&pool);

        let outcome = adapter
            .write(
                &fixture.context,
                Some(&metadata("白芷给裴烬买过一瓶难喝的饮料")),
                Some(&unit_embedding(4)),
            )
            .await
            .unwrap();

        let MemoryWriteOutcome::Written { event_id, edges } = outcome else {
            panic!("a valid metadata block must be stored, got {outcome:?}");
        };
        assert_eq!(edges, 0, "nothing precedes the first event");

        let row: (String, Vec<String>, Option<String>, String) = sqlx::query_as(
            "SELECT summary, participants, location, event_type FROM engine.recent_episodes WHERE id = $1",
        )
        .bind(event_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.0, "白芷给裴烬买过一瓶难喝的饮料");
        assert_eq!(row.1, vec!["白芷", "裴烬"]);
        assert_eq!(row.2.as_deref(), Some("便利店"));
        assert_eq!(row.3, "callback");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn write_derives_edges_without_asking_a_model(pool: PgPool) {
        let fixture = fixture(&pool).await;
        let adapter = MemoryWriteAdapter::new(&pool);

        adapter
            .write(
                &fixture.context,
                Some(&metadata("第一次一起去便利店")),
                None,
            )
            .await
            .unwrap();
        let second = adapter
            .write(
                &fixture.context,
                Some(&metadata("又去了同一家便利店")),
                None,
            )
            .await
            .unwrap();
        let MemoryWriteOutcome::Written { edges, .. } = second else {
            panic!("second write must store");
        };
        assert_eq!(edges, 1, "the shared cast and tag create `related_to`");

        let third = adapter
            .write(
                &fixture.context,
                Some(&metadata("第三次去便利店买了同样的饮料")),
                None,
            )
            .await
            .unwrap();
        let MemoryWriteOutcome::Written { edges, .. } = third else {
            panic!("third write must store");
        };
        assert_eq!(
            edges, 2,
            "the callback can be related to both older events, without chronological plot edges"
        );

        let relations: Vec<String> = sqlx::query_scalar(
            "SELECT relation_type FROM engine.event_edges ORDER BY relation_type",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(relations, vec!["related_to", "related_to", "related_to"]);
        let followed_by: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM engine.event_edges WHERE relation_type = 'followed_by'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(followed_by, 0, "callbacks must not form plot chronology");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn write_is_fail_open_when_the_metadata_breaks_the_contract(pool: PgPool) {
        let fixture = fixture(&pool).await;
        let adapter = MemoryWriteAdapter::new(&pool);

        let blank = metadata("   ");
        let outcome = adapter
            .write(&fixture.context, Some(&blank), None)
            .await
            .unwrap();
        assert_eq!(
            outcome,
            MemoryWriteOutcome::Rejected(MemoryMetadataError::EmptySummary)
        );
        assert_eq!(
            stored_events(&pool).await,
            0,
            "a rejected block must not leave a partial row"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn write_without_metadata_stores_nothing(pool: PgPool) {
        let fixture = fixture(&pool).await;
        let adapter = MemoryWriteAdapter::new(&pool);

        let outcome = adapter.write(&fixture.context, None, None).await.unwrap();
        assert_eq!(outcome, MemoryWriteOutcome::NoMetadata);
        assert_eq!(stored_events(&pool).await, 0);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn write_normalizes_before_validating(pool: PgPool) {
        let fixture = fixture(&pool).await;
        let adapter = MemoryWriteAdapter::new(&pool);
        let mut padded = metadata("  两人约好周日上午十点一起去松林公园  ");
        padded.participants = vec![" 白芷 ".into(), "白芷".into(), "裴烬".into()];
        padded.tags = vec![" 公园 ".into(), "公园".into()];

        let outcome = adapter
            .write(&fixture.context, Some(&padded), None)
            .await
            .unwrap();
        let MemoryWriteOutcome::Written { event_id, .. } = outcome else {
            panic!("padding must not reject an otherwise valid block");
        };

        let row: (String, Vec<String>, Vec<String>) = sqlx::query_as(
            "SELECT summary, participants, tags FROM engine.recent_episodes WHERE id = $1",
        )
        .bind(event_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.0, "两人约好周日上午十点一起去松林公园");
        assert_eq!(row.1, vec!["白芷", "裴烬"]);
        assert_eq!(row.2, vec!["公园"]);
    }

    // ── Manual memory ─────────────────────────────────────────────────────

    fn manual(summary: &str) -> MemoryMetadata {
        eros_engine_core::event_memory::manual_memory_metadata(summary, None).unwrap()
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn manual_memory_stores_with_its_provenance_tag_and_defaults(pool: PgPool) {
        let fixture = fixture(&pool).await;
        let adapter = MemoryWriteAdapter::new(&pool);

        let outcome = adapter
            .write_manual(
                &fixture.context,
                &manual("用户第一次喝到便利店的新品饮料，觉得非常难喝"),
                Some(&unit_embedding(7)),
            )
            .await
            .unwrap();
        let MemoryWriteOutcome::Written { event_id, edges } = outcome else {
            panic!("a valid manual memory must store, got {outcome:?}");
        };
        assert_eq!(edges, 0, "nothing precedes the first event");

        let row: (String, String, Vec<String>, bool) = sqlx::query_as(
            "SELECT event_type, importance, tags, embedding IS NOT NULL \
             FROM engine.recent_episodes WHERE id = $1",
        )
        .bind(event_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(row.0, "callback", "the default type");
        assert_eq!(row.1, "important", "the default tier");
        assert_eq!(
            row.2,
            vec![eros_engine_core::event_memory::MANUAL_MEMORY_TAG.to_string()],
            "provenance rides in tags — there is no provenance column"
        );
        assert!(row.3, "a manual memory is embedded like any other event");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn manual_memory_is_suppressed_when_the_same_turn_already_trailed_it(pool: PgPool) {
        // The two entry points in one turn: the model trails the fact, then the
        // user submits the same fact by hand. Exactly one event survives.
        let fixture = fixture(&pool).await;
        let adapter = MemoryWriteAdapter::new(&pool);

        let auto = manual("用户买到一瓶新品饮料，觉得非常难喝，只喝两口就扔掉了");
        let MemoryWriteOutcome::Written {
            event_id: auto_id, ..
        } = adapter
            .write(&fixture.context, Some(&auto), None)
            .await
            .unwrap()
        else {
            panic!("the auto event must store first");
        };

        let duplicate = manual(" 用户买到一瓶新品饮料，觉得非常难喝，只喝两口就扔掉了。 ");
        let outcome = adapter
            .write_manual(&fixture.context, &duplicate, None)
            .await
            .unwrap();
        assert_eq!(
            outcome,
            MemoryWriteOutcome::Duplicate {
                existing_event_id: auto_id
            },
            "punctuation and padding must not defeat the containment rule"
        );
        assert_eq!(
            stored_events(&pool).await,
            1,
            "the same fact must not become two events"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn manual_memory_still_stores_when_the_session_holds_something_else(pool: PgPool) {
        let fixture = fixture(&pool).await;
        let adapter = MemoryWriteAdapter::new(&pool);

        adapter
            .write(
                &fixture.context,
                Some(&metadata("白芷给裴烬买过一瓶难喝的饮料")),
                None,
            )
            .await
            .unwrap();
        let outcome = adapter
            .write_manual(
                &fixture.context,
                &manual("两人约好周日去松林公园拍秋叶"),
                None,
            )
            .await
            .unwrap();
        assert!(
            matches!(outcome, MemoryWriteOutcome::Written { .. }),
            "an unrelated fact must not be deduped away, got {outcome:?}"
        );
        assert_eq!(stored_events(&pool).await, 2);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn callback_paraphrase_is_deduped_even_after_several_turns(pool: PgPool) {
        let fixture = fixture(&pool).await;
        let adapter = MemoryWriteAdapter::new(&pool);
        let first = metadata("白芷在便利店买到新品饮料，觉得很难喝，只喝两口就扔了");
        let first_id = expect_written(
            adapter
                .write(&fixture.context, Some(&first), Some(&unit_embedding(7)))
                .await
                .unwrap(),
        );

        let mut later = fixture.context.clone();
        later.created_turn = 20;
        let paraphrase = metadata("白芷买的便利店新品饮料特别难喝，尝了两口便扔掉了");
        let outcome = adapter
            .write(&later, Some(&paraphrase), Some(&unit_embedding(7)))
            .await
            .unwrap();

        assert_eq!(
            outcome,
            MemoryWriteOutcome::Duplicate {
                existing_event_id: first_id
            }
        );
        assert_eq!(stored_events(&pool).await, 1);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn callback_paraphrase_is_deduped_across_sessions_when_prior_participants_are_unknown(
        pool: PgPool,
    ) {
        let fixture = fixture(&pool).await;
        let adapter = MemoryWriteAdapter::new(&pool);
        let mut first = metadata("用户嘴上嫌弃我挑的丑猫咪钥匙扣，其实特别喜欢");
        first.participants.clear();
        let first_id = expect_written(
            adapter
                .write(&fixture.context, Some(&first), Some(&unit_embedding(9)))
                .await
                .unwrap(),
        );

        let next_session_id: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_sessions (user_id, instance_id) VALUES ($1, $2) RETURNING id",
        )
        .bind(fixture.context.user_id)
        .bind(fixture.context.instance_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        let start: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_messages (session_id, role, content) VALUES ($1, 'user', 'seed') RETURNING id",
        )
        .bind(next_session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        let end: Uuid = sqlx::query_scalar(
            "INSERT INTO engine.chat_messages (session_id, role, content) VALUES ($1, 'assistant', 'seed') RETURNING id",
        )
        .bind(next_session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        let next_context = MemoryWriteContext {
            session_id: next_session_id,
            source_start_message_id: start,
            source_end_message_id: end,
            created_turn: 1,
            ..fixture.context.clone()
        };
        let repeated = metadata("用户在路过小摊时收到我主动挑的丑猫钥匙扣，嘴上嫌弃但心里很喜欢");
        let outcome = adapter
            .write(&next_context, Some(&repeated), Some(&unit_embedding(9)))
            .await
            .unwrap();

        assert_eq!(
            outcome,
            MemoryWriteOutcome::Duplicate {
                existing_event_id: first_id
            }
        );
        assert_eq!(stored_events(&pool).await, 1);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn similar_embedding_does_not_merge_unrelated_callbacks(pool: PgPool) {
        let fixture = fixture(&pool).await;
        let adapter = MemoryWriteAdapter::new(&pool);
        adapter
            .write(
                &fixture.context,
                Some(&metadata("白芷在便利店买到一瓶难喝的饮料")),
                Some(&unit_embedding(7)),
            )
            .await
            .unwrap();

        let unrelated = metadata("白芷在公园捡到一只丑猫造型的钥匙扣");
        let outcome = adapter
            .write(&fixture.context, Some(&unrelated), Some(&unit_embedding(7)))
            .await
            .unwrap();

        assert!(matches!(outcome, MemoryWriteOutcome::Written { .. }));
        assert_eq!(stored_events(&pool).await, 2);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn repeated_plot_fact_is_deduped_without_creating_a_false_progression(pool: PgPool) {
        let fixture = fixture(&pool).await;
        let adapter = MemoryWriteAdapter::new(&pool);
        let mut first = metadata("在废弃车站储物柜里发现一张被撕过的合影，决定追查照片里的人");
        first.memory_type = eros_engine_core::event_memory::MemoryType::Plot;
        let first_id = expect_written(
            adapter
                .write(&fixture.context, Some(&first), Some(&unit_embedding(17)))
                .await
                .unwrap(),
        );

        let mut later = fixture.context.clone();
        later.created_turn += 1;
        let mut repeated = metadata(
            "我们根据匿名信在废弃车站候车厅储物柜发现一张被撕过的合影，决定追查照片里的人",
        );
        repeated.memory_type = eros_engine_core::event_memory::MemoryType::Plot;
        let outcome = adapter
            .write(&later, Some(&repeated), Some(&unit_embedding(17)))
            .await
            .unwrap();

        assert_eq!(
            outcome,
            MemoryWriteOutcome::Duplicate {
                existing_event_id: first_id
            }
        );
        assert_eq!(stored_events(&pool).await, 1);
        let edges: i64 = sqlx::query_scalar("SELECT count(*) FROM engine.event_edges")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(edges, 0, "a duplicate plot fact must not form followed_by");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn plot_stage_change_is_not_deduped_even_with_the_same_embedding(pool: PgPool) {
        let fixture = fixture(&pool).await;
        let adapter = MemoryWriteAdapter::new(&pool);
        let mut injury = metadata("角色在冲突中手部严重受伤，伤口仍在流血");
        injury.memory_type = eros_engine_core::event_memory::MemoryType::Plot;
        expect_written(
            adapter
                .write(&fixture.context, Some(&injury), Some(&unit_embedding(18)))
                .await
                .unwrap(),
        );

        let mut later = fixture.context.clone();
        later.created_turn += 1;
        let mut hospital = metadata("角色因这次伤势住院并接受手术");
        hospital.memory_type = eros_engine_core::event_memory::MemoryType::Plot;
        let outcome = adapter
            .write(&later, Some(&hospital), Some(&unit_embedding(18)))
            .await
            .unwrap();

        assert!(matches!(outcome, MemoryWriteOutcome::Written { .. }));
        assert_eq!(stored_events(&pool).await, 2);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn manual_memory_rejects_a_blank_summary_without_storing(pool: PgPool) {
        let fixture = fixture(&pool).await;
        let adapter = MemoryWriteAdapter::new(&pool);

        let outcome = adapter
            .write_manual(&fixture.context, &manual("   "), None)
            .await
            .unwrap();
        assert_eq!(
            outcome,
            MemoryWriteOutcome::Rejected(MemoryMetadataError::EmptySummary)
        );
        assert_eq!(stored_events(&pool).await, 0);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn manual_provenance_tag_does_not_chain_two_manual_memories(pool: PgPool) {
        // Both rows carry `manual`, so they share a tag — but `related_to` also
        // demands a shared participant and a manual memory names no cast. The
        // second manual write must therefore remain unlinked.
        let fixture = fixture(&pool).await;
        let adapter = MemoryWriteAdapter::new(&pool);

        adapter
            .write_manual(
                &fixture.context,
                &manual("用户第一次喝到那瓶新品饮料"),
                None,
            )
            .await
            .unwrap();
        let outcome = adapter
            .write_manual(
                &fixture.context,
                &manual("用户后来把那只丑猫钥匙扣挂在了包上"),
                None,
            )
            .await
            .unwrap();
        let MemoryWriteOutcome::Written { edges, .. } = outcome else {
            panic!("the second unrelated manual memory must store, got {outcome:?}");
        };
        assert_eq!(edges, 0, "callbacks do not receive chronology edges");
    }

    // ── Retrieval ─────────────────────────────────────────────────────────

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn retrieve_returns_the_best_anchor_first(pool: PgPool) {
        let fixture = fixture(&pool).await;
        let writer = MemoryWriteAdapter::new(&pool);
        let near = writer
            .write(
                &fixture.context,
                Some(&metadata("白芷给裴烬买过一瓶难喝的饮料")),
                Some(&unit_embedding(4)),
            )
            .await
            .unwrap();
        let near_id = expect_written(near);
        let far_id = expect_written(
            writer
                .write(
                    &fixture.context,
                    Some(&metadata("两人聊了聊明天的天气")),
                    Some(&unit_embedding(9)),
                )
                .await
                .unwrap(),
        );

        let reader = MemoryRetrievalAdapter::new(&pool);
        let mut query = RetrievalQuery::new(
            fixture.context.user_id,
            fixture.context.instance_id,
            unit_embedding(4),
        );
        query.participants = vec!["白芷".into(), "裴烬".into()];

        let recalled = reader.retrieve(&query).await.unwrap();
        assert_eq!(recalled[0].event_id, near_id);
        assert_eq!(recalled[0].graph_distance, 0);
        assert!(recalled
            .iter()
            .any(|experience| experience.event_id == far_id));
        assert!(recalled[0].score > recalled[1].score);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn retrieve_reaches_a_one_hop_neighbour_that_could_never_be_an_anchor(pool: PgPool) {
        let fixture = fixture(&pool).await;
        let writer = MemoryWriteAdapter::new(&pool);
        let anchor = expect_written(
            writer
                .write(
                    &fixture.context,
                    Some(&metadata("白芷给裴烬买过一瓶难喝的饮料")),
                    Some(&unit_embedding(4)),
                )
                .await
                .unwrap(),
        );
        // No embedding: the anchor query can never return this row, which is
        // exactly the case graph expansion exists for.
        let neighbour = expect_written(
            writer
                .write(
                    &fixture.context,
                    Some(&metadata("裴烬嘴上嫌弃那瓶饮料，最后还是喝完了")),
                    None,
                )
                .await
                .unwrap(),
        );

        let reader = MemoryRetrievalAdapter::new(&pool);
        let recalled = reader
            .retrieve(&RetrievalQuery::new(
                fixture.context.user_id,
                fixture.context.instance_id,
                unit_embedding(4),
            ))
            .await
            .unwrap();

        assert_eq!(recalled.len(), 2);
        assert_eq!(recalled[0].event_id, anchor);
        assert_eq!(recalled[0].graph_distance, 0);
        assert_eq!(recalled[1].event_id, neighbour);
        assert_eq!(recalled[1].graph_distance, 1);
        assert!(
            recalled[1].score < recalled[0].score,
            "a neighbour inherits a discounted relevance and pays a graph penalty"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn retrieve_hides_scoped_events_from_every_other_viewer(pool: PgPool) {
        let fixture = fixture(&pool).await;
        let writer = MemoryWriteAdapter::new(&pool);
        let public = expect_written(
            writer
                .write(
                    &fixture.context,
                    Some(&metadata("两人在路边见过一只特别丑的猫")),
                    Some(&unit_embedding(4)),
                )
                .await
                .unwrap(),
        );
        let mut secret = metadata("裴烬独自去处理过一件事");
        secret.knowledge_scope = vec!["裴烬".into()];
        let secret_id = expect_written(
            writer
                .write(&fixture.context, Some(&secret), Some(&unit_embedding(4)))
                .await
                .unwrap(),
        );

        let reader = MemoryRetrievalAdapter::new(&pool);
        let mut query = RetrievalQuery::new(
            fixture.context.user_id,
            fixture.context.instance_id,
            unit_embedding(4),
        );

        query.viewer = Some("白芷".into());
        let as_baizhi = reader.retrieve(&query).await.unwrap();
        assert_eq!(
            as_baizhi.iter().map(|it| it.event_id).collect::<Vec<_>>(),
            vec![public],
            "白芷 must not receive 裴烬's scoped event"
        );

        query.viewer = Some("裴烬".into());
        let as_peijin = reader.retrieve(&query).await.unwrap();
        assert_eq!(as_peijin.len(), 2);
        assert!(as_peijin.iter().any(|it| it.event_id == secret_id));

        query.viewer = None;
        let no_pov = reader.retrieve(&query).await.unwrap();
        assert_eq!(
            no_pov.iter().map(|it| it.event_id).collect::<Vec<_>>(),
            vec![public],
            "an unresolved POV fails closed"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn retrieve_skips_what_shared_memories_already_say(pool: PgPool) {
        let fixture = fixture(&pool).await;
        let writer = MemoryWriteAdapter::new(&pool);
        let summary = "白芷给裴烬买过一瓶难喝的饮料，他嫌弃但最后喝完了";
        writer
            .write(
                &fixture.context,
                Some(&metadata(summary)),
                Some(&unit_embedding(4)),
            )
            .await
            .unwrap();

        let reader = MemoryRetrievalAdapter::new(&pool);
        let mut query = RetrievalQuery::new(
            fixture.context.user_id,
            fixture.context.instance_id,
            unit_embedding(4),
        );
        query.known_memories = vec![summary.to_string()];
        assert!(
            reader.retrieve(&query).await.unwrap().is_empty(),
            "the same history must not be injected twice in one turn"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn retrieve_marks_what_it_returned_so_the_cooldown_can_work(pool: PgPool) {
        let fixture = fixture(&pool).await;
        let writer = MemoryWriteAdapter::new(&pool);
        let id = expect_written(
            writer
                .write(
                    &fixture.context,
                    Some(&metadata("白芷给裴烬买过一瓶难喝的饮料")),
                    Some(&unit_embedding(4)),
                )
                .await
                .unwrap(),
        );

        let reader = MemoryRetrievalAdapter::new(&pool);
        let query = RetrievalQuery::new(
            fixture.context.user_id,
            fixture.context.instance_id,
            unit_embedding(4),
        );

        assert_eq!(recall_state(&pool, id).await, (0, false));
        let first = reader.retrieve(&query).await.unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(recall_state(&pool, id).await, (1, true));

        let second = reader.retrieve(&query).await.unwrap();
        assert_eq!(
            recall_state(&pool, id).await,
            (2, true),
            "a recall that was actually injected must count every time"
        );
        assert!(
            second[0].score < first[0].score,
            "and the second time it is worth less"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn retrieve_never_returns_more_than_max_results(pool: PgPool) {
        let fixture = fixture(&pool).await;
        let writer = MemoryWriteAdapter::new(&pool);
        for index in 0..5 {
            writer
                .write(
                    &fixture.context,
                    Some(&metadata(&format!("第 {index} 次一起外出"))),
                    Some(&unit_embedding(4)),
                )
                .await
                .unwrap();
        }

        let reader = MemoryRetrievalAdapter::new(&pool);
        let recalled = reader
            .retrieve(&RetrievalQuery::new(
                fixture.context.user_id,
                fixture.context.instance_id,
                unit_embedding(4),
            ))
            .await
            .unwrap();
        assert_eq!(recalled.len(), DEFAULT_MAX_RESULTS);
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn retrieve_never_crosses_a_relationship(pool: PgPool) {
        let mine = fixture(&pool).await;
        let theirs = fixture(&pool).await;
        let writer = MemoryWriteAdapter::new(&pool);
        writer
            .write(
                &mine.context,
                Some(&metadata("我的事")),
                Some(&unit_embedding(4)),
            )
            .await
            .unwrap();
        writer
            .write(
                &theirs.context,
                Some(&metadata("别人的事")),
                Some(&unit_embedding(4)),
            )
            .await
            .unwrap();

        let reader = MemoryRetrievalAdapter::new(&pool);
        let recalled = reader
            .retrieve(&RetrievalQuery::new(
                mine.context.user_id,
                mine.context.instance_id,
                unit_embedding(4),
            ))
            .await
            .unwrap();
        assert_eq!(recalled.len(), 1);
        assert_eq!(recalled[0].summary, "我的事");
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn retrieve_returns_nothing_when_the_store_is_empty(pool: PgPool) {
        let fixture = fixture(&pool).await;
        let reader = MemoryRetrievalAdapter::new(&pool);
        assert!(reader
            .retrieve(&RetrievalQuery::new(
                fixture.context.user_id,
                fixture.context.instance_id,
                unit_embedding(4),
            ))
            .await
            .unwrap()
            .is_empty());
    }

    // ── Injection ─────────────────────────────────────────────────────────

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn render_experiences_is_the_documented_two_line_block(pool: PgPool) {
        let fixture = fixture(&pool).await;
        let writer = MemoryWriteAdapter::new(&pool);
        writer
            .write(
                &fixture.context,
                Some(&metadata("白芷给裴烬买过一瓶难喝的饮料")),
                Some(&unit_embedding(4)),
            )
            .await
            .unwrap();

        let reader = MemoryRetrievalAdapter::new(&pool);
        let recalled = reader
            .retrieve(&RetrievalQuery::new(
                fixture.context.user_id,
                fixture.context.instance_id,
                unit_embedding(4),
            ))
            .await
            .unwrap();

        assert_eq!(
            render_experiences(&recalled).unwrap(),
            "[Relevant Past Experiences]\n- 白芷给裴烬买过一瓶难喝的饮料"
        );
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn case_a_callback_write_recall_render_and_cooldown_close_the_loop(pool: PgPool) {
        let fixture = fixture(&pool).await;
        let writer = MemoryWriteAdapter::new(&pool);
        let event_id = expect_written(
            writer
                .write(
                    &fixture.context,
                    Some(&metadata(
                        "白芷给裴烬买过一瓶难喝的饮料，他嫌弃但最后喝完了",
                    )),
                    Some(&unit_embedding(7)),
                )
                .await
                .unwrap(),
        );
        let reader = MemoryRetrievalAdapter::new(&pool);
        let query = RetrievalQuery::new(
            fixture.context.user_id,
            fixture.context.instance_id,
            unit_embedding(7),
        );
        let first = reader.retrieve(&query).await.unwrap();
        assert!(first.len() <= 3);
        assert_eq!(first[0].event_id, event_id);
        assert!(render_experiences(&first).unwrap().contains("难喝的饮料"));
        let second = reader.retrieve(&query).await.unwrap();
        assert!(second[0].score < first[0].score);
        assert_eq!(recall_state(&pool, event_id).await, (2, true));
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn case_b_event_chain_expands_one_hop_around_the_hospital_anchor(pool: PgPool) {
        let fixture = fixture(&pool).await;
        let writer = MemoryWriteAdapter::new(&pool);
        let mut ids = Vec::new();
        for (summary, embedding) in [
            ("裴烬替白芷挡刀", None),
            ("裴烬因此重伤", None),
            ("裴烬重伤后住院", Some(unit_embedding(12))),
            ("白芷在医院陪护裴烬", None),
        ] {
            let mut event = metadata(summary);
            event.memory_type = eros_engine_core::event_memory::MemoryType::Plot;
            event.location = Some("医院".into());
            event.tags = vec!["受伤".into(), "治疗".into()];
            ids.push(expect_written(
                writer
                    .write(&fixture.context, Some(&event), embedding.as_deref())
                    .await
                    .unwrap(),
            ));
        }

        let recalled = MemoryRetrievalAdapter::new(&pool)
            .retrieve(&RetrievalQuery::new(
                fixture.context.user_id,
                fixture.context.instance_id,
                unit_embedding(12),
            ))
            .await
            .unwrap();
        assert!(recalled.iter().any(|event| event.event_id == ids[2]));
        assert!(recalled.iter().any(|event| {
            event.graph_distance == 1 && (event.event_id == ids[1] || event.event_id == ids[3])
        }));
    }

    #[sqlx::test(migrations = "../eros-engine-store/migrations")]
    async fn case_c_scoped_event_never_reaches_the_rendered_prompt_block(pool: PgPool) {
        let fixture = fixture(&pool).await;
        let writer = MemoryWriteAdapter::new(&pool);
        let mut secret = metadata("只有角色甲知道的秘密事件");
        secret.knowledge_scope = vec!["角色甲".into()];
        writer
            .write(&fixture.context, Some(&secret), Some(&unit_embedding(21)))
            .await
            .unwrap();

        let mut query = RetrievalQuery::new(
            fixture.context.user_id,
            fixture.context.instance_id,
            unit_embedding(21),
        );
        query.viewer = Some("角色乙".into());
        let recalled = MemoryRetrievalAdapter::new(&pool)
            .retrieve(&query)
            .await
            .unwrap();
        assert!(recalled.is_empty());
        assert!(render_experiences(&recalled).is_none());
    }

    #[test]
    fn render_experiences_is_absent_when_nothing_was_recalled() {
        assert!(render_experiences(&[]).is_none());
    }

    /// Unwraps a write that the test expects to have stored an event.
    fn expect_written(outcome: MemoryWriteOutcome) -> Uuid {
        match outcome {
            MemoryWriteOutcome::Written { event_id, .. } => event_id,
            other => panic!("expected a stored event, got {other:?}"),
        }
    }
}
