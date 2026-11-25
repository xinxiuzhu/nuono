//! 宿主托管的插件级服务权益策略与扩展权限能力注册表（已禁用插件声明的策略与能力均不会生效）。
use crate::{
    auth::{CapabilityProvider, CapabilityProviderError, Role},
    entitlement::{EntitlementDecision, GroupEntitlementQuery, ServiceEntitlement},
};
use std::{
    collections::HashMap,
    sync::{Arc, LazyLock, RwLock},
};

#[derive(Clone, Default)]
#[non_exhaustive]
pub struct PluginExtensions {
    pub service_entitlement: Option<Arc<dyn ServiceEntitlement>>,
    pub capabilities: Option<Arc<dyn CapabilityProvider>>,
    /// 静态声明本插件将提供部署级服务权益策略（用于两段式生命周期在 `mount` 之前即保证 fail-closed）。
    pub protects_services: bool,
}
impl PluginExtensions {
    /// 构造空的扩展声明。
    pub fn new() -> Self {
        Self::default()
    }

    /// 绑定服务权益策略提供者，并自动标记 `protects_services = true`。
    pub fn with_service_entitlement(mut self, entitlement: Arc<dyn ServiceEntitlement>) -> Self {
        self.service_entitlement = Some(entitlement);
        self.protects_services = true;
        self
    }

    /// 绑定扩展能力提供者。
    pub fn with_capabilities(mut self, capabilities: Arc<dyn CapabilityProvider>) -> Self {
        self.capabilities = Some(capabilities);
        self
    }

    /// 静态声明本插件需要提供服务权益策略（在 `mount(ctx)` 运行前即生效 fail-closed 保护）。
    pub fn with_service_policy_required(mut self, required: bool) -> Self {
        self.protects_services = required;
        self
    }

    /// 是否声明了部署级服务权益策略（静态标记或已提供实例）。
    pub fn declares_service_policy(&self) -> bool {
        self.protects_services || self.service_entitlement.is_some()
    }

    pub fn validate(&self) -> Result<(), CapabilityProviderError> {
        if let Some(provider) = &self.capabilities {
            crate::auth::validate_provider(provider.as_ref())?;
        }
        Ok(())
    }
}
#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Disabled,
    Pending,
    Active,
}
#[derive(Clone)]
struct Entry {
    declarations: PluginExtensions,
    state: State,
    policy_required: bool,
    deployment_required: bool,
}
#[derive(Default)]
struct Registry {
    entries: RwLock<HashMap<(String, String), Entry>>,
}
impl Registry {
    fn snapshot(&self) -> Vec<Entry> {
        self.entries
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect()
    }
    async fn check_group_allowed(&self, query: &GroupEntitlementQuery) -> EntitlementDecision {
        for entry in self.snapshot() {
            let decision = match entry.state {
                State::Disabled => {
                    if entry.deployment_required {
                        EntitlementDecision::denied()
                    } else {
                        EntitlementDecision::Allowed
                    }
                }
                State::Pending => {
                    if entry.policy_required {
                        EntitlementDecision::denied()
                    } else {
                        EntitlementDecision::Allowed
                    }
                }
                State::Active => match entry.declarations.service_entitlement {
                    Some(provider) => {
                        crate::entitlement::query_provider(provider, query.clone()).await
                    }
                    None => {
                        if entry.deployment_required || entry.policy_required {
                            EntitlementDecision::denied()
                        } else {
                            EntitlementDecision::Allowed
                        }
                    }
                },
            };
            if !decision.is_allowed() {
                return decision;
            }
        }
        EntitlementDecision::Allowed
    }
    #[cfg(test)]
    async fn group_allowed(&self, group_id: i64) -> bool {
        self.check_group_allowed(&GroupEntitlementQuery::for_group(group_id))
            .await
            .is_allowed()
    }
    fn assistant_available(&self) -> bool {
        !self.snapshot().iter().any(|e| {
            (e.policy_required && e.state == State::Pending)
                || ((e.deployment_required || e.policy_required)
                    && e.state == State::Active
                    && e.declarations.service_entitlement.is_none())
                || (e.deployment_required && e.state != State::Active)
        })
    }
    fn assistant_grant_required(&self) -> bool {
        self.snapshot().iter().any(|e| {
            e.deployment_required
                || (e.state != State::Disabled
                    && (e.declarations.protects_services
                        && e.declarations.service_entitlement.is_none()))
                || (e.state != State::Disabled
                    && e.declarations
                        .service_entitlement
                        .as_ref()
                        .is_some_and(|p| {
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                                p.requires_assistant_grant()
                            }))
                            .unwrap_or(true)
                        }))
        })
    }
    fn capabilities(&self, role: Role) -> Vec<&'static str> {
        self.snapshot()
            .into_iter()
            .filter(|e| e.state == State::Active)
            .filter_map(|e| e.declarations.capabilities)
            .flat_map(|p| p.granted(role))
            .collect()
    }
}
static REGISTRY: LazyLock<Arc<Registry>> = LazyLock::new(|| Arc::new(Registry::default()));

/// 绑定到单个宿主实例生命周期的插件扩展作用域（随宿主销毁自动清理，插件回调自身无法越权解除宿主配置的必选策略要求）。
pub struct PluginExtensionScope {
    id: String,
    registry: Arc<Registry>,
}
impl Default for PluginExtensionScope {
    fn default() -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            registry: REGISTRY.clone(),
        }
    }
}
impl PluginExtensionScope {
    /// 在加载插件或对外处理请求前，先注入部署级必选的服务策略插件名单（名单内插件未就绪时自动拒绝放行）。
    pub fn require_providers(&self, providers: &[String]) {
        let mut entries = self
            .registry
            .entries
            .write()
            .unwrap_or_else(|e| e.into_inner());
        for ((host, _), entry) in entries.iter_mut() {
            if host == &self.id {
                entry.deployment_required = false;
                entry.policy_required = entry.declarations.declares_service_policy();
            }
        }
        for plugin in providers {
            let entry = entries
                .entry((self.id.clone(), plugin.clone()))
                .or_insert(Entry {
                    declarations: PluginExtensions::default(),
                    state: State::Pending,
                    policy_required: true,
                    deployment_required: true,
                });
            entry.deployment_required = true;
            entry.policy_required = true;
        }
    }
    pub fn prepare(&self, plugin: &str, declarations: PluginExtensions, enabled: bool) {
        let mut entries = self
            .registry
            .entries
            .write()
            .unwrap_or_else(|e| e.into_inner());
        let key = (self.id.clone(), plugin.to_owned());
        let policy_required = declarations.declares_service_policy()
            || entries
                .get(&key)
                .is_some_and(|e| e.policy_required && e.state != State::Disabled);
        let deployment_required = entries.get(&key).is_some_and(|e| e.deployment_required);
        entries.insert(
            key,
            Entry {
                declarations,
                policy_required,
                deployment_required,
                state: if enabled {
                    State::Pending
                } else {
                    State::Disabled
                },
            },
        );
    }
    fn transition(&self, plugin: &str, state: State) {
        if let Some(entry) = self
            .registry
            .entries
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(&(self.id.clone(), plugin.to_owned()))
        {
            entry.state = state;
            if state == State::Active {
                entry.policy_required =
                    entry.deployment_required || entry.declarations.declares_service_policy();
            }
        }
    }
    pub fn activate(&self, plugin: &str) {
        self.transition(plugin, State::Active);
    }
    pub fn disable(&self, plugin: &str) {
        self.transition(plugin, State::Disabled);
    }
    pub fn suspend(&self, plugin: &str) {
        if let Some(entry) = self
            .registry
            .entries
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(&(self.id.clone(), plugin.to_owned()))
            && entry.state != State::Disabled
        {
            entry.state = State::Pending;
        }
    }
    /// 判断该插件是否持有当前生效的服务权益门禁（插件激活失败进入 `Pending` 时仍保持门禁拦截，直到下次启动成功或被管理员显式禁用）。
    pub fn holds_service_policy(&self, plugin: &str) -> bool {
        self.registry
            .entries
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&(self.id.clone(), plugin.to_owned()))
            .is_some_and(|e| {
                e.deployment_required || (e.state != State::Disabled && e.policy_required)
            })
    }
}
impl Drop for PluginExtensionScope {
    fn drop(&mut self) {
        self.registry
            .entries
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(host, _), _| host != &self.id);
    }
}
pub(crate) async fn check_group_allowed(query: &GroupEntitlementQuery) -> EntitlementDecision {
    REGISTRY.check_group_allowed(query).await
}
pub(crate) fn has_entitlement_provider() -> bool {
    REGISTRY.snapshot().iter().any(|entry| {
        entry.deployment_required
            || entry.policy_required
            || (entry.state == State::Active && entry.declarations.service_entitlement.is_some())
    })
}
pub fn assistant_available() -> bool {
    REGISTRY.assistant_available()
}
pub(crate) fn assistant_grant_required() -> bool {
    REGISTRY.assistant_grant_required()
}
pub(crate) fn capabilities(role: Role) -> Vec<&'static str> {
    REGISTRY.capabilities(role)
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Paid;
    #[async_trait::async_trait]
    impl ServiceEntitlement for Paid {
        async fn check_group(
            &self,
            query: &GroupEntitlementQuery,
        ) -> nuono_error::Result<EntitlementDecision> {
            if query.group_id() == 7 {
                Ok(EntitlementDecision::Allowed)
            } else {
                Ok(EntitlementDecision::denied())
            }
        }
        fn requires_assistant_grant(&self) -> bool {
            true
        }
    }
    impl CapabilityProvider for Paid {
        fn granted(&self, _: Role) -> Vec<&'static str> {
            vec!["view_service_account"]
        }
    }
    #[tokio::test]
    async fn managed_service_policy_disable_failure_reload_and_host_drop() {
        let registry = Arc::new(Registry::default());
        let scope = PluginExtensionScope {
            id: "fixture".into(),
            registry: registry.clone(),
        };
        assert!(registry.group_allowed(8).await);
        assert!(!registry.assistant_grant_required());
        assert!(registry.capabilities(Role::User).is_empty());
        scope.prepare(
            "paid",
            PluginExtensions::new()
                .with_service_entitlement(Arc::new(Paid))
                .with_capabilities(Arc::new(Paid)),
            true,
        );
        let future = 7;
        assert!(!registry.group_allowed(future).await);
        assert!(!registry.assistant_available());
        scope.activate("paid");
        assert!(registry.group_allowed(future).await);
        assert!(!registry.group_allowed(8).await);
        assert_eq!(registry.capabilities(Role::User), ["view_service_account"]);
        scope.suspend("paid");
        assert!(
            !registry.group_allowed(future).await,
            "reload or failure cannot open service"
        );
        assert!(registry.capabilities(Role::User).is_empty());
        assert!(scope.holds_service_policy("paid"));
        scope.activate("paid");
        scope.disable("paid");
        assert!(registry.group_allowed(8).await);
        assert!(registry.assistant_available());
        assert!(!registry.assistant_grant_required());
        assert!(registry.capabilities(Role::User).is_empty());
        assert!(!scope.holds_service_policy("paid"));
        drop(scope);
        assert!(registry.snapshot().is_empty());
    }
}

#[cfg(test)]
mod durable_policy_tests {
    use super::*;
    struct Broken;
    #[async_trait::async_trait]
    impl ServiceEntitlement for Broken {
        async fn check_group(
            &self,
            _: &GroupEntitlementQuery,
        ) -> nuono_error::Result<EntitlementDecision> {
            panic!("fixture policy failed")
        }
    }
    #[tokio::test]
    async fn required_provider_is_closed_when_missing_disabled_or_panicking_and_owner_can_repair() {
        let registry = Arc::new(Registry::default());
        let scope = PluginExtensionScope {
            id: "durable-fixture".into(),
            registry: registry.clone(),
        };
        scope.require_providers(&["billing".into()]);
        assert!(!registry.group_allowed(8).await);
        assert!(!registry.assistant_available());
        scope.prepare(
            "billing",
            PluginExtensions::new().with_service_entitlement(Arc::new(Broken)),
            false,
        );
        assert!(!registry.group_allowed(8).await);
        assert!(!registry.assistant_available());
        scope.activate("billing");
        assert!(!registry.group_allowed(8).await);
        scope.disable("billing");
        assert!(!registry.group_allowed(8).await);
        scope.require_providers(&[]);
        assert!(registry.group_allowed(8).await);
        assert!(registry.assistant_available());
        let missing = PluginExtensionScope {
            id: "missing-fixture".into(),
            registry: registry.clone(),
        };
        missing.require_providers(&["absent".into()]);
        assert!(!registry.group_allowed(8).await);
        missing.require_providers(&[]);
        assert!(registry.group_allowed(8).await);
    }
}
