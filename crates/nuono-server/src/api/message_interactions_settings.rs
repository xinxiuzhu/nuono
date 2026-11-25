//! 消息路由列表查询与富消息交互策略（Markdown / 按钮）补丁更新接口。
use axum::extract::State;
use nuono_app::message_interactions::{
    ButtonMode, FallbackMode, MarkdownMode, MessageInteractionSettings, MessagePolicy,
    MessageRoutePolicy,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::{AppState, api::ApiResponse};

pub(super) fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(message_routes))
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = api::system_config::MessagePolicyPatch)]
pub(super) struct MessagePolicyPatch {
    #[schema(value_type = Option<String>)]
    markdown: Option<MarkdownMode>,
    #[schema(value_type = Option<String>)]
    buttons: Option<ButtonMode>,
    #[schema(value_type = Option<String>)]
    fallback: Option<FallbackMode>,
    #[schema(minimum = 1, maximum = 5)]
    button_columns: Option<u8>,
    markdown_template_id: Option<String>,
    markdown_template_key: Option<String>,
    button_template_id: Option<String>,
    onebot_extensions: Option<bool>,
}

impl MessagePolicyPatch {
    fn apply(self, policy: &mut MessagePolicy) -> Result<Value, String> {
        let mut patch = serde_json::Map::new();
        macro_rules! field {
            ($field:ident) => {
                if let Some(value) = self.$field {
                    patch.insert(stringify!($field).into(), json!(value));
                    policy.$field = value;
                }
            };
        }
        field!(markdown);
        field!(buttons);
        field!(fallback);
        field!(button_columns);
        field!(markdown_template_id);
        field!(markdown_template_key);
        field!(button_template_id);
        field!(onebot_extensions);
        policy.validate().map_err(|error| error.to_string())?;
        Ok(Value::Object(patch))
    }
}

#[derive(Debug, Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
#[schema(as = api::system_config::MessageInteractionPatch)]
pub(super) struct MessageInteractionPatch {
    default: Option<MessagePolicyPatch>,
    #[schema(value_type = Option<Vec<Object>>)]
    routes: Option<Vec<MessageRoutePolicy>>,
}

impl MessageInteractionPatch {
    pub(super) fn into_settings(
        self,
        mut candidate: MessageInteractionSettings,
    ) -> Result<Value, String> {
        let mut patch = serde_json::Map::new();
        if let Some(default) = self.default {
            patch.insert("default".into(), default.apply(&mut candidate.default)?);
        }
        if let Some(routes) = self.routes {
            patch.insert("routes".into(), json!(routes));
            candidate.routes = routes;
        }
        candidate.validate().map_err(|error| error.to_string())?;
        Ok(Value::Object(patch))
    }
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = api::system_config::MessageRoute)]
struct MessageRoute {
    platform: String,
    instance_id: String,
    bot_self_id: String,
    connected: bool,
    configured: bool,
    #[schema(value_type = Object)]
    capabilities: nuono_app::rich_message::RichCapabilities,
}

#[utoipa::path(
    get,
    path = "/message-routes",
    operation_id = "system_config_message_routes",
    tag = "system_config",
    responses((status = 200, description = "精确运行路由及已配置离线路由。能力表示实现支持，不证明账号获准使用。", body = ApiResponse<Vec<MessageRoute>>))
)]
async fn message_routes(State(state): State<AppState>) -> crate::api::ApiReply<Vec<MessageRoute>> {
    let snapshot = state.config.snapshot().await;
    let connected = state
        .manager
        .event_kernel
        .action_router()
        .connected_routes();
    let mut routes: std::collections::BTreeMap<_, _> = connected
        .into_iter()
        .map(|route| {
            (
                (route.platform, route.instance_id, route.bot_self_id),
                (true, false),
            )
        })
        .collect();
    for route in &snapshot.message_interactions.routes {
        routes
            .entry((
                route.platform.clone(),
                route.instance_id.clone(),
                route.bot_self_id.clone(),
            ))
            .or_insert((false, false))
            .1 = true;
    }
    ApiResponse::ok(
        routes
            .into_iter()
            .map(
                |((platform, instance_id, bot_self_id), (connected, configured))| MessageRoute {
                    capabilities: nuono_app::rich_message::capabilities(&platform),
                    platform,
                    instance_id,
                    bot_self_id,
                    connected,
                    configured,
                },
            )
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_rich_patch_keeps_unspecified_fields_and_replaces_only_explicit_routes() {
        let patch: MessageInteractionPatch =
            serde_json::from_value(json!({"default":{"button_columns":4}})).unwrap();
        assert_eq!(
            patch
                .into_settings(MessageInteractionSettings::default())
                .unwrap(),
            json!({"default":{"button_columns":4}})
        );
        let patch: MessageInteractionPatch = serde_json::from_value(json!({"routes":[]})).unwrap();
        assert_eq!(
            patch
                .into_settings(MessageInteractionSettings::default())
                .unwrap(),
            json!({"routes":[]})
        );
    }

    #[test]
    fn invalid_modes_templates_and_unknown_fields_reject() {
        for value in [
            json!({"other":true}),
            json!({"default":{"markdown":"html"}}),
            json!({"default":{"buttons":"off"}}),
            json!({"default":{"fallback":"silent"}}),
            json!({"default":{"unknown":true}}),
        ] {
            assert!(serde_json::from_value::<MessageInteractionPatch>(value).is_err());
        }
        for value in [
            json!({"default":{"button_columns":0}}),
            json!({"default":{"markdown":"template"}}),
            json!({"default":{"buttons":"template"}}),
        ] {
            let patch: MessageInteractionPatch = serde_json::from_value(value).unwrap();
            assert!(
                patch
                    .into_settings(MessageInteractionSettings::default())
                    .is_err()
            );
        }
    }
}
