//! 编译进宿主程序的 Rust 插件接口约定与工厂类型重导出。
pub use nuono_plugin::plugin::*;
pub mod source_loader;
pub struct LoadedPlugin {
    pub plugin: Box<dyn Plugin>,
}

impl std::ops::Deref for LoadedPlugin {
    type Target = Box<dyn Plugin>;
    fn deref(&self) -> &Self::Target {
        &self.plugin
    }
}

impl std::ops::DerefMut for LoadedPlugin {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.plugin
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn event_filter_combines_and_matches_post_types() {
        let all = PluginEventFilter::ALL;
        assert!(all.accepts("message") && all.accepts("notice") && all.accepts("unknown"));

        let messages_only = PluginEventFilter::MESSAGES;
        assert!(messages_only.accepts("message"));
        assert!(!messages_only.accepts("notice"));

        let combined = PluginEventFilter::MESSAGES | PluginEventFilter::NOTICES;
        assert!(combined.accepts("message") && combined.accepts("notice"));
        assert!(!combined.accepts("request"));

        // 非 ALL 的过滤器不接收未知 post_type；NONE 不接收任何事件。
        assert!(!combined.accepts("something_new"));
        assert!(!PluginEventFilter::NONE.accepts("message"));
    }

    #[tokio::test]
    async fn task_scope_shutdown_aborts_and_awaits_tasks() {
        let scope = PluginTaskScope::new("TestPlugin");
        let entered = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        for _ in 0..3 {
            let entered = entered.clone();
            scope.spawn("blocked", async move {
                entered.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                // 永不主动结束：只能靠 shutdown 的 abort 收束。
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
                }
            });
        }

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert_eq!(
            entered.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "三个任务都应已进入运行态"
        );

        // abort + await 都要发生： shutdown 能在测试时限内返回即证明任务被收束。
        scope.shutdown().await;
        // 可重复调用。
        scope.shutdown().await;
    }

    #[tokio::test]
    async fn task_scope_catches_task_panic() {
        let scope = PluginTaskScope::new("TestPlugin");
        scope.spawn("panicky", async {
            panic!("boom");
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        // 宿主侧 catch_unwind：任务 panic 不外溢，shutdown 正常返回。
        scope.shutdown().await;
    }

    #[test]
    fn plugin_secret_canonical_key_namespaces_and_validates() {
        assert_eq!(
            plugin_secret_canonical_key("weather", "captcha_api_key").unwrap(),
            "plugin.weather.captcha_api_key"
        );
        // 空值与危险字符必须拒绝，绝不能被静默改名造成碰撞。
        assert!(plugin_secret_canonical_key("", "k").is_err());
        assert!(plugin_secret_canonical_key("weather", "").is_err());
        assert!(plugin_secret_canonical_key("bad name", "k").is_err());
        assert!(plugin_secret_canonical_key("bad..name", "k").is_err());
        assert!(plugin_secret_canonical_key(".weather", "k").is_err());
        assert!(plugin_secret_canonical_key("weather", "bad:key").is_err());
        // 允许的字符：ASCII 字母数字、`-`、`_`；`.` 被排除保证 `plugin.<name>.<key>` 无歧义。
        assert_eq!(
            plugin_secret_canonical_key("my-plugin", "api_key_1").unwrap(),
            "plugin.my-plugin.api_key_1"
        );
    }

    #[tokio::test]
    async fn private_secret_store_roundtrip_is_namespaced_but_not_schema_declared() {
        let db = crate::test_support::test_db(2).await;
        db.sync_plugin("DemoPlugin", "1", &serde_json::json!({}))
            .await
            .unwrap();
        let store = PluginSecretStore::new(db.clone(), "DemoPlugin", vec![]);

        store
            .set_private("account_42_token", "private-token")
            .await
            .unwrap();
        assert_eq!(
            store
                .get_private("account_42_token")
                .await
                .unwrap()
                .as_deref(),
            Some("private-token")
        );
        assert_eq!(
            db.get_secret("plugin.DemoPlugin.account_42_token")
                .await
                .unwrap()
                .as_deref(),
            Some("private-token")
        );
        assert!(store.set_private("bad.key", "value").await.is_err());

        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn secret_store_roundtrip_and_undeclared_access_denied() {
        let db = crate::test_support::test_db(2).await;
        db.sync_plugin("weather", "1", &serde_json::json!({}))
            .await
            .unwrap();
        let store = PluginSecretStore::new(
            db.clone(),
            "weather",
            vec![PluginSecretField::new(
                "captcha_api_key",
                "验证码 API Key",
                "用于验证码服务",
            )],
        );

        // 未声明 key 的读写删必须报错——store 只能访问本插件已声明字段。
        assert!(store.get("nope").await.is_err());
        assert!(store.set("nope", "v").await.is_err());
        assert!(store.delete("nope").await.is_err());

        // 已声明字段默认不存在。
        assert_eq!(store.get("captcha_api_key").await.unwrap(), None);

        // set -> get 走 `plugin.<name>.<key>` canonical key，落库到 runtime_secrets。
        store.set("captcha_api_key", "sk-abc").await.unwrap();
        assert_eq!(
            store.get("captcha_api_key").await.unwrap().as_deref(),
            Some("sk-abc")
        );
        assert_eq!(
            db.get_secret("plugin.weather.captcha_api_key")
                .await
                .unwrap()
                .as_deref(),
            Some("sk-abc")
        );

        // 空值等价删除，避免残留空串被当作已配置。
        store.set("captcha_api_key", "  ").await.unwrap();
        assert_eq!(store.get("captcha_api_key").await.unwrap(), None);

        store.set("captcha_api_key", "sk-2").await.unwrap();
        store.delete("captcha_api_key").await.unwrap();
        assert_eq!(store.get("captcha_api_key").await.unwrap(), None);

        db.shutdown().await.unwrap();
    }
}
