-- V1 graph-enhanced episodic memory.
--
-- Why this extends `engine.recent_episodes` (0063) instead of adding a
-- parallel `events` table: that table already carries the two things this
-- layer must not invent — the (user_id, instance_id, session_id) scope and the
-- source message range — plus the recall-cooldown columns (`recall_count`,
-- `last_recalled_at`) that the V1 decay needs. What it lacks is the semantic
-- half, and every column added below is additive with a default that keeps
-- existing rows valid.
--
-- Lifecycle note: 0063's rows are short-lived. Events are not. A writer stores
-- an event with `expires_at = NULL`, which the existing candidate query already
-- treats as "never expires", and only an explicit deactivation flips
-- `is_active`.

ALTER TABLE engine.recent_episodes
    ADD COLUMN event_type            TEXT      NOT NULL DEFAULT 'callback',
    ADD COLUMN participants          TEXT[]    NOT NULL DEFAULT '{}',
    ADD COLUMN location              TEXT,
    ADD COLUMN importance            TEXT      NOT NULL DEFAULT 'normal',
    ADD COLUMN knowledge_scope       TEXT[]    NOT NULL DEFAULT '{}',
    ADD COLUMN relationship_relevant BOOLEAN   NOT NULL DEFAULT false,
    ADD COLUMN story_time            TIMESTAMPTZ,
    ADD COLUMN embedding             VECTOR(512);

-- Two closed vocabularies. `type` and `importance` originate in model output,
-- so the database is the last place a typo can still be caught.
ALTER TABLE engine.recent_episodes
    ADD CONSTRAINT recent_episodes_event_type_check
        CHECK (event_type IN ('callback', 'plot')),
    ADD CONSTRAINT recent_episodes_importance_check
        CHECK (importance IN ('light', 'normal', 'important', 'major'));

-- Same access method and parameters as companion_memories (0003): one `<=>`
-- query per recall, and `lists = 100` is already this project's tuning for
-- 512-dim vectors. NULL embeddings are simply not indexed, which is correct —
-- an event written without a vector is unreachable by anchor search and can
-- still be reached through its edges.
CREATE INDEX idx_recent_episodes_embedding
    ON engine.recent_episodes USING ivfflat (embedding vector_cosine_ops)
    WITH (lists = 100);

-- Both filters run before the vector scan, and both columns are arrays.
CREATE INDEX idx_recent_episodes_participants
    ON engine.recent_episodes USING gin (participants);
CREATE INDEX idx_recent_episodes_knowledge_scope
    ON engine.recent_episodes USING gin (knowledge_scope);

-- The entire V1 graph: event nodes plus event→event edges. Deliberately not a
-- person/location knowledge graph — participants and location are columns on
-- the node, not nodes of their own.
CREATE TABLE engine.event_edges (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    source_event_id UUID NOT NULL
        REFERENCES engine.recent_episodes(id) ON DELETE CASCADE,
    target_event_id UUID NOT NULL
        REFERENCES engine.recent_episodes(id) ON DELETE CASCADE,
    relation_type   TEXT NOT NULL,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT event_edges_relation_check
        CHECK (relation_type IN ('followed_by', 'caused', 'related_to')),
    -- A self loop would make 1-hop expansion return the anchor as its own
    -- neighbour.
    CONSTRAINT event_edges_no_self_loop CHECK (source_event_id <> target_event_id),
    -- Re-deriving the same edge (e.g. a retried turn) must be a no-op rather
    -- than a second row that doubles the candidate's weight.
    CONSTRAINT event_edges_unique
        UNIQUE (source_event_id, target_event_id, relation_type)
);

-- Expansion reads in both directions ("what came before / after this anchor"),
-- so the target column needs an index as much as the source does.
CREATE INDEX idx_event_edges_source ON engine.event_edges (source_event_id);
CREATE INDEX idx_event_edges_target ON engine.event_edges (target_event_id);

ALTER TABLE engine.event_edges ENABLE ROW LEVEL SECURITY;
