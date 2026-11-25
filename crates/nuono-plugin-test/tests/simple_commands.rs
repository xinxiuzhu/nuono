use nuono_plugin::prelude::*;
use nuono_plugin_test::{TestBot, group_event};

#[derive(Default)]
#[nuono_plugin(name = "simple-commands")]
struct Hello;

#[nuono_command(pattern = "问候")]
async fn greeting(bot: &Bot) -> NuonoResult<()> {
    bot.send("你好呀！").await
}

#[nuono_command(pattern = "回声")]
async fn echo(bot: &Bot, data: MatchData) -> NuonoResult<()> {
    bot.send(data.args).await
}

#[nuono_command(pattern = "事件")]
async fn event_reply(bot: &Bot, event: &Event<'_>) -> Result<()> {
    bot.send(event.text()).await?;
    Ok(())
}

#[tokio::test]
async fn short_handlers_are_registered_and_receive_only_the_requested_arguments() {
    let test = TestBot::new().in_group(123);
    let raw = group_event(123, 456, "事件");
    test.call_command(&Hello, "问候", &raw).await.unwrap();
    EchoHandler
        .handle(
            test.bot(),
            &Event::new(&raw),
            MatchData::new("回声".into()).with_args("小明".into()),
        )
        .await
        .unwrap();
    test.call_command(&Hello, "事件", &raw).await.unwrap();
    test.assert_sent_texts(&["你好呀！", "小明", "事件"]);
}
