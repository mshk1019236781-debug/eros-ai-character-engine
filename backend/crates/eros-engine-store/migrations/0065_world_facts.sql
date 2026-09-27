-- V1 minimal world/entity facts.
--
-- Why a table of its own rather than another `recent_episodes` row: a fact has
-- none of an event's properties. It has no story stage, no source message
-- range, no embedding and no edges — and it must survive the event layer's
-- recall cooldown and expiry rules untouched. Sharing the table would mean
-- either weakening those rules or teaching the event scorer about a row it
-- should never rank.
--
-- Scope columns mirror `recent_episodes` exactly: the same (user_id,
-- instance_id) tenancy, the same `knowledge_scope` allow-list semantics, the
-- same empty-scope-means-everyone rule. Nothing here is a second memory
-- namespace.

CREATE TABLE engine.world_facts (
    id                UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id           UUID NOT NULL,
    instance_id       UUID NOT NULL
        REFERENCES engine.persona_instances(id) ON DELETE CASCADE,
    subject           TEXT NOT NULL,
    predicate         TEXT NOT NULL,
    object            TEXT NOT NULL,
    fact_type         TEXT NOT NULL,
    -- Natural one-line rendering supplied by the model. Nullable: the writer
    -- falls back to a labelled triple, so a missing statement never blocks a
    -- fact from being stored.
    statement         TEXT,
    -- Characters allowed to see this fact. `{}` = everyone in this story; that
    -- has to be an explicit array comparison, not a NULL, so `is_active` stays
    -- the only "row exists or not" flag.
    knowledge_scope   TEXT[] NOT NULL DEFAULT '{}',
    -- Provenance: which message/turn stated it. Nullable because a fact may
    -- outlive the message it came from (session purge) without becoming
    -- meaningless.
    source_message_id UUID
        REFERENCES engine.chat_messages(id) ON DELETE SET NULL,
    source_turn       INTEGER,
    is_active         BOOLEAN NOT NULL DEFAULT true,
    created_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at        TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT world_facts_fact_type_check
        CHECK (fact_type IN ('identity', 'relationship', 'occupation',
                             'affiliation', 'world_setting')),
    -- The closed vocabulary above is a typo guard; these three are the
    -- "a fact must say something" guard, the same reason `recent_episodes`
    -- checks its own enums in the database rather than trusting the caller.
    CONSTRAINT world_facts_subject_check  CHECK (btrim(subject)   <> ''),
    CONSTRAINT world_facts_predicate_check CHECK (btrim(predicate) <> ''),
    CONSTRAINT world_facts_object_check   CHECK (btrim(object)    <> '')
);

-- One *active* fact per (subject, predicate). A re-stated identical fact is an
-- update-in-place; a changed object deactivates the old row in the same
-- transaction instead of accumulating a contradiction. Inactive rows stay for
-- traceability, which is why the uniqueness is partial rather than absolute.
CREATE UNIQUE INDEX world_facts_active_unique
    ON engine.world_facts (user_id, instance_id, subject, predicate)
    WHERE is_active;

-- The prompt path reads "this user/instance's active facts, newest first".
CREATE INDEX idx_world_facts_lookup
    ON engine.world_facts (user_id, instance_id, is_active, updated_at DESC);

-- Scope filtering is an array containment/equality check at read time.
CREATE INDEX idx_world_facts_scope
    ON engine.world_facts USING gin (knowledge_scope);

ALTER TABLE engine.world_facts ENABLE ROW LEVEL SECURITY;
