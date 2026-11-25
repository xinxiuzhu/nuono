use std::{path::PathBuf, sync::Arc, time::Duration};

use chrono::{FixedOffset, Utc};
use nuono_storage::status_stats::{StatusScopeStats, StatusStats, get_status_stats};

use super::CoreCommands;
use crate::core::{
    event_kernel::RuntimeEventContext, plugin_host::PluginHost, system_monitor::SystemStatus,
};

mod renderer;
use renderer::{CardDay, CardMetric, CardPlugin, StatusCard};

pub(super) struct StatusSource {
    pub db: crate::db::Database,
    pub host: Arc<PluginHost>,
    pub data_dir: PathBuf,
}

pub(super) async fn handle(commands: &CoreCommands, ctx: &RuntimeEventContext<'_>) {
    let Ok(_request) = commands.status_requests.clone().try_acquire_owned() else {
        ctx.send_reply_text("状态卡正在生成，请稍后重试。".into(), "CoreCommands")
            .await;
        return;
    };
    let system = commands.system_monitor.get_status();
    let card = collect(commands, ctx, &system).await;
    let settings = commands.config.snapshot().await.image_output;
    let data_dir = commands
        .status_source
        .as_ref()
        .map(|source| source.data_dir.clone());
    let rendered = tokio::time::timeout(
        Duration::from_secs(8),
        commands.status_render_scope.render("status-card", move || {
            let mut card = card;
            let disk = data_dir
                .as_deref()
                .and_then(crate::core::system_monitor::disk_space);
            card.system.push(match disk {
                Some(disk) if disk.total_bytes > 0 => {
                    let used = disk.total_bytes.saturating_sub(disk.available_bytes);
                    CardMetric {
                        label: "数据盘已用".into(),
                        value: format!("{} / {} GiB", gib(used), gib(disk.total_bytes)),
                        percent: Some(used as f32 * 100.0 / disk.total_bytes as f32),
                    }
                }
                _ => metric("数据盘已用", "暂不可用"),
            });
            nuono_plugin::image_output::encode(&renderer::render(&card)?, &settings)
        }),
    )
    .await;
    match rendered {
        Ok(Ok(bytes)) if ctx.send_reply_image(&bytes, "CoreCommands").await => {}
        Ok(Ok(_)) => {
            ctx.send_reply_text(
                format!("{}\n状态图片发送失败，已回退文字。", render(&system)),
                "CoreCommands",
            )
            .await;
        }
        result => {
            tracing::warn!(bot_id = ctx.bot_id, error = ?result, "Nuono 状态卡生成失败");
            ctx.send_reply_text(
                format!("{}\n状态图片暂不可用，已回退文字。", render(&system)),
                "CoreCommands",
            )
            .await;
        }
    }
}

async fn collect(
    commands: &CoreCommands,
    ctx: &RuntimeEventContext<'_>,
    system: &SystemStatus,
) -> StatusCard {
    let now = Utc::now();
    let bot_id = ctx
        .domain_event
        .as_ref()
        .map(|event| event.bot.account_id.value().as_str().to_owned())
        .unwrap_or_else(|| ctx.bot_id.to_string());
    let offset = FixedOffset::east_opt(8 * 3600).expect("valid UTC+08 offset");
    let mut card = StatusCard {
        subtitle: format!(
            "账号 {} · {} (UTC+08)",
            bot_id,
            now.with_timezone(&offset).format("%Y-%m-%d %H:%M:%S")
        ),
        uptime: uptime(commands.status_started.elapsed().as_secs()),
        system: system_metrics(system),
        current: None,
        all: None,
        traffic: vec![],
        daily: vec![],
        plugins: vec![],
        notes: vec!["今日及日曲线按 UTC+08；消息历史异步落盘，近期数据可能稍有延迟。".into()],
    };
    let Some(source) = &commands.status_source else {
        card.notes.push("当前宿主未接入历史与插件统计。".into());
        return card;
    };
    let (statistics, rows) = tokio::join!(
        tokio::time::timeout(
            Duration::from_secs(3),
            get_status_stats(&source.db, &bot_id, now)
        ),
        source.host.status_rows_at(now),
    );
    match statistics {
        Ok(Ok(stats)) => apply_statistics(&mut card, stats),
        result => {
            tracing::warn!(bot_id = ctx.bot_id, error = ?result, "状态卡历史统计暂不可用");
            card.notes
                .push("历史统计暂不可用，未以 0 替代缺失数据。".into());
        }
    }
    let callbacks = source.host.callback_totals();
    card.traffic = vec![
        metric("完成回调", callbacks.completed_calls.to_string()),
        metric("正在执行", callbacks.in_flight.to_string()),
        metric(
            "平均耗时",
            format!("{:.1} ms", callbacks.average_duration_ms),
        ),
        metric("最大耗时", format!("{:.1} ms", callbacks.max_duration_ms)),
    ];
    card.plugins = rows
        .into_iter()
        .map(|row| CardPlugin {
            name: row.display_name,
            unavailable: row.availability
                != crate::core::plugin_host::PluginStatusAvailability::Available,
            status: {
                use crate::core::plugin_host::PluginStatusAvailability::*;
                match row.availability {
                    Available => None,
                    TimedOut => Some("统计超时".into()),
                    Busy => Some("统计繁忙".into()),
                    Failed => Some("统计暂不可用".into()),
                    Invalid => Some("统计格式无效".into()),
                }
            },
            metrics: row
                .metrics
                .into_iter()
                .map(|item| metric(&item.label, item.value))
                .collect(),
        })
        .collect();
    card.notes
        .push("活跃用户/群仅计收到消息的身份；全部账号按账号与身份组合去重。".into());
    card.notes
        .push("回调耗时含异步等待，为本次进程累计；插件指标为插件公开的汇总。".into());
    card
}

pub(super) fn record_command(
    commands: &CoreCommands,
    ctx: &RuntimeEventContext<'_>,
    command: &str,
) {
    let Some(source) = &commands.status_source else {
        return;
    };
    let Ok(permit) = commands.status_command_slots.clone().try_acquire_owned() else {
        tracing::warn!("核心指令统计写入队列已满");
        return;
    };
    let db = source.db.clone();
    let bot_id = ctx
        .domain_event
        .as_ref()
        .map(|event| event.bot.account_id.value().as_str().to_owned())
        .unwrap_or_else(|| ctx.bot_id.to_string());
    let user_id = ctx
        .event
        .get("user_id")
        .and_then(|value| match value {
            serde_json::Value::String(value) => Some(value.clone()),
            serde_json::Value::Number(value) => Some(value.to_string()),
            _ => None,
        })
        .unwrap_or_default();
    let command = command.to_owned();
    tokio::spawn(async move {
        let _permit = permit;
        if let Err(error) =
            nuono_storage::status_commands::record_status_command(&db, &bot_id, &user_id, &command)
                .await
        {
            tracing::warn!(%error, "核心指令统计写入失败");
        }
    });
}

fn apply_statistics(card: &mut StatusCard, stats: StatusStats) {
    card.current = Some(activity_metrics(&stats.current));
    card.all = Some(activity_metrics(&stats.all));
    card.daily = stats
        .daily
        .into_iter()
        .map(|day| CardDay {
            label: day.date.format("%m-%d").to_string(),
            current_sent: day.current_sent,
            all_sent: day.sent,
            all_users: day.active_users,
        })
        .collect();
}

fn activity_metrics(stats: &StatusScopeStats) -> Vec<CardMetric> {
    vec![
        metric("今日接收", stats.received.to_string()),
        metric("今日发送", stats.sent.to_string()),
        metric("触发命令", stats.commands.to_string()),
        metric("活跃群组", stats.active_groups.to_string()),
        metric("活跃用户", stats.active_users.to_string()),
    ]
}

fn system_metrics(system: &SystemStatus) -> Vec<CardMetric> {
    if system.timestamp_secs == 0 {
        return ["CPU 使用率", "系统内存", "进程内存 RSS"]
            .into_iter()
            .map(|label| metric(label, "采集中"))
            .collect();
    }
    vec![
        CardMetric {
            label: "CPU 使用率".into(),
            value: format!("{:.1}%", system.cpu_percent),
            percent: Some(system.cpu_percent),
        },
        CardMetric {
            label: "系统内存".into(),
            value: format!(
                "{:.1} / {:.1} GiB",
                system.used_memory_mb as f64 / 1024.0,
                system.total_memory_mb as f64 / 1024.0
            ),
            percent: (system.total_memory_mb > 0)
                .then(|| system.used_memory_mb as f32 * 100.0 / system.total_memory_mb as f32),
        },
        metric("进程内存 RSS", format!("{} MiB", system.process_memory_mb)),
    ]
}

fn metric(label: &str, value: impl Into<String>) -> CardMetric {
    CardMetric {
        label: label.into(),
        value: value.into(),
        percent: None,
    }
}

fn gib(bytes: u64) -> String {
    format!("{:.1}", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
}

fn uptime(seconds: u64) -> String {
    format!(
        "{}天 {:02}:{:02}:{:02}",
        seconds / 86400,
        seconds / 3600 % 24,
        seconds / 60 % 60,
        seconds % 60
    )
}

/// 图片渲染或发送失败时的纯文本回退渲染
pub(super) fn render(status: &SystemStatus) -> String {
    let heading = format!("Nuono 状态\n版本：{}", env!("CARGO_PKG_VERSION"));
    if status.timestamp_secs == 0 {
        return format!("{heading}\n系统指标采集中，请稍后查看。");
    }
    format!(
        "{heading}\nCPU：{:.1}%\n系统内存：{} / {} MiB\n进程内存：{} MiB",
        status.cpu_percent, status.used_memory_mb, status.total_memory_mb, status.process_memory_mb,
    )
}
