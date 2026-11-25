//! 离线、不可变的应用发行包自检与清单生成（仅做静态声明与校验，不启动服务也不执行数据库迁移）。

use crate::{
    build_metadata,
    source_build::{self, SourceKind},
};
use anyhow::{Context, Result, ensure};
use nuono_core::plugin::source_loader::PluginFactory;

use nuono_tooling::release::verify;
use nuono_tooling::release::{ApplicationDescription, Migration, MigrationSet, digest};

/// 仅调用插件无参构造函数读取静态元数据与迁移声明，不初始化 `PluginContext`、异步运行时或数据库连接。
pub(crate) fn describe(factories: Option<&[PluginFactory]>) -> Result<()> {
    let factories =
        factories.context("源码启动器没有完整应用清单，请对已构建的应用使用 release describe")?;
    let description = collect(factories)?;
    println!("{}", serde_json::to_string(&description)?);
    Ok(())
}

pub(super) fn collect(factories: &[PluginFactory]) -> Result<ApplicationDescription> {
    let declarations = std::panic::catch_unwind(|| {
        factories
            .iter()
            .map(|factory| {
                let plugin = factory();
                let migrations = plugin.migrations();
                nuono_plugin::PluginMigrationForBackend::validate_all(&migrations)?;
                Ok((plugin.name().to_owned(), migrations))
            })
            .collect::<Result<Vec<_>>>()
    })
    .map_err(|_| anyhow::anyhow!("插件工厂或迁移声明 panic，未生成发行清单"))??;
    let extensions = source_build::running_manifest();
    ensure!(
        extensions
            .iter()
            .filter(|s| s.kind == SourceKind::Plugin)
            .count()
            == declarations.len(),
        "插件工厂与应用清单不一致"
    );
    let mut sets = vec![MigrationSet {
        namespace: "framework".into(),
        migrations: nuono_core::db::framework_migration_sources()
            .into_iter()
            .map(|(name, pg, sqlite)| Migration {
                version: name.into(),
                description: name.into(),
                postgres_sha256: digest(pg.as_bytes()),
                sqlite_sha256: digest(sqlite.as_bytes()),
            })
            .collect(),
    }];
    let mut names = std::collections::BTreeSet::new();
    for (name, mut migrations) in declarations {
        ensure!(names.insert(name.clone()), "重复插件运行身份: {name}");
        let source = extensions
            .iter()
            .find(|s| s.kind == SourceKind::Plugin && s.runtime_name.as_deref() == Some(&name))
            .with_context(|| format!("插件 {name} 缺少源码安装身份"))?;
        migrations.sort_by_key(|migration| migration.version);
        sets.push(MigrationSet {
            namespace: format!("plugin:{}", source.id),
            migrations: migrations
                .into_iter()
                .map(|m| Migration {
                    version: m.version.to_string(),
                    description: m.description,
                    postgres_sha256: digest(m.pg_sql.as_bytes()),
                    sqlite_sha256: digest(m.sqlite_sql.as_bytes()),
                })
                .collect(),
        });
    }
    sets.sort_by(|a, b| a.namespace.cmp(&b.namespace));
    Ok(ApplicationDescription {
        schema_version: 1,
        framework_version: env!("CARGO_PKG_VERSION").into(),
        build: serde_json::from_value(serde_json::to_value(build_metadata::current())?)?,
        extensions,
        migrations: sets,
    })
}

static STATIC_DIR: std::sync::OnceLock<std::path::PathBuf> = std::sync::OnceLock::new();
pub(crate) fn static_dir() -> Option<&'static std::path::Path> {
    STATIC_DIR.get().map(std::path::PathBuf::as_path)
}

/// 校验发行包目录完整性，并确保服务在发行包目录之外的独立工作目录中运行（保持发行包只读，配置与业务数据写入外部工作目录）。
pub(crate) fn prepare_runtime() -> Result<()> {
    let executable = std::env::current_exe()?;
    let parent = executable.parent().context("程序缺少所在目录")?;
    if !parent.join("release.json").try_exists()? {
        return Ok(());
    }
    let manifest = verify(parent)?;
    ensure!(
        executable.file_name().and_then(|name| name.to_str()) == Some(&manifest.executable),
        "当前程序不是发行清单声明的入口"
    );
    ensure!(
        !std::env::current_dir()?
            .canonicalize()?
            .starts_with(parent.canonicalize()?),
        "请从发行目录之外的独立运行目录启动，配置和数据不能写入发行包"
    );
    ensure!(
        serde_json::to_value(build_metadata::current())?
            == serde_json::to_value(&manifest.application.build)?,
        "运行程序的构建信息与发行清单不一致"
    );
    ensure!(
        source_build::same_sources(
            &source_build::running_manifest(),
            &manifest.application.extensions
        ),
        "运行程序扩展与发行清单不一致"
    );
    STATIC_DIR
        .set(parent.join("nuono-hub/dist"))
        .map_err(|_| anyhow::anyhow!("发行运行路径已经初始化"))?;
    Ok(())
}
