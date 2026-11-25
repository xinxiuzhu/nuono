use crate::AppState;
use crate::api::ApiResponse;
use crate::api::input::ApiJson as Json;
use axum::extract::State;
use serde::Serialize;
use utoipa_axum::{router::OpenApiRouter, routes};

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(sys_info))
        .routes(routes!(system_status))
        .routes(routes!(top_commands))
        .routes(routes!(top_groups))
        .routes(routes!(cooldown_status))
        .routes(routes!(operations))
        .routes(routes!(disk_usage))
        .routes(routes!(clean_build_cache))
}

#[utoipa::path(get, path = "/operations", operation_id = "system_operations", tag = "system",
    responses((status = 200, description = "运行健康及待处理事项", body = ApiResponse<crate::operations::OperationsSnapshot>)))]
async fn operations(
    State(state): State<AppState>,
) -> crate::api::ApiReply<crate::operations::OperationsSnapshot> {
    ApiResponse::ok(state.operations.snapshot().await)
}

#[utoipa::path(get, path = "/disk", operation_id = "system_disk_usage", tag = "system",
    responses((status = 200, description = "受限扫描得到的磁盘空间与目录占用", body = ApiResponse<crate::operations::DiskSnapshot>)))]
async fn disk_usage(
    State(state): State<AppState>,
) -> crate::api::ApiReply<crate::operations::DiskSnapshot> {
    match state.operations.disk(&state).await {
        Ok(value) => ApiResponse::ok(value),
        Err(error) => ApiResponse::internal(error),
    }
}

#[derive(serde::Deserialize, utoipa::ToSchema)]
struct CleanBuildCacheRequest {
    confirmation: String,
}

#[utoipa::path(post, path = "/build-cache/clean", operation_id = "system_clean_build_cache", tag = "system",
    request_body = CleanBuildCacheRequest,
    responses((status = 200, description = "清理可重建的 Cargo 缓存；构建中拒绝清理", body = ApiResponse<crate::operations::CleanResult>)))]
async fn clean_build_cache(
    State(state): State<AppState>,
    Json(body): Json<CleanBuildCacheRequest>,
) -> crate::api::ApiReply<crate::operations::CleanResult> {
    match state.operations.clean(&state, &body.confirmation).await {
        Ok(value) => ApiResponse::ok(value),
        Err(error) => ApiResponse::from_err(&error),
    }
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = api::system::SysInfoResponse)]
pub struct SysInfoResponse {
    pub version: String,
    /// 保留给旧版客户端兼容的 CPU 架构字段。
    pub target: String,
    pub build_profile: String,
    pub rust_version: Option<String>,
    pub compiler: Option<String>,
    pub compiler_commit: Option<String>,
    pub llvm_version: Option<String>,
    pub target_triple: Option<String>,
    pub build_host: Option<String>,
    pub operating_system: Option<String>,
    pub kernel_version: Option<String>,
    pub architecture: String,
    pub executable_path: Option<String>,
    pub pid: u32,
    /// 实际绑定的端口，包括命令行覆盖或操作系统分配的端口。
    pub server_port: Option<u16>,
    pub dependencies: Vec<CoreDependencyResponse>,
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = api::system::CoreDependencyResponse)]
pub struct CoreDependencyResponse {
    pub name: String,
    pub versions: Vec<String>,
}

fn system_info_snapshot(server_port: Option<u16>) -> SysInfoResponse {
    let metadata = crate::build_metadata::current();
    let (operating_system, kernel_version) =
        nuono_core::core::system_monitor::operating_system_info();
    let executable_path = crate::source_build::running_executable()
        .or_else(|| std::env::current_exe().ok())
        .map(|path| path.to_string_lossy().into_owned());
    SysInfoResponse {
        version: env!("CARGO_PKG_VERSION").into(),
        target: std::env::consts::ARCH.into(),
        build_profile: metadata.build_profile.into(),
        rust_version: metadata.rust_version.map(str::to_owned),
        compiler: metadata.compiler.map(str::to_owned),
        compiler_commit: metadata.compiler_commit.map(str::to_owned),
        llvm_version: metadata.llvm_version.map(str::to_owned),
        target_triple: metadata.target_triple.map(str::to_owned),
        build_host: metadata.build_host.map(str::to_owned),
        operating_system,
        kernel_version,
        architecture: std::env::consts::ARCH.into(),
        executable_path,
        pid: std::process::id(),
        server_port,
        dependencies: metadata
            .dependencies
            .iter()
            .map(|dependency| CoreDependencyResponse {
                name: dependency.name.into(),
                versions: dependency
                    .versions
                    .iter()
                    .map(|version| (*version).into())
                    .collect(),
            })
            .collect(),
    }
}

#[utoipa::path(
    get,
    path = "/info",
    operation_id = "system_sys_info",
    tag = "system",
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<SysInfoResponse>))
)]
async fn sys_info(State(state): State<AppState>) -> crate::api::ApiReply<SysInfoResponse> {
    ApiResponse::ok(system_info_snapshot(state.listen_port.get().copied()))
}

#[cfg(test)]
mod environment_tests {
    use super::*;

    #[test]
    fn runtime_snapshot_reports_this_process_and_compiled_metadata() {
        let snapshot = system_info_snapshot(Some(12345));
        let metadata = crate::build_metadata::current();
        assert_eq!(snapshot.pid, std::process::id());
        assert_eq!(snapshot.server_port, Some(12345));
        assert_eq!(snapshot.architecture, std::env::consts::ARCH);
        assert_eq!(snapshot.target, snapshot.architecture);
        assert_eq!(snapshot.build_profile, metadata.build_profile);
        assert_eq!(snapshot.rust_version.as_deref(), metadata.rust_version);
        assert_eq!(snapshot.compiler.as_deref(), metadata.compiler);
        assert!(
            snapshot
                .compiler
                .as_deref()
                .is_some_and(|value| value.starts_with("rustc "))
        );
        assert!(snapshot.rust_version.is_some());
        assert!(snapshot.target_triple.is_some());
        assert!(snapshot.operating_system.is_some());
        let executable = crate::source_build::running_executable()
            .unwrap_or_else(|| std::env::current_exe().unwrap());
        assert_eq!(
            snapshot.executable_path,
            Some(executable.to_string_lossy().into_owned())
        );
        let json = serde_json::to_value(snapshot).unwrap();
        assert!(json["pid"].is_u64());
        assert_eq!(json["server_port"], 12345);
        assert!(json["dependencies"].is_array());
        let unknown_port = serde_json::to_value(system_info_snapshot(None)).unwrap();
        assert_eq!(
            unknown_port.get("server_port"),
            Some(&serde_json::Value::Null)
        );
    }
}

#[utoipa::path(
    get,
    path = "/status",
    operation_id = "system_system_status",
    tag = "system",
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<nuono_core::SystemStatus>))
)]
async fn system_status(
    State(state): State<AppState>,
) -> crate::api::ApiReply<nuono_core::SystemStatus> {
    let status = state.manager.system_monitor.get_status();
    ApiResponse::ok(status)
}

#[utoipa::path(
    get,
    path = "/top-commands",
    operation_id = "system_top_commands",
    tag = "system",
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<Vec<super::docs::CountEntry>>))
)]
async fn top_commands(State(state): State<AppState>) -> crate::api::ApiReply<Vec<(String, u64)>> {
    let top = state
        .manager
        .plugin_host
        .command_stats
        .top_commands("all", "all", 20);
    ApiResponse::ok(top)
}

#[utoipa::path(
    get,
    path = "/top-groups",
    operation_id = "system_top_groups",
    tag = "system",
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<Vec<super::docs::CountEntry>>))
)]
async fn top_groups(State(state): State<AppState>) -> crate::api::ApiReply<Vec<(String, u64)>> {
    let top = state
        .manager
        .plugin_host
        .command_stats
        .top_groups("all", "all", 20);
    ApiResponse::ok(top)
}

#[derive(Debug, Serialize, utoipa::ToSchema)]
#[schema(as = api::system::CooldownStatus)]
pub struct CooldownStatus {
    pub active_user_cooldowns: usize,
}

#[utoipa::path(
    get,
    path = "/cooldowns",
    operation_id = "system_cooldown_status",
    tag = "system",
    responses((status = 200, description = "操作结果；请同时检查 success 与 error", body = ApiResponse<CooldownStatus>))
)]
async fn cooldown_status(State(state): State<AppState>) -> crate::api::ApiReply<CooldownStatus> {
    ApiResponse::ok(CooldownStatus {
        active_user_cooldowns: state
            .manager
            .plugin_host
            .cooldown_tracker
            .user_cooldown_count(),
    })
}
