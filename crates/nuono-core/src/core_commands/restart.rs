use super::{RestartReplyTarget, RestartService, localized, record_outcome};
use crate::core::event_kernel::RuntimeEventContext;
use crate::error::{NuonoError, Result};

pub(super) async fn handle(
    service: Option<&dyn RestartService>,
    ctx: &RuntimeEventContext<'_>,
) -> Result<()> {
    let locale = ctx.locale.locale;
    let Some(service) = service else {
        record_outcome(ctx, "restart", "unavailable");
        ctx.send_reply_text(
            localized(
                locale,
                "当前运行方式暂不支持重启。",
                "Restart is not supported in the current runtime mode.",
                "現在の実行方式では再起動できません。",
                "현재 실행 모드에서는 재시작을 지원하지 않습니다.",
            ),
            "CoreCommands",
        )
        .await;
        return Ok(());
    };
    let target = reply_target(ctx);
    if target.is_none() {
        tracing::warn!(target: "core_commands", "当前事件缺少可信原生会话身份，重启后无法回传完成通知");
    }
    match service
        .prepare_restart("core command: restart", target)
        .await
    {
        Ok(prepared) => {
            // 消息发送通道故障不能无限期卡住重启维护流程；
            // 若在 commit 前该处理器被取消，PreparedRestart 析构会自动释放重启预约。
            let reply = tokio::time::timeout(
                std::time::Duration::from_secs(5),
                ctx.send_reply_text(
                    localized(
                        locale,
                        "已安排 Nuono 重启。",
                        "Nuono restart has been scheduled.",
                        "Nuono の再起動を予約しました。",
                        "Nuono 재시작이 예약되었습니다.",
                    ),
                    "CoreCommands",
                ),
            )
            .await;
            if !matches!(reply, Ok(true)) {
                tracing::warn!(target: "core_commands", "重启回执未送达，继续执行已接受的重启请求");
            }
            if let Err(error) = prepared.commit().await {
                record_outcome(ctx, "restart", "failed");
                ctx.send_reply_text(
                    localized(
                        locale,
                        &format!("重启未执行：{error}"),
                        &format!("Restart aborted: {error}"),
                        &format!("再起動は実行されませんでした：{error}"),
                        &format!("재시작이 실행되지 않았습니다: {error}"),
                    ),
                    "CoreCommands",
                )
                .await;
                return Err(NuonoError::Internal(error));
            }
            record_outcome(ctx, "restart", "scheduled");
            Ok(())
        }
        Err(error) => {
            record_outcome(ctx, "restart", "failed");
            ctx.send_reply_text(
                localized(
                    locale,
                    &format!("无法重启 Nuono：{error}"),
                    &format!("Unable to restart Nuono: {error}"),
                    &format!("Nuono を再起動できません：{error}"),
                    &format!("Nuono를 재시작할 수 없습니다: {error}"),
                ),
                "CoreCommands",
            )
            .await;
            Err(NuonoError::Internal(error))
        }
    }
}

fn reply_target(ctx: &RuntimeEventContext<'_>) -> Option<RestartReplyTarget> {
    let event = nuono_plugin::Event::with_domain(&ctx.event, ctx.domain_event.as_ref());
    let domain = event.native_domain_event().ok()??;
    let route = ctx
        .event
        .get("route")
        .and_then(|value| serde_json::from_value(value.clone()).ok())
        .or_else(|| {
            Some(crate::core::RuntimeBotRoute::new(
                ctx.action_transport.as_ref()?.protocol().family.as_str(),
                domain.bot.instance_id.as_str(),
                domain.bot.account_id.value().as_str(),
            ))
        })?;
    if nuono_plugin::core::runtime_transport::route_identity(&route).ok()? != domain.bot {
        return None;
    }
    Some(RestartReplyTarget {
        route,
        bot: domain.bot.clone(),
        conversation: domain.conversation.clone()?,
    })
}
