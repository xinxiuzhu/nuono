use super::*;

fn connection_checks(
    issues: &[HealthIssue],
    healthy: &[&str],
    seen: &mut BTreeSet<String>,
) -> ConnectionChecks {
    let mut checks = ConnectionChecks {
        checked_kinds: BTreeSet::from(["onebot", "bot"]),
        healthy: healthy
            .iter()
            .map(|id| ((*id).into(), format!("{id} 在线")))
            .collect(),
        ..Default::default()
    };
    defer_initial_connections(&mut checks, issues, seen);
    checks
}

fn connection_problem(id: &str) -> HealthIssue {
    issue(id, "bots", "warning", "未连接", "连接断开", ROBOTS_LINK)
}

#[test]
fn initial_connection_wait_has_no_deadline_or_recovery_notification() {
    for id in ["onebot:42", "bot:instance"] {
        let problems = [connection_problem(id)];
        let mut state = BTreeMap::new();
        let mut seen = BTreeSet::new();
        for now in [1_000, 1_060, 4_600, 2_000_000] {
            let pending = connection_checks(&problems, &[], &mut seen);
            assert!(
                transitions(&mut state, &problems, &pending, now).is_empty(),
                "{id}"
            );
        }
        let online = connection_checks(&[], &[id], &mut seen);
        assert!(
            transitions(&mut state, &[], &online, 2_000_030).is_empty(),
            "{id}"
        );
        assert!(state.is_empty());
    }
}

#[test]
fn connection_notifications_pair_only_announced_outages() {
    let problems = [connection_problem("onebot:42")];
    let mut state = BTreeMap::new();
    let mut seen = BTreeSet::new();
    let online = connection_checks(&[], &["onebot:42"], &mut seen);
    assert!(transitions(&mut state, &[], &online, 1_000).is_empty());
    let offline = connection_checks(&problems, &[], &mut seen);
    assert_eq!(transitions(&mut state, &problems, &offline, 1_030).len(), 1);
    assert_eq!(state["onebot:42"].connection_alert_open, Some(true));
    let online = connection_checks(&[], &["onebot:42"], &mut seen);
    assert_eq!(transitions(&mut state, &[], &online, 1_060).len(), 1);
    assert!(transitions(&mut state, &[], &online, 1_090).is_empty());
    let offline = connection_checks(&problems, &[], &mut seen);
    assert!(transitions(&mut state, &problems, &offline, 1_120).is_empty());
    assert_eq!(state["onebot:42"].connection_alert_open, Some(false));
    let online = connection_checks(&[], &["onebot:42"], &mut seen);
    assert!(transitions(&mut state, &[], &online, 1_150).is_empty());
}

#[test]
fn each_announced_connection_outage_can_recover_even_near_previous_recovery() {
    let problems = [connection_problem("onebot:42")];
    let mut state = BTreeMap::new();
    let mut seen = BTreeSet::new();
    connection_checks(&[], &["onebot:42"], &mut seen);
    for (offline_at, online_at) in [(1_000, 1_400), (1_430, 1_460)] {
        let offline = connection_checks(&problems, &[], &mut seen);
        assert_eq!(
            transitions(&mut state, &problems, &offline, offline_at).len(),
            1
        );
        let online = connection_checks(&[], &["onebot:42"], &mut seen);
        assert_eq!(transitions(&mut state, &[], &online, online_at).len(), 1);
    }
}

#[test]
fn connection_observation_is_scoped_and_survives_failed_checks() {
    let mut state = BTreeMap::new();
    let mut seen = BTreeSet::new();
    connection_checks(&[], &["onebot:42"], &mut seen);
    defer_initial_connections(&mut ConnectionChecks::default(), &[], &mut seen);
    let problems = [
        connection_problem("onebot:42"),
        connection_problem("onebot:43"),
    ];
    let offline = connection_checks(&problems, &[], &mut seen);
    assert_eq!(transitions(&mut state, &problems, &offline, 1_000).len(), 1);
    assert!(state.contains_key("onebot:42"));
    assert!(!state.contains_key("onebot:43"));
    assert!(transitions(&mut state, &[], &ConnectionChecks::default(), 1_030).is_empty());
    assert!(state["onebot:42"].active);
}

#[test]
fn removed_connection_needs_new_online_evidence_before_alerting() {
    let problems = [connection_problem("bot:instance")];
    let mut state = BTreeMap::new();
    let mut seen = BTreeSet::new();
    connection_checks(&[], &["bot:instance"], &mut seen);
    let offline = connection_checks(&problems, &[], &mut seen);
    assert_eq!(transitions(&mut state, &problems, &offline, 1_000).len(), 1);
    let removed = connection_checks(&[], &[], &mut seen);
    assert!(transitions(&mut state, &[], &removed, 1_030).is_empty());
    assert!(seen.is_empty());
    assert_eq!(state["bot:instance"].connection_alert_open, Some(false));
    let pending = connection_checks(&problems, &[], &mut seen);
    assert!(transitions(&mut state, &problems, &pending, 2_000).is_empty());
    let online = connection_checks(&[], &["bot:instance"], &mut seen);
    assert!(transitions(&mut state, &[], &online, 2_030).is_empty());
}

#[test]
fn initial_wait_preserves_previous_alerts_and_reports_other_problems() {
    let robot = connection_problem("onebot:42");
    let backup = issue(
        "backup-failed",
        "backup",
        "error",
        "备份失败",
        "检查磁盘",
        "/system/database",
    );
    let mut state = BTreeMap::new();
    transitions(
        &mut state,
        std::slice::from_ref(&robot),
        &ConnectionChecks::default(),
        1_000,
    );
    let previous = state["onebot:42"].clone();
    let mut seen = BTreeSet::new();
    let problems = [robot, backup];
    let pending = connection_checks(&problems, &[], &mut seen);
    let messages = transitions(&mut state, &problems, &pending, 2_000);
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].0, "备份失败");
    assert_eq!(state["onebot:42"], previous);
    let online = connection_checks(&[], &["onebot:42"], &mut seen);
    let messages = transitions(&mut state, &problems[1..], &online, 2_030);
    assert_eq!(messages.len(), 1);
    assert_eq!(messages[0].0, "机器人连接已恢复");
}

#[test]
fn persisted_connection_alerts_support_old_snapshots_and_explicit_suppression() {
    for (last_recovered, marker, expected) in [
        (0, None, 1),
        (1_010, None, 0),
        (0, Some(false), 0),
        (0, Some(true), 1),
    ] {
        let mut saved = serde_json::json!({"onebot:42": {
            "active": true, "severity": "warning", "last_sent": 1_000,
            "last_recovered": last_recovered, "title": "未连接", "link": "/bots"
        }});
        if let Some(marker) = marker {
            saved["onebot:42"]["connection_alert_open"] = marker.into();
        }
        let mut state = serde_json::from_value(saved).unwrap();
        let online = connection_checks(&[], &["onebot:42"], &mut BTreeSet::new());
        assert_eq!(transitions(&mut state, &[], &online, 2_000).len(), expected);
        let mut restored = serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
        assert!(transitions(&mut restored, &[], &online, 2_030).is_empty());
    }
}

#[test]
fn notices_only_send_on_change_and_suppress_flapping() {
    let problem = issue(
        "plugin:demo",
        "plugins",
        "warning",
        "配置需要处理",
        "检查配置",
        "/plugins",
    );
    let mut state = BTreeMap::new();
    let connections = ConnectionChecks::default();
    assert_eq!(
        transitions(
            &mut state,
            std::slice::from_ref(&problem),
            &connections,
            1_000
        )
        .len(),
        1
    );
    assert!(
        transitions(
            &mut state,
            std::slice::from_ref(&problem),
            &connections,
            1_030
        )
        .is_empty()
    );
    assert_eq!(transitions(&mut state, &[], &connections, 1_060).len(), 1);
    assert!(
        transitions(
            &mut state,
            std::slice::from_ref(&problem),
            &connections,
            1_090
        )
        .is_empty()
    );
    assert!(transitions(&mut state, &[], &connections, 1_120).is_empty());
    assert_eq!(
        transitions(
            &mut state,
            std::slice::from_ref(&problem),
            &connections,
            1_400
        )
        .len(),
        1
    );
    // 重启读回去重记录，持续故障不会再次刷屏。
    let bytes = serde_json::to_vec(&state).unwrap();
    let mut restored = serde_json::from_slice(&bytes).unwrap();
    assert!(transitions(&mut restored, &[problem], &connections, 2_000).is_empty());
}

#[test]
fn new_monitor_does_not_claim_service_is_healthy() {
    let monitor = OperationsMonitor::new();
    let snapshot = monitor.snapshot.try_read().unwrap();
    assert_eq!(snapshot.status, "unknown");
    assert!(snapshot.checked_at.is_none());
}

#[tokio::test]
async fn backup_failure_and_recovery_are_persisted_without_secret_error_text() {
    let dir = tempfile::tempdir().unwrap();
    record_backup_result(dir.path(), Err("postgres://user:secret@host/db".into())).await;
    let bytes = std::fs::read(dir.path().join("backup/status.json")).unwrap();
    assert!(!String::from_utf8_lossy(&bytes).contains("secret"));
    assert!(
        backup_issues(dir.path())
            .iter()
            .any(|issue| issue.id == "backup-failed")
    );
    record_backup_result(dir.path(), Ok(())).await;
    assert!(
        !backup_issues(dir.path())
            .iter()
            .any(|issue| issue.id == "backup-failed")
    );
}

#[test]
fn removed_connection_is_retired_without_a_recovery_notification() {
    let problem = issue(
        "onebot:42",
        "bots",
        "warning",
        "OneBot 42 已离线",
        "连接断开",
        "/bots",
    );
    let mut state = BTreeMap::new();
    let connections = ConnectionChecks {
        checked_kinds: BTreeSet::from(["onebot"]),
        ..Default::default()
    };
    assert_eq!(
        transitions(&mut state, &[problem], &connections, 1_000).len(),
        1
    );
    assert!(transitions(&mut state, &[], &connections, 1_030).is_empty());
    assert!(!state["onebot:42"].active);
}

#[test]
fn failed_connection_check_keeps_the_problem_until_online_is_confirmed() {
    let problem = issue(
        "onebot:42",
        "bots",
        "warning",
        "OneBot 42 已离线",
        "连接断开",
        "/bots",
    );
    let mut state = BTreeMap::new();
    let unavailable = ConnectionChecks::default();
    transitions(&mut state, &[problem], &unavailable, 1_000);
    assert!(transitions(&mut state, &[], &unavailable, 1_030).is_empty());
    assert!(state["onebot:42"].active);

    let healthy = ConnectionChecks {
        checked_kinds: BTreeSet::from(["onebot"]),
        healthy: BTreeMap::from([("onebot:42".into(), "OneBot 42 已恢复在线".into())]),
        ..Default::default()
    };
    let recovered = transitions(&mut state, &[], &healthy, 1_060);
    assert_eq!(
        recovered,
        vec![(
            "机器人连接已恢复".into(),
            "OneBot 42 已恢复在线".into(),
            "/bots?connection=42".into()
        )]
    );
    assert!(transitions(&mut state, &[], &healthy, 1_090).is_empty());
}

#[tokio::test]
async fn startup_preserves_notification_history_without_replaying_connection_notices() {
    use nuono_core::db::{NewUser, ROLE_ADMIN, STATUS_ACTIVE};

    let db = nuono_core::test_support::test_db(2).await;
    let admin = db
        .create_user(NewUser {
            username: "startup-notices".into(),
            email: None,
            password: "correct-horse-battery".into(),
            role: ROLE_ADMIN.into(),
            status: STATUS_ACTIVE.into(),
            email_verified: true,
        })
        .await
        .unwrap()
        .id;
    let problem = issue(
        "onebot:42",
        "bots",
        "warning",
        "OneBot 42 已离线",
        "连接断开",
        ONEBOT_CONNECTIONS_LINK,
    );
    let backup = issue(
        "backup-failed",
        "backup",
        "error",
        "备份失败",
        "检查磁盘",
        "/system/backups",
    );
    let mut state = BTreeMap::new();
    let mut messages = transitions(
        &mut state,
        &[problem.clone(), backup.clone()],
        &ConnectionChecks::default(),
        1_000,
    );
    let healthy = connection_checks(&[], &["onebot:42"], &mut BTreeSet::new());
    messages.extend(transitions(
        &mut state,
        std::slice::from_ref(&backup),
        &healthy,
        1_060,
    ));
    db.publish_operations_notifications(
        &[admin],
        &messages,
        &serde_json::to_string(&state).unwrap(),
    )
    .await
    .unwrap();
    db.mark_all_notifications_read(admin).await.unwrap();
    db.push_notification(
        admin,
        "operations",
        "另一条机器人状态",
        "客户端断开",
        Some(ROBOTS_LINK),
    )
    .await
    .unwrap();
    db.push_notification(
        admin,
        "claim_reviewed",
        "审核通过",
        "群认领已审核",
        Some(ROBOTS_LINK),
    )
    .await
    .unwrap();

    let before = db.list_notifications(admin, 20).await.unwrap();
    let mut restored = load_notice_state(&db).await.unwrap();
    let mut seen = BTreeSet::new();
    let pending = connection_checks(std::slice::from_ref(&problem), &[], &mut seen);
    assert!(transitions(&mut restored, &[problem, backup.clone()], &pending, 2_000).is_empty());
    let online = connection_checks(&[], &["onebot:42"], &mut seen);
    assert!(transitions(&mut restored, &[backup], &online, 2_030).is_empty());
    assert_eq!(restored, state);
    assert_eq!(db.list_notifications(admin, 20).await.unwrap(), before);
    assert_eq!(db.unread_notification_count(admin).await.unwrap(), 2);

    db.shutdown().await.unwrap();
}

fn installed_plugin(
    name: &str,
    enabled: bool,
) -> nuono_core::service::plugins::InstalledPluginInfo {
    nuono_core::service::plugins::InstalledPluginInfo {
        id: Some("probe".into()),
        name: name.into(),
        display_name: name.into(),
        version: "1.0.0".into(),
        plugin_role: nuono_core::plugin::PluginRole::Feature,
        required_dependencies: Vec::new(),
        optional_dependencies: Vec::new(),
        enabled,
        customer_toggleable: true,
        allow_user_overrides: true,
    }
}

#[test]
fn installed_plugins_without_working_instances_still_report_runtime_failures() {
    let installed = vec![
        installed_plugin("Running", true),
        installed_plugin("FailedActivation", true),
        installed_plugin("RegisteredWithoutInstance", true),
        installed_plugin("LoadedWithoutRegistry", true),
        installed_plugin("Disabled", false),
    ];
    let loaded = vec!["Running".into(), "LoadedWithoutRegistry".into()];
    let enabled_registry = BTreeSet::from(["Running".into(), "RegisteredWithoutInstance".into()]);
    let mut checks = ConnectionChecks::default();
    let issues = plugin_runtime_issues(&installed, &loaded, &enabled_registry, &mut checks);
    let names: BTreeSet<_> = issues.iter().map(|issue| issue.id.as_str()).collect();
    assert_eq!(
        names,
        BTreeSet::from([
            "plugin-runtime:FailedActivation",
            "plugin-runtime:RegisteredWithoutInstance",
            "plugin-runtime:LoadedWithoutRegistry",
        ])
    );
    assert!(checks.healthy.contains_key("plugin-runtime:Running"));
    assert!(!checks.healthy.contains_key("plugin-runtime:Disabled"));
}

#[test]
fn removed_or_disabled_plugin_is_retired_without_a_recovery_notice() {
    for installed in [vec![], vec![installed_plugin("OldPlugin", false)]] {
        let problem = issue(
            "plugin-runtime:OldPlugin",
            "plugins",
            "error",
            "OldPlugin 未运行",
            "没有可用实例",
            "/plugins",
        );
        let mut state = BTreeMap::new();
        transitions(&mut state, &[problem], &ConnectionChecks::default(), 1_000);
        let mut checks = ConnectionChecks::default();
        let issues = plugin_runtime_issues(&installed, &[], &BTreeSet::new(), &mut checks);
        assert!(issues.is_empty());
        assert!(transitions(&mut state, &issues, &checks, 1_030).is_empty());
        assert!(!state["plugin-runtime:OldPlugin"].active);
    }
}

#[test]
fn plugin_runtime_recovery_requires_a_successful_current_check() {
    let installed = vec![installed_plugin("CurrentPlugin", true)];
    let mut failed = ConnectionChecks::default();
    let problems = plugin_runtime_issues(&installed, &[], &BTreeSet::new(), &mut failed);
    let mut state = BTreeMap::new();
    transitions(&mut state, &problems, &failed, 1_000);

    // 数据库暂不可用时没有核对结果，不能将旧问题当成已恢复。
    assert!(transitions(&mut state, &[], &ConnectionChecks::default(), 1_030).is_empty());
    assert!(state["plugin-runtime:CurrentPlugin"].active);

    let mut healthy = ConnectionChecks::default();
    let problems = plugin_runtime_issues(
        &installed,
        &["CurrentPlugin".into()],
        &BTreeSet::from(["CurrentPlugin".into()]),
        &mut healthy,
    );
    assert!(problems.is_empty());
    assert_eq!(
        transitions(&mut state, &problems, &healthy, 1_060),
        vec![(
            "插件已恢复运行".into(),
            "CurrentPlugin 已恢复运行".into(),
            "/plugins?plugin=CurrentPlugin&plugin-tab=service".into(),
        )]
    );
}

#[test]
fn removed_plugin_configuration_warning_does_not_claim_recovery() {
    let problem = issue(
        "plugin-config:OldPlugin",
        "plugins",
        "warning",
        "OldPlugin 配置需要处理",
        "配置无效",
        "/plugins",
    );
    let mut state = BTreeMap::new();
    transitions(&mut state, &[problem], &ConnectionChecks::default(), 1_000);
    let checks = ConnectionChecks {
        checked_kinds: BTreeSet::from(["plugin-config"]),
        ..Default::default()
    };
    assert!(transitions(&mut state, &[], &checks, 1_030).is_empty());
    assert!(!state["plugin-config:OldPlugin"].active);
}

#[test]
fn dashboard_issue_priority_and_observation_survive_rechecks() {
    let first = "2026-11-14T08:00:00Z";
    let later = "2026-11-14T08:05:00Z";
    let mut issues = vec![
        issue(
            "onebot:42",
            "bots",
            "warning",
            "离线",
            "连接断开",
            ROBOTS_LINK,
        ),
        issue(
            "plugin-runtime:demo",
            "plugins",
            "error",
            "未运行",
            "无法调用",
            "/plugins",
        ),
    ];
    prepare_issues(&mut issues, &[], first);
    assert_eq!(issues[0].severity, "error");
    let previous = issues.clone();
    prepare_issues(&mut issues, &previous, later);
    assert_eq!(issues[0].first_observed_at.as_deref(), Some(first));
    assert_eq!(issues[1].link, "/bots?connection=42");
    assert_eq!(issues[0].link, "/plugins?plugin=demo&plugin-tab=service");
    prepare_issues(&mut issues, &[], later);
    assert_eq!(issues[0].first_observed_at.as_deref(), Some(later));
    assert_eq!(
        issue_link("bot:a&b").as_deref(),
        Some("/bots?instance=a%26b")
    );
}

#[test]
fn dashboard_backup_coverage_requires_real_files_and_tracks_auto_schedule() {
    assert_eq!(OperationsSnapshot::default().backup.auto_enabled, None);
    use std::{
        fs::{File, FileTimes},
        time::{Duration, SystemTime},
    };
    let dir = tempfile::tempdir().unwrap();
    let now = chrono::Utc::now();
    let inspect = |enabled| backup_status::inspect(dir.path(), enabled, true, now).unwrap();
    assert_eq!(inspect(true).status, "missing");
    let backup_dir = dir.path().join("backup");
    std::fs::create_dir(&backup_dir).unwrap();
    // 仅有一条维护任务执行成功的 status.json 记录，但磁盘上并无实际备份文件时，不能算作已完成备份。
    std::fs::write(
        backup_dir.join("status.json"),
        r#"{"success":true,"completed_at":"2026-11-14T08:00:00Z"}"#,
    )
    .unwrap();
    assert_eq!(inspect(true).status, "missing");
    let manual = backup_dir.join("nuono_manual_backup_20261114_080000_test.dump");
    std::fs::write(&manual, b"SQLite format 3\0fixture").unwrap();
    assert_eq!(inspect(false).status, "manual");
    assert_eq!(inspect(true).status, "awaiting_auto");
    let automatic = backup_dir.join("nuono_auto_backup_20261113_080000_test.dump");
    std::fs::write(&automatic, b"SQLite format 3\0fixture").unwrap();
    File::options()
        .write(true)
        .open(&automatic)
        .unwrap()
        .set_times(
            FileTimes::new().set_modified(SystemTime::now() - Duration::from_secs(26 * 3600)),
        )
        .unwrap();
    assert_eq!(
        inspect(true).status,
        "overdue",
        "manual backups do not hide a missed automatic backup"
    );
    assert!(inspect(true).last_success_at.is_some());
    File::options()
        .write(true)
        .open(&automatic)
        .unwrap()
        .set_times(FileTimes::new().set_modified(SystemTime::now()))
        .unwrap();
    assert_eq!(inspect(true).status, "ready");
    assert_eq!(
        backup_status::inspect(dir.path(), true, false, now)
            .unwrap()
            .status,
        "missing",
        "backups for another database format do not count"
    );
}

#[tokio::test]
async fn health_and_stats_count_onebot_and_platform_instances_without_duplicates() {
    use nuono_adapter::{
        AdapterDescriptor, AdapterHost, AdapterInstanceConfig, AdapterProvider, AdapterRuntime,
        BotIdentity, ConfigField, DomainEvent, DomainModelError, PlatformAdapter, RuntimeStatus,
        UnifiedEvent,
    };
    use serde_json::json;
    use tokio::sync::watch;

    struct ConnectedRuntime;

    #[async_trait::async_trait]
    impl AdapterRuntime for ConnectedRuntime {
        fn platform(&self) -> Arc<dyn PlatformAdapter> {
            Arc::new(nuono_adapter_fixture::FixtureAdapter)
        }

        fn status(&self) -> RuntimeStatus {
            RuntimeStatus {
                connected: true,
                last_event_at: None,
                last_error: None,
            }
        }

        async fn run(&self, mut stop: watch::Receiver<bool>) -> nuono_core::error::Result<()> {
            while !*stop.borrow_and_update() && stop.changed().await.is_ok() {}
            Ok(())
        }
    }

    struct ConnectedProvider;

    #[async_trait::async_trait]
    impl AdapterProvider for ConnectedProvider {
        fn descriptor(&self) -> AdapterDescriptor {
            AdapterDescriptor {
                id: "qq_official".into(),
                platform: "qq_official".into(),
                display_name: "QQ 官方机器人".into(),
                description: String::new(),
                supports_multiple_instances: true,
                fields: vec![ConfigField::text("app_id", "App ID", "", "")],
                secret_fields: vec![],
            }
        }

        fn normalize(
            &self,
            _: &BotIdentity,
            _: &UnifiedEvent,
            _: chrono::DateTime<chrono::Utc>,
        ) -> Result<DomainEvent, DomainModelError> {
            unreachable!("测试不会触发事件归一化")
        }

        async fn create(
            &self,
            _: AdapterInstanceConfig,
            _: Arc<dyn AdapterHost>,
        ) -> nuono_core::error::Result<Arc<dyn AdapterRuntime>> {
            Ok(Arc::new(ConnectedRuntime))
        }
    }

    let mut fixture = crate::api::contract_tests::Fixture::new(false).await;
    let mut bot_instances = crate::bot_instances::BotInstanceRuntimeManager::new(
        fixture.state.db.clone(),
        fixture.state.adapter_manager.clone(),
        fixture.state.manager.event_kernel.clone(),
    );
    let mut registry = nuono_adapter::AdapterRegistry::default();
    registry.register(Arc::new(ConnectedProvider)).unwrap();
    bot_instances.source.registry = Arc::new(registry);
    let bot_instances = Arc::new(bot_instances);
    fixture.state.bot_instances = bot_instances.clone();

    // 接入 1 个在线 OneBot 连接
    let (tx, _rx) = tokio::sync::mpsc::channel(2);
    fixture.state.manager.bot_txs.insert(200001, tx.clone());
    fixture.state.manager.bot_states.register(
        200001,
        1,
        tx,
        "127.0.0.1".into(),
        "onebot".into(),
        Duration::from_secs(30),
    );
    fixture.state.manager.bot_states.probe_result(
        200001,
        1,
        Ok(&json!({"online": true, "good": true})),
    );

    // 创建并启动 1 个已连接的官方机器人实例
    let instance = bot_instances
        .create(crate::bot_instances::CreateBotInstanceRequest {
            name: "测试官Q实例".into(),
            adapter_type: "qq_official".into(),
            enabled: true,
            config: json!({"app_id": "100000001"}),
            ..Default::default()
        })
        .await
        .unwrap();
    bot_instances.start(&instance.id).await.unwrap();

    // 健康检查中的实时连接应为 2（1 个 OneBot + 1 个官方实例），不能因 adapter_manager 重复计为 3
    let collected = collect(&fixture.state).await;
    assert_eq!(collected.snapshot.active_connections, 2);
    assert!(collected.connections.healthy.contains_key("onebot:200001"));
    assert!(
        collected
            .connections
            .healthy
            .contains_key(&format!("bot:{}", instance.id))
    );
    assert!(
        !collected
            .connections
            .healthy
            .contains_key(&format!("adapter:{}", instance.id))
    );

    // 仪表盘统计中的在线机器人、Bot 登记和实时连接也应均为 2
    let axum::Json(response) =
        crate::api::stats::get_stats(axum::extract::State(fixture.state.clone()))
            .await
            .body;
    let stats = response.data.expect("stats response data");
    assert_eq!(stats.online_bots, 2);
    assert_eq!(stats.bot_count, 2);
    assert_eq!(stats.active_connections, 2);

    bot_instances.shutdown_all().await;
    fixture.close().await;
}
