-- Nuono 核心基线表结构（SQLite）。
-- 主键编号与 UTC Unix 毫秒时间统一使用有符号 64 位整数；落库 JSON 均经过校验。
-- 可选 AI / Agent / 陪伴模块表位于 ai_m0001_baseline.sql，按需启用时才创建。

CREATE TABLE nuono_schema_migrations (
    module TEXT NOT NULL DEFAULT 'core' CHECK (module IN ('core', 'ai')),
    version INTEGER NOT NULL CHECK (version > 0),
    description TEXT NOT NULL,
    checksum TEXT NOT NULL,
    applied_at_ms INTEGER NOT NULL,
    PRIMARY KEY (module, version),
    CHECK (instr(module, char(0)) = 0 AND instr(description, char(0)) = 0 AND instr(checksum, char(0)) = 0)
) STRICT;

CREATE TABLE nuono_deployment (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    deployment_id TEXT NOT NULL UNIQUE,
    maintenance_operation TEXT,
    assets_json TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(assets_json)),
    created_at_ms INTEGER NOT NULL,
    CHECK (instr(deployment_id, char(0)) = 0),
    CHECK (maintenance_operation IS NULL OR instr(maintenance_operation, char(0)) = 0),
    CHECK (instr(assets_json, char(0)) = 0)
) STRICT;

CREATE TABLE nuono_users (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id > 0),
    account_kind TEXT NOT NULL DEFAULT 'registered' CHECK (account_kind IN ('shadow', 'registered')),
    username TEXT,
    username_key TEXT UNIQUE,
    email TEXT,
    email_key TEXT UNIQUE,
    email_verified_at_ms INTEGER,
    password_hash TEXT,
    totp_enabled INTEGER NOT NULL DEFAULT 0 CHECK (totp_enabled IN (0, 1)),
    totp_secret BLOB,
    totp_recovery_json TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(totp_recovery_json) AND json_type(totp_recovery_json) = 'array'),
    totp_last_counter INTEGER CHECK (totp_last_counter >= 0),
    role TEXT NOT NULL DEFAULT 'user' CHECK (role IN ('owner', 'admin', 'user')),
    status TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'disabled', 'merged', 'deleted')),
    merged_into_user_id INTEGER REFERENCES nuono_users(id),
    auth_epoch INTEGER NOT NULL DEFAULT 1 CHECK (auth_epoch > 0),
    last_login_at_ms INTEGER,
    display_name TEXT NOT NULL DEFAULT '',
    birthday TEXT NOT NULL DEFAULT '',
    gender TEXT NOT NULL DEFAULT '',
    bio TEXT NOT NULL DEFAULT '',
    use_for_ai INTEGER NOT NULL DEFAULT 0 CHECK (use_for_ai IN (0, 1)),
    revision INTEGER NOT NULL DEFAULT 1 CHECK (revision > 0),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    CHECK ((username IS NULL) = (username_key IS NULL)),
    CHECK (username_key IS NULL OR (length(username_key) BETWEEN 3 AND 64 AND username_key = lower(username) AND username_key NOT GLOB '*[^a-z0-9_.-]*')),
    CHECK ((email IS NULL) = (email_key IS NULL) AND (email IS NULL) = (email_verified_at_ms IS NULL)),
    CHECK (email_key IS NULL OR (email_key = lower(trim(email)) AND email_key <> '')),
    CHECK ((status = 'merged' AND merged_into_user_id IS NOT NULL AND merged_into_user_id <> id) OR (status <> 'merged' AND merged_into_user_id IS NULL)),
    CHECK (
        (account_kind = 'registered' AND status IN ('active', 'disabled') AND username IS NOT NULL AND password_hash IS NOT NULL)
        OR (account_kind = 'shadow' AND status IN ('active', 'disabled') AND username IS NULL AND email IS NULL AND password_hash IS NULL AND totp_secret IS NULL AND totp_enabled = 0 AND totp_recovery_json = '[]' AND role = 'user')
        OR (status IN ('merged', 'deleted') AND username IS NULL AND email IS NULL AND password_hash IS NULL AND totp_secret IS NULL AND totp_enabled = 0 AND totp_recovery_json = '[]' AND role = 'user' AND display_name = '' AND birthday = '' AND gender = '' AND bio = '' AND use_for_ai = 0)
    ),
    CHECK (totp_enabled = 0 OR totp_secret IS NOT NULL),
    CHECK (username IS NULL OR instr(username, char(0)) = 0),
    CHECK (username_key IS NULL OR instr(username_key, char(0)) = 0),
    CHECK (email IS NULL OR instr(email, char(0)) = 0),
    CHECK (email_key IS NULL OR instr(email_key, char(0)) = 0),
    CHECK (password_hash IS NULL OR instr(password_hash, char(0)) = 0),
    CHECK (instr(totp_recovery_json, char(0)) = 0),
    CHECK (instr(display_name, char(0)) = 0 AND instr(birthday, char(0)) = 0 AND instr(gender, char(0)) = 0 AND instr(bio, char(0)) = 0)
) STRICT;

CREATE TABLE nuono_platform_identities (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id > 0),
    platform TEXT NOT NULL,
    namespace TEXT NOT NULL DEFAULT '',
    identity_kind TEXT NOT NULL,
    native_user_id TEXT NOT NULL,
    user_id INTEGER NOT NULL REFERENCES nuono_users(id) ON DELETE RESTRICT,
    verified_at_ms INTEGER,
    revision INTEGER NOT NULL DEFAULT 1 CHECK (revision > 0),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    UNIQUE (platform, namespace, identity_kind, native_user_id),
    CHECK (instr(platform, char(0)) = 0 AND instr(namespace, char(0)) = 0 AND instr(identity_kind, char(0)) = 0 AND instr(native_user_id, char(0)) = 0),
    CHECK (length(CAST(platform || namespace || identity_kind || native_user_id AS BLOB)) <= 1024)
) STRICT;

CREATE TABLE nuono_groups (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id > 0),
    platform TEXT NOT NULL,
    namespace TEXT NOT NULL DEFAULT '',
    conversation_kind TEXT NOT NULL,
    native_parent_id TEXT NOT NULL DEFAULT '',
    native_group_id TEXT NOT NULL,
    display_name TEXT NOT NULL DEFAULT '',
    enabled INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    revision INTEGER NOT NULL DEFAULT 1 CHECK (revision > 0),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    UNIQUE (platform, namespace, conversation_kind, native_parent_id, native_group_id),
    CHECK (instr(platform, char(0)) = 0 AND instr(namespace, char(0)) = 0 AND instr(conversation_kind, char(0)) = 0 AND instr(native_parent_id, char(0)) = 0 AND instr(native_group_id, char(0)) = 0 AND instr(display_name, char(0)) = 0),
    CHECK (length(CAST(platform || namespace || conversation_kind || native_parent_id || native_group_id AS BLOB)) <= 1024)
) STRICT;

CREATE TABLE nuono_plugins (
    plugin_id TEXT PRIMARY KEY NOT NULL CHECK (length(CAST(plugin_id AS BLOB)) BETWEEN 1 AND 64),
    plugin_role TEXT NOT NULL DEFAULT 'feature' CHECK (plugin_role IN ('feature', 'service')),
    enabled INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    allow_user_overrides INTEGER NOT NULL DEFAULT 0 CHECK (allow_user_overrides IN (0, 1)),
    allow_group_overrides INTEGER NOT NULL DEFAULT 0 CHECK (allow_group_overrides IN (0, 1)),
    slow_threshold_ms INTEGER CHECK (slow_threshold_ms BETWEEN 0 AND 3600000),
    trigger_policy_json TEXT CHECK (trigger_policy_json IS NULL OR json_valid(trigger_policy_json)),
    service_config_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(service_config_json)),
    config_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(config_json)),
    config_version INTEGER NOT NULL DEFAULT 1 CHECK (config_version > 0),
    table_prefix TEXT UNIQUE,
    schema_version INTEGER CHECK (schema_version > 0),
    code_version TEXT,
    installed INTEGER NOT NULL DEFAULT 0 CHECK (installed IN (0, 1)),
    portable_manifest_json TEXT,
    registry_updated_at_ms INTEGER,
    revision INTEGER NOT NULL DEFAULT 1 CHECK (revision > 0),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    CHECK (plugin_role <> 'service' OR (allow_user_overrides = 0 AND allow_group_overrides = 0)),
    CHECK (table_prefix IS NULL OR (length(table_prefix) BETWEEN 2 AND 62 AND table_prefix GLOB '[a-z]*_' AND table_prefix NOT GLOB '*[^a-z0-9_]*' AND instr(table_prefix, '__') = 0)),
    CHECK (portable_manifest_json IS NULL OR (json_valid(portable_manifest_json) AND json_type(portable_manifest_json) = 'object')),
    CHECK ((table_prefix IS NULL) = (portable_manifest_json IS NULL)),
    CHECK ((table_prefix IS NULL) = (code_version IS NULL)),
    CHECK (schema_version IS NULL OR table_prefix IS NOT NULL),
    CHECK (instr(plugin_id, char(0)) = 0 AND instr(plugin_role, char(0)) = 0 AND instr(service_config_json, char(0)) = 0 AND instr(config_json, char(0)) = 0),
    CHECK (trigger_policy_json IS NULL OR instr(trigger_policy_json, char(0)) = 0),
    CHECK (table_prefix IS NULL OR instr(table_prefix, char(0)) = 0),
    CHECK (code_version IS NULL OR instr(code_version, char(0)) = 0),
    CHECK (portable_manifest_json IS NULL OR instr(portable_manifest_json, char(0)) = 0)
) STRICT;

CREATE TABLE nuono_plugin_migrations (
    plugin_id TEXT NOT NULL REFERENCES nuono_plugins(plugin_id) ON DELETE RESTRICT ON UPDATE RESTRICT,
    version INTEGER NOT NULL CHECK (version > 0),
    description TEXT NOT NULL,
    checksum TEXT NOT NULL,
    applied_at_ms INTEGER NOT NULL,
    PRIMARY KEY (plugin_id, version),
    CHECK (instr(plugin_id, char(0)) = 0 AND instr(description, char(0)) = 0 AND instr(checksum, char(0)) = 0)
) STRICT;

CREATE TABLE nuono_plugin_overrides (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id > 0),
    plugin_id TEXT NOT NULL REFERENCES nuono_plugins(plugin_id) ON DELETE RESTRICT CHECK (length(CAST(plugin_id AS BLOB)) BETWEEN 1 AND 64),
    user_id INTEGER REFERENCES nuono_users(id) ON DELETE RESTRICT,
    group_id INTEGER REFERENCES nuono_groups(id) ON DELETE RESTRICT,
    enabled INTEGER CHECK (enabled IN (0, 1)),
    service_patch_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(service_patch_json)),
    config_patch_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(config_patch_json)),
    revision INTEGER NOT NULL DEFAULT 1 CHECK (revision > 0),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    CHECK ((user_id IS NULL) <> (group_id IS NULL)),
    UNIQUE (plugin_id, user_id),
    UNIQUE (plugin_id, group_id),
    CHECK (instr(plugin_id, char(0)) = 0 AND instr(service_patch_json, char(0)) = 0 AND instr(config_patch_json, char(0)) = 0)
) STRICT;

CREATE TABLE nuono_entitlements (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id > 0),
    subject_type TEXT NOT NULL CHECK (subject_type IN ('group', 'user')),
    subject_id INTEGER NOT NULL CHECK (subject_id > 0),
    feature_key TEXT NOT NULL CHECK (length(CAST(feature_key AS BLOB)) BETWEEN 1 AND 128),
    source TEXT NOT NULL CHECK (source IN ('billing', 'manual', 'trial')),
    source_ref TEXT CHECK (source_ref IS NULL OR length(CAST(source_ref AS BLOB)) BETWEEN 1 AND 256),
    status TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'revoked')),
    starts_at_ms INTEGER NOT NULL,
    expires_at_ms INTEGER,
    limits_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(limits_json) AND json_type(limits_json) = 'object'),
    updated_at_ms INTEGER NOT NULL,
    CHECK (expires_at_ms IS NULL OR expires_at_ms > starts_at_ms),
    CHECK (instr(subject_type, char(0)) = 0 AND instr(feature_key, char(0)) = 0 AND instr(source, char(0)) = 0 AND instr(status, char(0)) = 0 AND instr(limits_json, char(0)) = 0),
    CHECK (source_ref IS NULL OR instr(source_ref, char(0)) = 0)
) STRICT;

CREATE TABLE nuono_group_authorizations (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id > 0),
    user_id INTEGER NOT NULL REFERENCES nuono_users(id) ON DELETE RESTRICT,
    group_id INTEGER NOT NULL REFERENCES nuono_groups(id) ON DELETE RESTRICT,
    capability TEXT NOT NULL DEFAULT 'manage_plugin_config' CHECK (capability = 'manage_plugin_config'),
    scope_kind TEXT NOT NULL CHECK (scope_kind IN ('all_plugins', 'one_plugin')),
    plugin_id TEXT REFERENCES nuono_plugins(plugin_id) ON DELETE RESTRICT CHECK (plugin_id IS NULL OR length(CAST(plugin_id AS BLOB)) BETWEEN 1 AND 64),
    issuer_kind TEXT NOT NULL CHECK (issuer_kind IN ('manual', 'plugin')),
    issuer_user_id INTEGER REFERENCES nuono_users(id) ON DELETE RESTRICT,
    issuer_plugin_id TEXT REFERENCES nuono_plugins(plugin_id) ON DELETE RESTRICT CHECK (issuer_plugin_id IS NULL OR length(CAST(issuer_plugin_id AS BLOB)) BETWEEN 1 AND 64),
    source_key TEXT NOT NULL CHECK (length(source_key) BETWEEN 1 AND 256 AND length(CAST(source_key AS BLOB)) BETWEEN 1 AND 256),
    valid_from_ms INTEGER NOT NULL,
    expires_at_ms INTEGER,
    revoked_at_ms INTEGER,
    revision INTEGER NOT NULL DEFAULT 1 CHECK (revision > 0),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    CHECK ((scope_kind = 'all_plugins' AND plugin_id IS NULL) OR (scope_kind = 'one_plugin' AND plugin_id IS NOT NULL)),
    CHECK (expires_at_ms IS NULL OR expires_at_ms > valid_from_ms),
    CHECK ((issuer_kind = 'manual' AND issuer_user_id IS NOT NULL AND issuer_plugin_id IS NULL) OR (issuer_kind = 'plugin' AND issuer_plugin_id IS NOT NULL AND issuer_user_id IS NULL)),
    UNIQUE (issuer_user_id, source_key),
    UNIQUE (issuer_plugin_id, source_key),
    CHECK (instr(source_key, char(0)) = 0),
    CHECK (plugin_id IS NULL OR instr(plugin_id, char(0)) = 0),
    CHECK (issuer_plugin_id IS NULL OR instr(issuer_plugin_id, char(0)) = 0)
) STRICT;

CREATE TABLE nuono_sessions (
    id TEXT PRIMARY KEY CHECK (length(id) = 36 AND id = lower(id) AND substr(id, 9, 1) = '-' AND substr(id, 14, 1) = '-' AND substr(id, 19, 1) = '-' AND substr(id, 24, 1) = '-' AND replace(id, '-', '') NOT GLOB '*[^0-9a-f]*' AND length(replace(id, '-', '')) = 32),
    user_id INTEGER NOT NULL REFERENCES nuono_users(id),
    token_hash BLOB NOT NULL UNIQUE CHECK (length(token_hash) = 32),
    auth_epoch INTEGER NOT NULL CHECK (auth_epoch > 0),
    created_at_ms INTEGER NOT NULL,
    expires_at_ms INTEGER NOT NULL,
    last_seen_at_ms INTEGER,
    revoked_at_ms INTEGER,
    CHECK (expires_at_ms > created_at_ms)
) STRICT;

CREATE TABLE nuono_account_challenges (
    id TEXT PRIMARY KEY CHECK (length(id) = 36 AND id = lower(id) AND substr(id, 9, 1) = '-' AND substr(id, 14, 1) = '-' AND substr(id, 19, 1) = '-' AND substr(id, 24, 1) = '-' AND replace(id, '-', '') NOT GLOB '*[^0-9a-f]*' AND length(replace(id, '-', '')) = 32),
    purpose TEXT NOT NULL CHECK (purpose IN ('registration', 'email_verification', 'email_change', 'password_reset', 'platform_binding', 'merge_proof')),
    requester_user_id INTEGER REFERENCES nuono_users(id),
    subject_user_id INTEGER REFERENCES nuono_users(id),
    subject_auth_epoch INTEGER,
    session_id TEXT REFERENCES nuono_sessions(id),
    token_hash BLOB NOT NULL UNIQUE CHECK (length(token_hash) = 32),
    context_json TEXT NOT NULL CHECK (json_valid(context_json) AND json_type(context_json) = 'object'),
    candidate_identity_id INTEGER REFERENCES nuono_platform_identities(id),
    state TEXT NOT NULL CHECK (state IN ('pending', 'candidate', 'consumed', 'cancelled', 'expired')),
    attempts INTEGER NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    expires_at_ms INTEGER NOT NULL,
    consumed_at_ms INTEGER,
    revision INTEGER NOT NULL DEFAULT 1 CHECK (revision > 0),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    CHECK ((subject_user_id IS NULL) = (subject_auth_epoch IS NULL)),
    CHECK (subject_auth_epoch IS NULL OR subject_auth_epoch > 0),
    CHECK (purpose IN ('platform_binding', 'registration') OR (candidate_identity_id IS NULL AND state <> 'candidate')),
    CHECK ((state = 'consumed') = (consumed_at_ms IS NOT NULL)),
    CHECK (expires_at_ms > created_at_ms),
    CHECK (instr(context_json, char(0)) = 0),
    CHECK (session_id IS NULL OR instr(session_id, char(0)) = 0)
) STRICT;

CREATE TABLE nuono_account_merge_jobs (
    id TEXT PRIMARY KEY CHECK (length(id) = 36 AND id = lower(id) AND substr(id, 9, 1) = '-' AND substr(id, 14, 1) = '-' AND substr(id, 19, 1) = '-' AND substr(id, 24, 1) = '-' AND replace(id, '-', '') NOT GLOB '*[^0-9a-f]*' AND length(replace(id, '-', '')) = 32),
    source_user_id INTEGER NOT NULL REFERENCES nuono_users(id),
    target_user_id INTEGER NOT NULL REFERENCES nuono_users(id),
    source_proof_id TEXT REFERENCES nuono_account_challenges(id),
    target_proof_id TEXT REFERENCES nuono_account_challenges(id),
    status TEXT NOT NULL CHECK (status IN ('preflight', 'ready', 'running', 'completed', 'failed', 'cancelled')),
    plan_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(plan_json) AND json_type(plan_json) = 'object'),
    failure_code TEXT,
    completed_at_ms INTEGER,
    revision INTEGER NOT NULL DEFAULT 1 CHECK (revision > 0),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    CHECK (source_user_id <> target_user_id),
    CHECK ((status = 'completed') = (completed_at_ms IS NOT NULL)),
    CHECK (instr(plan_json, char(0)) = 0),
    CHECK (source_proof_id IS NULL OR instr(source_proof_id, char(0)) = 0),
    CHECK (target_proof_id IS NULL OR instr(target_proof_id, char(0)) = 0),
    CHECK (failure_code IS NULL OR instr(failure_code, char(0)) = 0)
) STRICT;

CREATE TABLE nuono_notifications (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id > 0),
    user_id INTEGER NOT NULL,
    kind TEXT NOT NULL,
    title TEXT NOT NULL,
    body TEXT NOT NULL,
    link TEXT,
    read_at INTEGER,
    created_at INTEGER NOT NULL,
    CHECK (instr(kind, char(0)) = 0 AND instr(title, char(0)) = 0 AND instr(body, char(0)) = 0),
    CHECK (link IS NULL OR instr(link, char(0)) = 0)
) STRICT;

CREATE TABLE nuono_global_configs (
    key TEXT PRIMARY KEY NOT NULL CHECK (length(CAST(key AS BLOB)) BETWEEN 1 AND 256),
    value_json TEXT NOT NULL CHECK (json_valid(value_json)),
    value_version INTEGER NOT NULL DEFAULT 1 CHECK (value_version > 0),
    revision INTEGER NOT NULL DEFAULT 1 CHECK (revision > 0),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    CHECK (instr(key, char(0)) = 0 AND instr(value_json, char(0)) = 0)
) STRICT;

CREATE TABLE nuono_runtime_secrets (
    key TEXT PRIMARY KEY NOT NULL CHECK (length(CAST(key AS BLOB)) BETWEEN 1 AND 256),
    owner_plugin_id TEXT REFERENCES nuono_plugins(plugin_id) ON DELETE RESTRICT CHECK (owner_plugin_id IS NULL OR length(CAST(owner_plugin_id AS BLOB)) BETWEEN 1 AND 64),
    value BLOB NOT NULL,
    revision INTEGER NOT NULL DEFAULT 1 CHECK (revision > 0),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    CHECK (instr(key, char(0)) = 0),
    CHECK (owner_plugin_id IS NULL OR instr(owner_plugin_id, char(0)) = 0)
) STRICT;

CREATE TABLE nuono_plugin_kv (
    plugin_id TEXT NOT NULL REFERENCES nuono_plugins(plugin_id) ON DELETE RESTRICT CHECK (length(CAST(plugin_id AS BLOB)) BETWEEN 1 AND 64),
    collection TEXT NOT NULL CHECK (length(CAST(collection AS BLOB)) BETWEEN 1 AND 128),
    scope_key TEXT NOT NULL CHECK (length(CAST(scope_key AS BLOB)) BETWEEN 1 AND 512),
    record_key TEXT NOT NULL CHECK (length(CAST(record_key AS BLOB)) BETWEEN 1 AND 200),
    generation TEXT NOT NULL CHECK (length(generation) = 36),
    expires_at_ms INTEGER,
    value_json TEXT NOT NULL CHECK (length(CAST(value_json AS BLOB)) <= 65536 AND json_valid(value_json)),
    value_version INTEGER NOT NULL DEFAULT 1 CHECK (value_version BETWEEN 1 AND 2147483647),
    revision INTEGER NOT NULL DEFAULT 1 CHECK (revision > 0),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    CHECK (expires_at_ms IS NULL OR expires_at_ms > created_at_ms),
    PRIMARY KEY (plugin_id, collection, scope_key, record_key),
    CHECK (instr(plugin_id, char(0)) = 0 AND instr(collection, char(0)) = 0 AND instr(scope_key, char(0)) = 0 AND instr(record_key, char(0)) = 0 AND instr(value_json, char(0)) = 0)
) STRICT;

CREATE TABLE nuono_bot_instances (
    id TEXT PRIMARY KEY NOT NULL CHECK (length(CAST(id AS BLOB)) BETWEEN 1 AND 128),
    name TEXT NOT NULL UNIQUE CHECK (length(trim(name)) BETWEEN 1 AND 128),
    platform TEXT NOT NULL CHECK (length(CAST(platform AS BLOB)) BETWEEN 1 AND 64),
    namespace TEXT NOT NULL DEFAULT '' CHECK (length(CAST(namespace AS BLOB)) <= 256),
    adapter_type TEXT NOT NULL CHECK (length(CAST(adapter_type AS BLOB)) BETWEEN 1 AND 64),
    native_bot_id TEXT CHECK (native_bot_id IS NULL OR length(CAST(native_bot_id AS BLOB)) BETWEEN 1 AND 256),
    enabled INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    config_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(config_json) AND json_type(config_json) = 'object'),
    runtime_status TEXT NOT NULL DEFAULT 'offline' CHECK (runtime_status IN ('offline', 'online', 'error')),
    last_error TEXT,
    last_connected_at_ms INTEGER,
    revision INTEGER NOT NULL DEFAULT 1 CHECK (revision > 0),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    CHECK (length(CAST(platform || namespace || COALESCE(native_bot_id, '') AS BLOB)) <= 1024),
    CHECK (instr(id, char(0)) = 0 AND instr(name, char(0)) = 0 AND instr(platform, char(0)) = 0 AND instr(namespace, char(0)) = 0 AND instr(adapter_type, char(0)) = 0 AND instr(config_json, char(0)) = 0 AND instr(runtime_status, char(0)) = 0),
    CHECK (native_bot_id IS NULL OR instr(native_bot_id, char(0)) = 0),
    CHECK (last_error IS NULL OR instr(last_error, char(0)) = 0)
) STRICT;

CREATE TABLE nuono_filter_rules (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id > 0),
    filter_type TEXT NOT NULL CHECK (length(filter_type) <= 20),
    pattern TEXT NOT NULL,
    match_mode TEXT NOT NULL DEFAULT 'simple' CHECK (length(match_mode) <= 10),
    target_id TEXT CHECK (target_id IS NULL OR length(target_id) <= 50),
    enabled INTEGER NOT NULL DEFAULT 1 CHECK (enabled IN (0, 1)),
    created_at INTEGER DEFAULT (CAST(unixepoch('subsec') * 1000 AS INTEGER)),
    CHECK (filter_type NOT IN ('blacklist_user', 'blacklist_group') OR (target_id IS NOT NULL AND target_id <> '')),
    CHECK (instr(filter_type, char(0)) = 0 AND instr(pattern, char(0)) = 0 AND instr(match_mode, char(0)) = 0),
    CHECK (target_id IS NULL OR instr(target_id, char(0)) = 0)
) STRICT;

CREATE TABLE nuono_conversations (
    id TEXT PRIMARY KEY NOT NULL CHECK (length(CAST(id AS BLOB)) BETWEEN 1 AND 128),
    platform TEXT NOT NULL CHECK (length(CAST(platform AS BLOB)) BETWEEN 1 AND 64),
    bot_instance_id TEXT CHECK (bot_instance_id IS NULL OR length(CAST(bot_instance_id AS BLOB)) BETWEEN 1 AND 256),
    scope_type TEXT NOT NULL CHECK (scope_type IN ('web', 'private', 'group', 'group_user', 'thread')),
    group_id INTEGER REFERENCES nuono_groups(id) ON DELETE SET NULL,
    owner_user_id INTEGER REFERENCES nuono_users(id) ON DELETE SET NULL,
    native_chat_id TEXT NOT NULL DEFAULT '' CHECK (length(CAST(native_chat_id AS BLOB)) <= 1024),
    native_thread_id TEXT NOT NULL DEFAULT '' CHECK (length(CAST(native_thread_id AS BLOB)) <= 256),
    title TEXT CHECK (title IS NULL OR length(CAST(title AS BLOB)) <= 256),
    status TEXT NOT NULL DEFAULT 'active' CHECK (status IN ('active', 'archived')),
    summary_text TEXT,
    last_message_at_ms INTEGER,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    CHECK (instr(id, char(0)) = 0 AND instr(platform, char(0)) = 0 AND instr(scope_type, char(0)) = 0 AND instr(native_chat_id, char(0)) = 0 AND instr(native_thread_id, char(0)) = 0 AND instr(status, char(0)) = 0),
    CHECK (bot_instance_id IS NULL OR instr(bot_instance_id, char(0)) = 0),
    CHECK (title IS NULL OR instr(title, char(0)) = 0),
    CHECK (summary_text IS NULL OR instr(summary_text, char(0)) = 0)
) STRICT;

CREATE TABLE nuono_messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id > 0),
    conversation_id TEXT REFERENCES nuono_conversations(id) ON DELETE SET NULL CHECK (conversation_id IS NULL OR length(CAST(conversation_id AS BLOB)) BETWEEN 1 AND 128),
    platform TEXT NOT NULL CHECK (length(CAST(platform AS BLOB)) BETWEEN 1 AND 64),
    bot_instance_id TEXT CHECK (bot_instance_id IS NULL OR length(CAST(bot_instance_id AS BLOB)) BETWEEN 1 AND 256),
    native_message_id TEXT CHECK (native_message_id IS NULL OR length(CAST(native_message_id AS BLOB)) BETWEEN 1 AND 512),
    reply_to_native_message_id TEXT CHECK (reply_to_native_message_id IS NULL OR length(CAST(reply_to_native_message_id AS BLOB)) BETWEEN 1 AND 512),
    sender_user_id INTEGER REFERENCES nuono_users(id) ON DELETE SET NULL,
    sender_identity_id INTEGER REFERENCES nuono_platform_identities(id) ON DELETE SET NULL,
    native_sender_id TEXT CHECK (native_sender_id IS NULL OR length(CAST(native_sender_id AS BLOB)) BETWEEN 1 AND 512),
    native_group_id TEXT CHECK (native_group_id IS NULL OR length(CAST(native_group_id AS BLOB)) BETWEEN 1 AND 512),
    sender_nickname TEXT CHECK (sender_nickname IS NULL OR length(sender_nickname) <= 100),
    direction TEXT NOT NULL CHECK (direction IN ('inbound', 'outbound')),
    role TEXT NOT NULL CHECK (role IN ('user', 'assistant', 'system')),
    text_content TEXT NOT NULL DEFAULT '',
    content_blocks_json TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(content_blocks_json) AND json_type(content_blocks_json) = 'array'),
    raw_payload_json TEXT,
    occurred_at_ms INTEGER NOT NULL,
    created_at_ms INTEGER NOT NULL,
    CHECK (instr(platform, char(0)) = 0 AND instr(direction, char(0)) = 0 AND instr(role, char(0)) = 0 AND instr(text_content, char(0)) = 0 AND instr(content_blocks_json, char(0)) = 0),
    CHECK (conversation_id IS NULL OR instr(conversation_id, char(0)) = 0),
    CHECK (bot_instance_id IS NULL OR instr(bot_instance_id, char(0)) = 0),
    CHECK (native_message_id IS NULL OR instr(native_message_id, char(0)) = 0),
    CHECK (reply_to_native_message_id IS NULL OR instr(reply_to_native_message_id, char(0)) = 0),
    CHECK (native_sender_id IS NULL OR instr(native_sender_id, char(0)) = 0),
    CHECK (native_group_id IS NULL OR instr(native_group_id, char(0)) = 0),
    CHECK (sender_nickname IS NULL OR instr(sender_nickname, char(0)) = 0),
    CHECK (raw_payload_json IS NULL OR instr(raw_payload_json, char(0)) = 0)
) STRICT;

CREATE TABLE nuono_ai_runs (
    id TEXT PRIMARY KEY NOT NULL CHECK (length(CAST(id AS BLOB)) BETWEEN 1 AND 128),
    conversation_id TEXT NOT NULL REFERENCES nuono_conversations(id) ON DELETE CASCADE CHECK (length(CAST(conversation_id AS BLOB)) BETWEEN 1 AND 128),
    trigger_message_id INTEGER REFERENCES nuono_messages(id) ON DELETE SET NULL,
    reply_message_id INTEGER REFERENCES nuono_messages(id) ON DELETE SET NULL,
    user_id INTEGER REFERENCES nuono_users(id) ON DELETE SET NULL,
    provider TEXT NOT NULL DEFAULT '' CHECK (length(CAST(provider AS BLOB)) <= 128),
    model TEXT NOT NULL DEFAULT '' CHECK (length(CAST(model AS BLOB)) <= 256),
    status TEXT NOT NULL CHECK (status IN ('running', 'succeeded', 'failed', 'cancelled')),
    prompt_tokens INTEGER NOT NULL DEFAULT 0 CHECK (prompt_tokens BETWEEN 0 AND 2147483647),
    completion_tokens INTEGER NOT NULL DEFAULT 0 CHECK (completion_tokens BETWEEN 0 AND 2147483647),
    latency_ms INTEGER NOT NULL DEFAULT 0 CHECK (latency_ms BETWEEN 0 AND 2147483647),
    steps_json TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(steps_json) AND json_type(steps_json) = 'array'),
    error_message TEXT,
    created_at_ms INTEGER NOT NULL,
    finished_at_ms INTEGER,
    CHECK (instr(id, char(0)) = 0 AND instr(conversation_id, char(0)) = 0 AND instr(provider, char(0)) = 0 AND instr(model, char(0)) = 0 AND instr(status, char(0)) = 0 AND instr(steps_json, char(0)) = 0),
    CHECK (error_message IS NULL OR instr(error_message, char(0)) = 0)
) STRICT;

CREATE TABLE nuono_audit_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id > 0),
    category TEXT NOT NULL DEFAULT 'audit' CHECK (category IN ('audit', 'event', 'command')),
    occurred_at_ms INTEGER NOT NULL,
    actor_user_id INTEGER REFERENCES nuono_users(id) ON DELETE RESTRICT,
    actor_plugin_id TEXT REFERENCES nuono_plugins(plugin_id) ON DELETE RESTRICT CHECK (actor_plugin_id IS NULL OR length(CAST(actor_plugin_id AS BLOB)) BETWEEN 1 AND 64),
    actor_label TEXT,
    bot_id TEXT,
    action TEXT NOT NULL CHECK (length(action) <= 100),
    result TEXT NOT NULL DEFAULT 'ok',
    subject_user_id INTEGER REFERENCES nuono_users(id) ON DELETE RESTRICT,
    group_id INTEGER REFERENCES nuono_groups(id) ON DELETE RESTRICT,
    operation_id TEXT,
    details_json TEXT NOT NULL DEFAULT '{}' CHECK (json_valid(details_json)),
    CHECK (instr(action, char(0)) = 0 AND instr(result, char(0)) = 0 AND instr(details_json, char(0)) = 0),
    CHECK (actor_plugin_id IS NULL OR instr(actor_plugin_id, char(0)) = 0),
    CHECK (actor_label IS NULL OR instr(actor_label, char(0)) = 0),
    CHECK (bot_id IS NULL OR instr(bot_id, char(0)) = 0),
    CHECK (operation_id IS NULL OR instr(operation_id, char(0)) = 0)
) STRICT;

CREATE TABLE nuono_activity_daily (
    bucket_key TEXT PRIMARY KEY NOT NULL CHECK (length(bucket_key) <= 64),
    day TEXT NOT NULL CHECK (length(day) <= 10),
    platform TEXT NOT NULL CHECK (length(platform) <= 64),
    bot_id TEXT NOT NULL,
    user_key TEXT NOT NULL CHECK (length(user_key) <= 64),
    user_id TEXT NOT NULL,
    namespace TEXT NOT NULL,
    scope_key TEXT NOT NULL CHECK (length(scope_key) <= 64),
    scope_id TEXT NOT NULL,
    scene TEXT NOT NULL CHECK (length(scene) <= 16),
    plugin TEXT NOT NULL CHECK (length(plugin) <= 128),
    kind TEXT NOT NULL CHECK (length(kind) <= 16),
    count INTEGER NOT NULL CHECK (count > 0),
    CHECK (instr(bucket_key, char(0)) = 0 AND instr(day, char(0)) = 0 AND instr(platform, char(0)) = 0 AND instr(bot_id, char(0)) = 0 AND instr(user_key, char(0)) = 0 AND instr(user_id, char(0)) = 0 AND instr(namespace, char(0)) = 0 AND instr(scope_key, char(0)) = 0 AND instr(scope_id, char(0)) = 0 AND instr(scene, char(0)) = 0 AND instr(plugin, char(0)) = 0 AND instr(kind, char(0)) = 0)
) STRICT;

CREATE TABLE nuono_scheduled_tasks (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id > 0),
    name TEXT NOT NULL CHECK (length(name) <= 100),
    cron_expr TEXT NOT NULL CHECK (length(cron_expr) <= 100),
    action_type TEXT NOT NULL CHECK (length(action_type) <= 50),
    action_params TEXT,
    enabled INTEGER DEFAULT 1 CHECK (enabled IN (0, 1)),
    last_run_at INTEGER,
    created_at INTEGER DEFAULT (CAST(unixepoch('subsec') * 1000 AS INTEGER)),
    bot_id TEXT,
    timezone TEXT NOT NULL DEFAULT 'UTC',
    revision INTEGER NOT NULL DEFAULT 0,
    CHECK (instr(name, char(0)) = 0 AND instr(cron_expr, char(0)) = 0 AND instr(action_type, char(0)) = 0 AND instr(timezone, char(0)) = 0),
    CHECK (action_params IS NULL OR instr(action_params, char(0)) = 0),
    CHECK (bot_id IS NULL OR instr(bot_id, char(0)) = 0)
) STRICT;

CREATE TABLE nuono_scheduled_task_runs (
    id TEXT PRIMARY KEY NOT NULL,
    task_id INTEGER NOT NULL,
    task_name TEXT NOT NULL,
    definition_digest TEXT NOT NULL,
    trigger_kind TEXT NOT NULL,
    status TEXT NOT NULL,
    detail TEXT,
    created_at INTEGER NOT NULL,
    finished_at INTEGER,
    CHECK (instr(id, char(0)) = 0 AND instr(task_name, char(0)) = 0 AND instr(definition_digest, char(0)) = 0 AND instr(trigger_kind, char(0)) = 0 AND instr(status, char(0)) = 0),
    CHECK (detail IS NULL OR instr(detail, char(0)) = 0)
) STRICT;

CREATE TABLE nuono_feed_subscriptions (
    id INTEGER PRIMARY KEY AUTOINCREMENT CHECK (id > 0),
    subscriber_id TEXT NOT NULL CHECK (length(subscriber_id) <= 100),
    topic TEXT NOT NULL CHECK (length(topic) <= 50),
    target_id TEXT CHECK (target_id IS NULL OR length(target_id) <= 50),
    enabled INTEGER DEFAULT 1 CHECK (enabled IN (0, 1)),
    created_at INTEGER DEFAULT (CAST(unixepoch('subsec') * 1000 AS INTEGER)),
    CHECK (instr(subscriber_id, char(0)) = 0 AND instr(topic, char(0)) = 0),
    CHECK (target_id IS NULL OR instr(target_id, char(0)) = 0)
) STRICT;

CREATE TABLE nuono_group_bots (
    group_id INTEGER NOT NULL REFERENCES nuono_groups(id) ON DELETE CASCADE,
    platform TEXT NOT NULL CHECK (length(platform) > 0),
    bot_id TEXT NOT NULL CHECK (length(bot_id) > 0),
    synced_at_ms INTEGER NOT NULL,
    PRIMARY KEY (group_id, platform, bot_id),
    CHECK (instr(platform, char(0)) = 0 AND instr(bot_id, char(0)) = 0)
) STRICT;

CREATE INDEX "idx-nuono_notifications-user" ON nuono_notifications (user_id);
CREATE UNIQUE INDEX idx_filter_rules_blacklist ON nuono_filter_rules (filter_type, target_id) WHERE filter_type IN ('blacklist_user', 'blacklist_group');
CREATE INDEX idx_nuono_conversations_owner ON nuono_conversations (owner_user_id, platform, status, last_message_at_ms);
CREATE INDEX idx_nuono_conversations_routing ON nuono_conversations (bot_instance_id, scope_type, native_chat_id, native_thread_id, status);
CREATE INDEX idx_nuono_conversations_chat_key ON nuono_conversations (platform, scope_type, native_chat_id, native_thread_id, status);
CREATE UNIQUE INDEX idx_nuono_messages_dedupe ON nuono_messages (bot_instance_id, native_message_id) WHERE bot_instance_id IS NOT NULL AND native_message_id IS NOT NULL;
CREATE INDEX idx_nuono_messages_conversation ON nuono_messages (conversation_id, id);
CREATE INDEX idx_nuono_messages_occurred ON nuono_messages (occurred_at_ms);
CREATE INDEX idx_nuono_messages_bot_occurred ON nuono_messages (bot_instance_id, occurred_at_ms);
CREATE INDEX idx_nuono_ai_runs_conversation ON nuono_ai_runs (conversation_id, created_at_ms);
CREATE INDEX idx_nuono_ai_runs_user ON nuono_ai_runs (user_id, created_at_ms);
CREATE INDEX idx_activity_day ON nuono_activity_daily (day);
CREATE INDEX idx_activity_platform_day ON nuono_activity_daily (platform, day);
CREATE INDEX idx_activity_user_day ON nuono_activity_daily (user_id, day);
CREATE INDEX idx_activity_scope_day ON nuono_activity_daily (scope_id, day);
CREATE INDEX idx_activity_plugin_day ON nuono_activity_daily (plugin, day);
CREATE INDEX idx_scheduled_task_runs_task_time ON nuono_scheduled_task_runs (task_id, created_at);
CREATE INDEX idx_scheduled_task_runs_time ON nuono_scheduled_task_runs (created_at);
CREATE UNIQUE INDEX idx_nuono_entitlements_subject_feature_source ON nuono_entitlements (subject_type, subject_id, feature_key, source);
CREATE INDEX idx_nuono_entitlements_active ON nuono_entitlements (subject_type, subject_id, feature_key, status, expires_at_ms);
CREATE INDEX nuono_group_authorizations_recipient ON nuono_group_authorizations (user_id, group_id, capability);
CREATE INDEX nuono_group_authorizations_group ON nuono_group_authorizations (group_id);
CREATE INDEX idx_nuono_platform_identities_user ON nuono_platform_identities (user_id);
CREATE INDEX idx_nuono_audit_events_operation ON nuono_audit_events (operation_id);
CREATE INDEX idx_nuono_audit_events_user ON nuono_audit_events (subject_user_id, occurred_at_ms);
CREATE INDEX idx_nuono_audit_events_category_action_id ON nuono_audit_events (category, action, id);
CREATE INDEX idx_nuono_audit_events_category_time ON nuono_audit_events (category, occurred_at_ms);
CREATE INDEX idx_nuono_sessions_user ON nuono_sessions (user_id);
CREATE INDEX idx_nuono_sessions_expiry ON nuono_sessions (expires_at_ms);
CREATE INDEX idx_nuono_challenges_subject ON nuono_account_challenges (subject_user_id, purpose, state);
CREATE INDEX idx_nuono_challenges_expiry ON nuono_account_challenges (expires_at_ms);
CREATE INDEX idx_nuono_merge_jobs_target ON nuono_account_merge_jobs (target_user_id, created_at_ms);
CREATE INDEX nuono_plugin_overrides_user ON nuono_plugin_overrides (user_id);
CREATE INDEX nuono_plugin_overrides_group ON nuono_plugin_overrides (group_id);
CREATE INDEX idx_nuono_secrets_owner ON nuono_runtime_secrets (owner_plugin_id);
CREATE INDEX idx_nuono_plugin_kv_expiry ON nuono_plugin_kv (expires_at_ms);
CREATE UNIQUE INDEX idx_nuono_bot_instances_native ON nuono_bot_instances (platform, namespace, native_bot_id) WHERE native_bot_id IS NOT NULL;
CREATE INDEX idx_nuono_group_bots_bot ON nuono_group_bots (platform, bot_id);
