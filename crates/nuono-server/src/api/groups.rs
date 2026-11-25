use crate::api::input::ApiJson as Json;
use axum::extract::State;
use nuono_core::auth::Capability;
use serde::Deserialize;
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::AppState;
use crate::api::{ApiReply, ApiResponse};
use crate::middleware::CallerIdentity;

/// 写操作前置校验：客户碰不属于自己的群直接拒。
///
/// 范围由 [`CallerIdentity::scope`] 给出，不在这里按角色现算 —— 现算过一次就会有第二处，
/// 两处判据迟早不一致。
async fn guard<T: serde::Serialize>(
    state: &AppState,
    caller: &CallerIdentity,
    group_id: i64,
) -> Result<(), ApiReply<T>> {
    nuono_core::service::groups::ensure_group_writable(
        &state.db,
        group_id,
        caller.scope(Capability::ManageAllGroups),
    )
    .await
    .map_err(|e| ApiResponse::from_err(&e))
}

/// 用读取范围校验群服务可用性，与 [`guard`] 的归属校验独立。
async fn service_gate<T: serde::Serialize>(
    state: &AppState,
    caller: &CallerIdentity,
    group_id: i64,
) -> Result<(), ApiReply<T>> {
    nuono_core::service::groups::ensure_group_serviceable(
        &state.db,
        group_id,
        caller.scope(Capability::ViewAllGroups),
    )
    .await
    .map_err(|e| ApiResponse::from_err(&e))
}

fn onebot_avatar_url(bot_id: &str) -> Option<String> {
    let number = bot_id
        .parse::<i64>()
        .ok()
        .filter(|n| *n > 0 && n.to_string() == bot_id)?;
    Some(format!("https://q1.qlogo.cn/g?b=qq&nk={number}&s=100"))
}

/// 补全群组来源标签（例如将 `qq_official` + `app_id` 解析为 `QQ 官方 · 小小`）、各群 AI 暂停状态与群内机器人列表。
async fn enrich_groups(state: &AppState, groups: &mut [nuono_core::service::groups::GroupRecord]) {
    use nuono_core::service::groups::GroupBotPresence;
    use std::collections::{HashMap, HashSet};

    let config_snapshot = state.config.snapshot().await;
    let blocked_groups = &config_snapshot.ai.blocked_groups;
    let mut group_ids = Vec::with_capacity(groups.len());
    for group in groups.iter_mut() {
        if let Ok(id) = group.group_id.parse::<i64>() {
            group.ai_blocked = blocked_groups.contains(&id);
            group_ids.push(id);
        } else {
            group.ai_blocked = false;
        }
    }

    let instances = state.bot_instances.list().await.unwrap_or_default();
    let onebot_bots =
        nuono_core::service::bots::get_bot_list_with_runtime(&state.db, &state.manager.bot_states)
            .await
            .unwrap_or_default();
    let connected_routes = state
        .manager
        .event_kernel
        .action_router()
        .connected_routes();
    let group_bots_map = state
        .db
        .list_group_bots(&group_ids)
        .await
        .unwrap_or_default();

    let mut onebot_by_id: HashMap<String, (String, bool)> = HashMap::new();
    for bot in onebot_bots {
        let label = bot
            .nickname
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .unwrap_or(&bot.bot_id)
            .to_owned();
        let online = bot.status == "online" || bot.transport_connected == Some(true);
        onebot_by_id.insert(bot.bot_id, (label, online));
    }

    let mut instances_by_id: HashMap<String, &crate::bot_instances::BotInstanceView> =
        HashMap::new();
    let mut instances_by_app: HashMap<(String, String), &crate::bot_instances::BotInstanceView> =
        HashMap::new();
    for instance in &instances {
        instances_by_id.insert(instance.id.clone(), instance);
        let app_id = instance
            .config
            .get("app_id")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .unwrap_or("");
        if !app_id.is_empty() {
            instances_by_app.insert((instance.adapter_type.clone(), app_id.to_owned()), instance);
        }
    }

    let mut route_instance_by_bot: HashMap<(String, String), String> = HashMap::new();
    for route in connected_routes {
        route_instance_by_bot.insert((route.platform, route.bot_self_id), route.instance_id);
    }

    for group in groups {
        let base = match group.platform.as_str() {
            "onebot" => "OneBot v11",
            "qq_official" => "QQ 官方",
            "feishu" => "飞书",
            "telegram" => "Telegram",
            "discord" => "Discord",
            other if !other.is_empty() => other,
            _ => "未知平台",
        };
        if !group.namespace.is_empty() {
            let instance_name = instances_by_app
                .get(&(group.platform.clone(), group.namespace.clone()))
                .map(|instance| instance.name.trim())
                .filter(|name| !name.is_empty())
                .unwrap_or(&group.namespace);
            group.source_label = format!("{base} · {instance_name}");
        } else {
            group.source_label = match group.platform.as_str() {
                "qq_official" => "QQ 官方机器人".to_owned(),
                _ => base.to_owned(),
            };
        }

        let mut seen = HashSet::<(String, String)>::new();
        let mut bots = Vec::<GroupBotPresence>::new();
        if let Ok(id) = group.group_id.parse::<i64>()
            && let Some(rows) = group_bots_map.get(&id)
        {
            for row in rows {
                if !seen.insert((row.platform.clone(), row.bot_id.clone())) {
                    continue;
                }
                if row.platform == "onebot" {
                    let (label, online) = onebot_by_id
                        .get(&row.bot_id)
                        .cloned()
                        .unwrap_or_else(|| (row.bot_id.clone(), false));
                    bots.push(GroupBotPresence {
                        avatar_url: onebot_avatar_url(&row.bot_id),
                        bot_id: row.bot_id.clone(),
                        platform: row.platform.clone(),
                        label,
                        online,
                    });
                } else {
                    let matched = instances_by_id
                        .get(&row.bot_id)
                        .copied()
                        .or_else(|| {
                            route_instance_by_bot
                                .get(&(row.platform.clone(), row.bot_id.clone()))
                                .and_then(|instance_id| instances_by_id.get(instance_id).copied())
                        })
                        .or_else(|| {
                            instances_by_app
                                .get(&(row.platform.clone(), row.bot_id.clone()))
                                .copied()
                        })
                        .or_else(|| {
                            (!group.namespace.is_empty())
                                .then(|| {
                                    instances_by_app
                                        .get(&(row.platform.clone(), group.namespace.clone()))
                                        .copied()
                                })
                                .flatten()
                        });
                    if let Some(instance) = matched {
                        let label = if instance.name.trim().is_empty() {
                            row.bot_id.clone()
                        } else {
                            instance.name.trim().to_owned()
                        };
                        bots.push(GroupBotPresence {
                            bot_id: row.bot_id.clone(),
                            platform: row.platform.clone(),
                            label,
                            online: instance.connected,
                            avatar_url: instance.avatar_url.clone(),
                        });
                    } else {
                        let online = route_instance_by_bot
                            .contains_key(&(row.platform.clone(), row.bot_id.clone()));
                        bots.push(GroupBotPresence {
                            bot_id: row.bot_id.clone(),
                            platform: row.platform.clone(),
                            label: row.bot_id.clone(),
                            online,
                            avatar_url: None,
                        });
                    }
                }
            }
        }

        // 历史按命名空间隔离的官方/飞书群尚未触发新事件前，仍按命名空间反查对应实例补齐展示。
        if bots.is_empty()
            && !group.namespace.is_empty()
            && let Some(instance) = instances_by_app
                .get(&(group.platform.clone(), group.namespace.clone()))
                .copied()
        {
            let label = if instance.name.trim().is_empty() {
                group.namespace.clone()
            } else {
                instance.name.trim().to_owned()
            };
            bots.push(GroupBotPresence {
                bot_id: group.namespace.clone(),
                platform: group.platform.clone(),
                label,
                online: instance.connected,
                avatar_url: instance.avatar_url.clone(),
            });
        }

        bots.sort_by(|a, b| {
            b.online
                .cmp(&a.online)
                .then_with(|| a.label.to_lowercase().cmp(&b.label.to_lowercase()))
                .then_with(|| a.bot_id.cmp(&b.bot_id))
        });
        group.bots = bots;
    }
}

/// 收集普通用户通过已验证绑定身份或归属扩展点拥有的机器人所在群 ID。
async fn discover_user_bot_groups(
    state: &AppState,
    user_id: i64,
    existing_ids: &std::collections::HashSet<i64>,
) -> nuono_core::error::Result<std::collections::HashSet<i64>> {
    use std::collections::{HashMap, HashSet};

    let rows = state.db.all_group_bots().await?;
    if rows.is_empty() {
        return Ok(HashSet::new());
    }
    let identities = state.db.verified_user_platform_identities(user_id).await?;
    let bound_bots: HashSet<(String, String)> = identities
        .into_iter()
        .map(|(platform, _namespace, native_user_id)| (platform, native_user_id))
        .collect();

    let mut owner_cache: HashMap<String, Option<i64>> = HashMap::new();
    let mut matched = HashSet::new();
    for row in rows {
        if existing_ids.contains(&row.group_id) || matched.contains(&row.group_id) {
            continue;
        }
        if bound_bots.contains(&(row.platform, row.bot_id.clone())) {
            matched.insert(row.group_id);
            continue;
        }
        let owner = if let Some(cached) = owner_cache.get(&row.bot_id) {
            *cached
        } else {
            let resolved =
                nuono_core::service::discovery::resolve_bot_owner(&state.db, &row.bot_id).await?;
            owner_cache.insert(row.bot_id.clone(), resolved);
            resolved
        };
        if owner == Some(user_id) {
            matched.insert(row.group_id);
        }
    }
    Ok(matched)
}

#[utoipa::path(
    get,
    path = "/",
    operation_id = "groups_list",
    tag = "groups",
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<Vec<nuono_core::service::groups::GroupRecord>>))
)]
pub async fn list(
    State(state): State<AppState>,
    crate::api::input::ApiCaller(caller): crate::api::input::ApiCaller,
) -> crate::api::ApiReply<Vec<nuono_core::service::groups::GroupRecord>> {
    match nuono_core::service::groups::get_groups(
        &state.db,
        caller.scope(Capability::ViewAllGroups),
    )
    .await
    {
        Ok(mut groups) => {
            if !caller.has(Capability::ViewAllGroups) {
                let existing_ids: std::collections::HashSet<i64> = groups
                    .iter()
                    .filter_map(|row| row.group_id.parse::<i64>().ok())
                    .collect();
                let mut verified_role_ids = std::collections::HashSet::new();
                let enabled = match state
                    .db
                    .get_global_config("configuration.platform_roles_enabled")
                    .await
                {
                    Ok(value) => value.and_then(|v| v.as_bool()).unwrap_or(false),
                    Err(error) => return ApiResponse::from_err(&error),
                };
                if enabled {
                    let raw_groups = match state.db.all_groups().await {
                        Ok(rows) => rows,
                        Err(error) => return ApiResponse::from_err(&error),
                    };
                    let candidates: Vec<_> = raw_groups
                        .into_iter()
                        .filter(|g| !existing_ids.contains(&g.id))
                        .collect();
                    verified_role_ids = match state
                        .manager
                        .plugin_host
                        .verified_platform_role_groups(&caller, &candidates)
                        .await
                    {
                        Ok(ids) => ids,
                        Err(error) => return ApiResponse::from_err(&error),
                    };
                }
                let bot_group_ids =
                    match discover_user_bot_groups(&state, caller.user_id, &existing_ids).await {
                        Ok(ids) => ids,
                        Err(error) => return ApiResponse::from_err(&error),
                    };
                if !verified_role_ids.is_empty() || !bot_group_ids.is_empty() {
                    let all = match nuono_core::service::groups::get_groups(
                        &state.db,
                        nuono_core::auth::Scope::All,
                    )
                    .await
                    {
                        Ok(rows) => rows,
                        Err(error) => return ApiResponse::from_err(&error),
                    };
                    let check_entitlement = nuono_app::entitlement::has_entitlement_provider();
                    for mut group in all {
                        let Ok(id) = group.group_id.parse::<i64>() else {
                            continue;
                        };
                        if existing_ids.contains(&id) {
                            continue;
                        }
                        if verified_role_ids.contains(&id) {
                            if check_entitlement
                                && !nuono_app::entitlement::is_group_active(id).await
                            {
                                group.enabled = None;
                                group.last_activity = None;
                            }
                            groups.push(group);
                        } else if bot_group_ids.contains(&id) {
                            // 仅因存在本人绑定或名下机器人而发现的群保持只读，不绕过群配置授权边界。
                            group.enabled = None;
                            group.last_activity = None;
                            groups.push(group);
                        }
                    }
                    groups.sort_by_key(|group| group.group_id.parse::<i64>().unwrap_or(i64::MAX));
                }
            }
            enrich_groups(&state, &mut groups).await;
            ApiResponse::ok(groups)
        }
        Err(e) => ApiResponse::from_err(&e),
    }
}

#[derive(Deserialize, utoipa::ToSchema)]
#[schema(as = api::groups::RenameBody)]
pub struct RenameBody {
    display_name: String,
}

#[utoipa::path(
    patch,
    path = "/{group_id}",
    operation_id = "groups_rename",
    tag = "groups",
    request_body = RenameBody,
    params(("group_id" = String, Path)),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<String>))
)]
pub async fn rename(
    State(state): State<AppState>,
    crate::api::input::ApiCaller(caller): crate::api::input::ApiCaller,
    crate::api::input::ApiPath(group_id): crate::api::input::ApiPath<i64>,
    Json(body): Json<RenameBody>,
) -> ApiReply<String> {
    if let Err(resp) = guard(&state, &caller, group_id).await {
        return resp;
    }
    if let Err(resp) = service_gate(&state, &caller, group_id).await {
        return resp;
    }
    match nuono_core::service::groups::rename_group(&state.db, group_id, &body.display_name).await {
        Ok(()) => ApiResponse::ok("群名称已更新".to_string()),
        Err(e) => ApiResponse::from_err(&e),
    }
}

#[derive(Deserialize, utoipa::ToSchema)]
#[schema(as = api::groups::ToggleBody)]
pub struct ToggleBody {
    enabled: bool,
}

#[utoipa::path(
    post,
    path = "/{group_id}/toggle",
    operation_id = "groups_toggle",
    tag = "groups",
    request_body = ToggleBody,
    params(("group_id" = String, Path)),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<String>))
)]
pub async fn toggle(
    State(state): State<AppState>,
    crate::api::input::ApiCaller(caller): crate::api::input::ApiCaller,
    crate::api::input::ApiPath(group_id): crate::api::input::ApiPath<i64>,
    Json(body): Json<ToggleBody>,
) -> ApiReply<String> {
    if let Err(resp) = guard(&state, &caller, group_id).await {
        return resp;
    }
    if let Err(resp) = service_gate(&state, &caller, group_id).await {
        return resp;
    }
    match nuono_core::service::groups::toggle_group(&state.db, group_id, body.enabled).await {
        Ok(()) => ApiResponse::ok(format!(
            "群组 {} 已{}",
            group_id,
            if body.enabled { "启用" } else { "禁用" }
        )),
        Err(e) => ApiResponse::from_err(&e),
    }
}

#[utoipa::path(
    post,
    path = "/sync/{bot_id}",
    operation_id = "groups_sync_groups",
    tag = "groups",
    params(("bot_id" = String, Path)),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<nuono_core::service::groups::GroupSyncSummary>))
)]
pub async fn sync_groups(
    State(state): State<AppState>,
    crate::api::input::ApiCaller(caller): crate::api::input::ApiCaller,
    crate::api::input::ApiPath(bot_id): crate::api::input::ApiPath<String>,
) -> crate::api::ApiReply<nuono_core::service::groups::GroupSyncSummary> {
    if !caller.has(Capability::ManageAllGroups) {
        return ApiResponse::forbidden(Capability::ManageAllGroups.denial_message());
    }
    let Ok(bot_id_i64) = bot_id.parse::<i64>() else {
        return ApiResponse::bad_request(format!("bot_id '{}' 无法解析为整数", bot_id));
    };

    let tx = match state.manager.bot_txs.get(&bot_id_i64) {
        Some(tx) => tx.clone(),
        None => {
            let online_ids: Vec<String> = state
                .manager
                .bot_txs
                .iter()
                .map(|entry| entry.key().to_string())
                .collect();
            return ApiResponse::conflict(format!(
                "Bot {} 未在线。当前在线 Bot: {:?}",
                bot_id, online_ids
            ));
        }
    };

    // bot 身份就是这里解析出来的 `bot_id_i64`（OneBot 的 self_id），
    // 共享的 api_client 只按 echo 关联，进不去函数里就再也认不出来是哪个 bot。
    match nuono_core::service::groups::sync_groups_from_bot(
        &state.db,
        bot_id_i64,
        &state.manager.api_client,
        &tx,
    )
    .await
    {
        Ok(summary) => ApiResponse::ok(summary),
        Err(e) => ApiResponse::from_err(&e),
    }
}

fn get_bot_tx<T: serde::Serialize>(
    state: &AppState,
    bot_id: &str,
) -> Result<tokio::sync::mpsc::Sender<nuono_app::transport::TransportFrame>, ApiReply<T>> {
    let Ok(bot_id_i64) = bot_id.parse::<i64>() else {
        return Err(ApiResponse::bad_request(format!(
            "bot_id '{}' 无法解析为整数",
            bot_id
        )));
    };
    match state.manager.bot_txs.get(&bot_id_i64) {
        Some(tx) => Ok(tx.clone()),
        // Bot 不在线属于运行状态冲突，返回 409：
        // 前端据此提示「重新扫码」，而 403 会被当成越权。
        None => Err(ApiResponse::conflict(format!("Bot {} 未在线", bot_id))),
    }
}

#[utoipa::path(
    get,
    path = "/{group_id}/members/{bot_id}",
    operation_id = "groups_get_members",
    tag = "groups",
    params(("group_id" = String, Path), ("bot_id" = String, Path)),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<Vec<super::docs::AnyJson>>))
)]
pub async fn get_members(
    State(state): State<AppState>,
    crate::api::input::ApiCaller(caller): crate::api::input::ApiCaller,
    crate::api::input::ApiPath((group_id, bot_id)): crate::api::input::ApiPath<(i64, String)>,
) -> ApiReply<Vec<serde_json::Value>> {
    if let Err(resp) = guard(&state, &caller, group_id).await {
        return resp;
    }
    if let Err(resp) = service_gate(&state, &caller, group_id).await {
        return resp;
    }
    let group_id = match nuono_core::service::groups::onebot_group_id(&state.db, group_id).await {
        Ok(value) => value,
        Err(error) => return ApiResponse::from_err(&error),
    };
    let tx = match get_bot_tx(&state, &bot_id) {
        Ok(tx) => tx,
        Err(resp) => return resp,
    };
    match nuono_core::service::groups::get_group_members(&state.manager.api_client, &tx, &group_id)
        .await
    {
        Ok(members) => ApiResponse::ok(members),
        Err(e) => ApiResponse::from_err(&e),
    }
}

#[derive(Deserialize, utoipa::ToSchema)]
#[schema(as = api::groups::SendMessageBody)]
pub struct SendMessageBody {
    content: String,
}

#[utoipa::path(
    post,
    path = "/{group_id}/send/{bot_id}",
    operation_id = "groups_send_message",
    tag = "groups",
    request_body = SendMessageBody,
    params(("group_id" = String, Path), ("bot_id" = String, Path)),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<String>))
)]
pub async fn send_message(
    State(state): State<AppState>,
    crate::api::input::ApiCaller(caller): crate::api::input::ApiCaller,
    crate::api::input::ApiPath((group_id, bot_id)): crate::api::input::ApiPath<(i64, String)>,
    Json(body): Json<SendMessageBody>,
) -> ApiReply<String> {
    if let Err(resp) = guard(&state, &caller, group_id).await {
        return resp;
    }
    if let Err(resp) = service_gate(&state, &caller, group_id).await {
        return resp;
    }
    let group_id = match nuono_core::service::groups::onebot_group_id(&state.db, group_id).await {
        Ok(value) => value,
        Err(error) => return ApiResponse::from_err(&error),
    };
    let tx = match get_bot_tx(&state, &bot_id) {
        Ok(tx) => tx,
        Err(resp) => return resp,
    };
    match nuono_core::service::groups::send_group_message(
        &state.manager.api_client,
        &tx,
        &group_id,
        &body.content,
    )
    .await
    {
        Ok(()) => ApiResponse::ok("消息已发送".to_string()),
        Err(e) => ApiResponse::from_err(&e),
    }
}

#[derive(Deserialize, utoipa::ToSchema)]
#[schema(as = api::groups::BanBody)]
pub struct BanBody {
    user_id: String,
    duration_secs: u64,
}

#[utoipa::path(
    post,
    path = "/{group_id}/ban/{bot_id}",
    operation_id = "groups_ban_member",
    tag = "groups",
    request_body = BanBody,
    params(("group_id" = String, Path), ("bot_id" = String, Path)),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<String>))
)]
pub async fn ban_member(
    State(state): State<AppState>,
    crate::api::input::ApiCaller(caller): crate::api::input::ApiCaller,
    crate::api::input::ApiPath((group_id, bot_id)): crate::api::input::ApiPath<(i64, String)>,
    Json(body): Json<BanBody>,
) -> ApiReply<String> {
    if let Err(resp) = guard(&state, &caller, group_id).await {
        return resp;
    }
    if let Err(resp) = service_gate(&state, &caller, group_id).await {
        return resp;
    }
    let group_id = match nuono_core::service::groups::onebot_group_id(&state.db, group_id).await {
        Ok(value) => value,
        Err(error) => return ApiResponse::from_err(&error),
    };
    let tx = match get_bot_tx(&state, &bot_id) {
        Ok(tx) => tx,
        Err(resp) => return resp,
    };
    match nuono_core::service::groups::set_group_ban(
        &state.manager.api_client,
        &tx,
        &group_id,
        &body.user_id,
        body.duration_secs,
    )
    .await
    {
        Ok(()) => ApiResponse::ok(format!("已禁言 {} 秒", body.duration_secs)),
        Err(e) => ApiResponse::from_err(&e),
    }
}

#[utoipa::path(
    get,
    path = "/users/{user_id}",
    operation_id = "groups_list_by_user",
    tag = "groups",
    params(("user_id" = i64, Path)),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<Vec<nuono_core::service::groups::GroupRecord>>))
)]
pub async fn list_by_user(
    State(state): State<AppState>,
    crate::api::input::ApiCaller(caller): crate::api::input::ApiCaller,
    crate::api::input::ApiPath(user_id): crate::api::input::ApiPath<i64>,
) -> ApiReply<Vec<nuono_core::service::groups::GroupRecord>> {
    // 账号页展开一个客户时要看他名下的群。客户自己只能用 `GET /`（已按 Scope 收窄），
    // 这条按任意 user_id 查询，所以必须要求跨归属读取能力。
    if !caller.has(Capability::ViewAllGroups) {
        return ApiResponse::forbidden(Capability::ViewAllGroups.denial_message());
    }
    match nuono_core::service::groups::get_groups(
        &state.db,
        nuono_core::auth::Scope::OwnedBy(user_id),
    )
    .await
    {
        Ok(mut groups) => {
            enrich_groups(&state, &mut groups).await;
            ApiResponse::ok(groups)
        }
        Err(e) => ApiResponse::from_err(&e),
    }
}

#[derive(Deserialize, utoipa::ToSchema)]
#[schema(as = api::groups::KickBody)]
pub struct KickBody {
    user_id: String,
}

#[utoipa::path(
    post,
    path = "/{group_id}/kick/{bot_id}",
    operation_id = "groups_kick_member",
    tag = "groups",
    request_body = KickBody,
    params(("group_id" = String, Path), ("bot_id" = String, Path)),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<String>))
)]
pub async fn kick_member(
    State(state): State<AppState>,
    crate::api::input::ApiCaller(caller): crate::api::input::ApiCaller,
    crate::api::input::ApiPath((group_id, bot_id)): crate::api::input::ApiPath<(i64, String)>,
    Json(body): Json<KickBody>,
) -> ApiReply<String> {
    if let Err(resp) = guard(&state, &caller, group_id).await {
        return resp;
    }
    // 踢人是不可撤销的群管理动作，到期后一律不放行。
    if let Err(resp) = service_gate(&state, &caller, group_id).await {
        return resp;
    }
    let group_id = match nuono_core::service::groups::onebot_group_id(&state.db, group_id).await {
        Ok(value) => value,
        Err(error) => return ApiResponse::from_err(&error),
    };
    let tx = match get_bot_tx(&state, &bot_id) {
        Ok(tx) => tx,
        Err(resp) => return resp,
    };
    match nuono_core::service::groups::set_group_kick(
        &state.manager.api_client,
        &tx,
        &group_id,
        &body.user_id,
    )
    .await
    {
        Ok(()) => ApiResponse::ok("已踢出群".to_string()),
        Err(e) => ApiResponse::from_err(&e),
    }
}

#[derive(Deserialize, utoipa::ToSchema)]
#[schema(as = api::groups::SetGroupAiAccessBody)]
pub struct SetGroupAiAccessBody {
    pub blocked: bool,
}

/// PATCH /api/groups/{group_id}/ai-access
///
/// 暂停或恢复指定群组的 AI 自动回复能力（按内部群 ID 记录在 `ai.blocked_groups` 中）。
#[utoipa::path(
    patch,
    path = "/{group_id}/ai-access",
    operation_id = "groups_set_ai_access",
    tag = "groups",
    request_body = SetGroupAiAccessBody,
    params(("group_id" = String, Path)),
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<super::docs::AnyJson>))
)]
pub async fn set_ai_access(
    State(state): State<AppState>,
    crate::api::input::ApiCaller(caller): crate::api::input::ApiCaller,
    crate::api::input::ApiPath(group_id): crate::api::input::ApiPath<i64>,
    Json(body): Json<SetGroupAiAccessBody>,
) -> ApiReply<serde_json::Value> {
    if let Err(resp) = guard(&state, &caller, group_id).await {
        return resp;
    }
    if let Err(resp) = service_gate(&state, &caller, group_id).await {
        return resp;
    }
    let mut blocked = state.config.get().await.ai.blocked_groups;
    let was_blocked = blocked.contains(&group_id);
    if body.blocked {
        if !was_blocked {
            blocked.push(group_id);
        }
    } else {
        blocked.retain(|id| *id != group_id);
    }
    blocked.sort_unstable();
    blocked.dedup();
    if let Err(e) = state
        .config
        .update_patch(nuono_core::db::RuntimeConfigPatch {
            settings: serde_json::json!({ "ai": { "blocked_groups": blocked } }),
            secrets: Vec::new(),
        })
        .await
    {
        return ApiResponse::from_err(&e);
    }
    if was_blocked != body.blocked {
        let _ = nuono_core::audit::record_event(
            &state.db,
            "group.ai_access_changed",
            None,
            Some(caller.username.as_str()),
            serde_json::json!({
                "group_id": group_id.to_string(),
                "ai_blocked": body.blocked,
                "actor": caller.username,
            }),
        )
        .await;
    }
    ApiResponse::ok(serde_json::json!({
        "group_id": group_id.to_string(),
        "ai_blocked": body.blocked,
    }))
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list))
        .routes(routes!(rename))
        .routes(routes!(toggle))
        .routes(routes!(set_ai_access))
        .routes(routes!(list_by_user))
        .routes(routes!(sync_groups))
        .routes(routes!(get_members))
        .routes(routes!(send_message))
        .routes(routes!(ban_member))
        .routes(routes!(kick_member))
}

#[cfg(test)]
mod tests {
    /// 按群操作同时检查归属和服务可用性，豁免的入口单独列出。
    #[test]
    fn every_per_group_write_checks_both_gates() {
        /// 读取自身源码。`include_str!` 在编译期解析，路径相对本文件。
        const SOURCE: &str = include_str!("groups.rs");

        /// 按设计不需要服务可用性校验的 handler。
        const OWNERSHIP_ONLY: &[(&str, &str)] = &[
            // bot 级全量同步，不针对单个群；
            // 路由层要求 ManageAllGroups，客户根本到不了。
            ("sync_groups", "bot 级操作，路由层已限制为员工"),
            // 群列表保留受限群的标识，详细字段由服务层裁剪。
            ("list", "群列表按可见性裁剪字段"),
        ];

        let mut current: Option<&str> = None;
        let mut has_guard = false;
        let mut has_service_gate = false;
        let mut missing: Vec<&str> = Vec::new();

        for line in SOURCE.lines() {
            if let Some(rest) = line.strip_prefix("pub async fn ") {
                current = rest.split('(').next();
                has_guard = false;
                has_service_gate = false;
            } else if line.contains("guard(&state") && !line.contains("service_gate") {
                has_guard = true;
            } else if line.contains("service_gate(&state") {
                has_service_gate = true;
            } else if line == "}"
                && let Some(name) = current.take()
            {
                let exempt = OWNERSHIP_ONLY.iter().any(|(n, _)| *n == name);
                if has_guard && !has_service_gate && !exempt {
                    missing.push(name);
                }
            }
        }

        assert!(
            missing.is_empty(),
            "以下 handler 缺少服务可用性校验: {missing:?}\n\
             要么补 service_gate，要么把它加进 OWNERSHIP_ONLY 并写明理由"
        );
    }

    /// 上面那道测试必须真的会失败 —— 否则它只是在空转。
    #[test]
    fn the_gate_audit_would_catch_a_missing_service_gate() {
        let sample = "pub async fn toggle(\n\
                          if let Err(resp) = guard(&state, &caller, group_id).await {\n\
                      }\n";
        let mut flagged = false;
        let mut has_guard = false;
        for line in sample.lines() {
            if line.starts_with("pub async fn ") {
                has_guard = false;
            } else if line.contains("guard(&state") {
                has_guard = true;
            } else if line == "}" && has_guard {
                flagged = true;
            }
        }
        assert!(flagged, "审计逻辑本身失效，无法发现缺失的服务可用性校验");
    }
}
