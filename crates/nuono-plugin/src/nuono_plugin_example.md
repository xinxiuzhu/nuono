# Examples

`struct` 形态生成完整的 `Plugin` 实现；需要自定义生命周期或事件过滤时，选择
`impl` 形态。同一个插件只选择其中一种形态，不能重复实现 `Plugin`。
下面用两个不同的插件演示，并验证自动收集的命令和保留的事件过滤规则。

```rust,standalone_crate
use nuono_plugin::prelude::*;
use std::sync::Arc;

#[derive(Default)]
struct MyState {
    greeting: String,
}

// struct 形态自动收集 Arc 字段，供命令注入状态。
#[derive(Default)]
#[nuono_plugin(name = "QuickPlugin", version = "1.0.0", description = "快速开始")]
struct QuickPlugin {
    state: Arc<MyState>,
}

// impl 形态通过 state_fields 指明状态字段，保留手写的方法。
#[derive(Default)]
struct CustomPlugin {
    state: Arc<MyState>,
}

#[nuono_plugin(name = "CustomPlugin", version = "1.0.0", state_fields = "state")]
impl Plugin for CustomPlugin {
    fn event_filter(&self) -> PluginEventFilter {
        PluginEventFilter::MESSAGES
    }

    async fn on_load(&mut self, _ctx: &PluginContext) -> Result<()> {
        Ok(())
    }
}

#[nuono_command(pattern = "ping")]
async fn ping(bot: &Bot, _event: &Event<'_>, _data: MatchData, state: &MyState) -> Result<()> {
    bot.send(format!("pong! {}", state.greeting)).await?;
    Ok(())
}

let quick = QuickPlugin::default();
assert_eq!(quick.name(), "QuickPlugin");
assert_eq!(quick.version(), "1.0.0");
assert_eq!(quick.commands()[0].name, "ping");
assert_eq!(quick.event_filter(), PluginEventFilter::ALL);

let custom = CustomPlugin::default();
assert_eq!(custom.name(), "CustomPlugin");
assert_eq!(custom.commands()[0].name, "ping");
assert!(custom.event_filter().accepts("message"));
assert!(!custom.event_filter().accepts("notice"));
```
