use nuono_plugin::TriggerRegistry;
use nuono_plugin::prelude::*;

#[nuono_command(pattern = "天气", aliases = ["weather", "查天气"], description = "查询城市天气", usage = "天气 北京", permission = "group_admin")]
async fn weather(_bot: &Bot, _event: &Event<'_>, _data: MatchData) -> Result<()> {
    Ok(())
}

#[nuono_command(pattern = "清理", aliases = ["clean"], description = "清理记录", usage = "清理", permission = "bot_admin")]
async fn cleanup(_bot: &Bot, _message: &Message, _data: MatchData) -> Result<()> {
    Ok(())
}

#[test]
fn metadata_reaches_the_registry_and_each_alias_keeps_permissions() {
    let native = nuono_plugin::__decorators::commands(env!("CARGO_PKG_NAME"));
    let command = native.iter().find(|c| c.name == "天气").unwrap();
    assert_eq!(command.aliases, ["weather", "查天气"]);
    assert_eq!(command.description, "查询城市天气");
    assert_eq!(command.usage, "天气 北京");
    let triggers = command.to_triggers();
    assert_eq!(triggers.len(), 3);
    assert!(
        triggers
            .iter()
            .all(|t| t.required_permission == Some(PermissionLevel::GroupAdmin))
    );
    let registry = TriggerRegistry::new();
    registry.register_command(command.clone());
    let projected = registry
        .get_all_commands()
        .into_iter()
        .find(|c| c.name == "天气")
        .unwrap();
    assert_eq!(projected.description, "查询城市天气");
    assert_eq!(projected.usage, "天气 北京");
    assert_eq!(projected.aliases, ["weather", "查天气"]);
    assert_eq!(projected.permission, PermissionLevel::GroupAdmin);
    let classic = nuono_plugin::__decorators::commands(env!("CARGO_PKG_NAME"));
    assert_eq!(
        classic
            .iter()
            .find(|c| c.name == "清理")
            .unwrap()
            .permission,
        PermissionLevel::BotAdmin
    );
    CleanupHandler::register(&registry);
    let commands = registry.get_all_commands();
    let c = commands.iter().find(|c| c.name == "清理").unwrap();
    assert_eq!(c.aliases, ["clean"]);
    assert_eq!(c.permission, PermissionLevel::BotAdmin);
    assert_eq!(c.description, "清理记录");
    assert_eq!(c.usage, "清理");
    // 按 "command" 类型过滤时，必须同时包含基于 Event 和基于 OneBot Message 的指令处理器（含别名），
    // 且不应向外部暴露插件底层的实现分类差异。
    let filtered = registry.get_triggers_by_type("command");
    assert_eq!(filtered.len(), 5);
    assert!(filtered.iter().any(|trigger| trigger.pattern == "查天气"));
    assert!(filtered.iter().any(|trigger| trigger.pattern == "clean"));
}
