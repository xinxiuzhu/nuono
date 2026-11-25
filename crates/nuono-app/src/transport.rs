use dashmap::DashMap;
use nuono_domain::{
    ActionError, ActionReceipt, BotIdentity, CapabilitySet, IdentityKind, IdentityRef, NativeId,
    OutboundAction,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Arc;

/// 由平台适配器或测试桩提供的出站动作发送接口（兼容旧版 JSON action 与原生文件发送）。
#[async_trait::async_trait]
pub trait RuntimeActionSink: Send + Sync {
    /// 原生文件发送接口。不支持该能力的适配器不会执行上传或发送。
    /// 调用方在调用此接口前已完成发送目标的完整校验。
    async fn call_native_file(
        &self,
        _request: crate::media::NativeFileSendRequest,
    ) -> nuono_error::Result<crate::media::FileSendResponse> {
        Ok(crate::media::FileSendResponse::Unsupported {})
    }

    /// 在将强类型发送目标编码为旧版字段前，校验目标身份是否属于当前适配器实例的作用域。
    /// 传输层还会校验平台的身份体系（scheme）与命名空间（namespace）约定；
    /// 采用应用级（app-scoped）身份的适配器必须与自身实际连接的客户端实例进行比对。
    async fn validate_native_target(
        &self,
        _target: &nuono_domain::DeliveryTarget,
    ) -> nuono_error::Result<()> {
        Ok(())
    }

    async fn send_action(&self, action: Value) -> nuono_error::Result<()>;

    async fn call_action(&self, action: Value) -> nuono_error::Result<Value> {
        self.send_action(action).await?;
        Ok(Value::Null)
    }
}

/// 外部协议描述。聊天平台与协议版本是两个独立维度。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ProtocolDescriptor {
    pub family: NativeId,
    pub version: NativeId,
}

impl ProtocolDescriptor {
    pub fn new(
        family: impl Into<String>,
        version: impl Into<String>,
    ) -> Result<Self, nuono_domain::DomainModelError> {
        Ok(Self {
            family: NativeId::new(family)?,
            version: NativeId::new(version)?,
        })
    }

    pub fn onebot(version: impl Into<String>) -> Self {
        Self::new("onebot", version).expect("static OneBot protocol descriptor is valid")
    }
}

/// 可寻址的 Bot 运行实例。
///
/// `instance_id` 区分同一平台账号的多套运行配置；`account_id` 保存平台原生字符串身份。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BotInstanceKey {
    pub instance_id: NativeId,
    pub protocol: ProtocolDescriptor,
    pub account_id: IdentityRef,
}

impl BotInstanceKey {
    pub fn new(
        instance_id: impl Into<String>,
        protocol: ProtocolDescriptor,
        account_id: IdentityRef,
    ) -> Result<Self, nuono_domain::DomainModelError> {
        if *account_id.kind() != IdentityKind::Bot {
            return Err(nuono_domain::DomainModelError::InvalidValue {
                field: "account_id",
                expected: "a bot identity",
            });
        }
        Ok(Self {
            instance_id: NativeId::new(instance_id)?,
            protocol,
            account_id,
        })
    }

    pub fn from_bot(bot: &BotIdentity, protocol: ProtocolDescriptor) -> Self {
        Self {
            instance_id: bot.instance_id.clone(),
            protocol,
            account_id: bot.account_id.clone(),
        }
    }

    pub fn canonical_id(&self) -> String {
        serde_json::to_string(self).expect("validated bot instance keys are serializable")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ConnectionId(pub NativeId);

impl ConnectionId {
    pub fn new(value: impl Into<String>) -> Result<Self, nuono_domain::DomainModelError> {
        Ok(Self(NativeId::new(value)?))
    }
}

/// 指定 Bot 实例的 typed action 请求。
#[derive(Debug, Clone)]
pub struct TransportAction {
    pub bot: BotIdentity,
    pub action: OutboundAction,
    pub authority: crate::action::ActionAuthority,
}

impl TransportAction {
    pub fn new(bot: BotIdentity, action: OutboundAction) -> Self {
        Self {
            bot,
            action,
            authority: crate::action::ActionAuthority::Runtime {
                stage: "application".into(),
            },
        }
    }
    pub fn with_authority(mut self, authority: crate::action::ActionAuthority) -> Self {
        self.authority = authority;
        self
    }
}

/// 一条已连接协议会话的 typed action 边界。
#[async_trait::async_trait]
pub trait ActionTransport: Send + Sync {
    fn connection_id(&self) -> &ConnectionId;

    fn protocol(&self) -> &ProtocolDescriptor;

    fn capabilities(&self) -> CapabilitySet;

    async fn notify(&self, action: TransportAction) -> Result<(), ActionError>;

    async fn call(&self, action: TransportAction) -> Result<ActionReceipt, ActionError>;

    async fn call_raw(
        &self,
        action: &str,
        params: Value,
        bot: Option<&BotIdentity>,
    ) -> Result<Value, ActionError>;

    async fn close(&self);
}

#[derive(Clone)]
pub struct RegisteredConnection {
    pub connection_id: ConnectionId,
    pub generation: u64,
    pub transport: Arc<dyn ActionTransport>,
}

/// 字符串身份连接注册表。
///
/// 相同实例的新 generation 会替换旧连接；旧连接结束时只能删除仍指向自己的条目。
#[derive(Default)]
pub struct ConnectionRegistry {
    connections: DashMap<BotInstanceKey, RegisteredConnection>,
}

impl ConnectionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(
        &self,
        key: BotInstanceKey,
        generation: u64,
        transport: Arc<dyn ActionTransport>,
    ) -> Option<RegisteredConnection> {
        let connection = RegisteredConnection {
            connection_id: transport.connection_id().clone(),
            generation,
            transport,
        };
        match self.connections.entry(key) {
            dashmap::mapref::entry::Entry::Vacant(entry) => {
                entry.insert(connection);
                None
            }
            dashmap::mapref::entry::Entry::Occupied(mut entry) => {
                if entry.get().generation > generation {
                    return Some(entry.get().clone());
                }
                Some(entry.insert(connection))
            }
        }
    }

    pub fn get(&self, key: &BotInstanceKey) -> Option<RegisteredConnection> {
        self.connections.get(key).map(|entry| entry.value().clone())
    }

    pub fn remove_if_current(
        &self,
        key: &BotInstanceKey,
        connection_id: &ConnectionId,
        generation: u64,
    ) -> Option<RegisteredConnection> {
        self.connections
            .remove_if(key, |_, current| {
                current.connection_id == *connection_id && current.generation == generation
            })
            .map(|(_, connection)| connection)
    }

    pub fn remove_connection(&self, connection_id: &ConnectionId, generation: u64) -> usize {
        let keys = self
            .connections
            .iter()
            .filter(|entry| entry.connection_id == *connection_id && entry.generation == generation)
            .map(|entry| entry.key().clone())
            .collect::<Vec<_>>();
        let mut removed = 0;
        for key in keys {
            if self
                .remove_if_current(&key, connection_id, generation)
                .is_some()
            {
                removed += 1;
            }
        }
        removed
    }

    pub fn list(&self) -> Vec<(BotInstanceKey, RegisteredConnection)> {
        self.connections
            .iter()
            .map(|entry| (entry.key().clone(), entry.value().clone()))
            .collect()
    }

    pub fn len(&self) -> usize {
        self.connections.len()
    }

    pub fn is_empty(&self) -> bool {
        self.connections.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nuono_domain::{ActionErrorKind, ActionReceiptStatus, IdentityScheme, Platform};

    struct TestTransport {
        connection_id: ConnectionId,
        protocol: ProtocolDescriptor,
    }

    #[async_trait::async_trait]
    impl ActionTransport for TestTransport {
        fn connection_id(&self) -> &ConnectionId {
            &self.connection_id
        }

        fn protocol(&self) -> &ProtocolDescriptor {
            &self.protocol
        }

        fn capabilities(&self) -> CapabilitySet {
            CapabilitySet::new()
        }

        async fn notify(&self, _action: TransportAction) -> Result<(), ActionError> {
            Ok(())
        }

        async fn call(&self, _action: TransportAction) -> Result<ActionReceipt, ActionError> {
            Ok(ActionReceipt {
                correlation_id: NativeId::new("correlation").unwrap(),
                platform: Platform::OneBot,
                status: ActionReceiptStatus::Completed,
                message_ids: Vec::new(),
                degradations: Vec::new(),
                raw: Value::Null,
            })
        }

        async fn call_raw(
            &self,
            _action: &str,
            _params: Value,
            _bot: Option<&BotIdentity>,
        ) -> Result<Value, ActionError> {
            Err(ActionError::new(
                ActionErrorKind::UnsupportedCapability,
                "unsupported",
            ))
        }

        async fn close(&self) {}
    }

    fn key(instance: &str) -> BotInstanceKey {
        let account_id = IdentityRef::new(
            Platform::OneBot,
            IdentityKind::Bot,
            IdentityScheme::Native,
            "bot-account",
            None::<String>,
        )
        .unwrap();
        BotInstanceKey::new(instance, ProtocolDescriptor::onebot("12"), account_id).unwrap()
    }

    fn transport(id: &str) -> Arc<dyn ActionTransport> {
        Arc::new(TestTransport {
            connection_id: ConnectionId::new(id).unwrap(),
            protocol: ProtocolDescriptor::onebot("12"),
        })
    }

    #[test]
    fn bot_instance_key_includes_instance_and_protocol() {
        let first = key("first");
        let second = key("second");
        assert_ne!(first, second);
        assert!(first.canonical_id().contains("onebot"));
    }

    #[test]
    fn stale_connection_cannot_remove_replacement() {
        let registry = ConnectionRegistry::new();
        let key = key("instance");
        let old = transport("old");
        let new = transport("new");
        registry.register(key.clone(), 1, old.clone());
        registry.register(key.clone(), 2, new.clone());

        assert!(
            registry
                .remove_if_current(&key, old.connection_id(), 1)
                .is_none()
        );
        assert_eq!(
            registry.get(&key).unwrap().connection_id,
            *new.connection_id()
        );
        assert!(
            registry
                .remove_if_current(&key, new.connection_id(), 2)
                .is_some()
        );
    }

    #[test]
    fn one_connection_can_register_multiple_bot_instances() {
        let registry = ConnectionRegistry::new();
        let transport = transport("shared");
        registry.register(key("first"), 7, transport.clone());
        registry.register(key("second"), 7, transport.clone());
        assert_eq!(registry.len(), 2);
        assert_eq!(registry.remove_connection(transport.connection_id(), 7), 2);
        assert!(registry.is_empty());
    }
}

/// 与具体网络框架无关的双工传输帧；各 HTTP/WebSocket 适配器负责与自身传输类型互转。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TransportFrame {
    Text(String),
    Binary(Vec<u8>),
    Ping(Vec<u8>),
    Pong(Vec<u8>),
    Close(Option<TransportClose>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportClose {
    pub code: u16,
    pub reason: String,
}

/// 已连接 Bot 动作接收端（ActionSink）在运行时的精确路由地址。
///
/// 所有字段均保留适配器原生的字符串表示；尤其是 `bot_self_id`，
/// 在用于路由前绝不能先经过旧版数值型 Bot API 转换。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RuntimeBotRoute {
    pub platform: String,
    pub instance_id: String,
    pub bot_self_id: String,
}

impl RuntimeBotRoute {
    pub fn new(
        platform: impl Into<String>,
        instance_id: impl Into<String>,
        bot_self_id: impl Into<String>,
    ) -> Self {
        Self {
            platform: platform.into(),
            instance_id: instance_id.into(),
            bot_self_id: bot_self_id.into(),
        }
    }
}
