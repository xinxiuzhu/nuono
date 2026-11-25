//! 框架级群配置管理授权记录（与商业订单或聊天平台原生群管理员角色相互独立）。
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct GroupAuthorization {
    #[serde(with = "nuono_domain::scalar::i64_string")]
    pub id: i64,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    pub user_id: i64,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    pub group_id: i64,
    pub plugin_id: Option<String>,
    pub issuer_kind: String,
    #[serde(with = "nuono_domain::scalar::optional_i64_string")]
    pub issuer_user_id: Option<i64>,
    pub issuer_plugin_id: Option<String>,
    pub source_key: String,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    pub valid_from_ms: i64,
    #[serde(with = "nuono_domain::scalar::optional_i64_string")]
    pub expires_at_ms: Option<i64>,
    #[serde(with = "nuono_domain::scalar::optional_i64_string")]
    pub revoked_at_ms: Option<i64>,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    pub revision: i64,
}

/// 群配置授权发放请求（相同的 `source_key` 具备幂等性；续期操作通过独立的 CAS 接口完成）。
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GroupGrantRequest {
    #[serde(with = "nuono_domain::scalar::i64_string")]
    pub user_id: i64,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    pub group_id: i64,
    pub plugin_id: Option<String>,
    pub source_key: String,
    #[serde(with = "nuono_domain::scalar::i64_string")]
    pub valid_from_ms: i64,
    #[serde(with = "nuono_domain::scalar::optional_i64_string")]
    pub expires_at_ms: Option<i64>,
}

/// 管理员持久化的插件授权签发白名单（默认空列表表示不允许任何插件自行签发群配置授权）。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginGrantDelegation {
    pub all_plugins: bool,
    pub plugin_ids: Vec<String>,
}

impl PluginGrantDelegation {
    pub fn allows(&self, plugin: Option<&str>) -> bool {
        self.all_plugins
            || plugin.is_some_and(|id| self.plugin_ids.iter().any(|allowed| allowed == id))
    }
}
