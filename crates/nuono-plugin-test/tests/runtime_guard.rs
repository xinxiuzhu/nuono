//! 验证 TestBot 测试在测试自身的 Tokio 运行时上执行本地 SDK 回调。
use nuono_plugin::{Event, Plugin, PluginResponse};
#[derive(Default)]
struct RuntimeProbe;
#[nuono_plugin::async_trait]
impl Plugin for RuntimeProbe {
    fn name(&self) -> &str {
        "runtime-probe"
    }
    fn version(&self) -> &str {
        "0.1.0"
    }
    fn description(&self) -> &str {
        "host runtime probe"
    }
    async fn on_event(&self, _: &Event) -> nuono_plugin::Result<PluginResponse> {
        tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_millis(1)).await;
        })
        .await?;
        Ok(PluginResponse::Continue)
    }
}
#[tokio::test]
async fn local_callback_uses_test_runtime() {
    let plugin = RuntimeProbe;
    assert_eq!(plugin.name(), "runtime-probe");
    let message = nuono_plugin_test::group_event(123, 456, "probe");
    assert!(matches!(
        plugin.on_event(&Event::new(&message)).await.unwrap(),
        PluginResponse::Continue
    ));
}
