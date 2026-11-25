use nuono_plugin::prelude::*;

#[derive(Default)]
#[nuono_plugin(
    name = "AutoPlugin",
    version = "0.1.0",
    description = "auto macro test"
)]
struct AutoPlugin;

#[nuono_command(
    pattern = "ping",
    display_name = "连通测试",
    sv = "诊断",
    priority = 7,
    block = false,
    to_me = true
)]
async fn ping(bot: &Bot, _event: &Message, _data: MatchData) -> Result<()> {
    bot.send("pong").await?;
    Ok(())
}

#[nuono_keyword(pattern = "hello", sv = "问候", priority = 3)]
async fn hello(bot: &Bot, _event: &Message, _data: MatchData) -> Result<()> {
    bot.send("world").await?;
    Ok(())
}

#[nuono_command(
    pattern = "native-ping",
    display_name = "原生连通测试",
    sv = "原生诊断",
    priority = 5,
    block = false,
    to_me = true
)]
async fn native_ping(_bot: &Bot, _event: &Event<'_>, _data: MatchData) -> Result<()> {
    Ok(())
}

#[test]
fn decorated_plugin_collects_commands_and_triggers() {
    let plugin = AutoPlugin;

    let commands = plugin.commands();
    assert_eq!(commands.len(), 2);
    let command = commands.iter().find(|c| c.name == "ping").unwrap();
    assert_eq!(command.name, "ping");
    assert_eq!(command.display_name.as_deref(), Some("连通测试"));
    assert_eq!(command.sv_name.as_deref(), Some("诊断"));
    assert_eq!(command.priority, 7);
    assert!(!command.block);
    assert!(command.to_me);

    let triggers = plugin.triggers();
    assert_eq!(triggers.len(), 1);
    assert_eq!(triggers[0].pattern, "hello");
    assert_eq!(triggers[0].sv_name.as_deref(), Some("问候"));
    assert_eq!(triggers[0].priority, 3);
    assert_eq!(triggers[0].trigger_type, TriggerType::Keyword);

    let command = commands.iter().find(|c| c.name == "native-ping").unwrap();
    assert_eq!(command.name, "native-ping");
    assert_eq!(command.display_name.as_deref(), Some("原生连通测试"));
    assert_eq!(command.sv_name.as_deref(), Some("原生诊断"));
    assert_eq!(command.priority, 5);
    assert!(!command.block);
    assert!(command.to_me);
}

#[test]
fn manual_macro_registration_keeps_group_and_display_metadata() {
    let registry = nuono_plugin::TriggerRegistry::new();
    PingHandler::register(&registry);
    HelloHandler::register(&registry);
    let commands = registry.get_all_commands();
    assert_eq!(commands[0].name, "ping");
    assert_eq!(commands[0].display_name.as_deref(), Some("连通测试"));
    assert_eq!(commands[0].sv_name.as_deref(), Some("诊断"));
    let triggers = registry.get_all_triggers();
    assert!(triggers.iter().any(|trigger|trigger.pattern == "hello" && trigger.sv_name.as_deref()==Some("问候")));
}
