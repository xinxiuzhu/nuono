use nuono_plugin::prelude::*;
use nuono_plugin_test::{TestBot, group_event, group_message};
use std::sync::Arc;

#[derive(Default)]
struct State;
#[derive(Default)]
#[nuono_plugin(name = "HelperPlugin")]
struct HelperPlugin {
    state: Arc<State>,
}

#[nuono_command(pattern = "helper-classic")]
async fn classic(bot: &Bot, _message: &Message, _data: MatchData, _state: &State) -> Result<()> {
    bot.send(MessageBuilder::new().text("first").at(1).text("second"))
        .await?;
    Ok(())
}
#[nuono_command(pattern = "helper-native")]
async fn native(bot: &Bot, event: &Event<'_>, _data: MatchData, _state: &State) -> Result<()> {
    bot.send(event.raw()["user_id"].as_str().unwrap_or("numeric"))
        .await?;
    Ok(())
}

#[tokio::test]
async fn helpers_use_registered_handlers_and_keep_string_identity() {
    let test = TestBot::new().in_group(123);
    let plugin = HelperPlugin::default();
    test.call_command(
        &plugin,
        "helper-classic",
        &group_message(123, 456, "helper-classic"),
    )
    .await
    .unwrap();
    let mut event = group_event(123, 456, "helper-native");
    event["user_id"] = nuono_plugin::serde_json::json!("18446744073709551615");
    test.call_command(&plugin, "helper-native", &event)
        .await
        .unwrap();
    test.assert_sent_texts(&["firstsecond", "18446744073709551615"]);
    test.clear();
    assert!(test.call_command(&plugin, "missing", &event).await.is_err());
    assert!(
        test.call_command(&plugin, "missing", &group_message(123, 456, "missing"))
            .await
            .is_err()
    );
    test.assert_no_actions();
}

#[nuono_command(pattern = "helper-scoped")]
async fn scoped(bot: &Bot, event: &Event<'_>, data: MatchData, _state: &State) -> Result<()> {
    let actor = event.domain_event().unwrap().actor.as_ref().unwrap();
    bot.send(format!(
        "{}:{}:{}",
        actor.namespace().unwrap(),
        actor.value(),
        data.args
    ))
    .await?;
    Ok(())
}

#[tokio::test]
async fn explicit_call_keeps_domain_namespace_and_matching_arguments() {
    use nuono_plugin::domain::{BotIdentity, IdentityKind, IdentityRef, IdentityScheme, Platform};
    let identity = |kind, value| {
        IdentityRef::new(
            Platform::Feishu,
            kind,
            IdentityScheme::OpenId,
            value,
            Some("test-app"),
        )
        .unwrap()
    };
    let bot = BotIdentity::new("test-instance", identity(IdentityKind::Bot, "bot"), None).unwrap();
    let domain = nuono_plugin_test::native_private_event(
        bot,
        identity(IdentityKind::User, "opaque-user"),
        "helper-scoped arbitrary raw text",
    )
    .unwrap();
    let event = Event::with_domain(&domain.raw, Some(&domain));
    assert_eq!(
        event.message_target().unwrap().route.instance_id,
        "test-instance"
    );
    let test = TestBot::new().in_private(1);
    test.call_command_with(
        &HelperPlugin::default(),
        "helper-scoped",
        &event,
        MatchData::default().with_args("explicit args".into()),
    )
    .await
    .unwrap();
    test.assert_sent_texts(&["test-app:opaque-user:explicit args"]);
}

#[test]
fn native_event_rejects_mismatched_platforms() {
    use nuono_plugin::domain::{BotIdentity, IdentityKind, IdentityRef, IdentityScheme, Platform};
    let bot = BotIdentity::new(
        "test",
        IdentityRef::new(
            Platform::OneBot,
            IdentityKind::Bot,
            IdentityScheme::Numeric,
            "1",
            None::<String>,
        )
        .unwrap(),
        None,
    )
    .unwrap();
    let user = IdentityRef::new(
        Platform::Telegram,
        IdentityKind::User,
        IdentityScheme::Numeric,
        "1",
        None::<String>,
    )
    .unwrap();
    assert!(nuono_plugin_test::native_private_event(bot, user, "hello").is_err());
}
