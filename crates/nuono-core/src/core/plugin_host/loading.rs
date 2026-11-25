//! 从已链接工厂发现实例，并在不发布宿主状态的前提下筛选、校验候选。
use super::*;

/// 提取并校验插件的 secret 声明，返回不可变的已验证 snapshot。
///
/// 只调用一次 [`Plugin::secret_fields`]。**无 secret 字段的插件不生成 canonical key**，因此
/// 允许非保留的插件名（含空格、点、Unicode），返回空 snapshot；只有声明了 secret 的插件才要求
/// 插件名与字段 key 满足安全字符规则，且字段 key 空值 / 非法字符 / 重复都会报错。返回的
/// snapshot 同时供 registry、[`PluginContext::secrets`] 与后续 toggle/re-enable context 使用。
#[cfg(test)]
pub(super) fn validated_plugin_secret_fields(
    plugin: &LoadedPlugin,
) -> crate::error::Result<Vec<crate::plugin::PluginSecretField>> {
    validate_secret_fields(plugin.name(), plugin.secret_fields())
}

fn validate_secret_fields(
    name: &str,
    fields: Vec<crate::plugin::PluginSecretField>,
) -> crate::error::Result<Vec<crate::plugin::PluginSecretField>> {
    if crate::core::Paths::is_reserved_plugin_name(name) {
        return Err(crate::error::NuonoError::Validation(
            "hub 是管理界面的保留目录名称，不能作为插件名".into(),
        ));
    }
    if fields.is_empty() {
        return Ok(fields);
    }
    crate::plugin::validate_plugin_secret_component(name)?;
    for field in &fields {
        crate::plugin::validate_plugin_secret_component(&field.key)?;
    }
    let mut seen = std::collections::HashSet::new();
    for field in &fields {
        if !seen.insert(field.key.as_str()) {
            return Err(crate::error::NuonoError::Validation(format!(
                "插件 {} 声明了重复的 secret 字段: {}",
                name, field.key
            )));
        }
    }
    Ok(fields)
}

/// reload 候选必须恰好一个：0 个或超过 1 个同名候选都拒绝（替换前），返回错误并保持旧
/// 实例 / schema 不变。
pub(super) fn single_reload_candidate(
    mut candidates: Vec<LoadedPlugin>,
    name: &str,
) -> crate::error::Result<LoadedPlugin> {
    if candidates.is_empty() {
        return Err(crate::error::NuonoError::PluginLoad {
            plugin: name.to_string(),
            reason: "已链接的插件工厂没有提供同名新实例".to_string(),
        });
    }
    if candidates.len() > 1 {
        return Err(crate::error::NuonoError::PluginLoad {
            plugin: name.to_string(),
            reason: format!("存在 {} 个同名候选实例，拒绝重载", candidates.len()),
        });
    }
    Ok(candidates.remove(0))
}

/// 插件的只读声明快照。
///
/// 展示名在候选预检前读取，确保其他声明失败时目录仍保留品牌信息。
/// 其余同步声明（version/service_config/config_fields）在统一 panic 边界内收集，
/// 供 init/enable/sync 复用，避免对可能带
/// 副作用的同步声明方法重复调用。secret_fields 由 registry 单独持有（已验证快照），
/// 供 registry / PluginSecretStore / context 共享。
pub(super) struct PluginDeclaration {
    pub(super) account_merge:
        Option<std::sync::Arc<dyn nuono_app::account_merge::AccountMergeParticipant>>,
    pub(super) extensions: nuono_app::plugin_extensions::PluginExtensions,
    pub(super) name: String,
    pub(super) display_name: String,
    pub(super) version: String,
    pub(super) description: String,
    pub(super) plugin_role: nuono_app::plugin_role::PluginRole,
    pub(super) required_dependencies: Vec<String>,
    pub(super) optional_dependencies: Vec<String>,
    pub(super) storage: nuono_plugin::storage_manifest::PluginStorageDeclaration,
    pub(super) service_config: Plugins,
    pub(super) message_listener: Option<crate::plugin::PluginMessageListener>,
    pub(super) config_default: Value,
    pub(super) config_version: i64,
    pub(super) config_migration: Option<Arc<dyn nuono_app::config_schema::ConfigurationMigration>>,
    pub(super) config_schema: Value,
    pub(super) config_resources:
        Vec<nuono_app::configuration_resources::ConfigurationResourceDefinition>,
    pub(super) config_fields: Vec<crate::core::PluginConfigField>,
    // 指令的声明快照：与 config_* 一起在统一 panic 边界内单次收集，
    // 保证 `plugin.commands()` 的同步 panic 被同一套 call_with_catch 捕获。
    pub(super) commands: Vec<crate::core::command::Command>,
    pub(super) web_routes: Vec<crate::plugin::PluginWebRoute>,
    pub(super) web_pages: Vec<crate::plugin::PluginWebPage>,
}

/// 校验插件声明的 Web 路由。
///
/// 路径必须是绝对路径（以 `/` 开头），可以包含 `:name` 参数段 —— 宿主只按整段匹配，
/// 不做前缀或通配。查重按**归一化**形式进行：`/groups/:a` 与 `/groups/:b` 是同一个模式，
/// 两条都留着会让匹配结果取决于声明顺序。
pub(super) fn validated_plugin_web_routes(
    plugin_name: &str,
    routes: Vec<crate::plugin::PluginWebRoute>,
) -> crate::error::Result<Vec<crate::plugin::PluginWebRoute>> {
    let mut seen = std::collections::HashSet::new();
    for route in &routes {
        if !(1..=32 * 1024 * 1024).contains(&route.max_body_bytes) {
            return Err(crate::error::NuonoError::Validation(format!(
                "插件 {plugin_name} 的 Web 请求大小上限必须为 1 byte 至 32 MiB"
            )));
        }
        let path = route.path.as_str();
        let valid = path == "/"
            || (path.starts_with('/')
                && !path.ends_with('/')
                && !path.contains("//")
                && !path.contains("?")
                && !path.contains('#')
                && !path.split('/').any(|part| part == "." || part == ".."));
        if !valid {
            return Err(crate::error::NuonoError::Validation(format!(
                "插件 {plugin_name} 的 Web 路由路径无效: {path:?}"
            )));
        }
        if let Err(reason) = super::route_pattern::parameter_names(path) {
            return Err(crate::error::NuonoError::Validation(format!(
                "插件 {plugin_name} 的 Web 路由参数无效: {path:?}（{reason}）"
            )));
        }
        let key = (route.method.clone(), super::route_pattern::normalized(path));
        if !seen.insert(key) {
            return Err(crate::error::NuonoError::Validation(format!(
                "插件 {plugin_name} 重复声明 Web 路由: {} {path}",
                route.method
            )));
        }
    }
    Ok(routes)
}

/// 同步收集插件声明。本身不捕获 panic；调用方应把它放进 [`PluginHost::call_with_catch`]
/// 边界内。
pub(super) fn collect_declaration_sync(
    plugin: &LoadedPlugin,
) -> crate::error::Result<PluginDeclaration> {
    let name = plugin.name().to_string();
    let display_name = plugin_display_name(&name, plugin.display_name());
    let extensions = plugin.extensions();
    extensions.validate().map_err(|e| anyhow::anyhow!(e))?;
    collect_named_declaration(plugin, name, display_name, extensions)
}

fn collect_named_declaration(
    plugin: &LoadedPlugin,
    name: String,
    display_name: String,
    extensions: nuono_app::plugin_extensions::PluginExtensions,
) -> crate::error::Result<PluginDeclaration> {
    let storage = plugin.storage_declaration()?;
    storage.validate()?;
    let account_merge = plugin.account_merge();
    let declared_merge = storage.manifest.merge_participant_version;
    if declared_merge
        != account_merge
            .as_ref()
            .map(|participant| participant.version())
    {
        return Err(anyhow::anyhow!(
            "account merge participant version does not match the persistent manifest"
        )
        .into());
    }
    let mut fields = plugin.config_fields();
    let resources = plugin.config_resources();
    let mut resource_keys = std::collections::HashSet::new();
    for resource in &resources {
        if !resource_keys.insert(resource.key.clone())
            || fields.iter().any(|field| field.key == resource.key)
        {
            return Err(
                anyhow::anyhow!("duplicate configuration resource {}", resource.key).into(),
            );
        }
        fields.push(resource.field()?);
    }
    let web_pages = plugin.web_pages();
    if !web_pages.is_empty()
        && (name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-')))
    {
        return Err(anyhow::anyhow!(
            "plugin ids declaring WebUI pages must contain only ASCII letters, digits, underscores and hyphens"
        ).into());
    }
    crate::plugin::PluginWebPage::validate_all(&web_pages)?;
    let schema = crate::config::plugin_fields_to_schema(&fields)?;
    let message_listener = plugin.message_listener();
    if let Some(listener) = &message_listener {
        listener.validate()?;
    }
    let mut service_config = plugin.service_config();
    service_config.auto_listen_enabled = message_listener
        .as_ref()
        .map(|_| service_config.auto_listen_enabled.unwrap_or(false));
    let (required_dependencies, optional_dependencies) = validate_dependencies(
        &name,
        plugin.required_dependencies(),
        plugin.optional_dependencies(),
    )?;
    Ok(PluginDeclaration {
        account_merge,
        extensions,
        web_routes: validated_plugin_web_routes(&name, plugin.web_routes())?,
        web_pages,
        name,
        display_name,
        version: plugin.version().to_string(),
        description: plugin.description().to_string(),
        plugin_role: plugin.plugin_role(),
        required_dependencies,
        optional_dependencies,
        storage,
        service_config,
        message_listener,
        config_default: nuono_plugin::config_validation::config_defaults(&fields),
        config_version: plugin.config_version(),
        config_migration: plugin.config_migration(),
        config_schema: serde_json::to_value(&schema)?,
        config_resources: resources,
        config_fields: fields,
        commands: plugin.commands(),
    })
}

/// 校验并规范化插件的强依赖与可选依赖声明：拒绝空名称、自依赖与同一依赖同时出现在两类列表中。
fn validate_dependencies(
    plugin_name: &str,
    required: Vec<&'static str>,
    optional: Vec<&'static str>,
) -> crate::error::Result<(Vec<String>, Vec<String>)> {
    let normalize = |raw: Vec<&'static str>| -> crate::error::Result<Vec<String>> {
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for dep in raw {
            let trimmed = dep.trim();
            if trimmed.is_empty() || trimmed != dep {
                return Err(crate::error::NuonoError::Validation(format!(
                    "插件 {plugin_name} 声明了无效依赖名称: {dep:?}"
                )));
            }
            if trimmed == plugin_name {
                return Err(crate::error::NuonoError::Validation(format!(
                    "插件 {plugin_name} 不能依赖自身"
                )));
            }
            if seen.insert(trimmed) {
                out.push(trimmed.to_owned());
            }
        }
        Ok(out)
    };
    let required = normalize(required)?;
    let optional = normalize(optional)?;
    let req_set: std::collections::HashSet<&str> = required.iter().map(String::as_str).collect();
    for dep in &optional {
        if req_set.contains(dep.as_str()) {
            return Err(crate::error::NuonoError::Validation(format!(
                "插件 {plugin_name} 同时将 {dep} 声明为强依赖和可选依赖"
            )));
        }
    }
    Ok((required, optional))
}

/// 按强依赖与可选依赖对本批候选插件执行拓扑排序（Kahn 算法）。
///
/// 无偏序约束的插件保持原有发现顺序；命中循环依赖的插件从候选列表剥离并标记拒绝。
fn sort_candidates_topologically(
    candidates: Vec<PreparedPlugin>,
) -> (Vec<PreparedPlugin>, Vec<PreparedPlugin>) {
    let count = candidates.len();
    if count <= 1 {
        return (candidates, Vec::new());
    }
    let index_by_name: HashMap<&str, usize> = candidates
        .iter()
        .enumerate()
        .map(|(idx, candidate)| (candidate.declaration.name.as_str(), idx))
        .collect();
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); count];
    let mut indegree: Vec<usize> = vec![0; count];
    for (idx, candidate) in candidates.iter().enumerate() {
        for dep in candidate
            .declaration
            .required_dependencies
            .iter()
            .chain(&candidate.declaration.optional_dependencies)
        {
            if let Some(&upstream_idx) = index_by_name.get(dep.as_str()) {
                dependents[upstream_idx].push(idx);
                indegree[idx] += 1;
            }
        }
    }
    let mut ready: std::collections::BTreeSet<usize> = indegree
        .iter()
        .enumerate()
        .filter_map(|(idx, &deg)| (deg == 0).then_some(idx))
        .collect();
    let mut order = Vec::with_capacity(count);
    while let Some(idx) = ready.pop_first() {
        order.push(idx);
        for &next in &dependents[idx] {
            indegree[next] -= 1;
            if indegree[next] == 0 {
                ready.insert(next);
            }
        }
    }
    let mut slots: Vec<Option<PreparedPlugin>> = candidates.into_iter().map(Some).collect();
    let mut ordered = Vec::with_capacity(order.len());
    for idx in order {
        if let Some(candidate) = slots[idx].take() {
            ordered.push(candidate);
        }
    }
    let cyclic: Vec<PreparedPlugin> = slots.into_iter().flatten().collect();
    (ordered, cyclic)
}

/// 已验证的候选持有本次安装使用的同一份声明；尚未加入任何宿主注册表。
pub(super) struct PreparedPlugin {
    pub(super) plugin: LoadedPlugin,
    pub(super) declaration: PluginDeclaration,
    pub(super) secret_fields: Vec<crate::plugin::PluginSecretField>,
}

pub(super) struct RejectedPlugin {
    pub(super) name: String,
    pub(super) display_name: String,
    pub(super) reason: String,
}

pub(super) struct PluginLoadPlan {
    pub(super) candidates: Vec<PreparedPlugin>,
    pub(super) rejected: Vec<RejectedPlugin>,
}

impl PluginHost {
    /// 发现仅创建已编译的 Rust 对象，不读取配置或修改数据库、注册表。
    pub(super) fn discover_plugin_instances() -> Result<Vec<LoadedPlugin>> {
        Ok(crate::plugin::source_loader::instantiate()?)
    }

    async fn candidate_extensions(
        plugin: &LoadedPlugin,
    ) -> Result<nuono_app::plugin_extensions::PluginExtensions> {
        Ok(Self::call_with_catch(async {
            let extensions = plugin.extensions();
            extensions.validate()?;
            Ok(extensions)
        })
        .await?)
    }

    async fn candidate_name(plugin: &LoadedPlugin) -> Result<String> {
        Ok(Self::call_with_catch(async { Ok(plugin.name().to_string()) }).await?)
    }

    async fn candidate_display_name(plugin: &LoadedPlugin, name: &str) -> String {
        Self::call_with_catch(async { Ok(plugin_display_name(name, plugin.display_name())) })
            .await
            .unwrap_or_else(|_| name.to_string())
    }

    async fn prepare_named_candidate(
        plugin: LoadedPlugin,
        name: String,
        display_name: String,
        extensions: nuono_app::plugin_extensions::PluginExtensions,
    ) -> Result<PreparedPlugin> {
        let (declaration, secret_fields) = Self::call_with_catch(async {
            let secret_fields = validate_secret_fields(&name, plugin.secret_fields())?;
            let declaration = collect_named_declaration(&plugin, name, display_name, extensions)?;
            Ok((declaration, secret_fields))
        })
        .await?;
        Ok(PreparedPlugin {
            plugin,
            declaration,
            secret_fields,
        })
    }

    /// 筛选只读已有身份，不调用生命周期、不发布 schema、不写配置或数据库。
    /// 保留工厂顺序与首个有效候选；无效候选的 disabled 收敛由安装阶段负责。
    /// 无法读取身份的 panic 无从安全归属，直接返回错误，不改变已有插件状态。
    pub(super) async fn resolve_load_candidates(
        &self,
        loaded: Vec<LoadedPlugin>,
    ) -> Result<PluginLoadPlan> {
        let mut seen: std::collections::HashSet<String> = self
            .plugins
            .read()
            .await
            .iter()
            .map(|plugin| plugin.name().to_owned())
            .collect();
        let mut candidates = Vec::new();
        let mut rejected = Vec::new();
        for plugin in loaded {
            let name = Self::candidate_name(&plugin).await?;
            if seen.contains(&name) {
                warn!(plugin = %name, "插件名重复，已跳过冲突实例");
                continue;
            }
            let display_name = Self::candidate_display_name(&plugin, &name).await;
            let extensions = Self::candidate_extensions(&plugin).await?;
            let protects_services = extensions.declares_service_policy();
            match Self::prepare_named_candidate(
                plugin,
                name.clone(),
                display_name.clone(),
                extensions,
            )
            .await
            {
                Ok(candidate) => {
                    seen.insert(name);
                    candidates.push(candidate);
                }
                Err(error) => {
                    // 已启用的服务策略插件一旦声明校验失败，启动时必须报错拦截，绝不能静默退回开源版的全放行默认策略。
                    if protects_services && self.db.is_plugin_enabled(&name).await? {
                        return Err(error);
                    }
                    rejected.push(RejectedPlugin {
                        name,
                        display_name,
                        reason: error.to_string(),
                    });
                }
            }
        }
        let (candidates, cyclic) = sort_candidates_topologically(candidates);
        for candidate in cyclic {
            let name = candidate.declaration.name;
            let display_name = candidate.declaration.display_name;
            let protects_services = candidate.declaration.extensions.declares_service_policy();
            let reason = "插件依赖存在循环，无法确定加载顺序".to_string();
            if protects_services && self.db.is_plugin_enabled(&name).await? {
                return Err(crate::error::NuonoError::PluginLoad {
                    plugin: name,
                    reason,
                });
            }
            rejected.push(RejectedPlugin {
                name,
                display_name,
                reason,
            });
        }
        Ok(PluginLoadPlan {
            candidates,
            rejected,
        })
    }

    /// 重建要求恰好一个同名候选，完整声明也必须在旧实例停用之前验证。
    pub(super) async fn resolve_reload_candidate(
        name: &str,
        loaded: Vec<LoadedPlugin>,
    ) -> Result<PreparedPlugin> {
        let mut matching = Vec::new();
        for plugin in loaded {
            if Self::candidate_name(&plugin).await? == name {
                matching.push(plugin);
            }
        }
        let plugin = single_reload_candidate(matching, name)?;
        let display_name = Self::candidate_display_name(&plugin, name).await;
        let extensions = Self::candidate_extensions(&plugin).await?;
        Self::prepare_named_candidate(plugin, name.to_string(), display_name, extensions).await
    }
}
