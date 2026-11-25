//! 离线只读的新旧发行包升级预检对比工具（不连接实际数据库，也不执行迁移演练）。
use super::{MigrationSet, manifest::ReleaseManifest, verify};
use anyhow::{Result, ensure};
use serde::Serialize;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

#[derive(Debug, Serialize)]
pub struct UpgradePreflight {
    pub current_release: String,
    pub candidate_release: String,
    pub blockers: Vec<String>,
    pub added_migrations: BTreeMap<String, Vec<String>>,
    pub added_extensions: Vec<String>,
    pub removed_extensions: Vec<String>,
    pub changed_extensions: Vec<String>,
    pub required_next_steps: Vec<String>,
}

pub fn preflight(current: &Path, candidate: &Path) -> Result<UpgradePreflight> {
    let current = verify(current)?;
    let candidate = verify(candidate)?;
    compare(&current, &candidate)
}

fn migrations(sets: &[MigrationSet]) -> Result<BTreeMap<&str, &MigrationSet>> {
    let mut result = BTreeMap::new();
    for set in sets {
        ensure!(!set.namespace.is_empty(), "迁移命名空间不能为空");
        ensure!(
            result.insert(set.namespace.as_str(), set).is_none(),
            "迁移命名空间重复: {}",
            set.namespace
        );
        let mut versions = BTreeSet::new();
        for migration in &set.migrations {
            ensure!(
                !migration.version.is_empty() && versions.insert(&migration.version),
                "迁移版本为空或重复: {}",
                set.namespace
            );
        }
    }
    ensure!(result.contains_key("framework"), "缺少框架迁移声明");
    Ok(result)
}

fn extensions(manifest: &ReleaseManifest) -> BTreeMap<String, String> {
    manifest
        .application
        .extensions
        .iter()
        .map(|extension| {
            let kind = match extension.kind {
                super::SourceKind::Plugin => "plugin",
                super::SourceKind::Adapter => "adapter",
            };
            (
                format!("{kind}:{}", extension.id),
                extension.source_hash.clone(),
            )
        })
        .collect()
}

fn compare(current: &ReleaseManifest, candidate: &ReleaseManifest) -> Result<UpgradePreflight> {
    let mut report = UpgradePreflight {
        current_release: current.release_id.clone(),
        candidate_release: candidate.release_id.clone(),
        blockers: Vec::new(),
        added_migrations: BTreeMap::new(),
        added_extensions: Vec::new(),
        removed_extensions: Vec::new(),
        changed_extensions: Vec::new(),
        required_next_steps: vec![
            "确认 current 是实际部署版本；本命令未连接运行实例或读取数据库迁移账本".into(),
            "停止写入后备份数据库、插件配置和 data 资产，并验证恢复路径".into(),
            "在隔离数据副本上执行候选版本迁移，验证数据库与必需扩展的本地初始化".into(),
            "安排维护窗口切换程序和对应 Dashboard；健康检查与外部连接分别确认".into(),
            "失败时协调恢复旧程序与升级前数据；迁移后不可仅替换旧可执行文件".into(),
        ],
    };
    if current.application.build.target_triple != candidate.application.build.target_triple {
        report
            .blockers
            .push("新旧发行的目标 OS、CPU 或 ABI 不一致".into());
    }
    let old = migrations(&current.application.migrations)?;
    let new = migrations(&candidate.application.migrations)?;
    for (namespace, before) in &old {
        match new.get(namespace) {
            Some(after)
                if after.migrations.len() >= before.migrations.len()
                    && before
                        .migrations
                        .iter()
                        .zip(&after.migrations)
                        .all(|(a, b)| {
                            a.version == b.version
                                && a.postgres_sha256 == b.postgres_sha256
                                && a.sqlite_sha256 == b.sqlite_sha256
                        }) => {}
            _ => report.blockers.push(format!(
                "迁移历史不兼容: {namespace}；已有版本必须保持顺序和双后端内容不变"
            )),
        }
    }
    for (namespace, after) in new {
        let count = old
            .get(namespace)
            .map(|set| set.migrations.len())
            .unwrap_or(0);
        let added = after
            .migrations
            .iter()
            .skip(count)
            .map(|migration| migration.version.clone())
            .collect::<Vec<_>>();
        if !added.is_empty() {
            report.added_migrations.insert(namespace.into(), added);
        }
    }
    let before = extensions(current);
    let after = extensions(candidate);
    report.added_extensions = after
        .keys()
        .filter(|key| !before.contains_key(*key))
        .cloned()
        .collect();
    report.removed_extensions = before
        .keys()
        .filter(|key| !after.contains_key(*key))
        .cloned()
        .collect();
    report.changed_extensions = after
        .iter()
        .filter(|(key, value)| before.get(*key).is_some_and(|old| old != *value))
        .map(|(key, _)| key.clone())
        .collect();
    for (kind, ids) in [
        ("plugin", &current.readiness.required_plugins),
        ("adapter", &current.readiness.required_adapters),
    ] {
        for id in ids {
            if !after.contains_key(&format!("{kind}:{id}")) {
                report
                    .blockers
                    .push(format!("候选发行缺少当前部署要求的扩展: {kind}:{id}"));
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
#[path = "../../tests/unit/release/preflight_tests.rs"]
mod tests;
