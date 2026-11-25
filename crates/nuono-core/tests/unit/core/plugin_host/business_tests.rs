use super::*;
use crate::plugin::{LoadedPlugin, Plugin};
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Default)]
struct Calls {
    loaded: AtomicUsize,
    enabled: AtomicUsize,
    background: AtomicUsize,
}

struct Probe {
    name: String,
    calls: Arc<Calls>,
    scope: Option<Arc<PluginTaskScope>>,
    fail_enable: bool,
    enable_wait: Option<(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>)>,
}

impl Probe {
    fn new(name: &str, calls: Arc<Calls>) -> Self {
        Self {
            name: name.into(),
            calls,
            scope: None,
            fail_enable: false,
            enable_wait: None,
        }
    }

    fn loaded(self) -> LoadedPlugin {
        LoadedPlugin {
            plugin: Box::new(self),
        }
    }
}

#[async_trait::async_trait]
impl Plugin for Probe {
    fn name(&self) -> &str {
        &self.name
    }
    fn version(&self) -> &str {
        "1.0.0"
    }
    fn description(&self) -> &str {
        "business start probe"
    }

    async fn on_load(&mut self, ctx: &PluginContext) -> anyhow::Result<()> {
        self.calls.loaded.fetch_add(1, Ordering::SeqCst);
        self.scope = Some(ctx.tasks().clone());
        let calls = self.calls.clone();
        ctx.tasks().spawn("loaded-worker", async move {
            calls.background.fetch_add(1, Ordering::SeqCst);
        });
        Ok(())
    }

    async fn on_enable(&mut self) -> anyhow::Result<()> {
        self.calls.enabled.fetch_add(1, Ordering::SeqCst);
        let calls = self.calls.clone();
        self.scope
            .as_ref()
            .expect("probe was loaded")
            .spawn("enabled-worker", async move {
                calls.background.fetch_add(1, Ordering::SeqCst);
            });
        if let Some((entered, release)) = &self.enable_wait {
            entered.notify_one();
            release.notified().await;
        }
        anyhow::ensure!(!self.fail_enable, "probe external service unavailable");
        Ok(())
    }
}

async fn host() -> (Arc<PluginHost>, Database) {
    let db = crate::test_support::test_db(2).await;
    let root = std::env::temp_dir().join(format!("nuono-business-{}", uuid::Uuid::new_v4()));
    let host = PluginHost::new_prepared_with_locales_and_image_output(
        db.clone(),
        Arc::new(EventBus::new()),
        Arc::new(PermissionManager::new(db.clone())),
        crate::core::Paths::with_root(&root),
        Arc::new(WaitManager::new()),
        FrameworkControl::new(),
        Arc::new(crate::config::PluginConfigManager::new(root)),
        nuono_app::localization::LocaleService::new(Arc::new(db.clone())),
        nuono_plugin::image_output::PluginImageOutput::default(),
    )
    .await
    .unwrap();
    (host, db)
}

async fn wait_for_background(calls: &Calls, expected: usize) {
    tokio::time::timeout(Duration::from_secs(2), async {
        while calls.background.load(Ordering::SeqCst) != expected {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("released background tasks should run");
}

#[tokio::test]
async fn prepared_plugins_publish_calls_and_tasks_only_after_business_start() {
    let (host, _) = host().await;
    let calls = Arc::new(Calls::default());
    host.init_with_plugins(vec![Probe::new("prepared", calls.clone()).loaded()])
        .await
        .unwrap();
    tokio::task::yield_now().await;
    assert_eq!(calls.loaded.load(Ordering::SeqCst), 1);
    assert_eq!(calls.enabled.load(Ordering::SeqCst), 0);
    assert_eq!(calls.background.load(Ordering::SeqCst), 0);
    let invocation = host.hosted_plugin("prepared").await.unwrap().invocation();
    assert!(
        invocation.read().await.is_none(),
        "prepared plugin must reject calls"
    );
    assert!(!host.business_is_running());

    host.start_business().await.unwrap();
    assert!(host.business_is_running());
    assert!(
        invocation.read().await.is_some(),
        "published plugin accepts calls"
    );
    wait_for_background(&calls, 2).await;
    host.start_business().await.unwrap();
    assert_eq!(
        calls.enabled.load(Ordering::SeqCst),
        1,
        "start is idempotent"
    );
    host.shutdown().await.unwrap();
    assert!(host.start_business().await.is_err(), "shutdown is terminal");
}

#[tokio::test]
async fn disabling_a_prepared_plugin_discards_queued_tasks_and_reenable_uses_new_scope() {
    let (host, _) = host().await;
    let calls = Arc::new(Calls::default());
    host.init_with_plugins(vec![
        Probe::new("disabled-before-start", calls.clone()).loaded(),
    ])
    .await
    .unwrap();
    host.toggle("disabled-before-start", false).await.unwrap();
    host.start_business().await.unwrap();
    assert_eq!(calls.enabled.load(Ordering::SeqCst), 0);
    assert_eq!(calls.background.load(Ordering::SeqCst), 0);
    host.toggle("disabled-before-start", true).await.unwrap();
    wait_for_background(&calls, 2).await;
    assert_eq!(calls.loaded.load(Ordering::SeqCst), 2);
    assert_eq!(calls.enabled.load(Ordering::SeqCst), 1);
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn one_plugin_enable_failure_does_not_block_other_plugins_or_release_failed_tasks() {
    let (host, db) = host().await;
    let failed = Arc::new(Calls::default());
    let healthy = Arc::new(Calls::default());
    let mut failing = Probe::new("external-unavailable", failed.clone());
    failing.fail_enable = true;
    host.init_with_plugins(vec![
        failing.loaded(),
        Probe::new("healthy", healthy.clone()).loaded(),
    ])
    .await
    .unwrap();
    host.start_business().await.unwrap();
    wait_for_background(&healthy, 2).await;
    assert_eq!(failed.enabled.load(Ordering::SeqCst), 1);
    assert_eq!(failed.background.load(Ordering::SeqCst), 0);
    assert!(!db.is_plugin_enabled("external-unavailable").await.unwrap());
    assert!(host.business_is_running());
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn rebuilding_prepared_plugin_only_starts_replacement() {
    let (host, _) = host().await;
    let old = Arc::new(Calls::default());
    let replacement = Arc::new(Calls::default());
    host.init_with_plugins(vec![
        Probe::new("replace-before-start", old.clone()).loaded(),
    ])
    .await
    .unwrap();
    host.reload_with_candidate(
        "replace-before-start",
        Probe::new("replace-before-start", replacement.clone()).loaded(),
        true,
    )
    .await
    .unwrap();
    host.start_business().await.unwrap();
    wait_for_background(&replacement, 2).await;
    assert_eq!(old.enabled.load(Ordering::SeqCst), 0);
    assert_eq!(old.background.load(Ordering::SeqCst), 0);
    assert_eq!(replacement.enabled.load(Ordering::SeqCst), 1);
    host.shutdown().await.unwrap();
}

#[tokio::test]
async fn shutdown_requested_during_enable_prevents_task_and_call_publication() {
    let (host, _) = host().await;
    let calls = Arc::new(Calls::default());
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let mut probe = Probe::new("shutdown-during-start", calls.clone());
    probe.enable_wait = Some((entered.clone(), release.clone()));
    host.init_with_plugins(vec![probe.loaded()]).await.unwrap();
    let runner = host.clone();
    let start = tokio::spawn(async move { runner.start_business().await });
    entered.notified().await;
    host.request_business_shutdown();
    release.notify_one();
    assert!(start.await.unwrap().is_err());
    assert_eq!(calls.background.load(Ordering::SeqCst), 0);
    assert!(
        host.hosted_plugin("shutdown-during-start")
            .await
            .unwrap()
            .invocation()
            .read()
            .await
            .is_none()
    );
    host.shutdown().await.unwrap();
}
