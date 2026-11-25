//! `nuono-tooling` 数据结构的 OpenAPI Schema 投影类型（仅用于生成 API 文档，绝不直接用于持久化）。

use nuono_tooling::{
    recovery::StoreJob,
    source_build::{BuildArtifact, SourceKind, SourcePlugin},
};
#[derive(serde::Serialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
#[schema(as = SourceKind)]
#[allow(dead_code)]
pub(crate) enum SourceKindSchema {
    Plugin,
    Adapter,
}
const _: fn(SourceKind) = |kind| match kind {
    SourceKind::Plugin => (),
    SourceKind::Adapter => (),
};

#[derive(serde::Serialize, utoipa::ToSchema)]
#[schema(as = SourcePlugin)]
#[allow(dead_code)]
pub(crate) struct SourcePluginSchema {
    #[serde(default)]
    #[schema(value_type = SourceKindSchema)]
    pub kind: SourceKind,
    pub id: String,
    pub package: String,
    pub version: String,
    pub commit: Option<String>,
    pub source_hash: String,
    pub runtime_name: Option<String>,
}
const _: fn(&SourcePlugin) = |source| {
    let SourcePlugin {
        kind,
        id,
        package,
        version,
        commit,
        source_hash,
        runtime_name,
    } = source;
    let _: &SourceKind = kind;
    let _: &String = id;
    let _: &String = package;
    let _: &String = version;
    let _: &Option<String> = commit;
    let _: &String = source_hash;
    let _: &Option<String> = runtime_name;
};

#[derive(serde::Serialize, utoipa::ToSchema)]
#[schema(as = BuildArtifact)]
#[allow(dead_code)]
pub(crate) struct BuildArtifactSchema {
    pub id: String,
    pub profile: String,
    pub sha256: String,
    #[schema(value_type = Vec<SourcePluginSchema>)]
    pub sources: Vec<SourcePlugin>,
}
const _: fn(&BuildArtifact) = |source| {
    let BuildArtifact {
        id,
        profile,
        sha256,
        sources,
    } = source;
    let _: &String = id;
    let _: &String = profile;
    let _: &String = sha256;
    let _: &Vec<SourcePlugin> = sources;
};

#[derive(serde::Serialize, utoipa::ToSchema)]
#[schema(as = StoreJob)]
#[allow(dead_code)]
pub(crate) struct StoreJobSchema {
    #[serde(default)]
    #[schema(value_type = SourceKindSchema)]
    pub kind: SourceKind,
    pub id: String,
    pub plugin_id: String,
    pub operation: String,
    pub phase: String,
    pub message: String,
    pub error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    pub repo_url: Option<String>,
    pub previous_commit: Option<String>,
    /// 显式指定的目标 Git commit（旧版任务记录中无此字段）。
    #[serde(default)]
    pub target_commit: Option<String>,
    #[serde(default)]
    pub expected_source_commit: Option<String>,
    /// 标记源码是否已完成 checkout 切换（兼容无此字段的旧版任务记录，新版任务靠它精确判断回滚边界）。
    #[serde(default)]
    pub source_switched: Option<bool>,
    pub staged_name: Option<String>,
    pub runtime_name: Option<String>,
    pub previously_enabled: bool,
    #[schema(value_type = Option<SourcePluginSchema>)]
    pub expected: Option<SourcePlugin>,
    /// 本次任务编译产出的可执行文件完整插件清单与二进制 SHA-256 摘要。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(value_type = Option<BuildArtifactSchema>)]
    pub artifact: Option<BuildArtifact>,
    #[serde(default)]
    pub owner_pid: u32,
    #[serde(default)]
    pub log: String,
}
const _: fn(&StoreJob) = |source| {
    let StoreJob {
        kind,
        id,
        plugin_id,
        operation,
        phase,
        message,
        error,
        created_at,
        updated_at,
        repo_url,
        previous_commit,
        target_commit,
        expected_source_commit,
        source_switched,
        staged_name,
        runtime_name,
        previously_enabled,
        expected,
        artifact,
        owner_pid,
        log,
    } = source;
    let _: &SourceKind = kind;
    let _: &String = id;
    let _: &String = plugin_id;
    let _: &String = operation;
    let _: &String = phase;
    let _: &String = message;
    let _: &Option<String> = error;
    let _: &String = created_at;
    let _: &String = updated_at;
    let _: &Option<String> = repo_url;
    let _: &Option<String> = previous_commit;
    let _: &Option<String> = target_commit;
    let _: &Option<String> = expected_source_commit;
    let _: &Option<bool> = source_switched;
    let _: &Option<String> = staged_name;
    let _: &Option<String> = runtime_name;
    let _: &bool = previously_enabled;
    let _: &Option<SourcePlugin> = expected;
    let _: &Option<BuildArtifact> = artifact;
    let _: &u32 = owner_pid;
    let _: &String = log;
};
