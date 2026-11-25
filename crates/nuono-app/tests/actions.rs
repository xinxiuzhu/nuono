use nuono_app::action::{call_action, notify_action};
use nuono_app::transport::{ActionTransport, ConnectionId, ProtocolDescriptor, TransportAction};
use nuono_domain::{
    ActionError, ActionErrorKind, ActionReceipt, ActionReceiptStatus, BotIdentity, CapabilitySet,
    DeliveryTarget, IdentityKind, IdentityRef, IdentityScheme, MessageSegment, NativeId,
    OutboundAction, Platform,
};
use serde_json::{Value, json};
use std::sync::Mutex;

struct Transport {
    id: ConnectionId,
    protocol: ProtocolDescriptor,
    calls: Mutex<Vec<TransportAction>>,
    failure: Option<ActionError>,
}
impl Transport {
    fn new(failure: Option<ActionError>) -> Self {
        Self {
            id: ConnectionId::new("test-connection").unwrap(),
            protocol: ProtocolDescriptor::new("test", "1").unwrap(),
            calls: Mutex::new(Vec::new()),
            failure,
        }
    }
    fn record(&self, action: TransportAction) -> Result<(), ActionError> {
        self.calls.lock().unwrap().push(action);
        match &self.failure {
            Some(error) => Err(error.clone()),
            None => Ok(()),
        }
    }
}
#[async_trait::async_trait]
impl ActionTransport for Transport {
    fn connection_id(&self) -> &ConnectionId {
        &self.id
    }
    fn protocol(&self) -> &ProtocolDescriptor {
        &self.protocol
    }
    fn capabilities(&self) -> CapabilitySet {
        CapabilitySet::new()
    }
    async fn notify(&self, action: TransportAction) -> Result<(), ActionError> {
        self.record(action)
    }
    async fn call(&self, action: TransportAction) -> Result<ActionReceipt, ActionError> {
        self.record(action)?;
        Ok(ActionReceipt {
            correlation_id: NativeId::new("receipt-1").unwrap(),
            platform: Platform::Discord,
            status: ActionReceiptStatus::Completed,
            message_ids: vec![NativeId::new("18446744073709551615").unwrap()],
            degradations: vec!["attachment rendered as URL".to_owned()],
            raw: json!({"opaque": "retained"}),
        })
    }
    async fn call_raw(
        &self,
        _: &str,
        _: Value,
        _: Option<&BotIdentity>,
    ) -> Result<Value, ActionError> {
        unreachable!("typed dispatch must not fall back to raw actions")
    }
    async fn close(&self) {}
}
fn identity(kind: IdentityKind, id: &str) -> IdentityRef {
    IdentityRef::new(
        Platform::Discord,
        kind,
        IdentityScheme::Snowflake,
        id,
        None::<String>,
    )
    .unwrap()
}
fn request(segments: Vec<MessageSegment>) -> TransportAction {
    let bot =
        BotIdentity::new("discord-instance", identity(IdentityKind::Bot, "100"), None).unwrap();
    TransportAction::new(
        bot,
        OutboundAction::SendMessage {
            target: DeliveryTarget::actor(identity(IdentityKind::User, "18446744073709551615"))
                .unwrap(),
            segments,
            reply_to: Some(NativeId::new("18446744073709551614").unwrap()),
        },
    )
}

#[tokio::test]
async fn invalid_actions_never_reach_the_transport() {
    let transport = Transport::new(None);
    assert!(notify_action(&transport, request(vec![])).await.is_err());
    assert!(call_action(&transport, request(vec![])).await.is_err());
    assert!(transport.calls.lock().unwrap().is_empty());
}

#[tokio::test]
async fn request_native_ids_and_receipts_survive_dispatch() {
    let transport = Transport::new(None);
    let action = request(vec![MessageSegment::text("hello")]);
    let receipt = call_action(&transport, action.clone()).await.unwrap();
    let calls = transport.calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].bot, action.bot);
    assert_eq!(calls[0].action, action.action);
    assert_eq!(receipt.message_ids[0].as_str(), "18446744073709551615");
    assert_eq!(receipt.degradations, vec!["attachment rendered as URL"]);
    assert_eq!(receipt.raw, json!({"opaque": "retained"}));
}

#[tokio::test]
async fn transport_failures_are_returned_without_retry() {
    let mut error = ActionError::new(
        ActionErrorKind::RateLimited {
            retry_after_ms: 500,
        },
        "retry later",
    );
    error.correlation_id = Some(NativeId::new("00018446744073709551615").unwrap());
    error.raw = Some(Box::new(
        json!({"retry_after_ms": 500, "details": ["限流", null]}),
    ));
    for call in [false, true] {
        let transport = Transport::new(Some(error.clone()));
        let action = request(vec![MessageSegment::text("hello")]);
        let result = if call {
            call_action(&transport, action).await.map(|_| ())
        } else {
            notify_action(&transport, action).await
        };
        assert_eq!(result.unwrap_err(), error);
        assert_eq!(transport.calls.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn unauthorized_dashboard_and_unapproved_agent_actions_never_dispatch() {
    use nuono_app::{
        action::ActionAuthority,
        agent::{AutonomyLevel, tool::ToolRiskLevel},
        auth::{CallerIdentity, Role},
    };
    let transport = Transport::new(None);
    for authority in [
        ActionAuthority::Dashboard(CallerIdentity::new(2, "member", Role::User)),
        ActionAuthority::Agent {
            autonomy: AutonomyLevel::ApprovedTools,
            risk: ToolRiskLevel::RequiresApproval,
        },
    ] {
        assert!(
            notify_action(
                &transport,
                request(vec![MessageSegment::text("hello")]).with_authority(authority)
            )
            .await
            .is_err()
        );
    }
    assert!(transport.calls.lock().unwrap().is_empty());
}
#[tokio::test]
async fn cross_platform_target_is_rejected_before_dispatch() {
    let transport = Transport::new(None);
    let mut action = request(vec![MessageSegment::text("hello")]);
    action.bot = BotIdentity::new(
        "another-instance",
        IdentityRef::new(
            Platform::Feishu,
            IdentityKind::Bot,
            IdentityScheme::AppId,
            "cli_app",
            None::<String>,
        )
        .unwrap(),
        None,
    )
    .unwrap();
    assert!(call_action(&transport, action).await.is_err());
    assert!(transport.calls.lock().unwrap().is_empty());
}
struct FailedAudit {
    final_only: bool,
}
#[async_trait::async_trait]
impl nuono_app::action::ActionAudit for FailedAudit {
    async fn record(
        &self,
        _action: &TransportAction,
        _authority: &nuono_app::action::ActionAuthority,
        phase: &str,
        _error: Option<&ActionError>,
    ) -> Result<(), ActionError> {
        if !self.final_only || phase != "started" {
            Err(ActionError::new(ActionErrorKind::Rejected, "audit failure"))
        } else {
            Ok(())
        }
    }
}
#[tokio::test]
async fn action_audit_failure_preserves_effect_boundaries() {
    use nuono_app::action::{ActionAuthority, execute_authorized};
    let action = request(vec![MessageSegment::text("hello")]);
    let authority = ActionAuthority::Runtime {
        stage: "test".into(),
    };
    let count = std::sync::atomic::AtomicUsize::new(0);
    let effect = || async {
        count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    };
    assert!(
        execute_authorized(
            &authority,
            &action,
            Some(&FailedAudit { final_only: false }),
            effect()
        )
        .await
        .is_err()
    );
    assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert!(
        execute_authorized(
            &authority,
            &action,
            Some(&FailedAudit { final_only: true }),
            effect()
        )
        .await
        .is_ok()
    );
    assert_eq!(count.load(std::sync::atomic::Ordering::SeqCst), 1);
}
