use anyhow::{Context, Result, bail};
use regex::Regex;
use serde::Deserialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    path::Path,
    process::Command,
};

#[derive(Deserialize)]
struct Metadata {
    packages: Vec<Package>,
    workspace_members: BTreeSet<String>,
}

#[derive(Deserialize)]
struct Package {
    id: String,
    name: String,
    dependencies: Vec<Dependency>,
}

#[derive(Deserialize)]
struct Dependency {
    name: String,
    kind: Option<String>,
    #[serde(default)]
    optional: bool,
    #[serde(default)]
    features: Vec<String>,
    #[serde(default)]
    uses_default_features: bool,
}

type Workspace = BTreeMap<String, Package>;

fn dependencies<'a>(workspace: &'a Workspace, name: &str) -> BTreeSet<&'a str> {
    workspace[name]
        .dependencies
        .iter()
        .filter(|d| d.kind.as_deref() != Some("dev"))
        .map(|d| d.name.as_str())
        .collect()
}

fn closure<'a>(workspace: &'a Workspace, name: &str, seen: &mut BTreeSet<&'a str>) {
    for dependency in dependencies(workspace, name) {
        // 宿主的 Cargo feature 合并机制不应被误判为 SDK 引入的依赖；
        // 是否显式开启了数据库错误转换 feature 会在下方单独检查。
        if name == "nuono-error"
            && workspace[name]
                .dependencies
                .iter()
                .any(|d| d.name == dependency && d.optional)
        {
            continue;
        }
        if seen.insert(dependency) && workspace.contains_key(dependency) {
            closure(workspace, dependency, seen);
        }
    }
}

fn prohibit(
    workspace: &Workspace,
    name: &str,
    forbidden: &BTreeSet<&str>,
    errors: &mut Vec<String>,
) {
    let bad = dependencies(workspace, name)
        .intersection(forbidden)
        .copied()
        .collect::<Vec<_>>();
    if !bad.is_empty() {
        errors.push(format!("{name}: forbidden dependencies {bad:?}"));
    }
}

fn prohibit_chain(
    workspace: &Workspace,
    name: &str,
    forbidden: &BTreeSet<&str>,
    errors: &mut Vec<String>,
) {
    let mut reachable = BTreeSet::new();
    closure(workspace, name, &mut reachable);
    let bad: Vec<_> = reachable.intersection(forbidden).collect();
    if !bad.is_empty() {
        errors.push(format!(
            "{name}: forbidden production dependency chain to {bad:?}"
        ));
    }
    // 宿主开启的可选错误转换依赖在闭包遍历时会被忽略，
    // 但如果可达的业务 crate 显式启用了 `database` feature，则依然要拦截报错。
    if forbidden.contains("nuono-storage") || forbidden.contains("sqlx") {
        for owner in reachable.iter().copied().filter(|owner| *owner != name) {
            if let Some(package) = workspace.get(owner) {
                for dependency in &package.dependencies {
                    if dependency.kind.as_deref() != Some("dev")
                        && dependency.name == "nuono-error"
                        && dependency
                            .features
                            .iter()
                            .any(|feature| feature == "database")
                    {
                        errors.push(format!(
                        "{name}: production dependency {owner} enables database error conversions"
                    ));
                    }
                }
            }
        }
    }
}

fn check_dependencies(workspace: &Workspace, errors: &mut Vec<String>) -> Result<()> {
    for name in [
        "nuono-domain",
        "nuono-adapter",
        "nuono-app",
        "nuono-storage",
        "nuono-error",
        "nuono-core",
        "nuono-server",
        "nuono-tooling",
        "nuono-plugin",
        "nuono-plugin-test",
        "nuono-plugin-macros",
        "nuono-ai",
        "nuono-ai-provider",
        "nuono-rag",
        "nuono-memory",
    ] {
        if !workspace.contains_key(name) {
            bail!("missing workspace package: {name}");
        }
    }
    prohibit_chain(
        workspace,
        "nuono-tooling",
        &BTreeSet::from([
            "nuono-core",
            "nuono-server",
            "nuono-storage",
            "nuono-app",
            "nuono-plugin",
            "nuono-adapter",
            "sea-orm",
            "sqlx",
            "sqlx-core",
            "sqlx-postgres",
            "sqlx-sqlite",
            "axum",
            "reqwest",
            "utoipa",
        ]),
        errors,
    );
    prohibit(
        workspace,
        "nuono-core",
        &BTreeSet::from(["nuono-tooling"]),
        errors,
    );
    let installed_adapters: BTreeSet<_> = workspace
        .keys()
        .filter(|n| n.starts_with("nuono-adapter-"))
        .map(String::as_str)
        .collect();
    let adapters: BTreeSet<_> = BTreeSet::from([
        "nuono-adapter-onebot",
        "nuono-adapter-telegram",
        "nuono-adapter-discord",
        "nuono-adapter-feishu",
        "nuono-adapter-qq",
        "nuono-qq-sdk",
    ]);
    for host in ["nuono-core", "nuono-server"] {
        prohibit_chain(workspace, host, &adapters, errors);
    }
    let mut adapter_sdk_forbidden = BTreeSet::from([
        "nuono-core",
        "nuono-server",
        "nuono-storage",
        "nuono-plugin",
        "reqwest",
        "axum",
        "sqlx",
        "sqlx-core",
        "sqlx-postgres",
        "sqlx-sqlite",
    ]);
    adapter_sdk_forbidden.extend(&adapters);
    prohibit_chain(workspace, "nuono-adapter", &adapter_sdk_forbidden, errors);
    let domain_allowed = BTreeSet::from(["serde", "serde_json", "chrono", "thiserror"]);
    prohibit(
        workspace,
        "nuono-domain",
        &dependencies(workspace, "nuono-domain")
            .difference(&domain_allowed)
            .copied()
            .collect(),
        errors,
    );
    let mut app_forbidden = BTreeSet::from([
        "nuono-core",
        "nuono-storage",
        "sqlx",
        "axum",
        "reqwest",
        "libloading",
        "nuono-ai-provider",
    ]);
    let http_forbidden = BTreeSet::from([
        "reqwest",
        "axum",
        "tower",
        "tower-http",
        "hyper",
        "hyper-util",
        "ureq",
        "isahc",
        "surf",
        "awc",
        "attohttpc",
    ]);
    app_forbidden.extend(&http_forbidden);
    app_forbidden.extend(&adapters);
    prohibit(workspace, "nuono-app", &app_forbidden, errors);
    prohibit_chain(workspace, "nuono-app", &app_forbidden, errors);
    let mut ai_host_forbidden = BTreeSet::from([
        "nuono-app",
        "nuono-core",
        "nuono-server",
        "nuono-storage",
        "nuono-plugin",
        "nuono-plugin-test",
        "nuono-plugin-macros",
        "nuono-adapter",
        "nuono-qq-sdk",
    ]);
    ai_host_forbidden.extend(&adapters);
    let mut ai_forbidden = ai_host_forbidden.clone();
    ai_forbidden.extend([
        "sea-orm",
        "sea-query-sqlx",
        "sqlx",
        "sqlx-core",
        "sqlx-postgres",
        "sqlx-sqlite",
        "sqlx-mysql",
        "rusqlite",
        "postgres",
        "tokio-postgres",
        "libsqlite3-sys",
        "libpq-sys",
    ]);
    for name in ["nuono-ai", "nuono-ai-provider", "nuono-rag", "nuono-memory"] {
        let mut forbidden = ai_forbidden.clone();
        if name != "nuono-ai-provider" {
            forbidden.extend(&http_forbidden);
            forbidden.insert("nuono-ai-provider");
        }
        if matches!(name, "nuono-ai" | "nuono-ai-provider") {
            forbidden.insert("nuono-rag");
        }
        if name != "nuono-memory" {
            forbidden.insert("nuono-memory");
        }
        prohibit(workspace, name, &forbidden, errors);
        prohibit_chain(workspace, name, &forbidden, errors);
        for dependency in &workspace[name].dependencies {
            if dependency.kind.as_deref() == Some("dev")
                && ai_host_forbidden.contains(dependency.name.as_str())
            {
                errors.push(format!(
                    "{name}: forbidden test dependency on host/SDK {}",
                    dependency.name
                ));
            }
        }
        if name == "nuono-memory" {
            // 纯内存记忆测试无需底层基础设施即可运行（即便经过测试辅助模块）；
            // 因此其 dev-dependencies 也必须同样遵守生产环境的宿主、传输层和数据库隔离约束。
            let mut tests = BTreeSet::new();
            for dependency in &workspace[name].dependencies {
                if dependency.kind.as_deref() == Some("dev") {
                    tests.insert(dependency.name.as_str());
                    if workspace.contains_key(dependency.name.as_str()) {
                        closure(workspace, &dependency.name, &mut tests);
                    }
                    if dependency.name == "nuono-error"
                        && dependency
                            .features
                            .iter()
                            .any(|feature| feature == "database")
                    {
                        errors.push(format!(
                            "{name}: test dependency enables database error conversions"
                        ));
                    }
                }
            }
            let bad = tests.intersection(&forbidden).copied().collect::<Vec<_>>();
            if !bad.is_empty() {
                errors.push(format!(
                    "{name}: forbidden test dependency chain to {bad:?}"
                ));
            }
            for owner in tests.iter().copied() {
                if let Some(package) = workspace.get(owner)
                    && package.dependencies.iter().any(|dependency| {
                        dependency.kind.as_deref() != Some("dev")
                            && dependency.name == "nuono-error"
                            && dependency
                                .features
                                .iter()
                                .any(|feature| feature == "database")
                    })
                {
                    errors.push(format!(
                        "{name}: test dependency {owner} enables database error conversions"
                    ));
                }
            }
        }
    }
    prohibit(
        workspace,
        "nuono-storage",
        &BTreeSet::from(["nuono-core", "nuono-server", "nuono-plugin"]),
        errors,
    );
    for name in &installed_adapters {
        prohibit(
            workspace,
            name,
            &BTreeSet::from([
                "nuono-core",
                "nuono-storage",
                "nuono-server",
                "nuono-plugin",
            ]),
            errors,
        );
    }
    for name in [
        "nuono-app",
        "nuono-plugin",
        "nuono-plugin-test",
        "nuono-ai",
        "nuono-ai-provider",
        "nuono-rag",
        "nuono-memory",
    ] {
        for dep in &workspace[name].dependencies {
            if dep.name == "nuono-error"
                && dep.features.iter().any(|f| f == "database")
                && (name == "nuono-app" || dep.kind.as_deref() != Some("dev"))
            {
                errors.push(format!(
                    "{name}: must not request database error conversions"
                ));
            }
            if dep.kind.as_deref() == Some("dev") {
                continue;
            }
            if dep.name == "sea-orm" {
                let drivers: Vec<_> = dep
                    .features
                    .iter()
                    .filter(|f| f.starts_with("sqlx-") || f.starts_with("runtime-"))
                    .collect();
                if dep.uses_default_features || !drivers.is_empty() {
                    errors.push(format!(
                        "{name}: ORM must remain driver-free, requested {drivers:?}"
                    ));
                }
            }
        }
    }
    // 插件 SDK 保留对 nuono-app 的既有依赖（可间接共享记忆 DTO 类型），
    // 但特意禁止直接依赖 nuono-memory。
    prohibit(
        workspace,
        "nuono-plugin",
        &BTreeSet::from(["nuono-memory"]),
        errors,
    );
    let mut plugin_forbidden = BTreeSet::from([
        "nuono-tooling",
        "nuono-adapter",
        "nuono-core",
        "nuono-storage",
        "nuono-server",
        "sqlx",
        "sea-query-sqlx",
        "axum",
        "libloading",
        "nuono-qq-sdk",
        "nuono-ai-provider",
    ]);
    plugin_forbidden.extend(&adapters);
    for name in ["nuono-plugin", "nuono-plugin-test"] {
        prohibit(workspace, name, &plugin_forbidden, errors);
        prohibit_chain(workspace, name, &plugin_forbidden, errors);
    }
    prohibit(
        workspace,
        "nuono-core",
        &BTreeSet::from(["axum", "tower", "tower-http", "libloading", "sqlx"]),
        errors,
    );
    for name in ["nuono-server", "nuono-error", "nuono-storage"] {
        prohibit(workspace, name, &BTreeSet::from(["sqlx"]), errors);
    }
    let macros_allowed = BTreeSet::from(["syn", "quote", "proc-macro2"]);
    prohibit(
        workspace,
        "nuono-plugin-macros",
        &dependencies(workspace, "nuono-plugin-macros")
            .difference(&macros_allowed)
            .copied()
            .collect(),
        errors,
    );
    if !dependencies(workspace, "nuono-core").contains("nuono-plugin") {
        errors.push("nuono-core must use the shared plugin SDK directly".into());
    }
    Ok(())
}

fn source(root: &Path, path: &str) -> Result<String> {
    Ok(fs::read_to_string(root.join(path))
        .with_context(|| format!("read {path}"))?
        .split("#[cfg(test)]\nmod tests")
        .next()
        .unwrap_or_default()
        .to_owned())
}

fn check_pattern(
    root: &Path,
    path: &str,
    pattern: &str,
    message: &str,
    errors: &mut Vec<String>,
) -> Result<()> {
    if Regex::new(pattern)?.is_match(&source(root, path)?) {
        errors.push(format!("{path}: {message}"));
    }
    Ok(())
}

fn check_source_tree(
    root: &Path,
    path: &Path,
    forbidden: &Regex,
    message: &str,
    errors: &mut Vec<String>,
) -> Result<()> {
    if path.is_dir() {
        for entry in fs::read_dir(path)? {
            check_source_tree(root, &entry?.path(), forbidden, message, errors)?;
        }
    } else if path.extension().is_some_and(|e| e == "rs") {
        let relative = path.strip_prefix(root)?.to_string_lossy();
        let body = source(root, &relative)?
            .lines()
            .filter(|l| !l.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n");
        if forbidden.is_match(&body) {
            errors.push(format!("{relative}: {message}"));
        }
    }
    Ok(())
}

pub fn run(root: &Path) -> Result<()> {
    let output = Command::new("cargo")
        .args([
            "metadata",
            "--locked",
            "--all-features",
            "--format-version",
            "1",
        ])
        .current_dir(root)
        .output()
        .context("run cargo metadata")?;
    if !output.status.success() {
        bail!(
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let metadata: Metadata = serde_json::from_slice(&output.stdout)?;
    let workspace: Workspace = metadata
        .packages
        .into_iter()
        .filter(|p| metadata.workspace_members.contains(&p.id))
        .map(|p| (p.name.clone(), p))
        .collect();
    let mut errors = Vec::new();
    check_dependencies(&workspace, &mut errors)?;
    // 校验各 crate 内部的模块依赖边界：兼容性重导出必须显式声明，底层构建模块不得反向依赖上层调用方。
    for (paths, pattern, message) in [
        (
            &[
                "crates/nuono-plugin/src/lib.rs",
                "crates/nuono-plugin/src/agent.rs",
            ][..],
            r"(?s)\bpub\s+use\s+nuono_app\s*::\s*agent\b[^;]*\*",
            "SDK Agent contracts and legacy paths must be exported explicitly",
        ),
        (
            &["crates/nuono-core/src/config/plugin_files.rs"][..],
            r"\bcrate\s*::\s*service\s*::",
            "configuration files publish snapshots; consumers own derived-cache invalidation",
        ),
        (
            &[
                "crates/nuono-server/src/source_build.rs",
                "crates/nuono-tooling/src/source_build",
                "crates/nuono-tooling/src/source_build.rs",
            ][..],
            r"\b(?:crate|super)\s*::\s*(?:api|source_operations|source_startup)\b|\b(?:axum|utoipa_axum|AppState)\b",
            "source build mechanisms must not depend on HTTP or operation/startup orchestration",
        ),
        (
            &[
                "crates/nuono-server/src/source_operations.rs",
                "crates/nuono-server/src/source_operations",
            ][..],
            r"\b(?:crate|super)\s*::\s*(?:api|source_startup)\b|\b(?:axum|utoipa_axum|AppState)\b",
            "source operations use host capabilities and build mechanisms, not HTTP or startup",
        ),
        (
            &["crates/nuono-server/src/source_startup.rs"][..],
            r"\bcrate\s*::\s*api\b|\b(?:axum|utoipa_axum|AppState)\b",
            "source startup must not depend on HTTP",
        ),
    ] {
        let forbidden = Regex::new(pattern)?;
        for path in paths {
            check_source_tree(root, &root.join(path), &forbidden, message, &mut errors)?;
        }
    }
    let sdk_path = "crates/nuono-plugin/src/lib.rs";
    if fs::read_to_string(root.join(sdk_path))?.contains("PLUGIN_ABI_VERSION") {
        errors.push(format!(
            "{sdk_path}: legacy Rust trait-object ABI must not be reintroduced"
        ));
    }
    let query = Regex::new(r"\.orm\s*\(|Entity\s*::")?;
    for path in [
        "crates/nuono-core/src/core/filter.rs",
        "crates/nuono-core/src/service/groups.rs",
        "crates/nuono-core/src/service/bot_instances.rs",
        "crates/nuono-core/src/service/messages.rs",
        "crates/nuono-core/src/service/bots.rs",
        "crates/nuono-core/src/service/plugins.rs",
        "crates/nuono-core/src/service/plugin_debug.rs",
        "crates/nuono-core/src/service/security.rs",
        "crates/nuono-core/src/service/tasks.rs",
        "crates/nuono-core/src/service/scheduler.rs",
        "crates/nuono-core/src/service/stats.rs",
        "crates/nuono-core/src/service/group_plugins.rs",
        "crates/nuono-core/src/service/pubsub/subscriptions.rs",
    ] {
        let body = source(root, path)?
            .lines()
            .filter(|l| !l.starts_with("use sea_orm") && !l.starts_with("use crate::entity"))
            .collect::<Vec<_>>()
            .join("\n");
        if query.is_match(&body) {
            errors.push(format!(
                "{path}: production ORM query belongs in nuono-storage"
            ));
        }
    }
    let ai_api_dir = root.join("crates/nuono-server/src/api/ai");
    check_source_tree(
        root,
        &ai_api_dir,
        &Regex::new(r"OpenAiCompatibleProvider\s*::\s*(?:new|from_config)\s*\(")?,
        "provider construction belongs in the core AI service",
        &mut errors,
    )?;
    check_source_tree(
        root,
        &ai_api_dir,
        &Regex::new(
            r"\bfn\s+(?:build_provider|select_routed_model|route_aliases|model_matches_route_alias)\b",
        )?,
        "model selection belongs in the core AI service",
        &mut errors,
    )?;
    let concrete_adapter_import = Regex::new(
        r"\b(?:use|extern\s+crate)\s+nuono_(?:adapter_(?:onebot|telegram|discord|feishu|qq)|qq_sdk)\b",
    )?;
    for path in ["crates/nuono-core/src", "crates/nuono-server/src"] {
        check_source_tree(
            root,
            &root.join(path),
            &concrete_adapter_import,
            "concrete source adapters must not be dependencies of the host",
            &mut errors,
        )?;
    }
    check_pattern(
        root,
        "crates/nuono-core/src/domain/normalizer.rs",
        r"\bfn\s+(?:normalize_(?:onebot|telegram|discord|feishu|qq_official)|feishu_namespace|message_id|event_id|occurred_at)\b",
        "native event rules belong in adapters",
        &mut errors,
    )?;
    check_pattern(
        root,
        "crates/nuono-app/src/platform/normalization.rs",
        r"Platform\s*::\s*(?:OneBot|Telegram|Discord|Feishu|QqOfficial)",
        "shared event assembly must not contain platform branches",
        &mut errors,
    )?;
    let account_api_dir = root.join("crates/nuono-server/src/api/account");
    for mutation in [
        "set_user_password",
        "mark_email_verified",
        "set_totp_secret",
        "set_totp_state",
        "consume_totp_step",
        "consume_user_token",
        "release_user_token",
        "revoke_user_sessions",
        "create_user",
        "set_user_email",
        "issue_user_token",
    ] {
        let pattern = Regex::new(&format!(r"\.\s*{}\s*\(", regex::escape(mutation)))?;
        let message =
            format!("account HTTP handler must not perform credential mutation: {mutation}(");
        check_source_tree(root, &account_api_dir, &pattern, &message, &mut errors)?;
    }
    if !errors.is_empty() {
        bail!("{}", errors.join("\n"));
    }
    println!("Architecture dependency checks passed.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_boundary_checks_cover_nested_sources_without_rejecting_test_fixtures() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::create_dir(root.join("build")).unwrap();
        fs::write(
            root.join("build/mod.rs"),
            "// crate::api is forbidden in production.\nfn build() {}\n#[cfg(test)]\nmod tests { use crate::api::Fixture; }\n",
        )
        .unwrap();
        let child = root.join("build/recovery.rs");
        fs::write(&child, "use crate::api::recover_before_build;\n").unwrap();
        let forbidden = Regex::new(r"\bcrate\s*::\s*api\s*::").unwrap();
        let mut errors = Vec::new();
        check_source_tree(
            root,
            &root.join("build"),
            &forbidden,
            "HTTP dependency",
            &mut errors,
        )
        .unwrap();
        assert_eq!(errors, ["build/recovery.rs: HTTP dependency"]);

        fs::write(&child, "use crate::source_build::application_lock;\n").unwrap();
        errors.clear();
        check_source_tree(
            root,
            &root.join("build"),
            &forbidden,
            "HTTP dependency",
            &mut errors,
        )
        .unwrap();
        assert!(errors.is_empty());
    }

    fn package(name: &str, dependencies: serde_json::Value) -> Package {
        serde_json::from_value(serde_json::json!({
            "id": name, "name": name, "dependencies": dependencies,
        }))
        .unwrap()
    }

    fn valid_workspace() -> Workspace {
        let mut workspace: Workspace = [
            "nuono-domain",
            "nuono-adapter",
            "nuono-app",
            "nuono-storage",
            "nuono-error",
            "nuono-core",
            "nuono-server",
            "nuono-tooling",
            "nuono-plugin",
            "nuono-plugin-test",
            "nuono-plugin-macros",
            "nuono-ai",
            "nuono-ai-provider",
            "nuono-rag",
            "nuono-memory",
        ]
        .into_iter()
        .map(|name| (name.into(), package(name, serde_json::json!([]))))
        .collect();
        for (name, dependencies) in [
            ("nuono-core", vec!["nuono-plugin", "nuono-ai-provider"]),
            (
                "nuono-plugin",
                vec!["nuono-app", "nuono-ai", "nuono-rag", "reqwest"],
            ),
            ("nuono-plugin-test", vec!["nuono-plugin"]),
            ("nuono-app", vec!["nuono-ai", "nuono-rag", "nuono-memory"]),
            ("nuono-storage", vec!["nuono-app", "nuono-memory"]),
            ("nuono-memory", vec!["nuono-ai", "nuono-rag", "nuono-error"]),
            ("nuono-ai-provider", vec!["nuono-ai", "reqwest"]),
            ("nuono-rag", vec!["nuono-ai"]),
        ] {
            workspace.insert(
                name.into(),
                package(
                    name,
                    serde_json::Value::Array(
                        dependencies
                            .into_iter()
                            .map(|name| serde_json::json!({"name": name, "kind": null}))
                            .collect(),
                    ),
                ),
            );
        }
        workspace
    }

    fn add_dependency(workspace: &mut Workspace, owner: &str, dependency: serde_json::Value) {
        workspace
            .get_mut(owner)
            .unwrap()
            .dependencies
            .push(serde_json::from_value(dependency).unwrap());
    }

    fn dependency_errors(workspace: &Workspace) -> Vec<String> {
        let mut errors = Vec::new();
        check_dependencies(workspace, &mut errors).unwrap();
        errors
    }

    fn rejects(workspace: &Workspace, owner: &str, dependency: &str) {
        let errors = dependency_errors(workspace);
        assert!(
            errors
                .iter()
                .any(|error| error.starts_with(&format!("{owner}:")) && error.contains(dependency)),
            "{owner} should reject {dependency}: {errors:?}"
        );
    }

    #[test]
    fn valid_workspace_passes_and_forbidden_edges_are_rejected() {
        assert!(dependency_errors(&valid_workspace()).is_empty());

        let mut bad_tooling = valid_workspace();
        add_dependency(
            &mut bad_tooling,
            "nuono-tooling",
            serde_json::json!({"name": "nuono-server", "kind": null}),
        );
        rejects(&bad_tooling, "nuono-tooling", "nuono-server");

        let mut bad_ai = valid_workspace();
        add_dependency(
            &mut bad_ai,
            "nuono-ai",
            serde_json::json!({"name": "nuono-storage", "kind": null}),
        );
        rejects(&bad_ai, "nuono-ai", "nuono-storage");

        let mut bad_sdk = valid_workspace();
        add_dependency(
            &mut bad_sdk,
            "nuono-plugin",
            serde_json::json!({"name": "nuono-ai-provider", "kind": null}),
        );
        rejects(&bad_sdk, "nuono-plugin", "nuono-ai-provider");
    }

    #[test]
    fn sdk_closure_ignores_dev_and_host_optional_conversions_but_follows_production() {
        let workspace = BTreeMap::from([
            (
                "sdk".into(),
                package(
                    "sdk",
                    serde_json::json!([
                        {"name":"nuono-error","kind":null}, {"name":"test-host","kind":"dev"}, {"name":"bridge","kind":null}
                    ]),
                ),
            ),
            (
                "nuono-error".into(),
                package(
                    "nuono-error",
                    serde_json::json!([
                        {"name":"sqlx","kind":null,"optional":true}, {"name":"thiserror","kind":null}
                    ]),
                ),
            ),
            (
                "bridge".into(),
                package(
                    "bridge",
                    serde_json::json!([
                        {"name":"axum","kind":"build"}, {"name":"sdk","kind":null}
                    ]),
                ),
            ),
        ]);
        let mut seen = BTreeSet::new();
        closure(&workspace, "sdk", &mut seen);
        assert!(seen.contains("axum"));
        assert!(seen.contains("thiserror"));
        assert!(!seen.contains("sqlx"));
        assert!(!seen.contains("test-host"));
    }
}
