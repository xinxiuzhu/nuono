use nuono_plugin::prelude::*;
use nuono_plugin::serde_json::json;
use serde::Deserialize;
use std::{path::PathBuf, sync::Arc};

#[derive(Debug, Deserialize, ConfigSchema)]
struct Settings {
    #[config(default = "pong")]
    greeting: String,
    #[config(default = 3)]
    limit: i64,
}

#[test]
fn typed_defaults_partial_overrides_and_rejected_binding() {
    let live = LiveConfig::<Settings>::default();
    assert_eq!(live.get().unwrap().greeting, "pong");
    live.bind_handle(PluginConfigHandle::new(json!({"greeting":"hello"})))
        .unwrap();
    assert_eq!(live.get().unwrap().limit, 3);
    assert_eq!(live.get().unwrap().greeting, "hello");
    assert!(
        live.bind_handle(PluginConfigHandle::new(json!({"greeting":123})))
            .is_err()
    );
    assert_eq!(
        live.get().unwrap().greeting,
        "hello",
        "failed binding keeps the previous source"
    );
    for invalid in [json!(null), json!([]), json!("invalid")] {
        assert!(Settings::from_value(&invalid).is_err());
    }
}

struct TempConfig(PathBuf);
impl Drop for TempConfig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
async fn already_registered_handler_reads_file_updates_without_rebinding() {
    let root = TempConfig(std::env::temp_dir().join(format!(
            "nuono-live-config-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )));
    let manager = nuono_core::config::PluginConfigManager::new(root.0.clone());
    manager
        .register(
            "live-test",
            nuono_core::config::plugin_config::plugin_fields_to_schema(&Settings::get_schema())
                .unwrap(),
        )
        .await
        .unwrap();
    let live = Arc::new(LiveConfig::<Settings>::default());
    live.bind_handle(manager.config_handle("live-test").await.unwrap())
        .unwrap();
    let plugin = Sample {
        config: live.clone(),
    };
    let command = plugin
        .commands()
        .into_iter()
        .find(|c| c.name == "live-ping")
        .unwrap();
    let test = nuono_plugin_test::TestBot::new().in_group(123);
    let value = nuono_plugin_test::group_event(123, 456, "live-ping");
    let event = Event::new(&value);
    command
        .handler
        .as_ref()
        .unwrap()
        .handle(test.bot(), &event, MatchData::default())
        .await
        .unwrap();
    std::fs::write(
        root.0.join("live-test/config.toml"),
        "greeting = \"updated\"\nlimit = 5\n",
    )
    .unwrap();
    manager.refresh("live-test").await.unwrap();
    command
        .handler
        .as_ref()
        .unwrap()
        .handle(test.bot(), &event, MatchData::default())
        .await
        .unwrap();
    test.assert_sent_texts(&["pong", "updated"]);
    assert_eq!(live.get().unwrap().limit, 5);
    std::fs::write(root.0.join("live-test/config.toml"), "greeting = [broken").unwrap();
    assert!(manager.refresh("live-test").await.is_err());
    assert_eq!(
        live.get().unwrap().greeting,
        "updated",
        "invalid edits retain the valid snapshot"
    );
}

struct Sample {
    config: Arc<LiveConfig<Settings>>,
}
#[nuono_plugin(name = "Sample", state_fields = "config")]
impl Plugin for Sample {}

#[nuono_command(pattern = "live-ping")]
async fn live_ping(
    bot: &Bot,
    _event: &Event<'_>,
    _data: MatchData,
    state: &LiveConfig<Settings>,
) -> Result<()> {
    let settings = state.snapshot()?;
    bot.send(settings.greeting.as_str()).await?;
    Ok(())
}
