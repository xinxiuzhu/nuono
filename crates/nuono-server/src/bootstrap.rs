//! 服务启动组装与优雅停机流程（不包含具体 HTTP 路由处理或聊天平台协议解析）。
use crate::{
    AppState, Cli, api, backup, bot_instances, http_server, middleware::RateLimiter, startup,
    startup_progress,
};
use axum::serve::ListenerExt;
use metrics_exporter_prometheus::PrometheusBuilder;
use nuono_core::{config::ConfigManager, core::ConnectionManager, db::Database, telemetry};
use std::{
    net::SocketAddr,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};
const HTTP_DRAIN_TIMEOUT: Duration = Duration::from_secs(10);
mod http_drain;
pub(crate) mod service_lease;
/// 装配并运行完整服务，返回供外部 supervisor 解释的进程退出码。
///
/// 配置来自单个 `nuono.toml`，随后应用环境变量/secret file 覆盖，再构造任何带 I/O
/// 副作用的组件。数据库就绪后会导入并覆盖 secret（见 `6. 初始化数据库` 之后）。
/// 监听地址的管理员认证安全检查。
///
/// 在 CLI `--host/--port` 覆盖之后执行：配置文件校验看不见 CLI 覆盖后的真实监听地址，
/// 所以「非 loopback 监听必须启用管理员认证」仍需在这里重查，否则任何访客都能走
/// 初始化向导建立 owner。OneBot WS token 是可选配置；为空时允许匿名连接。
fn ensure_listen_security(
    admin_auth_enabled: bool,
    addr: &std::net::SocketAddr,
) -> anyhow::Result<()> {
    if !admin_auth_enabled && !addr.ip().is_loopback() {
        return Err(anyhow::anyhow!(
            "监听地址 {} 不是 loopback，必须启用 security.admin_auth_enabled",
            addr
        ));
    }
    Ok(())
}

/// 监听器退出后按适配器、调度器、任务、插件、数据库的顺序回收资源。
pub(crate) async fn async_main(cli: Cli) -> anyhow::Result<i32> {
    let startup_started = Instant::now();
    let config_stage = startup_progress::Stage::new("启动配置加载与校验");
    // 3. 加载 bootstrap 配置（TOML + 环境变量）。此阶段数据库里的运行设置与 secret
    //    尚未读回，只做 bootstrap 校验（bind 与连库必需的字段）；完整校验推迟到
    //    attach_database 发布有效配置时，否则密钥只存在数据库中的合法部署会被误判。
    let cfg_manager = ConfigManager::load(cli.config.map(std::path::PathBuf::from)).await?;
    config_stage.finish();

    let paths = nuono_core::core::Paths::root();
    // 明文密钥（runtime_secrets、SQLite 整库）的既定安全边界是文件系统权限：
    // 启动即建立/修复 data 与 logs 根目录的 0700，覆盖历史宽松安装。
    paths.ensure_private_dirs();
    let cfg = cfg_manager.get().await;
    let transfer_stage = startup_progress::Stage::new("待处理数据迁移检查");
    backup::transfer::apply_pending(&cfg.database.url, &paths.data, cfg_manager.config_path())
        .await
        .map_err(anyhow::Error::msg)?;
    transfer_stage.finish();
    let assets_stage = startup_progress::Stage::new("管理界面资产检查与迁移");
    let migrated = paths.migrate_legacy_hub_assets()?;
    assets_stage.finish();
    let cfg_arc = Arc::new(cfg_manager);

    // 4. 初始化日志系统。此时只能拿到 bootstrap 级别（TOML/环境变量）；数据库里的
    //    日志级别在 attach_database 之后重载生效（见下方 reload_filter），输出通道
    //    （stdout/file）属于 bootstrap 配置，修改后仍需重启。
    let telemetry_config = cfg.telemetry_config();
    if telemetry_config.console_output {
        startup::print_banner();
    }
    let telemetry_stage = startup_progress::Stage::new("日志系统初始化");
    let telemetry_guard = telemetry::init(telemetry_config);
    startup_progress::telemetry_ready();
    telemetry_stage.finish();
    if migrated > 0 {
        info!(directories = migrated, "📁 管理界面资产已迁移到 data/hub");
    }
    debug!(path = "data/configs", "框架配置文件已就绪");

    info!(target: "nuono::startup", "Nuono v{} 启动中…", env!("CARGO_PKG_VERSION"));
    debug!(console = cli.console, otel = cli.otel, "启动调试选项");

    // 5. 初始化 Prometheus metrics
    let recorder = PrometheusBuilder::new().build_recorder();
    let prom_handle = recorder.handle();
    metrics::set_global_recorder(recorder).ok();
    metrics::counter!("nuono_startup_total").increment(1);

    // 5.5 待恢复标记：上次会话请求的「从备份恢复」在这里执行 —— 必须在数据库
    // 打开之前原子换库。失败保留标记并停止启动，避免未完成的恢复继续接收请求。
    let restore_stage = startup_progress::Stage::new("待恢复备份检查");
    let restored_backup = match backup::apply_pending_restore(&cfg.database.url, &paths.data).await
    {
        Ok(Some(name)) => {
            info!(backup = %name, "🔁 已从备份恢复数据库，即将在此之上启动");
            Some(name)
        }
        Ok(None) => None,
        Err(e) => {
            return Err(anyhow::anyhow!("待恢复标记处理失败，已停止启动：{e}"));
        }
    };
    restore_stage.finish();

    // 6. 初始化数据库
    let database_started = Instant::now();
    info!(target: "nuono::startup", "正在连接数据库并检查迁移…");
    let db = Database::from_config(&cfg.database).await?;
    let mut external_assets = Vec::new();
    if cfg_arc.config_path().exists() {
        external_assets.push(nuono_app::plugin_manifest::PersistentAsset::Reference {
            key: "bootstrap.config".into(),
            path: std::fs::canonicalize(cfg_arc.config_path())?
                .to_string_lossy()
                .into_owned(),
            required: true,
        });
    }
    db.register_deployment_assets(&external_assets).await?;

    nuono_core::service::privacy_journal::PrivacyJournal::new(&paths.data)
        .replay(&db)
        .await?;
    let restart_receipt = crate::restart_receipt::load(&db).await;
    api::theme::initialize(&db, &paths).await?;
    info!(target: "nuono_storage", "数据库就绪：{}（连接及迁移完成，耗时 {:.2} 秒）",
        match db.backend() {
            nuono_plugin::orm::DbBackend::Postgres => "PostgreSQL",
            nuono_plugin::orm::DbBackend::Sqlite => "SQLite",
            nuono_plugin::orm::DbBackend::MySql => "MySQL",
            _ => "其他后端",
        }, database_started.elapsed().as_secs_f64());
    if let Some(name) = restored_backup.as_deref() {
        let _ = nuono_core::audit::record_event(
            &db,
            "database.restore_completed",
            None,
            None,
            serde_json::json!({ "filename": name }),
        )
        .await;
    }

    // 6.1 运行设置与 secret 以数据库为唯一可信来源。
    //
    // attach_database 完成「导入种子（仅首次）→ 读取运行设置与 secret → 应用环境覆盖
    // → 完整校验 → 发布内存快照」的完整流程；之后 Dashboard 的修改只写数据库，
    // `nuono.toml` 不再被改写。
    let settings_stage = startup_progress::Stage::new("数据库运行设置与密钥加载");
    cfg_arc.attach_database(db.clone()).await?;
    let mut cfg = cfg_arc.get().await;
    if cfg.ai.enabled || cfg.ai.assistant.enabled {
        db.ensure_ai_module().await?;
    }
    // 短验证码必须由服务端持有的 pepper 验证。首次开启注册时自动生成并持久化，后续由
    // runtime_secrets 作为可信来源；环境变量若显式提供则继续拥有最高优先级。
    if cfg.mail.registration_enabled && cfg.security.registration_otp_pepper.trim().is_empty() {
        use rand::TryRng;
        let mut bytes = [0u8; 32];
        rand::rngs::SysRng
            .try_fill_bytes(&mut bytes)
            .map_err(|e| anyhow::anyhow!("生成注册验证码 pepper 失败: {e}"))?;
        let pepper = hex::encode(bytes);
        // 走统一写路径：候选校验、数据库提交与内存发布在同一把锁内完成。
        cfg_arc
            .update_secret("security.registration_otp_pepper", Some(&pepper))
            .await?;
        cfg = cfg_arc.get().await;
        info!("🔐 已生成注册验证码 pepper");
    }
    // 日志级别属于运行设置：bootstrap 阶段的 telemetry::init 只拿到 TOML 值，
    // 这里重载数据库级别；显式 RUST_LOG 覆盖在整个启动过程保持有效。
    let startup_log_filter = std::env::var("RUST_LOG")
        .ok()
        .filter(|filter| !filter.trim().is_empty())
        .unwrap_or_else(|| cfg.log.level.clone());
    if let Err(e) = telemetry::reload_filter(&startup_log_filter) {
        warn!(error = %e, "应用日志级别失败，沿用启动时的级别");
    }
    info!(target: "nuono_core::config", "配置就绪：系统设置与密钥已加载");
    settings_stage.finish();

    // 9. 构建速率限制器
    let rate_limiter = Arc::new(RateLimiter::new());

    // CLI 监听覆盖只影响本进程 bind，不写回 ConfigManager 或 Dashboard 配置。
    let listen_host = cli.host.as_deref().unwrap_or(&cfg.host);
    let listen_port = cli.port.unwrap_or(cfg.port);
    let listen_ip: std::net::IpAddr = listen_host
        .parse()
        .map_err(|e| anyhow::anyhow!("监听地址无效: {}:{} ({})", listen_host, listen_port, e))?;
    let addr = std::net::SocketAddr::new(listen_ip, listen_port);

    // 配置校验只覆盖配置文件中的 host；CLI --host 覆盖发生在其之后，
    // 必须在此重新执行监听安全的 fail-closed 检查，否则 `--host 0.0.0.0`
    // 可以绕过安全校验把关闭认证的实例暴露到公网。
    ensure_listen_security(cfg.security.admin_auth_enabled, &addr)?;
    if cfg.security.ws_token.trim().is_empty() && !addr.ip().is_loopback() {
        warn!(
            listen.address = %addr,
            "OneBot 连接令牌未配置，允许匿名连接；请在系统设置中配置 WS Token，或限制端口的访问来源"
        );
    }

    // 9.1 账号初始化。
    //
    // 数据库为空时通过首次访问 Dashboard 的初始化向导创建首个 owner 账号。
    let account_stage = startup_progress::Stage::new("管理员账号初始化检查");
    let setup_required = db.user_count().await? == 0;
    account_stage.finish();

    // 当前宿主进程是该数据库的唯一执行者。在启动任何运行时、插件或消息接入层产生新任务前，
    // 先清理并结算上次异常退出遗留的未完成任务（绝不自动重放具有外部副作用的操作）。
    let recovery_stage = startup_progress::Stage::new("Agent 遗留任务检查");
    let recovered = db
        .reconcile_agent_runs_before_start("服务上次退出后未完成运行收尾，启动时标记中断")
        .await?;
    let assistant_recovered = db.reconcile_assistant_turns_before_start().await?;
    if assistant_recovered > 0 {
        info!(
            assistant_recovered,
            "个人对话已核对结果并结算额度；未完成消息不自动重放"
        );
    }
    if recovered.interrupted_runs > 0 || recovered.unresolved_executing_approvals > 0 {
        warn!(
            interrupted_runs = recovered.interrupted_runs,
            interrupted_roots = recovered.interrupted_roots,
            unresolved_executing_approvals = recovered.unresolved_executing_approvals,
            "Agent 遗留任务已核对；执行结果未知的审批保留原状，不自动重放"
        );
    }
    recovery_stage.finish();

    // 10. 构建应用状态
    // 注册插件配置管理器
    let plugin_config_manager = Arc::new(
        nuono_core::config::PluginConfigManager::with_legacy_database(
            paths.data.clone(),
            db.clone(),
        ),
    );

    // 7. 初始化连接管理器（使用 OneBot 适配器）
    let host_stage = startup_progress::Stage::new("宿主与插件初始化");
    // 短期媒体链接使用当前进程内密钥签名；此处用本地监听地址作为无 Bot 路由时的回退基准 URL
    // （实际 OneBot 网关连接后会记录网关自身可访问的 origin）。
    let media_link_base = format!(
        "http://{}",
        if addr.ip().is_unspecified() {
            std::net::SocketAddr::new(
                std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
                addr.port(),
            )
        } else {
            addr
        }
    );
    nuono_core::core::media_links::init_global(media_link_base);
    let adapter = nuono_adapter::running()
        .get("onebot")
        .and_then(|provider| provider.legacy_codec())
        .unwrap_or_else(|| Arc::new(crate::disabled_adapter::DisabledAdapter));
    let manager = ConnectionManager::new_prepared(
        db.clone(),
        paths.clone(),
        adapter,
        cfg_arc.clone(),
        plugin_config_manager.clone(),
    )
    .await?;
    host_stage.finish();

    // 必须在平台连接启动（可能立刻收到重启/停机指令）之前完成控制信号订阅；
    // 这样后续启动阶段发出的广播信号也能被主循环捕获。
    let shutdown_rx = manager.framework_control.subscribe();
    manager.core_commands.set_restart_service(Arc::new(
        crate::maintenance::ServerRestartService::new(&paths, manager.framework_control.clone())
            .with_restart_receipts(db.clone()),
    ));

    let adapters_stage = startup_progress::Stage::new("平台适配器与管理接口装配");
    let adapter_manager = Arc::new(nuono_core::adapter::AdapterManager::new());
    let bot_instances = Arc::new(bot_instances::BotInstanceRuntimeManager::new(
        db.clone(),
        adapter_manager.clone(),
        manager.event_kernel.clone(),
    ));

    let readiness = Arc::new(AtomicBool::new(false));
    let business_startup = crate::business_startup::BusinessStartup::new();
    let (http_started, http_start) = tokio::sync::oneshot::channel();
    let runtime_listen_port = Arc::new(OnceLock::new());
    let app_state = AppState {
        db: db.clone(),
        operations: crate::operations::OperationsMonitor::new(),
        paths: paths.clone(),
        identity_assets_lock: Arc::new(Mutex::new(())),
        asset_uploads: crate::asset_uploads::AssetUploads::new(),
        plugin_page_sessions: crate::api::plugin_pages::PageSessions::new(),
        restore_lock: Arc::new(Mutex::new(())),
        database_operations: crate::database_operations::DatabaseOperations::new(),
        manager: manager.clone(),
        config: cfg_arc.clone(),
        listen_port: runtime_listen_port.clone(),
        rate_limiter: rate_limiter.clone(),
        plugin_config: plugin_config_manager.clone(),
        adapter_manager: adapter_manager.clone(),
        bot_instances: bot_instances.clone(),
        sandbox: crate::sandbox::SandboxManager::new(
            manager.event_kernel.clone(),
            manager.wait_manager.clone(),
        ),
        readiness: readiness.clone(),
        business_startup: business_startup.clone(),
        started_at: Instant::now(),
        prometheus: prom_handle,
    };

    let operations_monitor = crate::operations::start(app_state.clone());

    let database_backup_lock = app_state.restore_lock.clone();
    let database_operations = app_state.database_operations.clone();
    let asset_uploads = app_state.asset_uploads.clone();
    let app = http_server::router(app_state);
    adapters_stage.finish();

    let mut background_tasks = crate::background_tasks::HostBackgroundTasks::new();
    crate::business_startup::register(
        &mut background_tasks,
        business_startup.clone(),
        http_start,
        manager.clone(),
        db.clone(),
        cfg_arc.clone(),
        bot_instances.clone(),
    );
    background_tasks.spawn(
        "restart-receipt",
        crate::restart_receipt::run(
            restart_receipt,
            db.clone(),
            manager.event_kernel.action_router(),
            readiness.clone(),
            background_tasks.token(),
        ),
    );
    crate::host_maintenance::start(
        &mut background_tasks,
        &manager,
        &db,
        &cfg_arc,
        &paths,
        database_backup_lock,
    );

    // 将端口监听与 HTTP 服务运行包在 `run_http_phase` 内：一旦绑定或运行出错，
    // 会先触发后台维护任务的取消令牌，确保在关闭底层依赖前优雅退出所有后台任务。
    let mut config_monitor = None;
    let mut artifact_maintenance = None;
    let serve_result = run_http_phase(&background_tasks, async {
        // 12. 启动 HTTP + WS 服务（支持优雅关闭）
        let listen_stage = startup_progress::Stage::new("HTTP 服务监听");
        let plugin_configs = manager.plugin_host.list_plugin_configs().await;
        let enabled_plugins = plugin_configs
            .iter()
            .filter(|plugin| plugin.enabled)
            .count();
        let listener = startup::bind_and_report(
            &addr,
            startup_started,
            enabled_plugins,
            plugin_configs.len() - enabled_plugins,
            manager.plugin_host.trigger_registry().count(),
        )
        .await?;
        let bound_addr = listener.local_addr()?;
        runtime_listen_port
            .set(bound_addr.port())
            .map_err(|_| anyhow::anyhow!("监听端口重复初始化"))?;
        listen_stage.finish();
        debug!(target: "nuono::startup", "启动流程累计耗时 {:.2} 秒（外层 cargo run 编译耗时另计）",
        startup_progress::elapsed().as_secs_f64());
        if setup_required {
            startup::print_setup_required_banner(&bound_addr);
        }
        config_monitor = Some(manager.plugin_host.start_config_monitor());
        artifact_maintenance = crate::maintenance::start_artifact_maintenance(&paths);
        let maintenance_cancellation = artifact_maintenance
            .as_ref()
            .map(|maintenance| maintenance.cancellation_flag());
        let shutdown_readiness = readiness.clone();
        let shutdown_business = business_startup.clone();
        let shutdown_manager = manager.clone();
        let background_stop = background_tasks.token();
        let shutdown_agent = manager.event_kernel.agent_runtime.clone();
        let shutdown_database_operations = database_operations.clone();
        let shutdown_asset_uploads = asset_uploads.clone();
        let shutdown_sources = bot_instances.clone();
        let (drain_started, draining) = tokio::sync::oneshot::channel();
        let connections = tokio_util::sync::CancellationToken::new();
        // `tap_io` 返回的 `TapIo` 实现了 `Connected`，从而保留中间件与 WebSocket 路由依赖的 `ConnectInfo<SocketAddr>` 提取能力。
        let listener = http_drain::DrainListener::new(listener, connections.clone()).tap_io(|_| {});
        let serve = axum::serve(
            listener,
            app.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            let signal = await_shutdown(shutdown_signal(), shutdown_rx, || {
                shutdown_readiness.store(false, Ordering::Release);
                shutdown_business.close();
                shutdown_manager.request_business_shutdown();
                shutdown_agent.request_shutdown();
                shutdown_database_operations.close();
                shutdown_asset_uploads.close();
                shutdown_sources.source.close();
                shutdown_sources.qr_logins.close();
                if let Some(cancelled) = maintenance_cancellation {
                    cancelled.store(true, Ordering::Release);
                }
                crate::source_build::shutdown_operations();
                background_stop.cancel();
                metrics::counter!("nuono_shutdown_total").increment(1);
            })
            .await;

            match signal {
                Some(nuono_core::core::FrameworkControlSignal::Restart { reason }) => {
                    info!(reason = %reason, "♻️ 收到框架重启请求，正在优雅关闭...");
                }
                Some(nuono_core::core::FrameworkControlSignal::Shutdown { reason }) => {
                    info!(reason = %reason, "🛑 收到框架关闭请求，正在优雅关闭...");
                }
                Some(_) | None => {
                    info!("🛑 收到停止信号，正在优雅关闭...");
                }
            }

            // 该信号发出后 Axum 立即停止接受新连接；
            // HTTP 连接排空倒计时从这一刻才开始计算，而非从服务启动时起算。
            let _ = drain_started.send(());
        });
        let serve = crate::business_startup::serve_with_readiness(
            serve,
            http_started,
            readiness.clone(),
            background_tasks.token(),
        );
        http_drain::wait_for_drain(serve, draining, HTTP_DRAIN_TIMEOUT, connections).await?;
        Ok(())
    })
    .await;

    // listener / serve 失败时也会走到这里。取消是幂等的。
    readiness.store(false, Ordering::Release);
    business_startup.close();
    manager.request_business_shutdown();
    manager.event_kernel.agent_runtime.request_shutdown();
    background_tasks.cancel();
    database_operations.close();
    asset_uploads.close();
    bot_instances.source.cancel_all().await;
    if serve_result.is_err() {
        if let Some(maintenance) = artifact_maintenance.as_ref() {
            maintenance
                .cancellation_flag()
                .store(true, Ordering::Release);
        }
        crate::source_build::shutdown_operations();
    }
    operations_monitor.abort();
    let _ = operations_monitor.await;
    if let Some(monitor) = config_monitor {
        monitor.abort();
        let _ = monitor.await;
    }
    if let Some(maintenance) = artifact_maintenance {
        maintenance.stop().await;
    }
    // 以下三道等待屏障刻意放在 HTTP 连接排空超时与下方 20 秒业务停机超时之外：
    // 已受理的后台维护轮次、数据库备份/恢复及素材落盘必须完整执行完毕，即便对应的 HTTP 响应连接已被排空关闭。
    background_tasks.wait().await;
    database_operations.wait().await;
    asset_uploads.wait().await;
    let shutdown_result = tokio::time::timeout(Duration::from_secs(20), async {
        let _ = db.add_log("INFO", "system", "Nuono 服务器正在关闭").await;
        bot_instances.shutdown_all().await;
        if let Err(e) = adapter_manager.stop_all().await {
            warn!(error = %e, "停止适配器失败");
        }
        if let Err(e) = manager.scheduler.shutdown().await {
            warn!(error = %e, "停止调度器失败");
        }
        manager.task_executor.shutdown().await;
        if let Err(e) = manager.event_kernel.agent_runtime.shutdown().await {
            warn!(error = %e, "Agent 运行收尾失败，下次启动将核对遗留状态");
        }
        if let Err(e) = manager.plugin_host.shutdown().await {
            warn!(error = %e, "停止插件宿主时部分插件生命周期清理失败");
        }
        if let Err(e) = db.shutdown().await {
            warn!(error = %e, "刷新数据库消息历史失败");
        }
    })
    .await;
    if shutdown_result.is_err() {
        warn!("关闭流程超过 20 秒，继续退出");
    }

    serve_result?;
    let exit_code = match manager.framework_control.last_signal() {
        Some(signal) if signal.is_restart() => crate::launcher::restart_exit_code(),
        _ => 0,
    };

    info!("✅ 服务器已停止");
    drop(telemetry_guard);
    Ok(exit_code)
}

// 若 HTTP 监听或运行阶段报错退出，必须先通知后台维护任务停止，
// 再由外层调用方按既定顺序清理资源并返回原始错误。
async fn run_http_phase(
    background: &crate::background_tasks::HostBackgroundTasks,
    phase: impl std::future::Future<Output = anyhow::Result<()>>,
) -> anyhow::Result<()> {
    let result = phase.await;
    background.cancel();
    result
}

async fn await_shutdown(
    os_signal: impl std::future::Future<Output = ()>,
    mut framework: tokio::sync::broadcast::Receiver<nuono_core::core::FrameworkControlSignal>,
    notify: impl FnOnce(),
) -> Option<nuono_core::core::FrameworkControlSignal> {
    let signal = tokio::select! {
        _ = os_signal => None,
        signal = framework.recv() => signal.ok(),
    };
    notify();
    signal
}

#[cfg(test)]
#[path = "../tests/unit/bootstrap/background_tests.rs"]
mod background_tests;

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut terminate = signal(SignalKind::terminate()).expect("注册 SIGTERM 失败");
        let mut interrupt = signal(SignalKind::interrupt()).expect("注册 SIGINT 失败");
        // 必须先注册系统信号监听器，再检查启动器停机标志：
        // 这样无论信号是在启动阶段到达，还是在注册监听器与检查标志之间到达，都不会漏掉唤醒。
        if crate::launcher::shutdown_requested() {
            return;
        }
        tokio::select! {
            _ = interrupt.recv() => {},
            _ = terminate.recv() => {},
        }
    }

    #[cfg(not(unix))]
    {
        if !crate::launcher::shutdown_requested() {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

// 跨 crate 契约测试：nuono-core 的 routed `build_send_msg_action` 产物，必须能被
// nuono-server 各平台真实 target parser 正确解析。两个方向（core builder 产物 + server
// 真实 parser）在这里同时执行，才能抓住两侧漂移。故意不复制 SDK 键名到期望后喂另一个
// 复制函数，而让 parser 读 builder 的原始输出。
//
// 注意：nuono-server 是 pure binary crate（无 `[lib]`），无法把这类测试放进 `tests/`
// 里做跨 crate 集成测试（integration test 链接不到能被访问的 lib target，且各 target
// parser 又是私有实现）。因此按契约要求就地放在各 helper 所在模块内，仍满足"同时执行
// core builder 与 server 真实 parser"的契约语义。

#[cfg(test)]
mod listen_security_tests {
    use super::ensure_listen_security;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    fn addr(ip: IpAddr) -> SocketAddr {
        SocketAddr::new(ip, 9923)
    }

    #[test]
    fn loopback_development_needs_no_token() {
        assert!(ensure_listen_security(true, &addr(IpAddr::V4(Ipv4Addr::LOCALHOST))).is_ok());
        assert!(ensure_listen_security(false, &addr(IpAddr::V4(Ipv4Addr::LOCALHOST))).is_ok());
    }

    #[test]
    fn non_loopback_requires_admin_auth() {
        let err = ensure_listen_security(false, &addr(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0))))
            .unwrap_err();
        assert!(err.to_string().contains("admin_auth_enabled"));
    }

    #[test]
    fn non_loopback_with_admin_auth_is_allowed_without_ws_token() {
        assert!(ensure_listen_security(true, &addr(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)))).is_ok());
    }
}
