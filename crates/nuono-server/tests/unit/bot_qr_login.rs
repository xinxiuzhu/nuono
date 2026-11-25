use super::*;
use async_trait::async_trait;
use nuono_adapter::{
    AdapterHost, AdapterInstanceConfig, AdapterRuntime, BotIdentity, ConfigField, DomainEvent,
    DomainModelError, UnifiedEvent,
};
use std::sync::atomic::AtomicUsize;

struct Provider(Arc<AtomicUsize>);
pub(crate) fn provider() -> Arc<dyn AdapterProvider> {
    Arc::new(Provider(Arc::new(AtomicUsize::new(0))))
}

#[async_trait]
impl AdapterProvider for Provider {
    fn descriptor(&self) -> AdapterDescriptor {
        AdapterDescriptor {
            id: "qr_fixture".into(),
            platform: "qr_fixture".into(),
            display_name: "QR Fixture".into(),
            description: String::new(),
            supports_multiple_instances: true,
            fields: vec![
                ConfigField::text("account", "Account", "", "").required(),
                ConfigField::password("token", "Token", "").required(),
            ],
            secret_fields: vec!["token".into()],
        }
    }
    fn qr_login_fields(&self) -> Vec<String> {
        vec!["account".into(), "token".into()]
    }
    async fn start_qr_login(&self) -> Result<Box<dyn QrLoginSession>> {
        Ok(Box::new(Session(self.0.clone())))
    }
    fn normalize(
        &self,
        _: &BotIdentity,
        _: &UnifiedEvent,
        _: chrono::DateTime<chrono::Utc>,
    ) -> std::result::Result<DomainEvent, DomainModelError> {
        unreachable!("QR fixture does not normalize events")
    }
    async fn create(
        &self,
        _: AdapterInstanceConfig,
        _: Arc<dyn AdapterHost>,
    ) -> Result<Arc<dyn AdapterRuntime>> {
        unreachable!("QR fixture is saved disabled")
    }
}
struct Session(Arc<AtomicUsize>);
#[async_trait]
impl QrLoginSession for Session {
    fn qr_url(&self) -> &str {
        "https://example.invalid/authorize?task=fixture"
    }
    async fn poll(&mut self) -> Result<QrLoginStatus> {
        self.0.fetch_add(1, Ordering::Relaxed);
        Ok(QrLoginStatus::Complete(
            json!({"account":"fixture-account", "token":"synthetic-qr-secret"}),
        ))
    }
    fn profile(&self) -> BotProfile {
        BotProfile {
            name: Some("  小小  ".into()),
            avatar_url: Some("https://thirdqq.qlogo.cn/g?b=oidb&k=demo".into()),
        }
    }
}

#[tokio::test]
async fn qr_credentials_stay_server_side_and_sessions_are_scoped_and_cancellable() {
    let sessions = QrLogins::default();
    let polls = Arc::new(AtomicUsize::new(0));
    let start = sessions
        .start(7, Arc::new(Provider(polls.clone())))
        .await
        .unwrap();
    assert!(start.qr_image.starts_with("data:image/svg+xml;base64,"));
    assert!(sessions.poll(8, &start.id).await.is_err());
    assert!(sessions.cancel(8, &start.id).await.is_err());
    assert_eq!(polls.load(Ordering::Relaxed), 0);
    let ready = sessions.poll(7, &start.id).await.unwrap();
    assert_eq!(ready.status, "ready");
    assert_eq!(ready.config, json!({"account":"fixture-account"}));
    assert_eq!(ready.bot_name.as_deref(), Some("小小"));
    assert_eq!(
        ready.avatar_url.as_deref(),
        Some("https://thirdqq.qlogo.cn/g?b=oidb&k=demo")
    );
    assert!(
        !serde_json::to_string(&ready)
            .unwrap()
            .contains("synthetic-qr-secret")
    );
    sessions.poll(7, &start.id).await.unwrap();
    assert_eq!(
        polls.load(Ordering::Relaxed),
        1,
        "Ready credentials are not fetched again"
    );
    sessions.cancel(7, &start.id).await.unwrap();
    assert!(sessions.poll(7, &start.id).await.is_err());
}

#[tokio::test(start_paused = true)]
async fn qr_limits_release_on_expiry_and_shutdown() {
    let sessions = QrLogins::default();
    for _ in 0..MAX_USER_SESSIONS {
        sessions.start(7, provider()).await.unwrap();
    }
    assert!(sessions.start(7, provider()).await.is_err());
    let other = sessions.start(8, provider()).await.unwrap();
    tokio::task::yield_now().await;
    tokio::time::advance(SESSION_LIFETIME + Duration::from_secs(1)).await;
    tokio::task::yield_now().await;
    assert!(sessions.entry(8, &other.id).is_err());
    assert!(
        sessions.entries.lock().unwrap().is_empty(),
        "Idle expired sessions release credentials"
    );
    let new = sessions.start(7, provider()).await.unwrap();
    sessions.close();
    assert!(sessions.poll(7, &new.id).await.is_err());
    assert!(sessions.start(7, provider()).await.is_err());
}
