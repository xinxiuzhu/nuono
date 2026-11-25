//! 装饰器宏回路测试：impl 块形态与手写方法合并、Arc 状态注入、ConfigSchema 三件套。

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use nuono_plugin::prelude::*;
use serde::{Deserialize, Serialize};

#[derive(Default)]
struct CounterState {
    count: AtomicUsize,
}

#[derive(Default)]
struct MacroTestPlugin {
    state: Arc<CounterState>,
}

#[nuono_command(pattern = "native-bump", priority = 4, block = false)]
async fn native_bump(
    _bot: &Bot,
    _event: &Event<'_>,
    _data: MatchData,
    state: &CounterState,
) -> Result<()> {
    state.count.fetch_add(1, Ordering::SeqCst);
    Ok(())
}

#[nuono_command(pattern = "bump")]
async fn bump(_bot: &Bot, _msg: &Message, _data: MatchData, state: &CounterState) -> Result<()> {
    state.count.fetch_add(1, Ordering::SeqCst);
    Ok(())
}

/// 3 参经典签名仍然可用。
#[nuono_command(pattern = "noop")]
async fn noop(_bot: &Bot, _msg: &Message, _data: MatchData) -> Result<()> {
    Ok(())
}

/// impl 块形态：宏把 name/version/description 和收集器合并进这个块，
/// 手写的 event_filter / on_load 原样保留。
#[nuono_plugin(name = "MacroTestPlugin", state_fields = "state")]
#[async_trait]
impl Plugin for MacroTestPlugin {
    fn event_filter(&self) -> PluginEventFilter {
        PluginEventFilter::MESSAGES
    }

    async fn on_load(&mut self, _ctx: &PluginContext) -> Result<()> {
        Ok(())
    }
}

/// struct 形态（快速开始）也应继续工作，且自动收集 Arc 字段。
#[derive(Default)]
#[nuono_plugin(
    name = "StructFormPlugin",
    version = "0.2.0",
    description = "struct 形态"
)]
struct StructFormPlugin {
    counter: Arc<CounterState>,
}

#[nuono_command(pattern = "struct-bump")]
async fn struct_bump(
    _bot: &Bot,
    _msg: &Message,
    _data: MatchData,
    state: &CounterState,
) -> Result<()> {
    state.count.fetch_add(1, Ordering::SeqCst);
    Ok(())
}

#[derive(Debug, Default, Serialize, Deserialize, ConfigSchema)]
#[serde(default)]
struct DemoConfig {
    /// 城市名
    #[config(default = "北京")]
    city: String,
    /// 每日限额
    #[config(default = 10)]
    daily_limit: i64,
    /// 是否启用
    enabled: bool,
    /// 标签列表
    tags: Vec<String>,
}

#[tokio::test]
async fn impl_form_merges_collectors_and_keeps_hand_written_methods() {
    let plugin = MacroTestPlugin::default();

    // 手写方法保留
    assert_eq!(plugin.event_filter(), PluginEventFilter::MESSAGES);
    assert_eq!(plugin.name(), "MacroTestPlugin");
    assert_eq!(plugin.version(), "0.1.0");

    // 收集器合并进来，且状态经 state_fields 注入。inventory 按二进制收集，
    // 测试文件里全部装饰器都可见，因此断言"包含"而非精确计数。
    let commands = plugin.commands();
    assert!(
        commands.iter().any(|c| c.name == "bump"),
        "bump 命令应被收集"
    );

    let test = nuono_plugin_test::TestBot::new().in_group(1);
    let msg = nuono_plugin_test::group_message(1, 2, "/bump");
    let bump = commands
        .iter()
        .find(|c| c.name == "bump")
        .expect("bump 命令应存在");
    let handler = bump.message_handler.clone().expect("命令应带 handler");
    for _ in 0..3 {
        handler
            .handle(test.bot(), &msg, MatchData::new("/bump".into()))
            .await
            .unwrap();
    }
    assert_eq!(
        plugin.state.count.load(Ordering::SeqCst),
        3,
        "状态应经 TypeId 注入并在调用间保持"
    );

    let native = plugin
        .commands()
        .into_iter()
        .find(|command| command.name == "native-bump")
        .expect("native-bump 命令应被收集");
    assert_eq!(native.priority, 4);
    assert!(!native.block);
    let event_value = serde_json::json!({"post_type": "message", "user_id": "u-1"});
    native
        .handler
        .unwrap()
        .handle(test.bot(), &Event::new(&event_value), MatchData::default())
        .await
        .unwrap();
    assert_eq!(
        plugin.state.count.load(Ordering::SeqCst),
        4,
        "native handler 应复用同一份 Arc 状态"
    );
}

#[tokio::test]
async fn struct_form_auto_collects_arc_state() {
    let plugin = StructFormPlugin::default();
    assert_eq!(plugin.name(), "StructFormPlugin");

    let commands = plugin.commands();
    let target = commands
        .iter()
        .find(|c| c.name == "struct-bump")
        .expect("struct-bump 命令应被收集");
    let handler = target.message_handler.clone().expect("命令应带 handler");
    let test = nuono_plugin_test::TestBot::new().in_group(1);
    let msg = nuono_plugin_test::group_message(1, 2, "/struct-bump");
    handler
        .handle(test.bot(), &msg, MatchData::new("/struct-bump".into()))
        .await
        .unwrap();
    assert_eq!(
        plugin.counter.count.load(Ordering::SeqCst),
        1,
        "struct 形态应自动接线 Arc 字段"
    );
}

#[test]
fn config_schema_derive_provides_all_three_helpers() {
    // get_schema：字段元数据
    let schema = DemoConfig::get_schema();
    assert_eq!(schema.len(), 4);
    assert!(schema.iter().any(|f| f.key == "daily_limit"));

    // default_value：显式默认 + 类型零值
    let defaults = DemoConfig::default_value();
    assert_eq!(defaults["city"], "北京");
    assert_eq!(defaults["daily_limit"], 10);
    assert_eq!(defaults["enabled"], false);
    assert_eq!(defaults["tags"], serde_json::json!([]));

    // from_value：运行时配置反序列化（缺字段回退默认）
    let cfg = DemoConfig::from_value(&serde_json::json!({ "city": "上海" })).unwrap();
    assert_eq!(cfg.city, "上海");
    assert_eq!(cfg.daily_limit, 10, "缺省字段应回落到默认值");
}
