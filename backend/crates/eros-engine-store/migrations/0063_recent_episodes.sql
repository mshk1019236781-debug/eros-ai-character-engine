-- Lightweight, relationship-scoped recent episodes.
--
-- Episodes are stored independently from the existing memory layer. They are
-- short-lived source-linked summaries; no embedding or model-owned state is
-- involved here.
CREATE TABLE engine.recent_episodes (
    id                       UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    user_id                  UUID NOT NULL,
    instance_id              UUID NOT NULL
        REFERENCES engine.persona_instances(id) ON DELETE CASCADE,
    session_id               UUID NOT NULL
        REFERENCES engine.chat_sessions(id) ON DELETE CASCADE,
    summary                  TEXT NOT NULL,
    tags                     TEXT[] NOT NULL DEFAULT '{}',
    source_start_message_id  UUID NOT NULL
        REFERENCES engine.chat_messages(id) ON DELETE CASCADE,
    source_end_message_id    UUID NOT NULL
        REFERENCES engine.chat_messages(id) ON DELETE CASCADE,
    created_at               TIMESTAMPTZ NOT NULL DEFAULT now(),
    created_turn             INTEGER NOT NULL,
    salience                 DOUBLE PRECISION NOT NULL DEFAULT 0.0,
    last_recalled_at         TIMESTAMPTZ,
    recall_count             INTEGER NOT NULL DEFAULT 0,
    expires_at               TIMESTAMPTZ,
    is_active                BOOLEAN NOT NULL DEFAULT true
);

CREATE INDEX idx_recent_episodes_candidates
    ON engine.recent_episodes (user_id, instance_id, is_active, salience DESC, created_turn DESC);

CREATE INDEX idx_recent_episodes_expiry
    ON engine.recent_episodes (expires_at)
    WHERE is_active AND expires_at IS NOT NULL;

ALTER TABLE engine.recent_episodes ENABLE ROW LEVEL SECURITY;
