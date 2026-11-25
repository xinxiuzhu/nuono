//! 不可变的独立应用发行包构建与校验模块；运行时声明信息直接提取自编译好的候选应用二进制。
mod build;
mod manifest;
mod preflight;
#[cfg(test)]
#[path = "../../tests/unit/release/tests.rs"]
mod tests;
use crate::source_build::{self, SourceKind, SourcePlugin};
use anyhow::{Context, Result, ensure};
pub use build::build;
pub use manifest::verify;
pub use preflight::{UpgradePreflight, preflight};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
pub fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Migration {
    pub version: String,
    pub description: String,
    pub postgres_sha256: String,
    pub sqlite_sha256: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MigrationSet {
    pub namespace: String,
    pub migrations: Vec<Migration>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApplicationDescription {
    pub schema_version: u32,
    pub framework_version: String,
    pub build: BuildFacts,
    pub extensions: Vec<SourcePlugin>,
    pub migrations: Vec<MigrationSet>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildFacts {
    pub lock_sha256: Option<String>,
    pub rust_version: Option<String>,
    pub compiler: Option<String>,
    pub compiler_commit: Option<String>,
    pub llvm_version: Option<String>,
    pub target_triple: Option<String>,
    pub build_host: Option<String>,
    pub build_profile: String,
    pub dependencies: Vec<Dependency>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Dependency {
    pub name: String,
    pub versions: Vec<String>,
}
