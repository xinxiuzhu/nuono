//! 面向管理员的运行摘要。后台采样与通知共用快照，不由页面请求触发重复通知。
mod backup_status;
mod disk;
pub use backup_status::BackupHealth;
#[cfg(test)]
#[path = "../tests/unit/operations/tests.rs"]
mod tests;

use crate::AppState;
use nuono_core::db::Database;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::Arc,
    time::Duration,
};
use tokio::sync::{Mutex, RwLock};

pub use disk::{CleanResult, DiskSnapshot};

const ROBOTS_LINK: &str = "/bots";
const ONEBOT_CONNECTIONS_LINK: &str = "/bots?tab=connections";

#[derive(Debug, Clone, Serialize, Deserialize, utoipa::ToSchema, PartialEq, Eq)]
pub struct HealthIssue {
    pub id: String,
    pub area: String,
    pub severity: String,
    pub title: String,
    pub detail: String,
    pub link: String,
    #[serde(default)]
    pub first_observed_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, utoipa::ToSchema)]
pub struct OperationsSnapshot {
    pub status: String,
    pub checked_at: Option<String>,
    pub db_connected: bool,
    pub active_connections: usize,
    pub loaded_plugins: usize,
    pub issues: Vec<HealthIssue>,
    pub backup: BackupHealth,
}

impl Default for OperationsSnapshot {
    fn default() -> Self {
        Self {
            status: "unknown".into(),
            checked_at: None,
            db_connected: false,
            active_connections: 0,
            loaded_plugins: 0,
            issues: vec![],
            backup: BackupHealth::default(),
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
struct NoticeState {
    active: bool,
    severity: String,
    last_sent: u64,
    last_recovered: u64,
    title: String,
    link: String,
    // 旧快照没有此字段，按 active 和发送/恢复时间推断未结束的告警。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    connection_alert_open: Option<bool>,
}

#[derive(Default)]
struct ConnectionChecks {
    checked_kinds: BTreeSet<&'static str>,
    healthy: BTreeMap<String, String>,
    deferred: BTreeSet<String>,
}

fn connection_kind(id: &str) -> Option<&str> {
    match id.split_once(':') {
        Some((kind @ ("onebot" | "bot"), _)) => Some(kind),
        _ => None,
    }
}

fn defer_initial_connections(
    checks: &mut ConnectionChecks,
    issues: &[HealthIssue],
    confirmed_online: &mut BTreeSet<String>,
) {
    // 移除或禁用后重新接入需要重新建立在线依据；读取失败不抹掉已有依据。
    confirmed_online.retain(|id| {
        connection_kind(id).is_some_and(|kind| {
            !checks.checked_kinds.contains(kind)
                || checks.healthy.contains_key(id)
                || issues.iter().any(|issue| &issue.id == id)
        })
    });
    confirmed_online.extend(
        checks
            .healthy
            .keys()
            .filter(|id| connection_kind(id).is_some())
            .cloned(),
    );
    checks.deferred.clear();
    checks.deferred.extend(
        issues
            .iter()
            .filter(|issue| {
                connection_kind(&issue.id).is_some() && !confirmed_online.contains(&issue.id)
            })
            .map(|issue| issue.id.clone()),
    );
}

struct CollectedHealth {
    snapshot: OperationsSnapshot,
    connections: ConnectionChecks,
}

pub struct OperationsMonitor {
    snapshot: RwLock<OperationsSnapshot>,
    notices: Mutex<BTreeMap<String, NoticeState>>,
    pub(crate) disk: Arc<Mutex<Option<DiskSnapshot>>>,
}
impl OperationsMonitor {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            snapshot: RwLock::new(OperationsSnapshot::default()),
            notices: Mutex::new(BTreeMap::new()),
            disk: Arc::new(Mutex::new(None)),
        })
    }
    pub async fn snapshot(&self) -> OperationsSnapshot {
        self.snapshot.read().await.clone()
    }
    pub async fn disk(&self, state: &AppState) -> Result<DiskSnapshot, String> {
        disk::snapshot(self, state).await
    }
    pub async fn clean(
        &self,
        state: &AppState,
        confirmation: &str,
    ) -> nuono_core::error::Result<CleanResult> {
        if confirmation != "清理构建缓存" {
            return Err(nuono_core::NuonoError::Validation(
                "请输入“清理构建缓存”确认操作".into(),
            ));
        }
        let operation = crate::source_build::operation_lock()
            .try_lock_owned()
            .map_err(|_| {
                nuono_core::NuonoError::Conflict("正在安装、更新或构建插件，请完成后再清理".into())
            })?;
        let root = state.paths.root.clone();
        let result = tokio::task::spawn_blocking(move || {
            let _operation = operation;
            disk::clean(&root)
        })
        .await
        .map_err(|_| {
            nuono_core::NuonoError::Internal("清理任务未完成，请刷新磁盘占用后重试".into())
        })?
        .map_err(nuono_core::NuonoError::Internal)?;
        *self.disk.lock().await = None;
        Ok(result)
    }
}

pub fn start(state: AppState) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut loaded_notices = false;
        while !state.readiness.load(std::sync::atomic::Ordering::Acquire) {
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        let mut confirmed_online = BTreeSet::new();
        let mut interval = tokio::time::interval(Duration::from_secs(30));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            if !state.readiness.load(std::sync::atomic::Ordering::Acquire) {
                continue;
            }
            let previous = state.operations.snapshot().await;
            let result = tokio::time::timeout(Duration::from_secs(20), collect(&state)).await;
            let (mut snapshot, mut connections) = match result {
                Ok(health) => (health.snapshot, health.connections),
                Err(_) => {
                    let mut previous = previous.clone();
                    previous.status = "degraded".into();
                    previous.issues.retain(|item| item.id != "health-timeout");
                    previous.issues.push(issue(
                        "health-timeout",
                        "system",
                        "warning",
                        "运行检查超时",
                        "部分状态暂时无法确认，请检查运行日志。",
                        "/logs",
                    ));
                    (previous, ConnectionChecks::default())
                }
            };
            defer_initial_connections(&mut connections, &snapshot.issues, &mut confirmed_online);
            prepare_issues(
                &mut snapshot.issues,
                &previous.issues,
                &chrono::Utc::now().to_rfc3339(),
            );
            *state.operations.snapshot.write().await = snapshot.clone();
            // 数据库离线时仍发布内存故障快照；恢复后再补站内通知。
            if snapshot.db_connected {
                if !loaded_notices {
                    match load_notice_state(&state.db).await {
                        Ok(mut saved) => {
                            let mut notices = state.operations.notices.lock().await;
                            // 首次读库前已经观测到的数据库故障仍需保留。
                            if let Some(database) = notices.get("database") {
                                saved.insert("database".into(), database.clone());
                            }
                            *notices = saved;
                            loaded_notices = true;
                        }
                        Err(error) => {
                            tracing::warn!(%error, "读取运行通知状态失败，稍后重试");
                            continue;
                        }
                    }
                }
                publish_changes(&state, &snapshot.issues, &connections).await;
            } else if snapshot.issues.iter().any(|issue| issue.id == "database") {
                let mut notices = state.operations.notices.lock().await;
                let entry = notices.entry("database".into()).or_default();
                entry.active = true;
                entry.title = "数据库连接失败".into();
                entry.link = "/system/database".into();
            }
        }
    })
}

async fn load_notice_state(db: &Database) -> anyhow::Result<BTreeMap<String, NoticeState>> {
    match db.load_operations_notice_state().await? {
        Some(saved) => Ok(serde_json::from_str(&saved)?),
        None => Ok(BTreeMap::new()),
    }
}

fn issue(
    id: &str,
    area: &str,
    severity: &str,
    title: &str,
    detail: &str,
    link: &str,
) -> HealthIssue {
    HealthIssue {
        id: id.into(),
        area: area.into(),
        severity: severity.into(),
        title: title.into(),
        detail: detail.into(),
        link: issue_link(id).unwrap_or_else(|| link.into()),
        first_observed_at: None,
    }
}

fn issue_link(id: &str) -> Option<String> {
    let (kind, value) = id.split_once(':')?;
    let (path, key, extra) = match kind {
        "onebot" => ("/bots", "connection", None),
        "bot" => ("/bots", "instance", None),
        "plugin-config" => ("/plugins", "plugin", Some(("plugin-tab", "config"))),
        "plugin-runtime" => ("/plugins", "plugin", Some(("plugin-tab", "service"))),
        _ => return None,
    };
    let mut query = url::form_urlencoded::Serializer::new(String::new());
    query.append_pair(key, value);
    if let Some((key, value)) = extra {
        query.append_pair(key, value);
    }
    Some(format!("{path}?{}", query.finish()))
}

fn prepare_issues(issues: &mut [HealthIssue], previous: &[HealthIssue], now: &str) {
    for item in issues.iter_mut() {
        item.first_observed_at = previous
            .iter()
            .find(|old| old.id == item.id)
            .and_then(|old| old.first_observed_at.clone())
            .or_else(|| Some(now.into()));
    }
    issues.sort_by(|a, b| {
        (a.severity != "error")
            .cmp(&(b.severity != "error"))
            .then_with(|| a.first_observed_at.cmp(&b.first_observed_at))
            .then_with(|| a.id.cmp(&b.id))
    });
}

async fn collect(state: &AppState) -> CollectedHealth {
    let db_connected = tokio::time::timeout(Duration::from_secs(3), state.db.ping())
        .await
        .is_ok_and(|value| value.is_ok());
    let mut issues = Vec::new();
    let mut connections = ConnectionChecks::default();
    if !db_connected {
        issues.push(issue(
            "database",
            "database",
            "error",
            "数据库连接失败",
            "设置、消息和任务可能无法保存，请先检查数据库服务。",
            "/system/database",
        ));
    }
    let mut active_connections = state
        .manager
        .bot_txs
        .iter()
        .filter(|sender| !sender.is_closed())
        .count();
    if db_connected {
        if let Ok(configured) = nuono_core::service::bots::get_bot_list_with_runtime(
            &state.db,
            &state.manager.bot_states,
        )
        .await
        {
            connections.checked_kinds.insert("onebot");
            for bot in configured.iter().filter(|bot| bot.status == "online") {
                connections.healthy.insert(
                    format!("onebot:{}", bot.bot_id),
                    format!("OneBot {} 已恢复在线", bot.bot_id),
                );
            }
            for bot in configured.iter().filter(|bot| bot.status != "online") {
                let title = match bot.status.as_str() {
                    "offline" => "已离线",
                    "degraded" => "状态异常",
                    _ => "在线情况待确认",
                };
                issues.push(issue(
                    &format!("onebot:{}", bot.bot_id),
                    "bots",
                    "warning",
                    &format!("OneBot {} {title}", bot.bot_id),
                    bot.status_reason
                        .as_deref()
                        .unwrap_or("请检查客户端登录状态、心跳与反向 WebSocket 配置。"),
                    ONEBOT_CONNECTIONS_LINK,
                ));
            }
        } else {
            issues.push(issue(
                "onebot-check",
                "bots",
                "warning",
                "OneBot 状态读取失败",
                "连接记录暂时无法确认，请检查数据库和日志。",
                ROBOTS_LINK,
            ));
        }
    }
    if db_connected {
        match state.bot_instances.list().await {
            Ok(bots) => {
                connections.checked_kinds.insert("bot");
                for bot in bots.into_iter().filter(|bot| bot.enabled) {
                    if bot.connected {
                        active_connections += 1;
                        connections.healthy.insert(
                            format!("bot:{}", bot.id),
                            format!("{} 已恢复连接", bot.name),
                        );
                    } else {
                        issues.push(issue(
                            &format!("bot:{}", bot.id),
                            "bots",
                            "warning",
                            &format!("{} 尚未接通", bot.name),
                            if bot.connection_mode == "webhook" {
                                "请确认回调地址、校验结果和最近事件。"
                            } else {
                                "请查看实例的最近错误，核对凭据和连接。"
                            },
                            ROBOTS_LINK,
                        ));
                    }
                }
            }
            Err(_) => issues.push(issue(
                "bot-check",
                "bots",
                "warning",
                "机器人状态读取失败",
                "请检查数据库与运行日志。",
                ROBOTS_LINK,
            )),
        }
    }
    let loaded = state.manager.plugin_host.all_plugins().await;
    let registry = state.manager.plugin_host.list_plugin_configs().await;
    let loaded_plugins = registry
        .iter()
        .filter(|plugin| plugin.enabled && loaded.contains(&plugin.name))
        .count();
    connections.checked_kinds.insert("plugin-config");
    for name in &loaded {
        let status = state.plugin_config.status(name).await;
        if status.error.is_some() || status.runtime_error.is_some() || status.migration_conflict {
            issues.push(issue(
                &format!("plugin-config:{name}"),
                "plugins",
                "warning",
                &format!("{name} 配置需要处理"),
                "文件配置或生效回调存在错误，当前可能仍在使用上一次有效配置。",
                "/plugins",
            ));
        } else {
            connections.healthy.insert(
                format!("plugin-config:{name}"),
                format!("{name} 配置已恢复正常"),
            );
        }
    }
    if db_connected {
        if let Ok(installed) = nuono_core::service::plugins::get_installed_plugins(
            &state.db,
            &state.manager.plugin_host,
        )
        .await
        {
            let enabled_registry = registry
                .iter()
                .filter(|plugin| plugin.enabled)
                .map(|plugin| plugin.name.clone())
                .collect();
            issues.extend(plugin_runtime_issues(
                &installed,
                &loaded,
                &enabled_registry,
                &mut connections,
            ));
        } else {
            issues.push(issue(
                "plugin-check",
                "plugins",
                "warning",
                "插件状态读取失败",
                "无法核对已启用插件与运行实例，请检查数据库和日志。",
                "/plugins",
            ));
        }
    }
    let config = state.config.get().await;
    let auto_enabled = config.backup.enabled;
    let sqlite = state.db.backend().as_str() == "Sqlite";
    let data = state.paths.data.clone();
    let backup = tokio::task::spawn_blocking(move || {
        backup_status::inspect(&data, auto_enabled, sqlite, chrono::Utc::now())
    })
    .await
    .ok()
    .and_then(Result::ok)
    .unwrap_or_else(|| BackupHealth {
        auto_enabled: Some(auto_enabled),
        status: "unknown".into(),
        ..Default::default()
    });
    let coverage_issue = match backup.status.as_str() {
        "missing" => Some((
            "backup-missing",
            "暂无可用备份",
            "未找到适用于当前数据库的备份文件，请先创建备份。",
        )),
        "awaiting_auto" => Some((
            "backup-awaiting-auto",
            "尚无自动备份",
            "已找到手动备份，但尚未找到自动备份文件，请检查自动备份任务。",
        )),
        "overdue" => Some((
            "backup-overdue",
            "自动备份已超期",
            "超过 25 小时未生成备份，请检查自动备份任务与运行日志。",
        )),
        "unknown" => Some((
            "backup-coverage",
            "备份文件状态待确认",
            "无法确认备份目录或文件时间，请到备份页面检查。",
        )),
        _ => None,
    };
    if let Some((id, title, detail)) = coverage_issue {
        issues.push(issue(
            id,
            "backup",
            "warning",
            title,
            detail,
            "/system/backups",
        ));
    }
    let data = state.paths.data.clone();
    let backup_issues = tokio::task::spawn_blocking(move || backup_issues(&data))
        .await
        .unwrap_or_else(|_| {
            vec![issue(
                "backup-check",
                "backup",
                "warning",
                "备份状态读取失败",
                "请检查备份目录权限和磁盘空间。",
                "/system/backups",
            )]
        });
    issues.extend(backup_issues);
    let data = state.paths.data.clone();
    match tokio::task::spawn_blocking(move || crate::source_operations::health_attention(&data))
        .await
    {
        Ok(Ok(items)) => {
            for (id, title, detail) in items {
                issues.push(issue(
                    &id,
                    "plugins",
                    "warning",
                    &title,
                    &detail,
                    "/plugins/store",
                ));
            }
        }
        _ => issues.push(issue(
            "plugin-jobs-check",
            "plugins",
            "warning",
            "插件安装状态读取失败",
            "请检查任务记录目录权限，并打开插件商店查看结果。",
            "/plugins/store",
        )),
    }
    if let Ok(disk) = state.operations.disk(state).await {
        for volume in &disk.volumes {
            if volume.total_bytes > 0
                && (volume.available_bytes < 512 * 1024 * 1024
                    || volume.available_bytes.saturating_mul(100) / volume.total_bytes < 5)
            {
                issues.push(issue(
                    &format!("disk:{}", volume.mount),
                    "disk",
                    "error",
                    "磁盘可用空间不足",
                    "剩余空间不足 5% 或 512 MiB；请检查占用，避免写入和备份失败。",
                    "/#disk-usage",
                ));
            }
        }
    }
    issues.sort_by(|a, b| a.id.cmp(&b.id));
    issues.dedup_by(|a, b| a.id == b.id);
    let snapshot = OperationsSnapshot {
        status: if issues.is_empty() { "ok" } else { "degraded" }.into(),
        checked_at: Some(chrono::Utc::now().to_rfc3339()),
        db_connected,
        active_connections,
        loaded_plugins,
        issues,
        backup,
    };
    CollectedHealth {
        snapshot,
        connections,
    }
}

/// 只核对当前构建的插件。数据库保存的历史设置不代表当前安装清单。
fn plugin_runtime_issues(
    installed: &[nuono_core::service::plugins::InstalledPluginInfo],
    loaded: &[String],
    enabled_registry: &BTreeSet<String>,
    checks: &mut ConnectionChecks,
) -> Vec<HealthIssue> {
    checks.checked_kinds.insert("plugin-runtime");
    let mut issues = Vec::new();
    for plugin in installed.iter().filter(|plugin| plugin.enabled) {
        let id = format!("plugin-runtime:{}", plugin.name);
        if enabled_registry.contains(&plugin.name) && loaded.contains(&plugin.name) {
            checks
                .healthy
                .insert(id, format!("{} 已恢复运行", plugin.name));
        } else {
            issues.push(issue(
                &id,
                "plugins",
                "error",
                &format!("{} 未运行", plugin.name),
                "配置为启用，但宿主没有可用实例；请检查插件日志并重新构建或启用。",
                "/plugins",
            ));
        }
    }
    issues
}

fn backup_issues(data: &Path) -> Vec<HealthIssue> {
    let mut issues = Vec::new();
    if data.join(crate::backup::RESTORE_MARKER).exists() {
        issues.push(issue(
            "restore-pending",
            "backup",
            "warning",
            "数据库恢复尚未完成",
            "请查看恢复结果；恢复将在下一次启动时处理。",
            "/system/backups",
        ));
    }
    if crate::backup::transfer::is_pending(data) {
        issues.push(issue(
            "transfer-pending",
            "backup",
            "warning",
            "实例迁移等待重启",
            "请按备份页面提示重启并检查迁移结果。",
            "/system/backups",
        ));
    }
    let status_file = data.join("backup/status.json");
    match std::fs::File::open(status_file) {
        Ok(handle) => {
            use std::io::Read;
            let mut bytes = Vec::new();
            let status = handle
                .take(16 * 1024 + 1)
                .read_to_end(&mut bytes)
                .ok()
                .filter(|_| bytes.len() <= 16 * 1024)
                .and_then(|_| serde_json::from_slice::<BackupStatus>(&bytes).ok());
            match status {
                Some(status) if !status.success => issues.push(issue(
                    "backup-failed",
                    "backup",
                    "error",
                    "最近一次备份失败",
                    "请检查数据库工具、备份目录权限和磁盘空间后重试。",
                    "/system/backups",
                )),
                None => issues.push(issue(
                    "backup-status",
                    "backup",
                    "warning",
                    "备份结果无法读取",
                    "请在备份页面检查最近一次操作。",
                    "/system/backups",
                )),
                _ => {}
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => issues.push(issue(
            "backup-status",
            "backup",
            "warning",
            "备份结果无法读取",
            "请检查备份目录权限，并到备份页面查看最近一次操作。",
            "/system/backups",
        )),
    }
    // 整实例迁移由备份模块记录结果，读取失败也会明确提示。
    match crate::backup::transfer::read_status(data) {
        Ok(Some(status)) if status.status == "failed" => issues.push(issue(
            "transfer-failed",
            "backup",
            "error",
            "实例迁移失败",
            "请到备份页面查看迁移结果并处理。",
            "/system/backups",
        )),
        Err(_) => issues.push(issue(
            "transfer-status",
            "backup",
            "warning",
            "迁移结果无法读取",
            "请检查数据目录权限。",
            "/system/backups",
        )),
        _ => {}
    }
    issues
}

#[derive(Serialize, Deserialize)]
struct BackupStatus {
    success: bool,
    completed_at: String,
}

pub async fn record_backup_result(data: &Path, result: Result<(), String>) {
    let value = BackupStatus {
        success: result.is_ok(),
        completed_at: chrono::Utc::now().to_rfc3339(),
    };
    let path = data.join("backup/status.json");
    if let Some(parent) = path.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    if let Ok(bytes) = serde_json::to_vec(&value) {
        let temp = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
        if tokio::fs::write(&temp, bytes).await.is_ok()
            && tokio::fs::rename(&temp, path).await.is_err()
        {
            let _ = tokio::fs::remove_file(temp).await;
        }
    }
}

fn transitions(
    previous: &mut BTreeMap<String, NoticeState>,
    current: &[HealthIssue],
    connections: &ConnectionChecks,
    now: u64,
) -> Vec<(String, String, String)> {
    let mut notices = Vec::new();
    for issue in current {
        if connections.deferred.contains(&issue.id) {
            continue;
        }
        let entry = previous.entry(issue.id.clone()).or_default();
        let is_connection = connection_kind(&issue.id).is_some();
        if is_connection && !entry.active {
            entry.connection_alert_open = Some(false);
        }
        if (!entry.active || entry.severity != issue.severity)
            && (entry.last_sent == 0 || now.saturating_sub(entry.last_sent) >= 300)
        {
            notices.push((
                issue.title.clone(),
                issue.detail.clone(),
                issue.link.clone(),
            ));
            entry.last_sent = now;
            if is_connection {
                entry.connection_alert_open = Some(true);
            }
        }
        entry.active = true;
        entry.severity = issue.severity.clone();
        entry.title = issue.title.clone();
        entry.link = issue.link.clone();
    }
    for (id, entry) in previous.iter_mut() {
        if connections.deferred.contains(id) {
            continue;
        }
        if entry.active && !current.iter().any(|issue| &issue.id == id) {
            let is_connection = connection_kind(id).is_some();
            let connection_alert_open = entry
                .connection_alert_open
                .unwrap_or(entry.last_sent > entry.last_recovered);
            let recovery = match id.split_once(':') {
                Some((kind @ ("onebot" | "bot" | "plugin-runtime" | "plugin-config"), _)) => {
                    // 读取失败不能判定恢复；移除或禁用插件/实例也不等于恢复运行。
                    if !connections.checked_kinds.contains(kind) {
                        continue;
                    }
                    let Some(healthy) = connections.healthy.get(id) else {
                        entry.active = false;
                        if is_connection {
                            entry.connection_alert_open = Some(false);
                        }
                        continue;
                    };
                    let title = match kind {
                        "plugin-runtime" => "插件已恢复运行",
                        "plugin-config" => "插件配置已恢复",
                        _ => "机器人连接已恢复",
                    };
                    (title.into(), healthy.clone())
                }
                _ => ("运行问题已恢复".into(), format!("已解除：{}", entry.title)),
            };
            entry.active = false;
            if is_connection {
                entry.connection_alert_open = Some(false);
            }
            let notify = if is_connection {
                connection_alert_open
            } else {
                entry.last_recovered == 0 || now.saturating_sub(entry.last_recovered) >= 300
            };
            if notify {
                notices.push((recovery.0, recovery.1, entry.link.clone()));
                entry.last_recovered = now;
            }
        }
    }
    previous.retain(|_, entry| entry.active || now.saturating_sub(entry.last_sent) < 7 * 86400);
    notices
}

async fn publish_changes(state: &AppState, issues: &[HealthIssue], connections: &ConnectionChecks) {
    let Ok(users) = state.db.list_users().await else {
        return;
    };
    let admins: Vec<_> = users
        .into_iter()
        .filter(|user| (user.role == "owner" || user.role == "admin") && user.status == "active")
        .collect();
    if admins.is_empty() {
        return;
    }
    let mut old = state.operations.notices.lock().await;
    let mut next = old.clone();
    let messages = transitions(
        &mut next,
        issues,
        connections,
        chrono::Utc::now().timestamp().max(0) as u64,
    );
    if *old == next {
        return;
    }
    let Ok(saved) = serde_json::to_string(&next) else {
        return;
    };
    let ids: Vec<_> = admins.iter().map(|admin| admin.id).collect();
    if state
        .db
        .publish_operations_notifications(&ids, &messages, &saved)
        .await
        .is_err()
    {
        return;
    }
    *old = next;
}
