-- Expression Recovery V1: the exemplar pool and the recovery state.
--
-- Two tables, and the split is the point. `expression_exemplars` is a
-- reservoir of a character's historically stable expression; recovery state is
-- one row per relationship saying whether that reservoir is currently being
-- consulted. They have different lifetimes -- the pool only ever grows from
-- stable windows, the state row is rewritten on every review -- so one table
-- would mean rewriting the pool's rows to toggle a flag.
--
-- Tenancy mirrors `recent_episodes` and `world_facts` exactly: the same
-- (user_id, instance_id) scope, for the same reason migration 0047 gives for
-- keeping character state per relationship rather than per character. The
-- `character_id` column carries `persona_genomes.id` alongside it, so a
-- retrieval can be narrowed by identity as well as by tenancy -- a second
-- independent guard against one character's exemplars reaching another.

CREATE TABLE engine.expression_exemplars (
    id                UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id           UUID NOT NULL,
    instance_id       UUID NOT NULL
        REFERENCES engine.persona_instances(id) ON DELETE CASCADE,
    -- persona_genomes.id of the character this sample belongs to.
    character_id      UUID NOT NULL,
    -- The raw persisted Main RP reply. Always rendered verbatim; never a
    -- summary, never a rewrite.
    raw_text          TEXT NOT NULL,
    -- Whitespace-collapsed copy of raw_text, computed in Rust. raw_text stays
    -- byte-identical for the prompt; this column exists only to be the dedupe
    -- key, so the same reply re-observed with different line breaks is one row.
    text_key          TEXT NOT NULL,
    -- Closed vocabulary (conflict/care/refusal/jealousy/casual/confrontation),
    -- normalised before insert. Empty is legal: an untagged stable sample is
    -- still a valid generic exemplar.
    tags              TEXT[] NOT NULL DEFAULT '{}',
    -- Provenance. Both nullable and ON DELETE SET NULL: an exemplar outlives
    -- the session purge that removed the row it came from, and losing the
    -- pointer must not lose the sample.
    source_session_id UUID REFERENCES engine.chat_sessions(id) ON DELETE SET NULL,
    source_message_id UUID REFERENCES engine.chat_messages(id) ON DELETE SET NULL,
    source_turn       INTEGER,
    is_active         BOOLEAN NOT NULL DEFAULT true,
    -- Read-side bookkeeping only; nothing in selection depends on them today.
    used_count        INTEGER NOT NULL DEFAULT 0,
    last_used_at      TIMESTAMPTZ,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT expression_exemplars_text_check CHECK (btrim(raw_text) <> ''),
    CONSTRAINT expression_exemplars_key_check  CHECK (btrim(text_key)  <> '')
);

-- One active row per reply per character. Partial rather than absolute so a
-- deactivated row never blocks the same text from being re-adopted later.
CREATE UNIQUE INDEX expression_exemplars_unique
    ON engine.expression_exemplars (user_id, instance_id, character_id, text_key)
    WHERE is_active;

-- The prompt path reads "this character's active exemplars, newest first".
CREATE INDEX idx_expression_exemplars_pool
    ON engine.expression_exemplars
    (user_id, instance_id, character_id, is_active, created_at DESC);

-- Tag routing is an array overlap at read time.
CREATE INDEX idx_expression_exemplars_tags
    ON engine.expression_exemplars USING gin (tags);

ALTER TABLE engine.expression_exemplars ENABLE ROW LEVEL SECURITY;

-- The thinnest state the brief allows: is this character in recovery, how many
-- consecutive stable reviews have been seen, and which exemplars the current
-- recovery turn would inject. One row per (relationship, character); no history
-- table, no state machine beyond the three columns that decide the transition.
CREATE TABLE engine.expression_recovery_state (
    user_id             UUID NOT NULL,
    instance_id         UUID NOT NULL
        REFERENCES engine.persona_instances(id) ON DELETE CASCADE,
    character_id        UUID NOT NULL,
    active              BOOLEAN NOT NULL DEFAULT false,
    consecutive_stable  INTEGER NOT NULL DEFAULT 0,
    last_status         TEXT,
    last_severity       DOUBLE PRECISION,
    last_reviewed_at    TIMESTAMPTZ,
    started_at          TIMESTAMPTZ,
    -- The exemplars one recovery turn injects. Cleared when recovery ends.
    active_exemplar_ids UUID[] NOT NULL DEFAULT '{}',
    target_tags         TEXT[] NOT NULL DEFAULT '{}',
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (user_id, instance_id, character_id),
    CONSTRAINT expression_recovery_status_check
        CHECK (last_status IS NULL OR last_status IN ('stable', 'drift', 'collapse')),
    CONSTRAINT expression_recovery_stable_check CHECK (consecutive_stable >= 0)
);

ALTER TABLE engine.expression_recovery_state ENABLE ROW LEVEL SECURITY;
