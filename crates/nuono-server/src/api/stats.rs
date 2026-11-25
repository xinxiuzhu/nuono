use axum::extract::{Query, State};
use serde::{Deserialize, Serialize};

use crate::AppState;
use crate::api::ApiResponse;

#[derive(Serialize, utoipa::ToSchema)]
#[schema(as = api::stats::Stats)]
pub struct Stats {
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub online_bots: i64,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub total_messages: i64,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub active_connections: i64,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub total_plugins: i64,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub user_count: i64,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub group_count: i64,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub bot_count: i64,
}

#[derive(Serialize, utoipa::ToSchema)]
#[schema(as = api::stats::TrendItem)]
pub struct TrendItem {
    pub date: String,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub received: i64,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    #[schema(value_type = String)]
    pub sent: i64,
}

#[derive(Serialize, utoipa::ToSchema)]
#[schema(as = api::stats::CommandStats)]
pub struct CommandStats {
    pub receive: u64,
    pub send: u64,
    pub command: u64,
    pub groups: u64,
    pub users: u64,
    pub current_qps: u64,
    pub max_qps: u64,
    pub total_count: u64,
    pub uptime_secs: u64,
}

#[derive(Serialize, utoipa::ToSchema)]
#[schema(as = api::stats::HealthData)]
pub struct HealthData {
    pub status: String,
    pub uptime_secs: u64,
    pub db_connected: bool,
    pub active_connections: usize,
    pub loaded_plugins: usize,
    pub checked_at: Option<String>,
    pub issues: Vec<crate::operations::HealthIssue>,
    pub backup: crate::operations::BackupHealth,
    pub sample_age_secs: Option<u64>,
}

#[utoipa::path(
    get,
    path = "/api/stats",
    operation_id = "stats_get_stats",
    tag = "stats",
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<Stats>))
)]
pub async fn get_stats(State(state): State<AppState>) -> crate::api::ApiReply<Stats> {
    let s = match nuono_core::service::stats::get_dashboard_stats_with_runtime(
        &state.db,
        &state.manager.bot_states,
    )
    .await
    {
        Ok(s) => s,
        Err(e) => return ApiResponse::from_err(&e),
    };
    let instances = match state.bot_instances.list().await {
        Ok(instances) => instances,
        Err(e) => return ApiResponse::from_err(&e),
    };
    let connected_instances = instances
        .iter()
        .filter(|instance| instance.enabled && instance.connected)
        .count() as i64;
    let instance_count = instances.len() as i64;
    let plugins = match nuono_core::service::plugins::get_installed_plugins(
        &state.db,
        &state.manager.plugin_host,
    )
    .await
    {
        Ok(p) => p.len() as i64,
        Err(_) => state
            .manager
            .plugin_host
            .installed_plugin_infos()
            .await
            .len() as i64,
    };
    ApiResponse::ok(Stats {
        online_bots: s.online_bots + connected_instances,
        total_messages: s.total_messages,
        // 底层传输连接数（active_connections）与账号已确认在线数（online_bots）是两个独立指标。
        active_connections: state
            .manager
            .bot_txs
            .iter()
            .filter(|sender| !sender.is_closed())
            .count() as i64
            + connected_instances,
        total_plugins: plugins,
        user_count: s.user_count,
        group_count: s.group_count,
        bot_count: s.bot_count + instance_count,
    })
}

#[derive(Debug, Default, Deserialize, utoipa::IntoParams)]
pub struct TrendQuery {
    /// 统计窗口：`today`（今天）、`yesterday`（昨天）、`week`（7 天）、`month`（30 天），默认 `today`。
    #[serde(default)]
    #[param(inline)]
    pub range: Option<nuono_core::service::messages::MessageTrendRange>,
    /// 客户端相对 UTC 的分钟偏移（如东八区为 `480`），缺省时使用服务器本地时区。
    #[serde(default)]
    pub tz_offset: Option<i32>,
}

#[utoipa::path(
    get,
    path = "/api/trend",
    operation_id = "stats_get_trend",
    tag = "stats",
    params(TrendQuery),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<Vec<TrendItem>>))
)]
pub async fn get_trend(
    State(state): State<AppState>,
    Query(query): Query<TrendQuery>,
) -> crate::api::ApiReply<Vec<TrendItem>> {
    let range = query.range.unwrap_or_default();
    let tz_offset = query
        .tz_offset
        .unwrap_or_else(|| chrono::Local::now().offset().local_minus_utc() / 60)
        .clamp(-720, 840);
    match nuono_core::service::messages::get_message_trend(&state.db, range, tz_offset).await {
        Ok(rows) => ApiResponse::ok(
            rows.into_iter()
                .map(|r| TrendItem {
                    date: r.time,
                    received: r.received,
                    sent: r.sent,
                })
                .collect(),
        ),
        Err(e) => ApiResponse::from_err(&e),
    }
}

#[utoipa::path(
    get,
    path = "/api/command-stats",
    operation_id = "stats_get_command_stats",
    tag = "stats",
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<CommandStats>))
)]
pub async fn get_command_stats(
    State(state): State<AppState>,
) -> crate::api::ApiReply<CommandStats> {
    match nuono_core::service::stats::get_command_stats(&state.db).await {
        Ok(s) => ApiResponse::ok(CommandStats {
            receive: s.receive,
            send: s.send,
            command: s.command,
            groups: s.groups,
            users: s.users,
            current_qps: s.current_qps,
            max_qps: s.max_qps,
            total_count: s.total_count,
            uptime_secs: state.started_at.elapsed().as_secs(),
        }),
        Err(e) => ApiResponse::from_err(&e),
    }
}

#[utoipa::path(
    get,
    path = "/api/health",
    operation_id = "stats_get_health",
    tag = "stats",
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<HealthData>))
)]
pub async fn get_health(State(state): State<AppState>) -> crate::api::ApiReply<HealthData> {
    let health = state.operations.snapshot().await;
    let sample_age_secs = health
        .checked_at
        .as_deref()
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .map(|time| {
            chrono::Utc::now()
                .signed_duration_since(time)
                .num_seconds()
                .max(0) as u64
        });
    ApiResponse::ok(HealthData {
        status: health.status,
        uptime_secs: state.started_at.elapsed().as_secs(),
        db_connected: health.db_connected,
        active_connections: health.active_connections,
        loaded_plugins: health.loaded_plugins,
        checked_at: health.checked_at,
        issues: health.issues,
        backup: health.backup,
        sample_age_secs,
    })
}
