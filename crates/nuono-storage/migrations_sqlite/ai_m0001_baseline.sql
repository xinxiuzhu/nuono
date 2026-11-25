-- Nuono 可选 AI / Agent / 陪伴模块表结构（SQLite）。
-- 仅在启用 AI 或个人助手能力时按需安装。

CREATE TABLE nuono_ai_models (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id > 0),
    name TEXT NOT NULL CHECK (length(name) <= 100),
    provider TEXT NOT NULL CHECK (length(provider) <= 50),
    model_id TEXT NOT NULL CHECK (length(model_id) <= 200),
    api_key TEXT,
    base_url TEXT NOT NULL CHECK (length(base_url) <= 500),
    is_default INTEGER NOT NULL DEFAULT 0 CHECK (is_default IN (0, 1)),
    enabled INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    created_at INTEGER DEFAULT (CAST(unixepoch('subsec') * 1000 AS INTEGER)),
    updated_at INTEGER DEFAULT (CAST(unixepoch('subsec') * 1000 AS INTEGER)),
    provider_type TEXT NOT NULL DEFAULT 'openai_compatible' CHECK (length(provider_type) <= 30),
    CHECK (instr(name, char(0)) = 0 AND instr(provider, char(0)) = 0 AND instr(model_id, char(0)) = 0 AND instr(base_url, char(0)) = 0 AND instr(provider_type, char(0)) = 0),
    CHECK (api_key IS NULL OR instr(api_key, char(0)) = 0)
) STRICT;

CREATE TABLE nuono_ai_usage_events (
    request_id TEXT PRIMARY KEY NOT NULL CHECK (length(request_id) <= 64),
    started_at_ms INTEGER NOT NULL,
    day TEXT NOT NULL CHECK (length(day) <= 10),
    model_id INTEGER,
    model TEXT NOT NULL CHECK (length(model) <= 256),
    name TEXT NOT NULL CHECK (length(name) <= 512),
    provider TEXT NOT NULL CHECK (length(provider) <= 256),
    source TEXT NOT NULL CHECK (length(source) <= 64),
    operation TEXT NOT NULL CHECK (length(operation) <= 32),
    outcome TEXT NOT NULL CHECK (length(outcome) <= 16),
    elapsed_ms INTEGER NOT NULL,
    input_tokens INTEGER,
    output_tokens INTEGER,
    total_tokens INTEGER,
    cached_input_tokens INTEGER,
    reasoning_output_tokens INTEGER,
    CHECK (instr(request_id, char(0)) = 0 AND instr(day, char(0)) = 0 AND instr(model, char(0)) = 0 AND instr(name, char(0)) = 0 AND instr(provider, char(0)) = 0 AND instr(source, char(0)) = 0 AND instr(operation, char(0)) = 0 AND instr(outcome, char(0)) = 0)
) STRICT;

CREATE TABLE nuono_ai_personas (
    id TEXT PRIMARY KEY NOT NULL CHECK (length(id) <= 64),
    owner_user_id INTEGER REFERENCES nuono_users(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    description TEXT NOT NULL DEFAULT '',
    system_prompt TEXT NOT NULL DEFAULT '',
    user_address TEXT NOT NULL DEFAULT '',
    enabled INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    avatar_filename TEXT,
    portrait_filename TEXT,
    reference_audio_filename TEXT,
    reference_text TEXT NOT NULL DEFAULT '',
    language TEXT NOT NULL DEFAULT 'zh',
    tts_provider TEXT,
    voice_id TEXT,
    revision INTEGER NOT NULL DEFAULT 1 CHECK (revision > 0),
    updated_at INTEGER NOT NULL DEFAULT (CAST(unixepoch('subsec') * 1000 AS INTEGER)),
    CHECK (instr(id, char(0)) = 0 AND instr(name, char(0)) = 0 AND instr(description, char(0)) = 0 AND instr(system_prompt, char(0)) = 0 AND instr(user_address, char(0)) = 0 AND instr(reference_text, char(0)) = 0 AND instr(language, char(0)) = 0),
    CHECK (avatar_filename IS NULL OR instr(avatar_filename, char(0)) = 0),
    CHECK (portrait_filename IS NULL OR instr(portrait_filename, char(0)) = 0),
    CHECK (reference_audio_filename IS NULL OR instr(reference_audio_filename, char(0)) = 0),
    CHECK (tts_provider IS NULL OR instr(tts_provider, char(0)) = 0),
    CHECK (voice_id IS NULL OR instr(voice_id, char(0)) = 0)
) STRICT;

CREATE TABLE nuono_ai_persona_bindings (
    target_key TEXT PRIMARY KEY NOT NULL CHECK (length(target_key) <= 64),
    platform TEXT NOT NULL,
    bot_id TEXT NOT NULL,
    group_id TEXT NOT NULL,
    namespace TEXT,
    persona_id TEXT NOT NULL REFERENCES nuono_ai_personas(id) ON DELETE RESTRICT CHECK (length(persona_id) <= 64),
    CHECK (instr(target_key, char(0)) = 0 AND instr(platform, char(0)) = 0 AND instr(bot_id, char(0)) = 0 AND instr(group_id, char(0)) = 0 AND instr(persona_id, char(0)) = 0),
    CHECK (namespace IS NULL OR instr(namespace, char(0)) = 0)
) STRICT;

CREATE TABLE nuono_account_assistant (
    user_id INTEGER PRIMARY KEY REFERENCES nuono_users(id) ON DELETE CASCADE,
    grant_until INTEGER,
    grant_daily_limit INTEGER CHECK (grant_daily_limit BETWEEN 1 AND 500),
    usage_day INTEGER NOT NULL DEFAULT -1,
    request_ids TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(request_ids) AND json_type(request_ids) = 'array'),
    active_card_id TEXT,
    context_epochs_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(context_epochs_json) AND json_type(context_epochs_json) = 'object'),
    CHECK ((grant_until IS NULL) = (grant_daily_limit IS NULL)),
    CHECK (instr(request_ids, char(0)) = 0 AND instr(context_epochs_json, char(0)) = 0),
    CHECK (active_card_id IS NULL OR instr(active_card_id, char(0)) = 0)
) STRICT;

CREATE TABLE nuono_assistant_turns (
    id TEXT PRIMARY KEY NOT NULL,
    user_id INTEGER NOT NULL REFERENCES nuono_users(id) ON DELETE CASCADE,
    conversation_id TEXT REFERENCES nuono_conversations(id) ON DELETE SET NULL CHECK (conversation_id IS NULL OR length(CAST(conversation_id AS BLOB)) BETWEEN 1 AND 128),
    card_id TEXT NOT NULL,
    scope_key TEXT NOT NULL,
    context_epoch INTEGER NOT NULL DEFAULT 0,
    prompt_snapshot TEXT NOT NULL DEFAULT '',
    redacted INTEGER NOT NULL DEFAULT 0 CHECK (redacted IN (0, 1)),
    control INTEGER NOT NULL DEFAULT 0 CHECK (control IN (0, 1)),
    compacted INTEGER NOT NULL DEFAULT 0 CHECK (compacted IN (0, 1)),
    request_key TEXT NOT NULL,
    text TEXT NOT NULL,
    retry_of TEXT,
    status TEXT NOT NULL CHECK (status IN ('running', 'succeeded', 'failed', 'interrupted')),
    failure TEXT,
    reply TEXT,
    quota_day INTEGER NOT NULL,
    delivery TEXT NOT NULL CHECK (delivery IN ('none', 'pending', 'sent', 'unknown')),
    delivery_key TEXT,
    created_at INTEGER NOT NULL,
    UNIQUE (user_id, request_key),
    UNIQUE (user_id, retry_of),
    CHECK (instr(id, char(0)) = 0 AND instr(card_id, char(0)) = 0 AND instr(scope_key, char(0)) = 0 AND instr(prompt_snapshot, char(0)) = 0 AND instr(request_key, char(0)) = 0 AND instr(text, char(0)) = 0),
    CHECK (conversation_id IS NULL OR instr(conversation_id, char(0)) = 0),
    CHECK (retry_of IS NULL OR instr(retry_of, char(0)) = 0),
    CHECK (failure IS NULL OR instr(failure, char(0)) = 0),
    CHECK (reply IS NULL OR instr(reply, char(0)) = 0),
    CHECK (delivery_key IS NULL OR instr(delivery_key, char(0)) = 0)
) STRICT;

CREATE TABLE nuono_agent_runs (
    id TEXT PRIMARY KEY NOT NULL CHECK (length(id) <= 64),
    conversation_id TEXT REFERENCES nuono_conversations(id) ON DELETE SET NULL CHECK (conversation_id IS NULL OR length(CAST(conversation_id AS BLOB)) BETWEEN 1 AND 128),
    session_id TEXT NOT NULL,
    status TEXT NOT NULL CHECK (length(status) <= 32),
    state_json TEXT NOT NULL CHECK (json_valid(state_json)),
    approval_id INTEGER,
    revision INTEGER NOT NULL DEFAULT 0,
    updated_at INTEGER NOT NULL DEFAULT (CAST(unixepoch('subsec') * 1000 AS INTEGER)),
    parent_id TEXT REFERENCES nuono_agent_runs(id) CHECK (parent_id IS NULL OR length(parent_id) <= 64),
    CHECK (instr(id, char(0)) = 0 AND instr(session_id, char(0)) = 0 AND instr(status, char(0)) = 0 AND instr(state_json, char(0)) = 0),
    CHECK (conversation_id IS NULL OR instr(conversation_id, char(0)) = 0),
    CHECK (parent_id IS NULL OR instr(parent_id, char(0)) = 0)
) STRICT;

CREATE TABLE nuono_agent_policy_rules (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id > 0),
    effect TEXT NOT NULL,
    target_type TEXT NOT NULL,
    target_value TEXT NOT NULL,
    priority INTEGER NOT NULL DEFAULT 100 CHECK (priority BETWEEN -2147483648 AND 2147483647),
    enabled INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    reason TEXT,
    created_by TEXT,
    updated_by TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    CHECK (instr(effect, char(0)) = 0 AND instr(target_type, char(0)) = 0 AND instr(target_value, char(0)) = 0),
    CHECK (reason IS NULL OR instr(reason, char(0)) = 0),
    CHECK (created_by IS NULL OR instr(created_by, char(0)) = 0),
    CHECK (updated_by IS NULL OR instr(updated_by, char(0)) = 0)
) STRICT;

CREATE TABLE nuono_agent_tool_approvals (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id > 0),
    session_id TEXT NOT NULL,
    bot_id INTEGER NOT NULL,
    platform TEXT NOT NULL,
    message_type TEXT NOT NULL,
    user_id INTEGER NOT NULL,
    group_id INTEGER,
    channel_id TEXT,
    message_id TEXT,
    tool_name TEXT NOT NULL,
    risk_level TEXT NOT NULL,
    status TEXT NOT NULL,
    input_json TEXT CHECK (input_json IS NULL OR json_valid(input_json)),
    reason TEXT,
    decision_by TEXT,
    decision_reason TEXT,
    decided_at INTEGER,
    created_at INTEGER NOT NULL,
    context_json TEXT CHECK (context_json IS NULL OR json_valid(context_json)),
    execution_json TEXT CHECK (execution_json IS NULL OR json_valid(execution_json)),
    run_id TEXT CHECK (run_id IS NULL OR length(run_id) <= 64),
    CHECK (instr(session_id, char(0)) = 0 AND instr(platform, char(0)) = 0 AND instr(message_type, char(0)) = 0 AND instr(tool_name, char(0)) = 0 AND instr(risk_level, char(0)) = 0 AND instr(status, char(0)) = 0),
    CHECK (channel_id IS NULL OR instr(channel_id, char(0)) = 0),
    CHECK (message_id IS NULL OR instr(message_id, char(0)) = 0),
    CHECK (input_json IS NULL OR instr(input_json, char(0)) = 0),
    CHECK (reason IS NULL OR instr(reason, char(0)) = 0),
    CHECK (decision_by IS NULL OR instr(decision_by, char(0)) = 0),
    CHECK (decision_reason IS NULL OR instr(decision_reason, char(0)) = 0),
    CHECK (context_json IS NULL OR instr(context_json, char(0)) = 0),
    CHECK (execution_json IS NULL OR instr(execution_json, char(0)) = 0),
    CHECK (run_id IS NULL OR instr(run_id, char(0)) = 0)
) STRICT;

CREATE TABLE nuono_agent_tool_receipts (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id > 0),
    session_id TEXT NOT NULL,
    bot_id INTEGER NOT NULL,
    platform TEXT NOT NULL,
    message_type TEXT NOT NULL,
    user_id INTEGER NOT NULL,
    group_id INTEGER,
    channel_id TEXT,
    message_id TEXT,
    tool_name TEXT NOT NULL,
    risk_level TEXT NOT NULL,
    status TEXT NOT NULL,
    input_json TEXT CHECK (input_json IS NULL OR json_valid(input_json)),
    output_json TEXT CHECK (output_json IS NULL OR json_valid(output_json)),
    error TEXT,
    elapsed_ms INTEGER NOT NULL DEFAULT 0 CHECK (elapsed_ms BETWEEN -2147483648 AND 2147483647),
    created_at INTEGER NOT NULL,
    context_json TEXT CHECK (context_json IS NULL OR json_valid(context_json)),
    CHECK (instr(session_id, char(0)) = 0 AND instr(platform, char(0)) = 0 AND instr(message_type, char(0)) = 0 AND instr(tool_name, char(0)) = 0 AND instr(risk_level, char(0)) = 0 AND instr(status, char(0)) = 0),
    CHECK (channel_id IS NULL OR instr(channel_id, char(0)) = 0),
    CHECK (message_id IS NULL OR instr(message_id, char(0)) = 0),
    CHECK (input_json IS NULL OR instr(input_json, char(0)) = 0),
    CHECK (output_json IS NULL OR instr(output_json, char(0)) = 0),
    CHECK (error IS NULL OR instr(error, char(0)) = 0),
    CHECK (context_json IS NULL OR instr(context_json, char(0)) = 0)
) STRICT;

CREATE TABLE nuono_agent_memories (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id > 0),
    scope_type TEXT NOT NULL,
    scope_key TEXT NOT NULL,
    memory_key TEXT NOT NULL,
    content TEXT NOT NULL,
    tags_json TEXT CHECK (tags_json IS NULL OR json_valid(tags_json)),
    pinned INTEGER CHECK (pinned IN (0, 1)),
    recall_count INTEGER NOT NULL DEFAULT 0 CHECK (recall_count >= 0),
    last_recalled_at INTEGER,
    enabled INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    created_by TEXT,
    updated_by TEXT,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    expires_at INTEGER,
    CHECK (pinned IS NOT NULL OR (recall_count = 0 AND last_recalled_at IS NULL)),
    CHECK (instr(scope_type, char(0)) = 0 AND instr(scope_key, char(0)) = 0 AND instr(memory_key, char(0)) = 0 AND instr(content, char(0)) = 0),
    CHECK (tags_json IS NULL OR instr(tags_json, char(0)) = 0),
    CHECK (created_by IS NULL OR instr(created_by, char(0)) = 0),
    CHECK (updated_by IS NULL OR instr(updated_by, char(0)) = 0)
) STRICT;

CREATE TABLE nuono_agent_memory_embeddings (
    memory_id INTEGER NOT NULL REFERENCES nuono_agent_memories(id) ON DELETE CASCADE,
    model_id INTEGER NOT NULL CHECK (model_id > 0),
    model_name TEXT NOT NULL CHECK (length(CAST(model_name AS BLOB)) BETWEEN 1 AND 512),
    model_revision TEXT NOT NULL CHECK (length(CAST(model_revision AS BLOB)) BETWEEN 1 AND 128),
    content_fingerprint TEXT NOT NULL CHECK (length(content_fingerprint) = 64),
    dimensions INTEGER NOT NULL CHECK (dimensions BETWEEN 1 AND 4096),
    vector_json TEXT NOT NULL CHECK (length(CAST(vector_json AS BLOB)) <= 65536 AND json_valid(vector_json)),
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (memory_id, model_id),
    CHECK (instr(model_name, char(0)) = 0 AND instr(model_revision, char(0)) = 0 AND instr(content_fingerprint, char(0)) = 0 AND instr(vector_json, char(0)) = 0)
) STRICT;

CREATE TABLE nuono_agent_memory_graphs (
    scope_type TEXT NOT NULL,
    scope_key TEXT NOT NULL,
    source_fingerprint TEXT NOT NULL,
    source_limit INTEGER NOT NULL CHECK (source_limit BETWEEN 2 AND 128),
    graph_json TEXT NOT NULL CHECK (json_valid(graph_json)),
    updated_at INTEGER NOT NULL,
    PRIMARY KEY (scope_type, scope_key),
    CHECK (instr(scope_type, char(0)) = 0 AND instr(scope_key, char(0)) = 0 AND instr(source_fingerprint, char(0)) = 0 AND instr(graph_json, char(0)) = 0)
) STRICT;

CREATE TABLE nuono_companion_memory_sources (
    memory_id INTEGER NOT NULL REFERENCES nuono_agent_memories(id) ON DELETE CASCADE,
    source_id TEXT NOT NULL,
    scope_key TEXT NOT NULL,
    detail TEXT NOT NULL CHECK (detail IN ('full', 'summary', 'outline')),
    original TEXT NOT NULL CHECK (original IN ('available', 'reclaimed', 'deleted')),
    importance REAL NOT NULL DEFAULT 0.5,
    observed_at INTEGER NOT NULL,
    PRIMARY KEY (memory_id, source_id),
    CHECK (instr(source_id, char(0)) = 0 AND instr(scope_key, char(0)) = 0)
) STRICT;

CREATE INDEX idx_ai_personas_owner ON nuono_ai_personas (owner_user_id, id);
CREATE INDEX ai_persona_bindings_persona ON nuono_ai_persona_bindings (persona_id);
CREATE INDEX idx_assistant_turns_account ON nuono_assistant_turns (user_id, created_at DESC, id);
CREATE INDEX idx_assistant_turns_pending ON nuono_assistant_turns (status, user_id);
CREATE INDEX idx_assistant_turns_conversation ON nuono_assistant_turns (conversation_id) WHERE conversation_id IS NOT NULL;
CREATE INDEX idx_companion_sources_scope ON nuono_companion_memory_sources (scope_key, source_id);
CREATE INDEX idx_ai_usage_started_at ON nuono_ai_usage_events (started_at_ms);
CREATE INDEX idx_ai_usage_model_started_at ON nuono_ai_usage_events (model_id, started_at_ms);
CREATE INDEX idx_agent_memories_enabled ON nuono_agent_memories (enabled);
CREATE INDEX idx_agent_memories_expires_at ON nuono_agent_memories (expires_at);
CREATE UNIQUE INDEX idx_agent_memories_unique_key ON nuono_agent_memories (scope_type, scope_key, memory_key);
CREATE INDEX idx_agent_policy_rules_enabled ON nuono_agent_policy_rules (enabled);
CREATE INDEX idx_agent_tool_approvals_status ON nuono_agent_tool_approvals (status);
CREATE INDEX idx_agent_tool_receipts_session_id ON nuono_agent_tool_receipts (session_id);
CREATE UNIQUE INDEX agent_runs_approval ON nuono_agent_runs (approval_id) WHERE approval_id IS NOT NULL;
CREATE INDEX agent_runs_parent ON nuono_agent_runs (parent_id);
CREATE INDEX idx_agent_runs_conversation ON nuono_agent_runs (conversation_id) WHERE conversation_id IS NOT NULL;
CREATE UNIQUE INDEX agent_runs_active_session ON nuono_agent_runs (session_id)
    WHERE status IN ('queued', 'running', 'waiting_approval', 'waiting_children');
