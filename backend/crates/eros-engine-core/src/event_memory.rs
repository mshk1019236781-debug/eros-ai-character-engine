// SPDX-License-Identifier: AGPL-3.0-only
//! V1 graph-enhanced episodic memory: the pure, I/O-free half.
//!
//! Three concerns live here, deliberately split from anything that touches a
//! database or a model:
//!
//! 1. the **Memory Metadata contract** the main RP model may return alongside
//!    its reply ([`MemoryMetadata`]) — one call, no second model;
//! 2. the **program-side edge rules** that turn a stream of events into a
//!    minimal event graph ([`derive_edges`]) — the model never names an edge;
//! 3. **recall filtering and scoring** ([`recall_score`],
//!    [`knowledge_scope_allows`], [`dedupe_against_known`],
//!    [`render_past_experiences`]).
//!
//! Storage lives in `eros-engine-store::event_memory`; the write/retrieval
//! adapters live in `eros-engine-server::pipeline::memory_adapter`. Keeping the
//! rules here means they are unit-testable without a database, which is the
//! whole point of splitting them out.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Hard cap on one event summary. Long enough for a two-clause 中文 summary
/// ("白芷在便利店买了一瓶新品饮料，裴烬嫌难喝但最后喝完了"), short enough that
/// three recalled events cannot dominate the prompt.
pub const MAX_EVENT_SUMMARY_CHARS: usize = 280;
/// Participants per event. The RP cast is small; more than this is a symptom
/// of the model listing everyone present rather than everyone involved.
pub const MAX_EVENT_PARTICIPANTS: usize = 8;
/// Free-form tags per event (used only by the `related_to` edge rule).
pub const MAX_EVENT_TAGS: usize = 8;
/// Explicit locations are short by nature ("便利店", "松林公园").
pub const MAX_EVENT_LOCATION_CHARS: usize = 64;
/// Characters allowed to see a scoped event (see [`knowledge_scope_allows`]).
pub const MAX_EVENT_KNOWLEDGE_SCOPE: usize = 8;

/// How many `related_to` edges one new event may create. `followed_by` and
/// `caused` are each capped at one by construction, so the total degree of a
/// fresh node is at most `1 + MAX_RELATED_EDGES + 1`. This is the brake that
/// keeps the V1 graph from exploding into a clique.
pub const MAX_RELATED_EDGES: usize = 3;

// ── Recall scoring weights (see `recall_score`) ─────────────────────────────
/// Weight of cosine similarity to the current context. Dominant on purpose:
/// the graph exists to widen the candidate set, not to outvote relevance.
pub const W_VECTOR: f64 = 0.55;
/// Weight of the event's own importance tier.
pub const W_IMPORTANCE: f64 = 0.15;
/// Weight of participant overlap with the current scene.
pub const W_PARTICIPANT: f64 = 0.15;
/// Weight of recency, an exponential decay with this half-life.
pub const W_RECENCY: f64 = 0.10;
pub const RECENCY_HALFLIFE_DAYS: f64 = 30.0;
/// Penalty applied per hop of graph distance from an anchor. An anchor itself
/// is distance 0 (no penalty); a 1-hop neighbour is distance 1.
pub const W_GRAPH_PENALTY: f64 = 0.10;

// ── Cooldown (see `cooldown_penalty`) ──────────────────────────────────────
/// Penalty added per past recall, before decay.
pub const COOLDOWN_STEP: f64 = 0.06;
/// Ceiling on the un-decayed cooldown, so a frequently recalled event cannot be
/// pushed arbitrarily far down.
pub const COOLDOWN_MAX: f64 = 0.30;
/// Half-life of the cooldown, in days since the last recall.
pub const COOLDOWN_HALFLIFE_DAYS: f64 = 3.0;
/// `important` / `major` events keep a quarter of the penalty — they get
/// deprioritised when they are not relevant, but they never disappear.
pub const COOLDOWN_IMPORTANT_DAMPENER: f64 = 0.25;

/// Floor below which a candidate is not worth injecting. Deliberately low: the
/// consumer still takes at most three, and a weak-but-true callback beats an
/// empty section for the RP.
pub const DEFAULT_MIN_RECALL_SCORE: f64 = 0.20;

/// A known memory must normalize to at least this many characters before it may
/// suppress a candidate. Below it, a short greeting would match everything.
///
/// The floor is deliberately one-sided: a *candidate* has no minimum length,
/// because "两人去公园" is a real event and must not be discarded for being
/// brief.
pub const MIN_DEDUP_CHARS: usize = 6;

// ───────────────────────────────────────────────────────────────────────────
// 1. Memory Metadata contract
// ───────────────────────────────────────────────────────────────────────────

/// What kind of experience the model claims this is. Two values only: V1 does
/// not ask the model for a broader taxonomy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryType {
    /// A small, recallable shared beat ("丑猫", "难喝的饮料").
    Callback,
    /// A key plot event (受伤 / 住院 / 争吵 / 决定).
    Plot,
}

impl MemoryType {
    pub fn as_str(self) -> &'static str {
        match self {
            MemoryType::Callback => "callback",
            MemoryType::Plot => "plot",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "callback" => Some(MemoryType::Callback),
            "plot" => Some(MemoryType::Plot),
            _ => None,
        }
    }
}

/// Four tiers. The tier is the model's judgement; the weights are ours and are
/// the only thing recall reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventImportance {
    Light,
    Normal,
    Important,
    Major,
}

impl EventImportance {
    pub fn as_str(self) -> &'static str {
        match self {
            EventImportance::Light => "light",
            EventImportance::Normal => "normal",
            EventImportance::Important => "important",
            EventImportance::Major => "major",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "light" => Some(EventImportance::Light),
            "normal" => Some(EventImportance::Normal),
            "important" => Some(EventImportance::Important),
            "major" => Some(EventImportance::Major),
            _ => None,
        }
    }

    /// Fold into `0.0..=1.0`. Doubles as the stored `salience` value, so the
    /// existing `recent_episodes` ordering column keeps meaning something.
    pub fn weight(self) -> f64 {
        match self {
            EventImportance::Light => 0.0,
            EventImportance::Normal => 1.0 / 3.0,
            EventImportance::Important => 2.0 / 3.0,
            EventImportance::Major => 1.0,
        }
    }

    /// Whether this tier is protected from full cooldown decay.
    pub fn is_durable(self) -> bool {
        matches!(self, EventImportance::Important | EventImportance::Major)
    }
}

/// Deterministic compatibility mapping from values models actually emit to the
/// four tiers the strict schema accepts. Known valid tiers are preserved; the
/// aliases below are folded onto their nearest tier; every other non-empty
/// value falls back to `normal` rather than failing the whole trailer.
pub fn normalize_importance_value(raw: &str) -> &'static str {
    match raw.trim().to_ascii_lowercase().as_str() {
        "light" => "light",
        "normal" => "normal",
        "important" => "important",
        "major" => "major",
        "significant" => "important",
        "high" => "important",
        "critical" => "major",
        "medium" => "normal",
        "low" => "light",
        _ => "normal",
    }
}

/// Rewrites only the top-level `importance` field of a memory-metadata JSON
/// payload. Every other field, including unknown ones, is left untouched so the
/// strict `MemoryMetadata` deserializer still rejects it. Non-object payloads
/// (notably the literal `null` that means "no memory") pass through unchanged.
pub fn normalize_memory_metadata_importance(payload: &str) -> String {
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(payload) else {
        return payload.to_string();
    };
    let Some(object) = value.as_object_mut() else {
        return payload.to_string();
    };
    let Some(importance) = object.get("importance").and_then(|value| value.as_str()) else {
        return payload.to_string();
    };
    object.insert(
        "importance".to_string(),
        serde_json::Value::String(normalize_importance_value(importance).to_string()),
    );
    value.to_string()
}

/// The optional structured block the main RP model may return in the same call
/// as its reply. Field names are part of the contract with the model, so `type`
/// is spelled the way the model spells it.
///
/// Deliberately absent (all program-side): graph edges, `caused_by`, entity
/// ids, embedding, uuid and the source message range.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryMetadata {
    #[serde(rename = "type")]
    pub memory_type: MemoryType,
    pub summary: String,
    #[serde(default)]
    pub participants: Vec<String>,
    #[serde(default)]
    pub location: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub importance: EventImportance,
    /// Characters allowed to recall this event. Empty (the model's `[]`) means
    /// "everyone in this story".
    #[serde(default)]
    pub knowledge_scope: Vec<String>,
    #[serde(default)]
    pub relationship_relevant: bool,
    /// Only stored when it parses as an absolute timestamp. The model is never
    /// asked to convert relative time, and a vague string is dropped rather
    /// than guessed at.
    #[serde(default)]
    pub story_time: Option<String>,
    /// Stable world/entity facts the main RP model emitted in the same trailer.
    ///
    /// A deliberately separate layer: these are *not* events, get no embedding
    /// and no graph edge, and a failure on any one of them never invalidates
    /// the event beside it. Empty (the common case) leaves the contract and the
    /// stored event byte-identical to before this field existed.
    #[serde(default)]
    pub world_facts: Vec<crate::world_fact::WorldFactCandidate>,
}

/// The other trailer shape: stable world/entity facts and **no** event.
///
/// Its own type rather than a loosened [`MemoryMetadata`] on purpose. That
/// struct is `deny_unknown_fields` and still requires `type` + `summary` +
/// `importance`, so nothing added here can make a half-formed event look
/// valid. This shape exists because a turn can produce a lasting fact and no
/// experience worth an event — the case that previously left the fact with
/// nowhere to go, since the whole trailer was rejected.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorldFactsTrailer {
    #[serde(default)]
    pub world_facts: Vec<crate::world_fact::WorldFactCandidate>,
}

impl MemoryMetadata {
    /// Carry a facts-only trailer through the existing pipeline.
    ///
    /// The returned value has an empty `summary`, so [`Self::validate`] rejects
    /// it as an event and the event writer makes no embedding call for it —
    /// which is the intent, not an oversight. The world-fact writer reads
    /// `world_facts` off the same value, so the wire keeps one trailer
    /// vocabulary and the runtime keeps one place where facts are written.
    pub fn facts_only(world_facts: Vec<crate::world_fact::WorldFactCandidate>) -> Self {
        Self {
            memory_type: MemoryType::Callback,
            summary: String::new(),
            participants: Vec::new(),
            location: None,
            tags: Vec::new(),
            importance: EventImportance::Light,
            knowledge_scope: Vec::new(),
            relationship_relevant: false,
            story_time: None,
            world_facts,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MemoryMetadataError {
    EmptySummary,
    SummaryTooLong { limit: usize },
    TooManyParticipants { limit: usize },
    EmptyParticipant { index: usize },
    TooManyTags { limit: usize },
    EmptyTag { index: usize },
    LocationTooLong { limit: usize },
    TooManyKnowledgeScopes { limit: usize },
    EmptyKnowledgeScope { index: usize },
}

impl std::fmt::Display for MemoryMetadataError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MemoryMetadataError::EmptySummary => write!(formatter, "summary must not be empty"),
            MemoryMetadataError::SummaryTooLong { limit } => {
                write!(formatter, "summary exceeds {limit} characters")
            }
            MemoryMetadataError::TooManyParticipants { limit } => {
                write!(formatter, "more than {limit} participants")
            }
            MemoryMetadataError::EmptyParticipant { index } => {
                write!(formatter, "participants[{index}] is blank")
            }
            MemoryMetadataError::TooManyTags { limit } => {
                write!(formatter, "more than {limit} tags")
            }
            MemoryMetadataError::EmptyTag { index } => {
                write!(formatter, "tags[{index}] is blank")
            }
            MemoryMetadataError::LocationTooLong { limit } => {
                write!(formatter, "location exceeds {limit} characters")
            }
            MemoryMetadataError::TooManyKnowledgeScopes { limit } => {
                write!(formatter, "more than {limit} knowledge scopes")
            }
            MemoryMetadataError::EmptyKnowledgeScope { index } => {
                write!(formatter, "knowledge_scope[{index}] is blank")
            }
        }
    }
}

impl std::error::Error for MemoryMetadataError {}

impl MemoryMetadata {
    /// Trim and de-duplicate every list, then drop a blank location.
    ///
    /// Order matters: normalising before validating means " 裴烬 " and "裴烬"
    /// collapse first, so a padded duplicate cannot fail an otherwise valid
    /// event against the length caps.
    pub fn normalize(&mut self) {
        self.summary = self.summary.trim().to_string();
        self.participants = normalize_names(&self.participants);
        self.tags = normalize_names(&self.tags);
        self.knowledge_scope = normalize_names(&self.knowledge_scope);
        self.location = self
            .location
            .as_ref()
            .map(|location| location.trim().to_string())
            .filter(|location| !location.is_empty());
    }

    /// Validate the normalized form. Callers should run [`Self::normalize`]
    /// first; this method does not mutate.
    pub fn validate(&self) -> Result<(), MemoryMetadataError> {
        if self.summary.trim().is_empty() {
            return Err(MemoryMetadataError::EmptySummary);
        }
        if self.summary.chars().count() > MAX_EVENT_SUMMARY_CHARS {
            return Err(MemoryMetadataError::SummaryTooLong {
                limit: MAX_EVENT_SUMMARY_CHARS,
            });
        }
        if self.participants.len() > MAX_EVENT_PARTICIPANTS {
            return Err(MemoryMetadataError::TooManyParticipants {
                limit: MAX_EVENT_PARTICIPANTS,
            });
        }
        if let Some(index) = self
            .participants
            .iter()
            .position(|name| name.trim().is_empty())
        {
            return Err(MemoryMetadataError::EmptyParticipant { index });
        }
        if self.tags.len() > MAX_EVENT_TAGS {
            return Err(MemoryMetadataError::TooManyTags {
                limit: MAX_EVENT_TAGS,
            });
        }
        if let Some(index) = self.tags.iter().position(|tag| tag.trim().is_empty()) {
            return Err(MemoryMetadataError::EmptyTag { index });
        }
        if let Some(location) = &self.location {
            if location.chars().count() > MAX_EVENT_LOCATION_CHARS {
                return Err(MemoryMetadataError::LocationTooLong {
                    limit: MAX_EVENT_LOCATION_CHARS,
                });
            }
        }
        if self.knowledge_scope.len() > MAX_EVENT_KNOWLEDGE_SCOPE {
            return Err(MemoryMetadataError::TooManyKnowledgeScopes {
                limit: MAX_EVENT_KNOWLEDGE_SCOPE,
            });
        }
        if let Some(index) = self
            .knowledge_scope
            .iter()
            .position(|viewer| viewer.trim().is_empty())
        {
            return Err(MemoryMetadataError::EmptyKnowledgeScope { index });
        }
        Ok(())
    }

    /// Parse `story_time` into an absolute instant, or `None`.
    ///
    /// RFC3339 only. A model that answers "上周" — or that invents a date for a
    /// relative phrase — yields `None`, because a wrong timestamp is worse than
    /// no timestamp: it silently reorders the graph.
    pub fn story_time_utc(&self) -> Option<DateTime<Utc>> {
        let raw = self.story_time.as_ref()?.trim();
        if raw.is_empty() {
            return None;
        }
        DateTime::parse_from_rfc3339(raw)
            .ok()
            .map(|stamp| stamp.with_timezone(&Utc))
    }
}

/// The tag every user-authored memory carries.
///
/// Provenance is the one thing a manual memory must be able to show, and
/// `recent_episodes` has no provenance column — so it rides in `tags`, which
/// the store already persists and the runtime already reads back. Nothing
/// matches on it: the `related_to` rule still needs a shared *participant*
/// as well, and a manual memory has none, so this tag cannot create edges.
pub const MANUAL_MEMORY_TAG: &str = "manual";

/// A memory the user explicitly asked the character to keep, alongside an
/// ordinary turn.
///
/// The user supplies the fact and, optionally, whether it is a plot beat.
/// Everything else — id, embedding, participants, scope, importance — is
/// program-side, exactly as for the model's own trailer. `deny_unknown_fields`
/// keeps a caller from smuggling in a field of [`MemoryMetadata`] the contract
/// deliberately withholds.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManualMemoryRequest {
    pub summary: String,
    /// `callback` when omitted. `plot` is the opt-in for a story beat.
    #[serde(default, rename = "type")]
    pub memory_type: Option<MemoryType>,
}

/// Fold a manual request into the same contract the model's trailer produces,
/// then normalize and validate it.
///
/// One function for both the request validator (which turns its error into a
/// 4xx) and the write path (which fails open), so the two can never disagree
/// about what a valid manual memory is.
///
/// Defaults: `callback` and `important` — a memory the user asked for by hand
/// is worth keeping without being a plot beat — with no participants and an
/// empty `knowledge_scope`, i.e. visible to every character in this story.
/// A user-written fact is not a character secret; scope stays a program
/// decision rather than a field the caller can set.
pub fn manual_memory_metadata(
    summary: &str,
    memory_type: Option<MemoryType>,
) -> Result<MemoryMetadata, MemoryMetadataError> {
    let mut metadata = MemoryMetadata {
        memory_type: memory_type.unwrap_or(MemoryType::Callback),
        summary: summary.to_string(),
        participants: Vec::new(),
        location: None,
        tags: vec![MANUAL_MEMORY_TAG.to_string()],
        importance: EventImportance::Important,
        knowledge_scope: Vec::new(),
        relationship_relevant: false,
        story_time: None,
        world_facts: Vec::new(),
    };
    metadata.normalize();
    metadata.validate()?;
    Ok(metadata)
}

/// Trim, drop blanks and de-duplicate, preserving first-seen order so a stored
/// list is stable across retries. Case-sensitive on purpose: 中文 names have no
/// case, and folding ASCII names would merge two different characters.
fn normalize_names(values: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(values.len());
    for value in values {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            continue;
        }
        if !out.iter().any(|seen| seen == trimmed) {
            out.push(trimmed.to_string());
        }
    }
    out
}

// ───────────────────────────────────────────────────────────────────────────
// 2. Event graph: program-derived edges
// ───────────────────────────────────────────────────────────────────────────

/// The only three edge types V1 allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventRelation {
    /// Chronological successor inside one session.
    FollowedBy,
    /// Explicitly declared cause. Never inferred.
    Caused,
    /// Shared participants *and* shared tags inside one session.
    RelatedTo,
}

impl EventRelation {
    pub fn as_str(self) -> &'static str {
        match self {
            EventRelation::FollowedBy => "followed_by",
            EventRelation::Caused => "caused",
            EventRelation::RelatedTo => "related_to",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "followed_by" => Some(EventRelation::FollowedBy),
            "caused" => Some(EventRelation::Caused),
            "related_to" => Some(EventRelation::RelatedTo),
            _ => None,
        }
    }
}

/// A directed edge to persist: `source_event_id` → `target_event_id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DerivedEdge {
    pub source_event_id: Uuid,
    pub target_event_id: Uuid,
    pub relation: EventRelation,
}

/// The minimal view of an already-stored event that the edge rules need.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PriorEvent {
    pub id: Uuid,
    pub session_id: Uuid,
    pub participants: Vec<String>,
    pub tags: Vec<String>,
    pub created_at: DateTime<Utc>,
}

/// What the caller knows about the event being written right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewEventFacts {
    pub id: Uuid,
    pub session_id: Uuid,
    pub participants: Vec<String>,
    pub tags: Vec<String>,
}

/// Derive the edges for one freshly written event. Pure: same inputs, same
/// edges, no query and no model.
///
/// The three rules, in the order they are applied:
///
/// 1. **`followed_by`** — at most one edge, from `previous` (the most recent
///    event in the same session) to this one. A candidate from another session
///    is ignored: two parallel stories must not be chained.
/// 2. **`caused`** — only from `explicit_caused_by`, which V1 never sets. The
///    parameter exists so the rule is implemented and testable rather than
///    guessed at later; causality inferred from co-occurrence is exactly the
///    kind of edge we refuse to invent.
/// 3. **`related_to`** — at most [`MAX_RELATED_EDGES`], drawn from
///    `session_candidates` that share ≥1 participant **and** ≥1 tag, newest
///    first, skipping the ids already linked above. Requiring both signals is
///    what keeps "everyone was in the same scene" from becoming a clique.
pub fn derive_edges(
    new_event: &NewEventFacts,
    previous: Option<&PriorEvent>,
    session_candidates: &[PriorEvent],
    explicit_caused_by: Option<Uuid>,
) -> Vec<DerivedEdge> {
    let mut edges: Vec<DerivedEdge> = Vec::new();

    let followed_from = previous
        .filter(|prior| prior.session_id == new_event.session_id && prior.id != new_event.id);
    if let Some(prior) = followed_from {
        edges.push(DerivedEdge {
            source_event_id: prior.id,
            target_event_id: new_event.id,
            relation: EventRelation::FollowedBy,
        });
    }

    if let Some(cause) = explicit_caused_by {
        if cause != new_event.id {
            edges.push(DerivedEdge {
                source_event_id: cause,
                target_event_id: new_event.id,
                relation: EventRelation::Caused,
            });
        }
    }

    let mut related: Vec<&PriorEvent> = session_candidates
        .iter()
        .filter(|candidate| {
            candidate.session_id == new_event.session_id
                && candidate.id != new_event.id
                && followed_from.map(|prior| prior.id) != Some(candidate.id)
                && explicit_caused_by != Some(candidate.id)
                && shares_participant(candidate, new_event)
                && shares_tag(candidate, new_event)
        })
        .collect();
    related.sort_by(|left, right| {
        right
            .created_at
            .cmp(&left.created_at)
            .then(right.id.cmp(&left.id))
    });

    let mut linked: Vec<Uuid> = Vec::with_capacity(MAX_RELATED_EDGES);
    for candidate in related {
        if linked.contains(&candidate.id) {
            continue;
        }
        if linked.len() == MAX_RELATED_EDGES {
            break;
        }
        linked.push(candidate.id);
        edges.push(DerivedEdge {
            source_event_id: candidate.id,
            target_event_id: new_event.id,
            relation: EventRelation::RelatedTo,
        });
    }

    edges
}

fn shares_participant(candidate: &PriorEvent, new_event: &NewEventFacts) -> bool {
    candidate
        .participants
        .iter()
        .any(|prior| new_event.participants.iter().any(|now| now == prior))
}

fn shares_tag(candidate: &PriorEvent, new_event: &NewEventFacts) -> bool {
    candidate
        .tags
        .iter()
        .any(|prior| new_event.tags.iter().any(|now| now == prior))
}

// ───────────────────────────────────────────────────────────────────────────
// 3. Retrieval: scope, scoring, cooldown, injection
// ───────────────────────────────────────────────────────────────────────────

/// Can a viewer whose point-of-view character is `pov` recall an event carrying
/// `scope`?
///
/// `[]` is public — everyone in the story. A non-empty scope is an allow-list of
/// character names matched exactly, so `["裴烬"]` is visible to 裴烬 and to
/// nobody else, which is the entire reason the field exists.
///
/// A `None` viewer (no POV resolved, e.g. a background job) sees public events
/// only: for a field whose job is hiding secrets, failing closed is the only
/// safe default.
pub fn knowledge_scope_allows(scope: &[String], pov: Option<&str>) -> bool {
    if scope.is_empty() {
        return true;
    }
    match pov {
        Some(viewer) => {
            let viewer = viewer.trim();
            !viewer.is_empty() && scope.iter().any(|allowed| allowed == viewer)
        }
        None => false,
    }
}

/// Everything the scorer needs about one candidate.
#[derive(Debug, Clone, PartialEq)]
pub struct RecallSignals {
    /// Cosine similarity to the current context, `0.0..=1.0`.
    pub vector_similarity: f64,
    pub importance: EventImportance,
    /// Participants shared with the current scene.
    pub participant_overlap: usize,
    /// Size of the union of both participant sets — the denominator, so an
    /// event naming one of three people scores lower than one naming one of one.
    pub participant_union: usize,
    /// 0 for a vector anchor, 1 for a 1-hop neighbour. V1 cannot reach deeper,
    /// and anything deeper is scored as if it were two hops.
    pub graph_distance: u8,
    /// Age of the event in days at query time.
    pub age_days: f64,
    pub recall_count: i32,
    /// Days since the last recall, or `None` if never recalled.
    pub days_since_recall: Option<f64>,
}

/// Score = weighted relevance minus graph distance and cooldown.
///
/// ```text
/// score = 0.55·similarity
///       + 0.15·importance_weight      (light 0 … major 1)
///       + 0.15·participant_overlap
///       + 0.10·exp(-age_days / 30)
///       − 0.10·graph_distance
///       − cooldown_penalty
/// ```
///
/// Every term is bounded, so the score stays inside roughly `-0.45..=0.90` and
/// a threshold means the same thing at every call site. No ML reranker: the
/// point is that a human can read a score and say why it came out that way.
pub fn recall_score(signals: &RecallSignals) -> f64 {
    let similarity = signals.vector_similarity.clamp(0.0, 1.0);
    let importance = signals.importance.weight();
    let participant = if signals.participant_union == 0 {
        0.0
    } else {
        (signals.participant_overlap as f64 / signals.participant_union as f64).clamp(0.0, 1.0)
    };
    let recency = (-signals.age_days.max(0.0) / RECENCY_HALFLIFE_DAYS).exp();
    let graph_penalty = match signals.graph_distance {
        0 => 0.0,
        1 => 1.0,
        _ => 2.0,
    };
    let cooldown = cooldown_penalty(
        signals.recall_count,
        signals.days_since_recall,
        signals.importance,
    );

    W_VECTOR * similarity
        + W_IMPORTANCE * importance
        + W_PARTICIPANT * participant
        + W_RECENCY * recency
        - W_GRAPH_PENALTY * graph_penalty
        - cooldown
}

/// "Don't bring up the drink every single time she walks into a convenience
/// store."
///
/// The penalty grows with how often the event has been recalled, decays with a
/// 3-day half-life since the last recall, and is dampened to a quarter for
/// `important` / `major` events — those get deprioritised when they are
/// irrelevant but must never become permanently invisible.
///
/// Pure and monotone: more recalls never *raise* a score, and the ceiling at
/// [`COOLDOWN_MAX`] means one over-shown event cannot be pushed arbitrarily far
/// down.
pub fn cooldown_penalty(
    recall_count: i32,
    days_since_recall: Option<f64>,
    importance: EventImportance,
) -> f64 {
    if recall_count <= 0 {
        return 0.0;
    }
    let Some(since) = days_since_recall else {
        return 0.0;
    };
    let base = (COOLDOWN_STEP * recall_count.min(5) as f64).min(COOLDOWN_MAX);
    let decay = (-since.max(0.0) / COOLDOWN_HALFLIFE_DAYS).exp();
    let dampener = if importance.is_durable() {
        COOLDOWN_IMPORTANT_DAMPENER
    } else {
        1.0
    };
    base * decay * dampener
}

/// Participant overlap and union, for [`RecallSignals`].
pub fn participant_overlap(scene: &[String], event: &[String]) -> (usize, usize) {
    let overlap = event
        .iter()
        .filter(|name| scene.iter().any(|scene_name| name == &scene_name))
        .count();
    let mut union: Vec<&String> = scene.iter().collect();
    for name in event {
        if !union.contains(&name) {
            union.push(name);
        }
    }
    (overlap, union.len())
}

/// Fold text to a comparison key: letters and digits only, lowercased.
///
/// Deliberately Unicode-aware rather than ASCII-only, because every name in
/// this project is Chinese. An `[^a-z0-9]` filter — the shape that broke the
/// Graphiti fuzzy-dedup path we audited — would erase 裴烬 entirely and make
/// every event look like every other.
pub fn normalize_for_dedup(text: &str) -> String {
    text.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// Drop candidates already covered by a memory the prompt will inject anyway.
///
/// The overlap is `[shared_memories]` (companion_memories) against the new
/// `[Relevant Past Experiences]` section: the same history must not be told
/// twice in one turn. A candidate is dropped when either normalized string
/// contains the other. The length floor applies to the known side only, and a
/// candidate with no letters or digits at all is dropped outright.
pub fn dedupe_against_known(candidates: &[String], known: &[String]) -> Vec<String> {
    let known_keys: Vec<String> = known
        .iter()
        .map(|text| normalize_for_dedup(text))
        .filter(|key| key.chars().count() >= MIN_DEDUP_CHARS)
        .collect();

    candidates
        .iter()
        .filter(|candidate| {
            let key = normalize_for_dedup(candidate);
            if key.is_empty() {
                return false;
            }
            !known_keys
                .iter()
                .any(|known_key| key.contains(known_key.as_str()) || known_key.contains(&key))
        })
        .cloned()
        .collect()
}

/// Render the prompt section. Empty input renders nothing at all, so a turn
/// with no recall leaves the caller's prompt byte-identical.
///
/// Summaries only: no embedding, no importance, no score, no edge and no
/// database field ever reaches the model.
pub fn render_past_experiences(summaries: &[String]) -> Option<String> {
    let lines: Vec<String> = summaries
        .iter()
        .map(|summary| summary.trim())
        .filter(|summary| !summary.is_empty())
        .map(|summary| format!("- {summary}"))
        .collect();
    if lines.is_empty() {
        return None;
    }
    Some(format!("[Relevant Past Experiences]\n{}", lines.join("\n")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata(json: &str) -> MemoryMetadata {
        serde_json::from_str(json).expect("metadata parses")
    }

    /// The second trailer shape: facts, no event. It has to reach the fact
    /// writer while staying impossible to mistake for an event.
    #[test]
    fn a_facts_only_trailer_carries_no_event() {
        let payload = r#"{"world_facts":[{"subject":"白远舟","predicate":"的哥哥","object":"白芷","type":"relationship","statement":"白远舟是白芷的哥哥","knowledge_scope":[]}]}"#;
        let trailer: WorldFactsTrailer =
            serde_json::from_str(payload).expect("facts-only payload parses");
        let mut metadata = MemoryMetadata::facts_only(trailer.world_facts);
        metadata.normalize();
        assert_eq!(metadata.validate(), Err(MemoryMetadataError::EmptySummary));
        assert_eq!(metadata.world_facts.len(), 1);
        assert_eq!(metadata.world_facts[0].object, "白芷");
    }

    /// Neither shape may be read as the other, in either direction.
    #[test]
    fn the_two_trailer_shapes_stay_distinct() {
        let event = r#"{"type":"callback","summary":"买了瓶难喝的饮料","importance":"light"}"#;
        let facts = r#"{"world_facts":[{"subject":"白远舟","predicate":"的哥哥","object":"白芷","type":"relationship"}]}"#;
        assert!(serde_json::from_str::<WorldFactsTrailer>(event).is_err());
        assert!(serde_json::from_str::<MemoryMetadata>(facts).is_err());
    }

    fn prior(id: u8, session: Uuid, participants: &[&str], tags: &[&str], day: i64) -> PriorEvent {
        PriorEvent {
            id: Uuid::from_u128(id as u128),
            session_id: session,
            participants: participants.iter().map(|name| name.to_string()).collect(),
            tags: tags.iter().map(|tag| tag.to_string()).collect(),
            created_at: DateTime::from_timestamp(1_700_000_000 + day * 86_400, 0).unwrap(),
        }
    }

    // ── Contract ──────────────────────────────────────────────────────────

    #[test]
    fn metadata_parses_the_documented_shape_including_type() {
        let parsed = metadata(
            r#"{
                "type": "callback",
                "summary": "白芷给裴烬买过一瓶难喝的饮料，他嫌弃但最后喝完了",
                "participants": ["白芷", "裴烬"],
                "location": "便利店",
                "tags": ["饮料"],
                "importance": "light",
                "knowledge_scope": [],
                "relationship_relevant": true,
                "story_time": null
            }"#,
        );
        assert_eq!(parsed.memory_type, MemoryType::Callback);
        assert_eq!(parsed.importance, EventImportance::Light);
        assert_eq!(parsed.participants, vec!["白芷", "裴烬"]);
        assert!(parsed.knowledge_scope.is_empty());
        assert!(parsed.story_time_utc().is_none());
    }

    #[test]
    fn metadata_rejects_forbidden_extra_fields() {
        // The contract is closed: a model that starts emitting a relation or
        // affinity judgement must be caught, not silently stored.
        let err = serde_json::from_str::<MemoryMetadata>(
            r#"{"type":"plot","summary":"x","importance":"major","affinity":0.4}"#,
        );
        assert!(err.is_err());
    }

    #[test]
    fn metadata_defaults_missing_lists() {
        let parsed = metadata(r#"{"type":"plot","summary":"裴烬重伤住院","importance":"major"}"#);
        assert!(parsed.participants.is_empty());
        assert!(parsed.tags.is_empty());
        assert!(!parsed.relationship_relevant);
        assert_eq!(parsed.memory_type, MemoryType::Plot);
    }

    #[test]
    fn importance_aliases_normalize_to_the_strict_four_tiers() {
        let cases = [
            ("significant", "important"),
            ("high", "important"),
            ("critical", "major"),
            ("medium", "normal"),
            ("low", "light"),
            ("severe", "normal"),
            ("", "normal"),
            (" Major ", "major"),
        ];
        for (raw, expected) in cases {
            let payload = format!(r#"{{"type":"callback","summary":"x","importance":"{raw}"}}"#);
            let normalized = normalize_memory_metadata_importance(&payload);
            let parsed: MemoryMetadata =
                serde_json::from_str(&normalized).expect("normalized metadata parses");
            assert_eq!(parsed.importance.as_str(), expected, "raw={raw:?}");
        }
    }

    #[test]
    fn normalization_keeps_every_other_field_strict() {
        let payload = r#"{"type":"plot","summary":"x","importance":"significant","affinity":0.4}"#;
        let normalized = normalize_memory_metadata_importance(payload);
        assert!(serde_json::from_str::<MemoryMetadata>(&normalized).is_err());
    }

    #[test]
    fn normalize_trims_dedupes_and_drops_blank_location() {
        let mut parsed = metadata(
            r#"{"type":"callback","summary":"  a coffee spill  ","importance":"normal",
                "participants":[" 白芷 ","裴烬","白芷","  "],
                "tags":[" 咖啡 ","咖啡"],"location":"   "}"#,
        );
        parsed.normalize();
        assert_eq!(parsed.summary, "a coffee spill");
        assert_eq!(parsed.participants, vec!["白芷", "裴烬"]);
        assert_eq!(parsed.tags, vec!["咖啡"]);
        assert_eq!(parsed.location, None);
        assert!(parsed.validate().is_ok());
    }

    #[test]
    fn validate_rejects_empty_summary_and_overlong_lists() {
        let mut empty = metadata(r#"{"type":"plot","summary":"   ","importance":"major"}"#);
        empty.normalize();
        assert_eq!(empty.validate(), Err(MemoryMetadataError::EmptySummary));

        let too_long = MemoryMetadata {
            memory_type: MemoryType::Plot,
            summary: "x".repeat(MAX_EVENT_SUMMARY_CHARS + 1),
            participants: Vec::new(),
            location: None,
            tags: Vec::new(),
            importance: EventImportance::Major,
            knowledge_scope: Vec::new(),
            relationship_relevant: false,
            story_time: None,
            world_facts: Vec::new(),
        };
        assert_eq!(
            too_long.validate(),
            Err(MemoryMetadataError::SummaryTooLong {
                limit: MAX_EVENT_SUMMARY_CHARS
            })
        );
    }

    #[test]
    fn summary_length_is_counted_in_chinese_characters_not_bytes() {
        // 280 汉字 is 840 bytes; a byte-based cap would reject a legal summary.
        let mut parsed = MemoryMetadata {
            memory_type: MemoryType::Callback,
            summary: "白".repeat(MAX_EVENT_SUMMARY_CHARS),
            participants: Vec::new(),
            location: None,
            tags: Vec::new(),
            importance: EventImportance::Light,
            knowledge_scope: Vec::new(),
            relationship_relevant: false,
            story_time: None,
            world_facts: Vec::new(),
        };
        parsed.normalize();
        assert!(parsed.validate().is_ok());
        parsed.summary.push('芷');
        assert!(parsed.validate().is_err());
    }

    #[test]
    fn story_time_only_parses_absolute_timestamps() {
        let relative = metadata(
            r#"{"type":"callback","summary":"上周两人一起去过公园","importance":"normal","story_time":"上周"}"#,
        );
        assert!(relative.story_time_utc().is_none());

        let absolute = metadata(
            r#"{"type":"plot","summary":"出院","importance":"important","story_time":"2026-09-01T10:00:00+08:00"}"#,
        );
        assert_eq!(
            absolute.story_time_utc().unwrap().to_rfc3339(),
            "2026-09-01T02:00:00+00:00"
        );
    }

    // ── Manual memory ─────────────────────────────────────────────────────

    #[test]
    fn manual_memory_defaults_to_a_public_callback() {
        let metadata =
            manual_memory_metadata("  用户第一次喝到那瓶新品饮料，觉得非常难喝  ", None).unwrap();
        assert_eq!(metadata.memory_type, MemoryType::Callback);
        assert_eq!(metadata.importance, EventImportance::Important);
        assert_eq!(metadata.tags, vec![MANUAL_MEMORY_TAG]);
        assert_eq!(
            metadata.summary, "用户第一次喝到那瓶新品饮料，觉得非常难喝",
            "the stored summary is the trimmed one"
        );
        assert!(
            metadata.participants.is_empty(),
            "the user never names a cast"
        );
        assert!(
            metadata.knowledge_scope.is_empty(),
            "a user-written fact is visible to everyone in this story"
        );
    }

    #[test]
    fn manual_memory_takes_the_plot_opt_in() {
        let metadata = manual_memory_metadata("裴烬重伤住院", Some(MemoryType::Plot)).unwrap();
        assert_eq!(metadata.memory_type, MemoryType::Plot);
        assert_eq!(
            metadata.importance,
            EventImportance::Important,
            "type and importance are separate axes"
        );
    }

    #[test]
    fn manual_memory_rejects_blank_and_overlong_summaries() {
        assert_eq!(
            manual_memory_metadata("   ", None).unwrap_err(),
            MemoryMetadataError::EmptySummary
        );
        let overlong = "字".repeat(MAX_EVENT_SUMMARY_CHARS + 1);
        assert_eq!(
            manual_memory_metadata(&overlong, None).unwrap_err(),
            MemoryMetadataError::SummaryTooLong {
                limit: MAX_EVENT_SUMMARY_CHARS
            }
        );
    }

    #[test]
    fn manual_memory_request_parses_the_documented_shape() {
        let omitted: ManualMemoryRequest =
            serde_json::from_str(r#"{"summary":"两人去公园看了那只丑猫"}"#).unwrap();
        assert_eq!(omitted.memory_type, None);
        let plot: ManualMemoryRequest =
            serde_json::from_str(r#"{"summary":"裴烬重伤住院","type":"plot"}"#).unwrap();
        assert_eq!(plot.memory_type, Some(MemoryType::Plot));
    }

    #[test]
    fn manual_memory_request_is_closed_to_ids_embeddings_and_other_internals() {
        for payload in [
            r#"{"summary":"x","event_id":"00000000-0000-0000-0000-000000000000"}"#,
            r#"{"summary":"x","embedding":[0.1]}"#,
            r#"{"summary":"x","importance":"major"}"#,
            r#"{"summary":"x","type":"memory"}"#,
        ] {
            assert!(
                serde_json::from_str::<ManualMemoryRequest>(payload).is_err(),
                "must refuse {payload}"
            );
        }
    }

    // ── Edge rules ────────────────────────────────────────────────────────

    #[test]
    fn followed_by_links_the_previous_event_in_the_same_session() {
        let session = Uuid::from_u128(7);
        let previous = prior(1, session, &["白芷"], &["公园"], 1);
        let new_event = NewEventFacts {
            id: Uuid::from_u128(2),
            session_id: session,
            participants: vec!["白芷".into()],
            tags: vec!["公园".into()],
        };
        let edges = derive_edges(&new_event, Some(&previous), &[], None);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].relation, EventRelation::FollowedBy);
        assert_eq!(edges[0].source_event_id, previous.id);
        assert_eq!(edges[0].target_event_id, new_event.id);
    }

    #[test]
    fn followed_by_never_crosses_sessions() {
        let previous = prior(1, Uuid::from_u128(99), &["白芷"], &["公园"], 1);
        let new_event = NewEventFacts {
            id: Uuid::from_u128(2),
            session_id: Uuid::from_u128(7),
            participants: vec!["白芷".into()],
            tags: vec!["公园".into()],
        };
        assert!(derive_edges(&new_event, Some(&previous), &[], None).is_empty());
    }

    #[test]
    fn caused_is_only_written_when_explicitly_declared() {
        let session = Uuid::from_u128(7);
        let cause = prior(1, session, &["裴烬"], &["受伤"], 1);
        let new_event = NewEventFacts {
            id: Uuid::from_u128(2),
            session_id: session,
            participants: vec!["裴烬".into()],
            tags: vec!["受伤".into()],
        };
        // Same participant and same tag — a tempting inference, and still no
        // `caused` edge, because co-occurrence is not causality.
        let inferred = derive_edges(&new_event, None, std::slice::from_ref(&cause), None);
        assert!(inferred
            .iter()
            .all(|edge| edge.relation != EventRelation::Caused));

        let explicit = derive_edges(&new_event, None, &[], Some(cause.id));
        assert_eq!(explicit.len(), 1);
        assert_eq!(explicit[0].relation, EventRelation::Caused);
        assert_eq!(explicit[0].source_event_id, cause.id);
    }

    #[test]
    fn related_to_needs_both_a_shared_participant_and_a_shared_tag() {
        let session = Uuid::from_u128(7);
        let same_person_only = prior(1, session, &["白芷"], &["天气"], 1);
        let same_tag_only = prior(2, session, &["温景行"], &["饮料"], 1);
        let both = prior(3, session, &["白芷"], &["饮料"], 2);
        let new_event = NewEventFacts {
            id: Uuid::from_u128(9),
            session_id: session,
            participants: vec!["白芷".into()],
            tags: vec!["饮料".into()],
        };
        let edges = derive_edges(
            &new_event,
            None,
            &[same_person_only, same_tag_only, both.clone()],
            None,
        );
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].relation, EventRelation::RelatedTo);
        assert_eq!(edges[0].source_event_id, both.id);
    }

    #[test]
    fn related_to_is_capped_and_prefers_the_newest() {
        let session = Uuid::from_u128(7);
        let candidates: Vec<PriorEvent> = (1..=6)
            .map(|n| prior(n, session, &["白芷"], &["饮料"], n as i64))
            .collect();
        let new_event = NewEventFacts {
            id: Uuid::from_u128(9),
            session_id: session,
            participants: vec!["白芷".into()],
            tags: vec!["饮料".into()],
        };
        let edges = derive_edges(&new_event, None, &candidates, None);
        assert_eq!(edges.len(), MAX_RELATED_EDGES);
        let sources: Vec<Uuid> = edges.iter().map(|edge| edge.source_event_id).collect();
        assert_eq!(
            sources,
            vec![Uuid::from_u128(6), Uuid::from_u128(5), Uuid::from_u128(4)]
        );
    }

    #[test]
    fn previous_event_is_not_duplicated_as_related_to() {
        // Without this skip a long story would write two edges between the same
        // pair on every turn, and the 1-hop expansion would return the previous
        // event twice.
        let session = Uuid::from_u128(7);
        let previous = prior(1, session, &["白芷"], &["饮料"], 1);
        let new_event = NewEventFacts {
            id: Uuid::from_u128(2),
            session_id: session,
            participants: vec!["白芷".into()],
            tags: vec!["饮料".into()],
        };
        let edges = derive_edges(
            &new_event,
            Some(&previous),
            std::slice::from_ref(&previous),
            None,
        );
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].relation, EventRelation::FollowedBy);
    }

    #[test]
    fn unrelated_sessions_and_tags_produce_no_edges() {
        let session = Uuid::from_u128(7);
        let other_session = prior(1, Uuid::from_u128(8), &["白芷"], &["饮料"], 1);
        let other_tag = prior(2, session, &["白芷"], &["天气"], 1);
        let new_event = NewEventFacts {
            id: Uuid::from_u128(9),
            session_id: session,
            participants: vec!["白芷".into()],
            tags: vec!["饮料".into()],
        };
        assert!(derive_edges(&new_event, None, &[other_session, other_tag], None).is_empty());
    }

    // ── Knowledge scope ───────────────────────────────────────────────────

    #[test]
    fn empty_scope_is_visible_to_everyone() {
        assert!(knowledge_scope_allows(&[], Some("裴烬")));
        assert!(knowledge_scope_allows(&[], Some("白芷")));
        assert!(knowledge_scope_allows(&[], None));
    }

    #[test]
    fn scoped_event_is_visible_only_to_the_named_viewers() {
        let scope = vec!["裴烬".to_string()];
        assert!(knowledge_scope_allows(&scope, Some("裴烬")));
        assert!(!knowledge_scope_allows(&scope, Some("白芷")));
        assert!(!knowledge_scope_allows(&scope, Some("陆衍舟")));
        // Fails closed: an unresolved POV must not leak a secret.
        assert!(!knowledge_scope_allows(&scope, None));
        assert!(!knowledge_scope_allows(&scope, Some("   ")));
    }

    // ── Scoring ───────────────────────────────────────────────────────────

    fn signals(similarity: f64, importance: EventImportance, distance: u8) -> RecallSignals {
        RecallSignals {
            vector_similarity: similarity,
            importance,
            participant_overlap: 1,
            participant_union: 1,
            graph_distance: distance,
            age_days: 0.0,
            recall_count: 0,
            days_since_recall: None,
        }
    }

    #[test]
    fn similarity_dominates_the_score() {
        let high = recall_score(&signals(0.9, EventImportance::Normal, 0));
        let low = recall_score(&signals(0.1, EventImportance::Normal, 0));
        assert!(high > low);
        assert!((high - low - W_VECTOR * 0.8).abs() < 1e-9);
    }

    #[test]
    fn graph_distance_costs_exactly_one_penalty_unit() {
        let anchor = recall_score(&signals(0.5, EventImportance::Normal, 0));
        let neighbour = recall_score(&signals(0.5, EventImportance::Normal, 1));
        assert!((anchor - neighbour - W_GRAPH_PENALTY).abs() < 1e-9);
    }

    #[test]
    fn importance_raises_the_score_monotonically() {
        let scores: Vec<f64> = [
            EventImportance::Light,
            EventImportance::Normal,
            EventImportance::Important,
            EventImportance::Major,
        ]
        .iter()
        .map(|importance| recall_score(&signals(0.5, *importance, 0)))
        .collect();
        for pair in scores.windows(2) {
            assert!(pair[1] > pair[0], "{scores:?} must increase");
        }
    }

    #[test]
    fn older_events_score_lower() {
        let fresh = RecallSignals {
            age_days: 0.0,
            ..signals(0.5, EventImportance::Normal, 0)
        };
        let stale = RecallSignals {
            age_days: RECENCY_HALFLIFE_DAYS,
            ..signals(0.5, EventImportance::Normal, 0)
        };
        assert!(recall_score(&fresh) > recall_score(&stale));
    }

    #[test]
    fn participant_overlap_is_a_ratio_not_a_count() {
        let named_everyone = RecallSignals {
            participant_overlap: 1,
            participant_union: 4,
            ..signals(0.5, EventImportance::Normal, 0)
        };
        let named_one = RecallSignals {
            participant_overlap: 1,
            participant_union: 1,
            ..signals(0.5, EventImportance::Normal, 0)
        };
        assert!(recall_score(&named_one) > recall_score(&named_everyone));
    }

    #[test]
    fn participant_overlap_counts_shared_names_and_unions_the_rest() {
        let scene = vec!["白芷".to_string(), "裴烬".to_string()];
        let event = vec!["裴烬".to_string(), "陆衍舟".to_string()];
        assert_eq!(participant_overlap(&scene, &event), (1, 3));
        assert_eq!(participant_overlap(&[], &event), (0, 2));
    }

    // ── Cooldown ──────────────────────────────────────────────────────────

    #[test]
    fn never_recalled_events_are_not_penalised() {
        assert_eq!(
            cooldown_penalty(0, None, EventImportance::Normal),
            0.0,
            "a fresh event must not be penalised"
        );
        assert_eq!(cooldown_penalty(3, None, EventImportance::Normal), 0.0);
    }

    #[test]
    fn cooldown_grows_with_recalls_and_decays_with_time() {
        let just_now = cooldown_penalty(3, Some(0.0), EventImportance::Light);
        let yesterday = cooldown_penalty(3, Some(1.0), EventImportance::Light);
        let long_ago = cooldown_penalty(3, Some(30.0), EventImportance::Light);
        assert!(just_now > yesterday && yesterday > long_ago);
        assert!(long_ago < 0.01, "30 days later the penalty is gone");

        let once = cooldown_penalty(1, Some(0.0), EventImportance::Light);
        let thrice = cooldown_penalty(3, Some(0.0), EventImportance::Light);
        assert!(thrice > once);
    }

    #[test]
    fn cooldown_is_capped_so_a_beat_never_vanishes() {
        let capped = cooldown_penalty(50, Some(0.0), EventImportance::Light);
        assert!((capped - COOLDOWN_MAX).abs() < 1e-9);
    }

    #[test]
    fn durable_events_keep_only_a_quarter_of_the_penalty() {
        let light = cooldown_penalty(3, Some(0.0), EventImportance::Light);
        let normal = cooldown_penalty(3, Some(0.0), EventImportance::Normal);
        let major = cooldown_penalty(3, Some(0.0), EventImportance::Major);
        assert_eq!(light, normal, "the tier only matters once it is durable");
        assert!((major - light * COOLDOWN_IMPORTANT_DAMPENER).abs() < 1e-9);
    }

    #[test]
    fn a_recently_shown_light_event_loses_to_an_unshown_equal_one() {
        let shown = RecallSignals {
            recall_count: 4,
            days_since_recall: Some(0.0),
            ..signals(0.6, EventImportance::Light, 0)
        };
        let unseen = signals(0.6, EventImportance::Light, 0);
        assert!(recall_score(&shown) < recall_score(&unseen));
    }

    // ── Dedup + injection ─────────────────────────────────────────────────

    #[test]
    fn dedup_drops_candidates_already_covered_by_shared_memories() {
        let candidates = vec![
            "白芷给裴烬买过一瓶难喝的饮料，他嫌弃但最后喝完了".to_string(),
            "两人约好周日上午十点一起去松林公园拍秋叶".to_string(),
        ];
        let known = vec!["白芷给裴烬买过一瓶难喝的饮料，他嫌弃但最后喝完了。".to_string()];
        let kept = dedupe_against_known(&candidates, &known);
        assert_eq!(kept, vec!["两人约好周日上午十点一起去松林公园拍秋叶"]);
    }

    #[test]
    fn dedup_keeps_everything_when_nothing_overlaps() {
        let candidates = vec!["两人在路边见过一只特别丑的猫".to_string()];
        assert_eq!(
            dedupe_against_known(&candidates, &["用户住在上海".to_string()]),
            candidates
        );
    }

    #[test]
    fn dedup_keeps_a_short_candidate_unless_a_known_memory_covers_it() {
        // A brief but real event survives.
        let candidates = vec!["两人去公园".to_string()];
        assert_eq!(dedupe_against_known(&candidates, &[]), candidates);

        // A candidate with nothing but punctuation is not injectable text.
        assert!(dedupe_against_known(&["？？".to_string()], &[]).is_empty());

        // And a short candidate an existing memory already states is dropped.
        let known = vec!["他们约好明天上午一起去公园散步".to_string()];
        assert!(dedupe_against_known(&["去公园".to_string()], &known).is_empty());
    }

    #[test]
    fn dedup_key_keeps_cjk_instead_of_stripping_it() {
        assert_eq!(normalize_for_dedup("裴烬：难喝的饮料！"), "裴烬难喝的饮料");
        assert_eq!(normalize_for_dedup("  Coffee  "), "coffee");
    }

    #[test]
    fn render_past_experiences_matches_the_documented_shape() {
        let rendered = render_past_experiences(&[
            "很久以前，白芷曾给裴烬买过一瓶很难喝的饮料。".to_string(),
            "  ".to_string(),
        ])
        .unwrap();
        assert_eq!(
            rendered,
            "[Relevant Past Experiences]\n- 很久以前，白芷曾给裴烬买过一瓶很难喝的饮料。"
        );
    }

    #[test]
    fn render_past_experiences_is_absent_when_there_is_nothing_to_say() {
        assert!(render_past_experiences(&[]).is_none());
        assert!(render_past_experiences(&["   ".to_string()]).is_none());
    }
}
