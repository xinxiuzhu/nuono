use super::*;
use nuono_app::configuration::{ConfigurationActor, ConfigurationMode};
use nuono_storage::{
    groups::GroupIdentity,
    plugin_overrides::{OverridePatch, OverrideTarget},
};

async fn group_target(db: &Database) -> OverrideTarget {
    let group = db
        .ensure_group(&GroupIdentity::onebot("10001"), "retained group")
        .await
        .unwrap();
    OverrideTarget::Group(group.id)
}
async fn retained(db: &Database, name: &str) -> Option<nuono_storage::plugin_overrides::Override> {
    db.plugin_override(name, group_target(db).await)
        .await
        .unwrap()
}
async fn seed_registration(db: &Database, name: &str) {
    db.sync_plugin(name, "previous", &serde_json::json!({}))
        .await
        .unwrap();
    let actor = match db.find_user_by_username("root").await.unwrap() {
        Some(user) => user.id,
        None => {
            db.create_user(crate::db::NewUser {
                username: "root".into(),
                email: None,
                password: "valid-password-123".into(),
                role: "owner".into(),
                status: "active".into(),
                email_verified: false,
            })
            .await
            .unwrap()
            .id
        }
    };
    let policy = db.plugin_policy(name).await.unwrap();
    db.save_plugin_policy(
        actor,
        name,
        nuono_storage::plugin_global_settings::PluginPolicyPatch {
            expected_revision: policy.revision,
            allow_group_overrides: Some(true),
            ..Default::default()
        },
    )
    .await
    .unwrap();
    db.save_plugin_override(
        &ConfigurationActor {
            user_id: actor,
            mode: ConfigurationMode::Administrator,
        },
        name,
        group_target(db).await,
        OverridePatch {
            expected_revision: 0,
            config_version: 1,
            set_enabled: Some(false),
            ..Default::default()
        },
    )
    .await
    .unwrap();
}

async fn registered_names(db: &Database) -> Vec<String> {
    let mut names = crate::service::plugins::get_plugins(db)
        .await
        .unwrap()
        .into_iter()
        .map(|plugin| plugin.name)
        .collect::<Vec<_>>();
    names.sort();
    names
}

#[tokio::test]
async fn complete_startup_retains_removed_identity_settings() {
    let (host, db, suffix) = host_for_activation_test().await;
    let old = format!("Old{suffix}");
    let current = format!("Current{suffix}");
    for name in [&old, &current] {
        seed_registration(&db, name).await;
    }
    db.set_plugin_enabled(&current, false).await.unwrap();
    assert_eq!(retained(&db, &old).await.unwrap().enabled, Some(false));

    host.init_registered_plugins(vec![LoadedPlugin {
        plugin: Box::new(DeclarationCandidatePlugin::new(current.clone())),
    }])
    .await
    .unwrap();

    assert_eq!(
        registered_names(&db).await,
        vec![current.clone(), old.clone()]
    );
    assert!(!db.is_plugin_enabled(&current).await.unwrap());
    assert!(retained(&db, &current).await.is_some());
    assert!(retained(&db, &old).await.is_some());
    assert_eq!(retained(&db, &old).await.unwrap().enabled, Some(false));
    host.shutdown().await.unwrap();
    drop(host);
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn confirmed_empty_startup_preserves_registrations_settings_and_files() {
    let (host, db, name) = host_for_activation_test().await;
    seed_registration(&db, &name).await;
    let path = host.paths.plugin_data(&name).join("config.toml");
    tokio::fs::create_dir_all(path.parent().unwrap())
        .await
        .unwrap();
    tokio::fs::write(&path, "retained = true\n").await.unwrap();

    host.init_registered_plugins(vec![]).await.unwrap();

    assert_eq!(registered_names(&db).await, vec![name.clone()]);
    assert!(retained(&db, &name).await.is_some());
    assert_eq!(
        tokio::fs::read_to_string(&path).await.unwrap(),
        "retained = true\n"
    );
    host.shutdown().await.unwrap();
    drop(host);
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn empty_host_reconciliation_cannot_run_twice() {
    let (host, db, name) = host_for_activation_test().await;
    host.init_registered_plugins(vec![]).await.unwrap();
    seed_registration(&db, &name).await;

    assert!(host.init_registered_plugins(vec![]).await.is_err());
    assert_eq!(registered_names(&db).await, vec![name.clone()]);
    assert!(retained(&db, &name).await.is_some());
    host.shutdown().await.unwrap();
    drop(host);
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn complete_startup_keeps_rejected_and_failed_current_identities() {
    let (host, db, suffix) = host_for_activation_test().await;
    let rejected = format!("Rejected{suffix}");
    let failed = format!("Failed{suffix}");
    let removed = format!("Removed{suffix}");
    for name in [&rejected, &failed, &removed] {
        seed_registration(&db, name).await;
    }
    host.init_registered_plugins(vec![
        LoadedPlugin {
            plugin: Box::new(DeclarationCandidatePlugin {
                panic_config_fields: true,
                ..DeclarationCandidatePlugin::new(rejected.clone())
            }),
        },
        LoadedPlugin {
            plugin: Box::new(ActivationFailPlugin {
                name: failed.clone(),
                fail_on_load: true,
                panic_on_load: false,
                fail_on_enable: false,
                panic_on_enable: false,
                secret_fields: vec![],
            }),
        },
    ])
    .await
    .unwrap();

    assert_eq!(
        registered_names(&db).await,
        vec![failed.clone(), rejected.clone(), removed.clone()]
    );
    for name in [&rejected, &failed] {
        assert!(host.is_plugin_installed(name).await);
        assert!(!db.is_plugin_enabled(name).await.unwrap());
        assert!(retained(&db, name).await.is_some());
    }
    assert!(retained(&db, &removed).await.is_some());
    host.shutdown().await.unwrap();
    drop(host);
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn incomplete_identity_discovery_never_prunes_existing_records() {
    let (host, db, name) = host_for_activation_test().await;
    seed_registration(&db, &name).await;
    let result = host
        .init_registered_plugins(vec![LoadedPlugin {
            plugin: Box::new(DeclarationCandidatePlugin {
                panic_name: true,
                ..DeclarationCandidatePlugin::new(name.clone())
            }),
        }])
        .await;
    assert!(result.is_err());
    assert_eq!(registered_names(&db).await, vec![name.clone()]);
    assert!(retained(&db, &name).await.is_some());
    host.shutdown().await.unwrap();
    drop(host);
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn partial_injection_cannot_be_used_to_prune_a_running_host() {
    let (host, db, old) = host_for_activation_test().await;
    let current = format!("Current{old}");
    seed_registration(&db, &old).await;
    host.init_with_plugins(vec![LoadedPlugin {
        plugin: Box::new(DeclarationCandidatePlugin::new(current.clone())),
    }])
    .await
    .unwrap();

    assert!(host.init_registered_plugins(vec![]).await.is_err());
    let mut expected = vec![old.clone(), current];
    expected.sort();
    assert_eq!(registered_names(&db).await, expected);
    assert!(retained(&db, &old).await.is_some());
    host.shutdown().await.unwrap();
    drop(host);
    db.shutdown().await.unwrap();
}

#[tokio::test]
async fn startup_rejects_undeclared_triggers_without_deleting_uninstalled_data() {
    let (host, db, old) = host_for_activation_test().await;
    let current = format!("Current{old}");
    seed_registration(&db, &old).await;
    let fixture_sql = if db.backend() == sea_orm::DbBackend::Postgres {
        "CREATE FUNCTION reject_host_prune() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'fixture deletion failure'; END; $$; CREATE TRIGGER reject_host_prune BEFORE DELETE ON nuono_plugins FOR EACH ROW EXECUTE FUNCTION reject_host_prune();"
    } else {
        "CREATE TRIGGER reject_host_prune BEFORE DELETE ON nuono_plugins BEGIN SELECT RAISE(ABORT, 'fixture deletion failure'); END;"
    };
    db.orm().execute_unprepared(fixture_sql).await.unwrap();

    assert!(
        host.init_registered_plugins(vec![LoadedPlugin {
            plugin: Box::new(DeclarationCandidatePlugin::new(current.clone())),
        }])
        .await
        .is_err()
    );

    let mut expected = vec![old.clone(), current];
    expected.sort();
    assert_eq!(registered_names(&db).await, expected);
    assert!(retained(&db, &old).await.is_some());
    assert!(host.all_plugins().await.is_empty());
    host.shutdown().await.unwrap();
    drop(host);
    db.shutdown().await.unwrap();
}
