//! 用户个人级插件配置接口（配置跟随已认证的 Nuono 账号，在所绑定的各聊天平台间共通生效）。
use crate::{
    AppState,
    api::{
        ApiReply, ApiResponse,
        input::{ApiCaller, ApiJson, ApiPath},
    },
};
use axum::extract::State;
use nuono_app::configuration::{ConfigurationActor, ConfigurationMode};
use nuono_core::{
    db::plugin_overrides::{OverridePatch, OverrideTarget},
    service::plugin_configuration,
};

#[utoipa::path(operation_id="personal_plugins_list",get,path="/personal",tag="plugins",responses((status=200,body=ApiResponse<Vec<super::docs::AnyJson>>)))]
pub async fn list(
    State(state): State<AppState>,
    ApiCaller(caller): ApiCaller,
) -> ApiReply<Vec<serde_json::Value>> {
    let result=async {
  let mut result=Vec::new();
  for plugin in nuono_core::service::plugins::get_installed_plugins(&state.db,&state.manager.plugin_host).await? {
   if plugin.plugin_role.is_service() {
    continue;
   }
   let target=OverrideTarget::User(caller.user_id);
   let row=state.db.plugin_override(&plugin.name,target).await?;
   let saved=state.db.plugin_configuration(&plugin.name).await?;
   let configuration=plugin_configuration::resolve(&state.db,&state.plugin_config,&plugin.name,Some(target)).await?;
   let mut explicit=serde_json::Map::new();
   if let Some(row)=&row {explicit.extend(serde_json::from_str::<serde_json::Map<String,serde_json::Value>>(&row.config_patch_json)?);explicit.extend(serde_json::from_str::<serde_json::Map<String,serde_json::Value>>(&row.service_patch_json)?);}
   let mut fields=Vec::new();
   for (key,field) in state.plugin_config.get_schema(&plugin.name).await.unwrap_or_default() {
    if field.user_scoped() {fields.push(serde_json::json!({"key":key,"title":field.title(),"desc":field.desc(),"schema":field,"service":nuono_core::config::plugin_config::is_service_field(&key),"value":if nuono_core::config::plugin_config::secret_fields(&state.plugin_config.get_schema(&plugin.name).await.unwrap_or_default()).contains(&key){None}else{configuration.values.get(&key)},"overridden":explicit.contains_key(&key)}));}
   }
   fields.sort_by(|a,b|a["key"].as_str().cmp(&b["key"].as_str()));
   result.push(serde_json::json!({"plugin_id":plugin.name,"display_name":plugin.display_name,"enabled":configuration.enabled,"global_enabled":plugin.enabled,"allow_overrides":plugin.allow_user_overrides,"fields":fields,"override_id":row.as_ref().map(|r|r.id.to_string()),"revision":row.as_ref().map_or_else(||"0".into(),|r|r.revision.to_string()),"config_version":saved.config_version.to_string()}));
  }
  Ok::<_,nuono_core::NuonoError>(result)
 }.await;
    ApiResponse::from_result(result)
}

#[utoipa::path(operation_id="personal_plugins_save",post,path="/personal/{name}",tag="plugins",params(("name"=String,Path)),request_body=OverridePatch,responses((status=200,body=ApiResponse<super::docs::AnyJson>)))]
pub async fn save(
    State(state): State<AppState>,
    ApiCaller(caller): ApiCaller,
    ApiPath(name): ApiPath<String>,
    ApiJson(patch): ApiJson<OverridePatch>,
) -> ApiReply<serde_json::Value> {
    if !state.manager.plugin_host.is_plugin_installed(&name).await {
        return ApiResponse::not_found("插件未安装");
    }
    if state
        .manager
        .plugin_host
        .plugin_role(&name)
        .await
        .is_service()
    {
        return ApiResponse::forbidden("底层服务插件不支持个人配置覆盖");
    }
    let actor = ConfigurationActor {
        user_id: caller.user_id,
        mode: ConfigurationMode::Account,
    };
    ApiResponse::from_result(
        plugin_configuration::save(
            &state.db,
            &state.plugin_config,
            &actor,
            &name,
            OverrideTarget::User(caller.user_id),
            patch,
        )
        .await
        .map(|row| serde_json::json!(row)),
    )
}
