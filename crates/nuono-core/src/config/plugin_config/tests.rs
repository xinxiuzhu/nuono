use super::*;

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn file_configuration_preserves_last_valid_value_after_invalid_edit() {
        let db = crate::test_support::test_db(2).await;
        let root = std::env::temp_dir().join(format!("nuono-plugin-db-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join("example")).unwrap();
        let path = root.join("example/config.toml");
        std::fs::write(
            &path,
            "[answer]\ntype = \"string\"\ntitle = \"Answer\"\ndesc = \"\"\ndata = \"original\"\n",
        )
        .unwrap();
        let manager = super::PluginConfigManager::new(root.clone());
        manager
            .register("example", Default::default())
            .await
            .unwrap();
        std::fs::write(&path, "not valid toml").unwrap();
        assert_eq!(
            manager.get_runtime_config("example").await.unwrap()["answer"],
            "original"
        );
        db.shutdown().await.unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    use super::*;
    use crate::config::schema::{ConfigField, StructuredConfigField};
    use crate::core::config_schema::{ConfigFieldOption, ConfigFieldType, PluginConfigField};

    fn rich_model_field() -> PluginConfigField {
        PluginConfigField::json_object_list(
            "models",
            "Models",
            "",
            "[]",
            vec![
                PluginConfigField::text("name", "Name", "", "").required(),
                PluginConfigField::decimal("price", "Price", "", 0.0)
                    .with_min(0)
                    .with_max(10),
                PluginConfigField::json_number_map("pricing", "Pricing", "", "{}")
                    .with_encoding("native")
                    .with_default(serde_json::json!({}))
                    .with_fields(vec![
                        PluginConfigField::decimal("value", "Value", "", 0.0).with_min(0),
                    ]),
            ],
            "name",
        )
        .with_order(2)
    }

    #[tokio::test]
    async fn converted_schema_cannot_bypass_nested_credential_rejection() {
        let root =
            std::env::temp_dir().join(format!("nuono-nested-credential-{}", uuid::Uuid::new_v4()));
        let manager = PluginConfigManager::new(root.clone());
        for widget in [None, Some("password"), Some("token")] {
            let mut schema = plugin_fields_to_schema(&[rich_model_field()]).unwrap();
            let ConfigField::String {
                ui,
                widget: saved_widget,
                ..
            } = schema.get_mut("models").unwrap()
            else {
                unreachable!();
            };
            *saved_widget = widget.map(str::to_owned);
            ui.fields
                .push(PluginConfigField::password("api_key", "API key", ""));
            let error = manager
                .register("nested-credential", schema)
                .await
                .unwrap_err();
            assert!(error.to_string().contains("独立顶层密码字段"));
            assert!(manager.get_schema("nested-credential").await.is_none());
        }
        assert!(
            !root.exists(),
            "invalid declarations must not create configuration files"
        );
    }

    #[test]
    fn rich_schema_metadata_survives_api_round_trip_but_stays_out_of_toml() {
        let declaration = rich_model_field();
        let schema = plugin_fields_to_schema(&[declaration]).unwrap();
        let field = &schema["models"];
        let encoded = serde_json::to_value(field).unwrap();
        let decoded: ConfigField = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded.ui(), field.ui());
        assert_eq!(decoded.ui().fields[1].field_type, ConfigFieldType::Float);
        let structured = StructuredConfigField::from_config_field(field);
        let file = toml::to_string(&HashMap::from([("models", structured.clone())])).unwrap();
        assert!(!file.contains("ui"));
        assert!(!file.contains("object_list"));
        assert!(!file.contains("visible_when"));
        assert_eq!(structured.to_config_field().ui(), &ConfigFieldUi::default());
    }

    #[test]
    fn rich_json_containers_validate_nested_types_ranges_and_unique_labels() {
        let schema = plugin_fields_to_schema(&[rich_model_field()]).unwrap();
        let validate = |json: &str| {
            validate_config_value(
                "models",
                &ConfigValue::String(json.into()),
                &schema["models"],
            )
        };
        assert!(
            validate(
                r#"[{"name":"alpha","price":0.25,"pricing":{"hd":1.5},"future":{"keep":true}}]"#
            )
            .is_ok()
        );
        for invalid in [
            r#"{}"#,
            r#"[true]"#,
            r#"[{"name":"alpha","price":"0.25"}]"#,
            r#"[{"name":"alpha","price":-0.25}]"#,
            r#"[{"name":"alpha","price":11}]"#,
            r#"[{"name":"alpha","pricing":{"hd":"private-value"}}]"#,
            r#"[{"name":"alpha","pricing":{"hd":-1}}]"#,
            r#"[{"name":"alpha"},{"name":"alpha"}]"#,
            r#"[{"name":" "}]"#,
            r#"[{"price":1}]"#,
            "not-json-private-value",
        ] {
            let error = validate(invalid).unwrap_err();
            assert!(!error.contains("private-value"));
        }
        // 声明的可见性条件只控制展示，从不控制验证。
        assert!(validate(r#"[{"name":"alpha","price":-1}]"#).is_err());
        let plain =
            plugin_fields_to_schema(&[PluginConfigField::text("notes", "Notes", "", "")]).unwrap();
        assert!(
            validate_config_value(
                "notes",
                &ConfigValue::String("[unstructured note]".into()),
                &plain["notes"]
            )
            .is_ok()
        );
    }

    #[test]
    fn required_and_enums_validate_native_and_json_string_lists() {
        let options = vec![ConfigFieldOption {
            label: "Allowed label".into(),
            value: "allowed".into(),
        }];
        let schema = plugin_fields_to_schema(&[
            PluginConfigField::string_list("native", "Native", "", vec!["allowed".into()])
                .required()
                .with_options(options.clone()),
            PluginConfigField::json_string_list("json", "JSON", "", r#"["allowed"]"#)
                .required()
                .with_options(options),
            PluginConfigField::password("secret", "Secret", "")
                .with_default(serde_json::json!("example"))
                .required(),
        ])
        .unwrap();
        assert!(
            validate_config_value(
                "native",
                &ConfigValue::ListStr(vec!["allowed".into()]),
                &schema["native"]
            )
            .is_ok()
        );
        assert!(
            validate_config_value("native", &ConfigValue::ListStr(vec![]), &schema["native"])
                .is_err()
        );
        let error = validate_config_value(
            "native",
            &ConfigValue::ListStr(vec!["private-value".into()]),
            &schema["native"],
        )
        .unwrap_err();
        assert!(!error.contains("private-value"));
        assert!(
            validate_config_value(
                "json",
                &ConfigValue::String(r#"["allowed"]"#.into()),
                &schema["json"]
            )
            .is_ok()
        );
        assert!(
            validate_config_value("json", &ConfigValue::String("[]".into()), &schema["json"])
                .is_err()
        );
        assert!(
            validate_config_value("json", &ConfigValue::String("[1]".into()), &schema["json"])
                .is_err()
        );
        assert!(
            validate_config_value(
                "secret",
                &ConfigValue::String(" ".into()),
                &schema["secret"]
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn custom_list_options_preserve_historical_values_without_relaxing_other_enums() {
        let options = vec![ConfigFieldOption {
            label: "Twitter".into(),
            value: "twitter".into(),
        }];
        let schema = plugin_fields_to_schema(&[
            PluginConfigField::string_list("native", "Native", "", vec!["Twitter".into()])
                .with_editor("multiselect")
                .with_options(options.clone())
                .with_custom_options(),
            PluginConfigField::json_string_list("json", "JSON", "", r#"["Twitter"]"#)
                .with_editor("multiselect")
                .with_options(options.clone())
                .with_custom_options(),
            PluginConfigField::string_list("strict", "Strict", "", vec![])
                .with_options(options.clone()),
            PluginConfigField::text("text", "Text", "", "twitter")
                .with_options(options.clone())
                .with_custom_options(),
            PluginConfigField::select("mode", "Mode", "", "twitter", options).with_custom_options(),
        ])
        .unwrap();
        assert!(
            validate_config_value(
                "strict",
                &ConfigValue::ListStr(vec!["Twitter".into()]),
                &schema["strict"]
            )
            .is_err()
        );
        for key in ["text", "mode"] {
            assert!(
                validate_config_value(key, &ConfigValue::String("Twitter".into()), &schema[key])
                    .is_err()
            );
        }
        assert!(
            validate_config_value("json", &ConfigValue::String("[1]".into()), &schema["json"])
                .is_err()
        );
        assert!(
            validate_config_value("json", &ConfigValue::String("{}".into()), &schema["json"])
                .is_err()
        );
        let root =
            std::env::temp_dir().join(format!("nuono-custom-options-{}", uuid::Uuid::new_v4()));
        let manager = PluginConfigManager::new(root.clone());
        manager.register("Example", schema).await.unwrap();
        manager
            .set_values(
                "Example",
                HashMap::from([
                    (
                        "native".into(),
                        ConfigValue::ListStr(vec!["Twitter".into(), "future-resolver".into()]),
                    ),
                    (
                        "json".into(),
                        ConfigValue::String(r#"["Twitter","future-resolver"]"#.into()),
                    ),
                ]),
            )
            .await
            .unwrap();
        let runtime = manager.get_runtime_config("Example").await.unwrap();
        assert_eq!(
            runtime["native"],
            serde_json::json!(["Twitter", "future-resolver"])
        );
        assert_eq!(runtime["json"], r#"["Twitter","future-resolver"]"#);
        let file = tokio::fs::read_to_string(root.join("Example/config.toml"))
            .await
            .unwrap();
        assert!(!file.contains("allow_custom"));
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn rich_json_file_save_preserves_encoding_and_unknown_keys_and_rejects_invalid_writes() {
        let root = std::env::temp_dir().join(format!("nuono-rich-config-{}", uuid::Uuid::new_v4()));
        let manager = PluginConfigManager::new(root.clone());
        manager
            .register(
                "Example",
                plugin_fields_to_schema(&[rich_model_field()]).unwrap(),
            )
            .await
            .unwrap();
        let json = r#"[{"name":"alpha","price":0.75,"pricing":{"hd":1.5},"future":{"keep":true}}]"#;
        manager
            .set_values(
                "Example",
                HashMap::from([("models".into(), ConfigValue::String(json.into()))]),
            )
            .await
            .unwrap();
        let path = root.join("Example/config.toml");
        let before = tokio::fs::read(&path).await.unwrap();
        let revision = manager.status("Example").await.revision;
        assert_eq!(
            manager.get_runtime_config("Example").await.unwrap()["models"],
            json
        );
        let schema = manager.get_form_schema("Example").await.unwrap();
        assert_eq!(schema["models"].ui().editor.as_deref(), Some("object_list"));
        let invalid = r#"[{"name":"alpha","pricing":{"hd":"private-value"}}]"#;
        let error = manager
            .set_values(
                "Example",
                HashMap::from([("models".into(), ConfigValue::String(invalid.into()))]),
            )
            .await
            .unwrap_err();
        assert!(!error.to_string().contains("private-value"));
        assert_eq!(tokio::fs::read(&path).await.unwrap(), before);
        assert_eq!(manager.status("Example").await.revision, revision);
        let mut edited: toml::Table =
            toml::from_str(std::str::from_utf8(&before).unwrap()).unwrap();
        edited["models"]["data"] = toml::Value::String(invalid.into());
        let edited_bytes = toml::to_string(&edited).unwrap();
        tokio::fs::write(&path, &edited_bytes).await.unwrap();
        assert!(manager.refresh("Example").await.is_err());
        assert_eq!(
            manager.config_handle("Example").await.unwrap().snapshot()["models"],
            json
        );
        assert_eq!(
            tokio::fs::read_to_string(&path).await.unwrap(),
            edited_bytes
        );
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn structured_integer_storage_rejects_float_updates_without_silent_success() {
        let root = file_test_root();
        let manager = PluginConfigManager::new(root.clone());
        manager
            .register(
                "Example",
                plugin_fields_to_schema(&[PluginConfigField::number("price", "Price", "", 1)])
                    .unwrap(),
            )
            .await
            .unwrap();
        manager
            .register(
                "Example",
                plugin_fields_to_schema(&[PluginConfigField::decimal("price", "Price", "", 1.0)])
                    .unwrap(),
            )
            .await
            .unwrap();
        let path = root.join("Example/config.toml");
        let before = tokio::fs::read(&path).await.unwrap();
        let error = manager
            .set_values(
                "Example",
                HashMap::from([("price".into(), ConfigValue::Float(0.25))]),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("持久化类型不兼容"));
        assert_eq!(tokio::fs::read(&path).await.unwrap(), before);
        assert_eq!(
            manager.config_handle("Example").await.unwrap().snapshot()["price"],
            1
        );
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn required_top_level_deletion_keeps_snapshot_and_new_fields_use_valid_defaults() {
        let root = file_test_root();
        let manager = PluginConfigManager::new(root.clone());
        manager
            .register(
                "Example",
                plugin_fields_to_schema(&[PluginConfigField::text(
                    "original", "Original", "", "kept",
                )])
                .unwrap(),
            )
            .await
            .unwrap();
        manager
            .register(
                "Example",
                plugin_fields_to_schema(&[
                    PluginConfigField::text("original", "Original", "", "new default"),
                    PluginConfigField::text("required", "Required", "", "seed").required(),
                ])
                .unwrap(),
            )
            .await
            .unwrap();
        let snapshot = manager.config_handle("Example").await.unwrap();
        assert_eq!(snapshot.snapshot()["original"], "kept");
        assert_eq!(snapshot.snapshot()["required"], "seed");
        let path = root.join("Example/config.toml");
        let before_revision = manager.status("Example").await.revision;
        let mut document: toml::Table =
            toml::from_str(&tokio::fs::read_to_string(&path).await.unwrap()).unwrap();
        document.remove("required");
        let invalid = toml::to_string(&document).unwrap();
        tokio::fs::write(&path, &invalid).await.unwrap();
        let error = manager.refresh("Example").await.unwrap_err();
        assert!(error.to_string().contains("必填"));
        assert_eq!(snapshot.snapshot()["required"], "seed");
        assert_eq!(manager.status("Example").await.revision, before_revision);
        assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), invalid);
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn invalid_required_schema_defaults_are_rejected_before_file_write() {
        let root = file_test_root();
        let manager = PluginConfigManager::new(root.clone());
        manager
            .register(
                "Example",
                plugin_fields_to_schema(&[PluginConfigField::text(
                    "original", "Original", "", "kept",
                )])
                .unwrap(),
            )
            .await
            .unwrap();
        let path = root.join("Example/config.toml");
        let before = tokio::fs::read(&path).await.unwrap();
        let before_revision = manager.status("Example").await.revision;
        assert!(
            plugin_fields_to_schema(&[
                PluginConfigField::text("original", "Original", "", "ignored"),
                PluginConfigField::json_object_list(
                    "models",
                    "Models",
                    "",
                    "[]",
                    vec![PluginConfigField::text("name", "Name", "", "").required()],
                    "name"
                )
                .required(),
            ])
            .is_err()
        );
        assert_eq!(tokio::fs::read(&path).await.unwrap(), before);
        assert_eq!(manager.status("Example").await.revision, before_revision);
        assert!(
            manager
                .config_handle("Example")
                .await
                .unwrap()
                .snapshot()
                .get("models")
                .is_none()
        );
        assert!(
            plugin_fields_to_schema(&[
                PluginConfigField::text("required", "Required", "", "").required()
            ])
            .is_err()
        );
        assert!(!root.join("Fresh/config.toml").exists());
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn failed_registration_preserves_live_schema_and_first_failure_stays_unpublished() {
        let root = file_test_root();
        let manager = PluginConfigManager::new(root.clone());
        manager
            .register(
                "Example",
                plugin_fields_to_schema(&[PluginConfigField::text("answer", "Answer", "", "seed")])
                    .unwrap(),
            )
            .await
            .unwrap();
        manager
            .set_values(
                "Example",
                HashMap::from([("answer".into(), ConfigValue::String("original".into()))]),
            )
            .await
            .unwrap();
        let path = root.join("Example/config.toml");
        let before_file = tokio::fs::read(&path).await.unwrap();
        let before_schema = serde_json::to_value(manager.get_form_schema("Example").await).unwrap();
        let before_revision = manager.status("Example").await.revision;
        let handle = manager.config_handle("Example").await.unwrap();
        let incompatible =
            plugin_fields_to_schema(&[PluginConfigField::number("answer", "Answer", "", 3)])
                .unwrap();

        assert!(
            manager
                .register("Example", incompatible.clone())
                .await
                .is_err()
        );
        assert_eq!(
            serde_json::to_value(manager.get_form_schema("Example").await).unwrap(),
            before_schema,
            "失败的候选不能替换当前表单与写入校验声明"
        );
        assert_eq!(tokio::fs::read(&path).await.unwrap(), before_file);
        assert_eq!(manager.status("Example").await.revision, before_revision);
        assert_eq!(handle.snapshot()["answer"], "original");
        manager.refresh("Example").await.unwrap();
        assert_eq!(
            manager.get_runtime_config("Example").await.unwrap()["answer"],
            "original"
        );
        manager
            .set_values(
                "Example",
                HashMap::from([("answer".into(), ConfigValue::String("still valid".into()))]),
            )
            .await
            .unwrap();
        assert_eq!(handle.snapshot()["answer"], "still valid");
        assert_eq!(
            manager.get_runtime_config("Example").await.unwrap()["answer"],
            "still valid"
        );

        let fresh_path = root.join("Fresh/config.toml");
        tokio::fs::create_dir_all(fresh_path.parent().unwrap())
            .await
            .unwrap();
        let original = "answer = 'keep me'\n";
        tokio::fs::write(&fresh_path, original).await.unwrap();
        assert!(manager.register("Fresh", incompatible).await.is_err());
        assert!(manager.get_schema("Fresh").await.is_none());
        assert!(manager.get_form_schema("Fresh").await.is_none());
        assert!(
            !manager
                .list_configs()
                .await
                .iter()
                .any(|name| name == "Fresh")
        );
        assert!(manager.config_handle("Fresh").await.is_none());
        assert_eq!(
            tokio::fs::read_to_string(&fresh_path).await.unwrap(),
            original
        );
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[test]
    fn plugin_fields_to_schema_preserves_widgets_and_options() {
        let fields = vec![
            PluginConfigField::select(
                "mode",
                "Mode",
                "Runtime mode",
                "safe",
                vec![
                    ConfigFieldOption {
                        label: "Safe".to_string(),
                        value: "safe".to_string(),
                    },
                    ConfigFieldOption {
                        label: "Fast".to_string(),
                        value: "fast".to_string(),
                    },
                ],
            ),
            PluginConfigField::textarea("notes", "Notes", "Long text", "default"),
            PluginConfigField::password("secret", "Secret", "Sensitive value"),
        ];

        let schema = plugin_fields_to_schema(&fields).unwrap();
        assert!(matches!(
            schema.get("mode"),
            Some(ConfigField::Select { default, options, .. })
                if default == "safe" && options == &vec!["safe".to_string(), "fast".to_string()]
        ));
        assert!(matches!(
            schema.get("notes"),
            Some(ConfigField::String { widget: Some(widget), .. }) if widget == "textarea"
        ));
        assert!(matches!(
            schema.get("secret"),
            Some(ConfigField::String { widget: Some(widget), .. }) if widget == "password"
        ));
    }

    #[tokio::test]
    async fn automatic_listener_defaults_are_backfilled_without_overwriting_saved_switches() {
        let root = file_test_root();
        let manager = PluginConfigManager::new(root.clone());
        let path = manager.plugin_dir("Listener").join("config.toml");
        tokio::fs::create_dir_all(path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&path, "prefix = [\"url\"]\n")
            .await
            .unwrap();
        let policy = Plugins::new("Listener").with_auto_listen_enabled(true);
        manager
            .register_with_service_config("Listener", ConfigSchema::new(), Some(&policy))
            .await
            .unwrap();
        let schema = manager.get_form_schema("Listener").await.unwrap();
        assert!(schema["auto_listen_enabled"].group_scoped());
        assert_eq!(
            manager.get_runtime_config("Listener").await.unwrap()["auto_listen_enabled"],
            true
        );
        assert_eq!(
            manager.get_runtime_config("Listener").await.unwrap()["prefix"],
            serde_json::json!(["url"])
        );

        manager
            .set_values(
                "Listener",
                HashMap::from([("auto_listen_enabled".into(), ConfigValue::Bool(false))]),
            )
            .await
            .unwrap();
        // 插件升级和宿主重启都采用文件中的显式 false，不能再次套用新声明默认值。
        manager
            .register_with_service_config("Listener", ConfigSchema::new(), Some(&policy))
            .await
            .unwrap();
        let reloaded = PluginConfigManager::new(root.clone());
        reloaded
            .register_with_service_config("Listener", ConfigSchema::new(), Some(&policy))
            .await
            .unwrap();
        assert_eq!(
            reloaded.get_runtime_config("Listener").await.unwrap()["auto_listen_enabled"],
            false
        );
        assert!(
            tokio::fs::read_to_string(&path)
                .await
                .unwrap()
                .contains("data = false")
        );

        // 手工编辑走相同的 schema，错误输入保留上一个有效开关。
        tokio::fs::write(&path, "auto_listen_enabled = \"yes\"\n")
            .await
            .unwrap();
        assert!(reloaded.refresh("Listener").await.is_err());
        assert_eq!(
            reloaded.config_handle("Listener").await.unwrap().snapshot()["auto_listen_enabled"],
            false
        );
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn automatic_listener_setting_requires_a_framework_capability_declaration() {
        let root = file_test_root();
        let manager = PluginConfigManager::new(root.clone());
        let path = manager.plugin_dir("Ordinary").join("config.toml");
        tokio::fs::create_dir_all(path.parent().unwrap())
            .await
            .unwrap();
        // 未声明的存量键可以留存，但不能授予管理能力。
        tokio::fs::write(&path, "auto_listen_enabled = true\n")
            .await
            .unwrap();
        manager
            .register_with_service_config(
                "Ordinary",
                ConfigSchema::new(),
                Some(&Plugins::new("Ordinary")),
            )
            .await
            .unwrap();
        assert!(
            !manager
                .get_form_schema("Ordinary")
                .await
                .unwrap()
                .contains_key("auto_listen_enabled")
        );
        let before = tokio::fs::read(&path).await.unwrap();
        let error = manager
            .set_values(
                "Ordinary",
                HashMap::from([("auto_listen_enabled".into(), ConfigValue::Bool(false))]),
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("未声明"));
        assert_eq!(tokio::fs::read(&path).await.unwrap(), before);

        let forged = plugin_fields_to_schema(&[PluginConfigField::switch(
            "auto_listen_enabled",
            "伪造监听",
            "",
            true,
        )])
        .unwrap();
        let error = manager.register("Forged", forged).await.unwrap_err();
        assert!(error.to_string().contains("框架自动监听设置"));
        assert!(!manager.plugin_dir("Forged").exists());
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn saved_slash_prefix_is_preserved_when_plugin_declares_another_default() {
        let root = std::env::temp_dir().join(format!(
            "nuono-plugin-prefix-preservation-{}",
            rand::random::<u64>()
        ));
        let plugin_dir = root.join("DemoPlugin");
        tokio::fs::create_dir_all(&plugin_dir).await.unwrap();
        let path = plugin_dir.join("config.toml");
        tokio::fs::write(
            &path,
            "[prefix]\ntype = \"list_str\"\ntitle = \"指令前缀\"\ndesc = \"旧\"\ndata = [\"/\"]\n",
        )
        .await
        .unwrap();

        let manager = PluginConfigManager::new(root.clone());
        manager
            .register_with_service_config(
                "DemoPlugin",
                ConfigSchema::default(),
                Some(&Plugins::new("DemoPlugin").with_prefix(vec!["dm".to_string()])),
            )
            .await
            .unwrap();

        assert!(matches!(
            manager.get_values("DemoPlugin").await.unwrap().get("prefix"),
            Some(ConfigValue::ListStr(values)) if values == &["/".to_string()]
        ));
        assert!(
            tokio::fs::read_to_string(&path)
                .await
                .unwrap()
                .contains("data = [\"/\"]")
        );
        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn runtime_config_flattens_structured_values_and_overrides_defaults() {
        let root =
            std::env::temp_dir().join(format!("nuono-plugin-config-{}", rand::random::<u64>()));
        let manager = PluginConfigManager::new(root.clone());
        let fields = vec![
            PluginConfigField::number("count", "Count", "Counter", 3),
            PluginConfigField::switch("enabled", "Enabled", "Feature flag", false),
        ];
        manager
            .register("Example", plugin_fields_to_schema(&fields).unwrap())
            .await
            .unwrap();
        manager
            .set_values(
                "Example",
                HashMap::from([
                    ("count".to_string(), ConfigValue::Int(9)),
                    ("enabled".to_string(), ConfigValue::Bool(true)),
                ]),
            )
            .await
            .unwrap();

        let updated_fields = vec![
            PluginConfigField::number("count", "Count", "Counter", 42),
            PluginConfigField::switch("enabled", "Enabled", "Feature flag", false),
            PluginConfigField::text("added", "Added", "New field", "new"),
        ];
        manager
            .register("Example", plugin_fields_to_schema(&updated_fields).unwrap())
            .await
            .unwrap();

        let merged = manager
            .merge_runtime_config(
                "Example",
                serde_json::json!({"count": 1, "code_only": "preserved"}),
            )
            .await
            .unwrap();

        assert_eq!(merged["count"], 9);
        assert_eq!(merged["enabled"], true);
        assert_eq!(merged["added"], "new");
        assert_eq!(merged["code_only"], "preserved");

        let _ = tokio::fs::remove_dir_all(root).await;
    }

    /// 存量 `config.toml`（写在 `group_scoped` 之前，因此没有这一项）必须照旧解析。
    ///
    /// 这是新标记唯一的兼容风险点：`group_scoped` 加在 [`ConfigField`] 上，而磁盘格式是
    /// [`StructuredConfigField`]，两者刻意不共享这一项。真的写一遍旧文件再读，比断言
    /// `#[serde(default)]` 存在更能守住这条线 —— 后者哪天被挪走，只有这个用例会红。
    #[tokio::test]
    async fn legacy_config_files_without_group_scoped_still_parse() {
        let root = std::env::temp_dir().join(format!(
            "nuono-plugin-legacy-config-{}",
            rand::random::<u64>()
        ));
        let plugin_dir = root.join("Legacy");
        tokio::fs::create_dir_all(&plugin_dir).await.unwrap();
        let path = plugin_dir.join("config.toml");
        let original = concat!(
            "[greeting]\n",
            "type = \"string\"\n",
            "title = \"欢迎语\"\n",
            "desc = \"进群欢迎语\"\n",
            "data = \"老配置里的值\"\n",
            "\n",
            "[cooldown]\n",
            "type = \"int\"\n",
            "title = \"冷却秒数\"\n",
            "desc = \"触发间隔\"\n",
            "data = 42\n",
            "min = 0\n",
            "max = 60\n",
        );
        tokio::fs::write(&path, original).await.unwrap();

        let manager = PluginConfigManager::new(root.clone());
        let values = manager.get_values("Legacy").await.unwrap();
        assert!(matches!(
            values.get("greeting"),
            Some(ConfigValue::String(v)) if v == "老配置里的值"
        ));
        assert!(matches!(values.get("cooldown"), Some(ConfigValue::Int(42))));

        // 未注册的磁盘字段仍可读取，但不能反向生成管理表单。
        assert!(manager.get_form_schema("Legacy").await.is_none());

        // 未改动就不该重写文件，存量部署的配置保持逐字节原样。
        assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), original);

        let _ = tokio::fs::remove_dir_all(root).await;
    }

    #[tokio::test]
    async fn runtime_config_accepts_flat_toml_without_rewriting_it() {
        let root = std::env::temp_dir().join(format!(
            "nuono-plugin-flat-config-{}",
            rand::random::<u64>()
        ));
        let plugin_dir = root.join("Example");
        tokio::fs::create_dir_all(&plugin_dir).await.unwrap();
        let path = plugin_dir.join("config.toml");
        // 扁平 TOML（没有结构化的 type/title/data 包装）应原样作为运行时值读取。
        let original = "count = 7\n\n[nested]\nmode = \"fast\"\n";
        tokio::fs::write(&path, original).await.unwrap();

        let manager = PluginConfigManager::new(root.clone());
        let runtime = manager.get_runtime_config("Example").await.unwrap();
        assert_eq!(
            runtime,
            serde_json::json!({"count": 7, "nested": {"mode": "fast"}})
        );
        assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), original);

        let _ = tokio::fs::remove_dir_all(root).await;
    }
    fn file_test_root() -> PathBuf {
        std::env::temp_dir().join(format!("nuono-file-config-{}", uuid::Uuid::new_v4()))
    }

    #[tokio::test]
    async fn manual_edit_refreshes_handle_and_preserves_taken_snapshot() {
        let root = file_test_root();
        let manager = PluginConfigManager::new(root.clone());
        manager
            .register(
                "Example",
                plugin_fields_to_schema(&[PluginConfigField::number("count", "Count", "", 3)])
                    .unwrap(),
            )
            .await
            .unwrap();
        let handle = manager.config_handle("Example").await.unwrap();
        let previous = handle.snapshot();
        let path = root.join("Example/config.toml");
        tokio::fs::write(&path, "count = 9\n").await.unwrap();
        manager.refresh("Example").await.unwrap();
        assert_eq!(handle.snapshot()["count"], 9);
        assert_eq!(previous["count"], 3);
        tokio::fs::write(&path, "count = \"wrong type\"\n")
            .await
            .unwrap();
        assert!(manager.refresh("Example").await.is_err());
        assert_eq!(handle.snapshot()["count"], 9);
        assert_eq!(
            manager.get_runtime_config("Example").await.unwrap()["count"],
            9
        );
        tokio::fs::remove_file(&path).await.unwrap();
        assert!(matches!(
            manager.get_values("Example").await.unwrap()["count"],
            ConfigValue::Int(9)
        ));
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn management_snapshots_pair_values_with_their_revision_during_writes() {
        let root = file_test_root();
        let manager = PluginConfigManager::new(root.clone());
        manager
            .register(
                "Example",
                plugin_fields_to_schema(&[PluginConfigField::number("count", "Count", "", 0)])
                    .unwrap(),
            )
            .await
            .unwrap();
        let initial = manager.read_latest("Example").await.unwrap();
        let mut by_revision = HashMap::from([(initial.revision.clone(), 0)]);
        let writer = async {
            let mut current = initial;
            for count in 1..=32 {
                current = manager
                    .update_config_versioned(
                        "Example",
                        serde_json::json!({"count": count}),
                        &current.revision,
                    )
                    .await
                    .unwrap();
                by_revision.insert(current.revision.clone(), count);
                tokio::task::yield_now().await;
            }
        };
        let reader = async {
            let mut snapshots = Vec::new();
            for _ in 0..64 {
                snapshots.push(manager.management_snapshot("Example").await.unwrap());
                tokio::task::yield_now().await;
            }
            snapshots
        };
        let (_, snapshots) = tokio::join!(writer, reader);
        for snapshot in snapshots {
            let revision = snapshot.status.revision.unwrap();
            assert!(
                matches!(snapshot.values.get("count"), Some(ConfigValue::Int(value)) if *value == by_revision[&revision])
            );
            assert!(snapshot.schema.contains_key("count"));
            assert!(snapshot.status.error.is_none());
        }
        let before = manager.management_snapshot("Example").await.unwrap();
        tokio::fs::write(root.join("Example/config.toml"), "count = 'invalid'\n")
            .await
            .unwrap();
        let invalid = manager.management_snapshot("Example").await.unwrap();
        assert_eq!(
            serde_json::to_value(invalid.values).unwrap(),
            serde_json::to_value(before.values).unwrap()
        );
        assert_eq!(invalid.status.revision, before.status.revision);
        assert!(invalid.status.error.is_some());
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn concurrent_disjoint_patches_and_stale_revision_are_safe() {
        let root = file_test_root();
        let manager = Arc::new(PluginConfigManager::new(root.clone()));
        manager
            .register(
                "Example",
                plugin_fields_to_schema(&[
                    PluginConfigField::number("a", "A", "", 0),
                    PluginConfigField::number("b", "B", "", 0),
                ])
                .unwrap(),
            )
            .await
            .unwrap();
        let revision = manager.status("Example").await.revision.unwrap();
        let a = manager.clone();
        let b = manager.clone();
        let (a, b) = tokio::join!(
            a.set_values(
                "Example",
                HashMap::from([("a".into(), ConfigValue::Int(11))])
            ),
            b.set_values(
                "Example",
                HashMap::from([("b".into(), ConfigValue::Int(22))])
            )
        );
        a.unwrap();
        b.unwrap();
        let values = manager.get_runtime_config("Example").await.unwrap();
        assert_eq!(values["a"], 11);
        assert_eq!(values["b"], 22);
        assert!(
            manager
                .set_values_if_revision(
                    "Example",
                    HashMap::from([("a".into(), ConfigValue::Int(33))]),
                    Some(&revision)
                )
                .await
                .is_err()
        );
        assert!(
            manager
                .set_values(
                    "Example",
                    HashMap::from([("unknown".into(), ConfigValue::Int(2))])
                )
                .await
                .is_err()
        );
        assert!(
            manager
                .set_values(
                    "Example",
                    HashMap::from([("a".into(), ConfigValue::String("bad".into()))])
                )
                .await
                .is_err()
        );
        assert_eq!(manager.get_runtime_config("Example").await.unwrap(), values);
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn missing_defaults_persist_without_overwriting_values_or_nested_data() {
        let root = file_test_root();
        let manager = PluginConfigManager::new(root.clone());
        let dir = root.join("Example");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(
            dir.join("config.toml"),
            "count = 9\n[nested]\nmode = \"fast\"\n",
        )
        .await
        .unwrap();
        manager
            .register(
                "Example",
                plugin_fields_to_schema(&[PluginConfigField::number("count", "Count", "", 3)])
                    .unwrap(),
            )
            .await
            .unwrap();
        manager
            .register(
                "Example",
                plugin_fields_to_schema(&[
                    PluginConfigField::number("count", "Count", "", 42),
                    PluginConfigField::text("new", "New", "", "added"),
                ])
                .unwrap(),
            )
            .await
            .unwrap();
        manager
            .set_values(
                "Example",
                HashMap::from([("count".into(), ConfigValue::Int(7))]),
            )
            .await
            .unwrap();
        let restarted = PluginConfigManager::new(root.clone());
        let value = restarted.get_runtime_config("Example").await.unwrap();
        assert_eq!(value["count"], 7);
        assert_eq!(value["new"], "added");
        assert_eq!(value["nested"]["mode"], "fast");
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn corrupt_first_file_and_unsafe_plugin_name_are_rejected_without_rewriting() {
        let root = file_test_root();
        let manager = PluginConfigManager::new(root.clone());
        let dir = root.join("Example");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        let original = "count = [";
        tokio::fs::write(dir.join("config.toml"), original)
            .await
            .unwrap();
        assert!(
            manager
                .register("Example", ConfigSchema::new())
                .await
                .is_err()
        );
        assert_eq!(
            tokio::fs::read_to_string(dir.join("config.toml"))
                .await
                .unwrap(),
            original
        );
        for name in ["../escape", "/tmp/escape", "..", "", r"nested\escape"] {
            assert!(manager.register(name, ConfigSchema::new()).await.is_err());
        }
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn database_configuration_requires_toml_choice_then_keeps_differences_on_restart() {
        let db = crate::test_support::test_db(2).await;
        let root = file_test_root();
        tokio::fs::create_dir_all(root.join("Example"))
            .await
            .unwrap();
        let path = root.join("Example/config.toml");
        tokio::fs::write(&path, "invalid [ toml").await.unwrap();
        let schema = plugin_fields_to_schema(&[
            PluginConfigField::number("count", "Count", "", 8),
            PluginConfigField::text("greeting", "Greeting", "", "original"),
        ])
        .unwrap();
        let manager = PluginConfigManager::with_database(root.clone(), db.clone());
        assert!(manager.register("Example", schema).await.is_err());
        assert!(manager.status("Example").await.migration_conflict);
        assert!(manager.get_runtime_config("Example").await.is_err());
        manager
            .resolve_migration("Example", PluginConfigMigrationSource::Database)
            .await
            .unwrap();
        assert!(!manager.status("Example").await.migration_conflict);
        assert!(
            db.plugin_configuration("Example")
                .await
                .unwrap()
                .values
                .is_empty()
        );
        let before = manager.read_latest("Example").await.unwrap();
        manager
            .update_config_versioned(
                "Example",
                serde_json::json!({"count": 8, "prefix": ["!"]}),
                &before.revision,
            )
            .await
            .unwrap();
        let saved = db.plugin_configuration("Example").await.unwrap();
        assert_eq!(
            saved.values,
            serde_json::Map::from_iter([("count".into(), serde_json::json!(8))])
        );
        assert_eq!(saved.service["prefix"], serde_json::json!(["!"]));
        assert_eq!(
            tokio::fs::read_to_string(&path).await.unwrap(),
            "invalid [ toml"
        );
        let restarted = PluginConfigManager::with_database(root.clone(), db.clone());
        restarted
            .register(
                "Example",
                plugin_fields_to_schema(&[
                    PluginConfigField::number("count", "Count", "", 99),
                    PluginConfigField::text("greeting", "Greeting", "", "new default"),
                ])
                .unwrap(),
            )
            .await
            .unwrap();
        let after = restarted.get_runtime_config("Example").await.unwrap();
        assert_eq!(
            after["count"], 8,
            "explicit value equal to the old default remains explicit"
        );
        assert_eq!(
            after["greeting"], "new default",
            "missing value inherits updated code default"
        );
        assert_eq!(restarted.status("Example").await.source, "database");
        let revision = restarted.read_latest("Example").await.unwrap().revision;
        restarted
            .reset_fields("Example", &["count".into()], &revision)
            .await
            .unwrap();
        assert_eq!(
            restarted.get_runtime_config("Example").await.unwrap()["count"],
            99
        );
        assert!(
            db.plugin_configuration("Example")
                .await
                .unwrap()
                .values
                .is_empty()
        );
        assert_eq!(
            db.plugin_configuration("Example").await.unwrap().service["prefix"],
            serde_json::json!(["!"])
        );
        assert!(
            restarted
                .reset_fields("Example", &["prefix".into()], &revision)
                .await
                .is_err(),
            "stale reset must not erase newer settings"
        );
        db.shutdown().await.unwrap();
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn explicit_toml_import_validates_before_commit_and_preserves_the_source() {
        let db = crate::test_support::test_db(2).await;
        let root = file_test_root();
        tokio::fs::create_dir_all(root.join("Example"))
            .await
            .unwrap();
        let path = root.join("Example/config.toml");
        tokio::fs::write(&path, "count = 7\nunknown = 1\n")
            .await
            .unwrap();
        let manager = PluginConfigManager::with_database(root.clone(), db.clone());
        let schema = plugin_fields_to_schema(&[
            PluginConfigField::number("count", "Count", "", 1),
            PluginConfigField::text("greeting", "Greeting", "", "default"),
        ])
        .unwrap();
        assert!(manager.register("Example", schema.clone()).await.is_err());
        assert!(
            manager
                .resolve_migration("Example", PluginConfigMigrationSource::File)
                .await
                .is_err()
        );
        let unchanged = db.plugin_configuration("Example").await.unwrap();
        assert_eq!(unchanged.revision, 1);
        assert!(unchanged.values.is_empty());
        assert!(manager.get_schema("Example").await.is_none());
        let source = "count = 7\nprefix = ['!']\n";
        tokio::fs::write(&path, source).await.unwrap();
        manager
            .resolve_migration("Example", PluginConfigMigrationSource::File)
            .await
            .unwrap();
        let imported = db.plugin_configuration("Example").await.unwrap();
        assert_eq!(imported.values["count"], 7);
        assert!(!imported.values.contains_key("greeting"));
        assert_eq!(imported.service["prefix"], serde_json::json!(["!"]));
        assert_eq!(
            manager.get_runtime_config("Example").await.unwrap()["greeting"],
            "default"
        );
        assert_eq!(tokio::fs::read_to_string(&path).await.unwrap(), source);
        tokio::fs::write(&path, "invalid [ toml").await.unwrap();
        let restarted = PluginConfigManager::with_database(root.clone(), db.clone());
        restarted.register("Example", schema).await.unwrap();
        assert_eq!(
            restarted.get_runtime_config("Example").await.unwrap()["count"],
            7
        );
        db.shutdown().await.unwrap();
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn database_revision_conflict_preserves_the_winning_write() {
        let db = crate::test_support::test_db(4).await;
        let root = file_test_root();
        let first = PluginConfigManager::with_database(root.clone(), db.clone());
        let second = PluginConfigManager::with_database(root.clone(), db.clone());
        let schema =
            plugin_fields_to_schema(&[PluginConfigField::number("count", "Count", "", 0)]).unwrap();
        first.register("Example", schema.clone()).await.unwrap();
        second.register("Example", schema).await.unwrap();
        let revision = first.read_latest("Example").await.unwrap().revision;
        let (left, right) = tokio::join!(
            first.update_config_versioned("Example", serde_json::json!({"count": 1}), &revision),
            second.update_config_versioned("Example", serde_json::json!({"count": 2}), &revision),
        );
        assert_ne!(left.is_ok(), right.is_ok(), "one stale writer must lose");
        let winner = left.or(right).unwrap();
        assert_eq!(
            first.read_latest("Example").await.unwrap().values,
            winner.values
        );
        assert!(!root.join("Example/config.toml").exists());
        db.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn undeclared_file_keys_are_preserved_but_cannot_be_edited_or_define_forms() {
        let root =
            std::env::temp_dir().join(format!("nuono-declared-only-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(root.join("Example"))
            .await
            .unwrap();
        let path = root.join("Example/config.toml");
        tokio::fs::write(&path, "count = 7\nextra = 'keep me'\n")
            .await
            .unwrap();
        let manager = PluginConfigManager::new(root.clone());
        manager
            .register(
                "Example",
                plugin_fields_to_schema(&[PluginConfigField::number(
                    "count",
                    "Declared title",
                    "",
                    3,
                )
                .with_max(10)])
                .unwrap(),
            )
            .await
            .unwrap();
        let schema = manager.get_form_schema("Example").await.unwrap();
        assert!(!schema.contains_key("extra"));
        assert_eq!(schema["count"].title(), "Declared title");
        assert_eq!(
            manager.get_runtime_config("Example").await.unwrap()["count"],
            7
        );
        let before = tokio::fs::read(&path).await.unwrap();
        assert!(
            manager
                .set_values(
                    "Example",
                    HashMap::from([("extra".into(), ConfigValue::String("changed".into()))])
                )
                .await
                .is_err()
        );
        assert_eq!(tokio::fs::read(&path).await.unwrap(), before);
        manager
            .set_values(
                "Example",
                HashMap::from([("count".into(), ConfigValue::Int(8))]),
            )
            .await
            .unwrap();
        assert_eq!(
            manager.get_runtime_config("Example").await.unwrap()["extra"],
            "keep me"
        );
        let raw = tokio::fs::read_to_string(&path).await.unwrap();
        // 磁盘文件中的元数据不能突破插件作者在代码中声明的作用域限制。
        let mut document: toml::Value = toml::from_str(&raw).unwrap();
        document["count"] = toml::Value::try_from(serde_json::json!({
            "type": "int", "title": "User title", "desc": "", "max": 999, "data": 99
        }))
        .unwrap();
        tokio::fs::write(&path, toml::to_string(&document).unwrap())
            .await
            .unwrap();
        assert!(manager.refresh("Example").await.is_err());
        assert_eq!(
            manager.config_handle("Example").await.unwrap().snapshot()["count"],
            8
        );
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[tokio::test]
    async fn service_defaults_omit_priority_and_allow_a_declared_business_key() {
        let root = file_test_root();
        let manager = PluginConfigManager::new(root.clone());
        manager
            .register("Example", ConfigSchema::new())
            .await
            .unwrap();
        assert!(
            !manager
                .get_form_schema("Example")
                .await
                .unwrap()
                .contains_key("priority")
        );
        assert!(
            manager
                .get_runtime_config("Example")
                .await
                .unwrap()
                .get("priority")
                .is_none()
        );
        let raw = tokio::fs::read_to_string(root.join("Example/config.toml"))
            .await
            .unwrap();
        assert!(!raw.contains("priority"));

        // 已从框架保留字段中移除的名称，可以正常用作插件自身的业务配置字段。
        let schema = plugin_fields_to_schema(&[PluginConfigField::number(
            "priority",
            "业务排序值",
            "",
            -200,
        )
        .with_min(-1000)])
        .unwrap();
        manager.register("Business", schema).await.unwrap();
        manager
            .set_values(
                "Business",
                HashMap::from([("priority".into(), ConfigValue::Int(-300))]),
            )
            .await
            .unwrap();
        assert_eq!(
            manager.get_runtime_config("Business").await.unwrap()["priority"],
            -300
        );
        assert_eq!(
            manager.get_form_schema("Business").await.unwrap()["priority"].title(),
            "业务排序值"
        );
        tokio::fs::remove_dir_all(root).await.unwrap();
    }

    #[test]
    fn legacy_priority_values_are_parsed_as_undeclared_file_data() {
        for raw in [
            "priority = -9999\n",
            "priority = 'legacy value'\n",
            "[priority]\ntype = 'int'\ntitle = '旧字段'\ndesc = ''\ndata = -9999\nmin = 0\nmax = 99\n",
        ] {
            let (document, _, values) =
                crate::config::plugin_files::parse(raw, &ConfigSchema::new()).unwrap();
            let original: toml::Table = toml::from_str(raw).unwrap();
            assert_eq!(document, original);
            assert!(values.get("priority").is_some());
        }
    }
}

#[cfg(test)]
mod first_release_tests {
    use super::*;
    use crate::core::config_schema::PluginConfigField;
    use nuono_app::configuration::{ConfigurationActor, ConfigurationMode};
    use nuono_storage::plugin_overrides::{OverridePatch, OverrideTarget};

    async fn account(db: &crate::db::Database, name: &str, role: &str) -> i64 {
        db.create_user(crate::db::NewUser {
            username: name.into(),
            email: None,
            password: "a-valid-password-123".into(),
            role: role.into(),
            status: "active".into(),
            email_verified: false,
        })
        .await
        .unwrap()
        .id
    }
    fn schema() -> ConfigSchema {
        plugin_fields_to_schema(&[
            PluginConfigField::text("greeting", "Greeting", "", "default")
                .user_scoped()
                .group_scoped(),
            PluginConfigField::password("token", "Token", ""),
        ])
        .unwrap()
    }
    #[tokio::test]
    async fn import_requires_review_preserves_invalid_source_and_archives_after_atomic_commit() {
        let db = nuono_storage::test_support::test_db(2).await;
        let root = account(&db, "root", "owner").await;
        let path = std::env::temp_dir().join(format!("nuono-import-{}", uuid::Uuid::new_v4()));
        let manager = PluginConfigManager::with_database(path.clone(), db.clone());
        tokio::fs::create_dir_all(manager.plugin_dir("probe"))
            .await
            .unwrap();
        let source = "greeting = 'same-as-old-default'\ntoken = 'private-value'\nunknown = 4\n";
        tokio::fs::write(manager.config_path("probe"), source)
            .await
            .unwrap();
        assert!(manager.register("probe", schema()).await.is_err());
        let preview = manager.preview_import("probe").await.unwrap();
        assert!(
            preview
                .fields
                .iter()
                .any(|field| field.key == "unknown" && field.error.is_some())
        );
        assert!(
            preview
                .fields
                .iter()
                .find(|field| field.key == "token")
                .unwrap()
                .value
                .is_none()
        );
        let mut fields = HashMap::new();
        fields.insert("greeting".into(), PluginImportChoice::Import);
        fields.insert("token".into(), PluginImportChoice::Import);
        assert!(
            manager
                .import_selected(
                    root,
                    "probe",
                    PluginImportRequest {
                        source_revision: preview.source_revision.clone(),
                        database_revision: preview.database_revision.clone(),
                        fields
                    }
                )
                .await
                .is_err()
        );
        assert_eq!(
            tokio::fs::read_to_string(manager.config_path("probe"))
                .await
                .unwrap(),
            source
        );
        let fields = HashMap::from([
            ("greeting".into(), PluginImportChoice::Import),
            ("token".into(), PluginImportChoice::Import),
            ("unknown".into(), PluginImportChoice::Discard),
        ]);
        manager
            .import_selected(
                root,
                "probe",
                PluginImportRequest {
                    source_revision: preview.source_revision,
                    database_revision: preview.database_revision,
                    fields,
                },
            )
            .await
            .unwrap();
        assert!(!manager.config_path("probe").exists());
        assert!(
            !db.plugin_policy("probe")
                .await
                .unwrap()
                .config_json
                .contains("private-value")
        );
        tokio::fs::write(
            manager.config_path("probe"),
            "greeting = 'must-not-override'\n",
        )
        .await
        .unwrap();
        let reopened = PluginConfigManager::with_database(path.clone(), db.clone());
        reopened.register("probe", schema()).await.unwrap();
        assert_eq!(
            reopened.get_runtime_config("probe").await.unwrap()["greeting"],
            "same-as-old-default"
        );
        db.shutdown().await.unwrap();
        tokio::fs::remove_dir_all(path).await.unwrap();
    }
    #[tokio::test]
    async fn first_import_initializes_current_format_without_reinterpreting_it_as_version_one() {
        let db = nuono_storage::test_support::test_db(2).await;
        let actor = account(&db, "root", "owner").await;
        let path =
            std::env::temp_dir().join(format!("nuono-import-version-{}", uuid::Uuid::new_v4()));
        let manager = PluginConfigManager::with_database(path.clone(), db.clone());
        tokio::fs::create_dir_all(manager.plugin_dir("probe"))
            .await
            .unwrap();
        tokio::fs::write(manager.config_path("probe"), "greeting = 'imported'\n")
            .await
            .unwrap();
        assert!(
            manager
                .register_with_migration("probe", schema(), None, 2, None)
                .await
                .is_err()
        );
        let preview = manager.preview_import("probe").await.unwrap();
        manager
            .import_selected(
                actor,
                "probe",
                PluginImportRequest {
                    source_revision: preview.source_revision,
                    database_revision: preview.database_revision,
                    fields: HashMap::from([("greeting".into(), PluginImportChoice::Import)]),
                },
            )
            .await
            .unwrap();
        assert_eq!(
            db.plugin_configuration("probe")
                .await
                .unwrap()
                .config_version,
            2
        );
        let reopened = PluginConfigManager::with_database(path.clone(), db.clone());
        reopened
            .register_with_migration("probe", schema(), None, 2, None)
            .await
            .unwrap();
        assert_eq!(
            reopened.get_runtime_config("probe").await.unwrap()["greeting"],
            "imported"
        );
        db.shutdown().await.unwrap();
        tokio::fs::remove_dir_all(path).await.unwrap();
    }

    #[tokio::test]
    async fn user_and_group_differences_never_layer_over_each_other_and_defaults_remain_live() {
        let db = nuono_storage::test_support::test_db(4).await;
        let root = account(&db, "root", "owner").await;
        let user = account(&db, "person", "user").await;
        let manager = PluginConfigManager::with_database(
            std::env::temp_dir().join(format!("nuono-cfg-{}", uuid::Uuid::new_v4())),
            db.clone(),
        );
        manager.register("probe", schema()).await.unwrap();
        db.save_plugin_policy(
            root,
            "probe",
            nuono_storage::plugin_global_settings::PluginPolicyPatch {
                expected_revision: 1,
                allow_user_overrides: Some(true),
                allow_group_overrides: Some(true),
                enabled: None,
            },
        )
        .await
        .unwrap();
        let group = db
            .ensure_group(
                &nuono_storage::groups::GroupIdentity {
                    platform: "qq".into(),
                    namespace: "".into(),
                    conversation_kind: "group:numeric".into(),
                    native_parent_id: "".into(),
                    native_group_id: "00123".into(),
                },
                "",
            )
            .await
            .unwrap();
        let actor = ConfigurationActor {
            user_id: root,
            mode: ConfigurationMode::Administrator,
        };
        for (target, value) in [
            (OverrideTarget::User(user), "private"),
            (OverrideTarget::Group(group.id), "group"),
        ] {
            crate::service::plugin_configuration::save(
                &db,
                &manager,
                &actor,
                "probe",
                target,
                OverridePatch {
                    config_version: 1,
                    set_config: serde_json::json!({"greeting":value})
                        .as_object()
                        .unwrap()
                        .clone(),
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        }
        let global = crate::service::plugin_configuration::resolve(&db, &manager, "probe", None)
            .await
            .unwrap();
        assert_eq!(global.values["greeting"], "default");
        for (target, value) in [
            (OverrideTarget::User(user), "private"),
            (OverrideTarget::Group(group.id), "group"),
        ] {
            assert_eq!(
                crate::service::plugin_configuration::resolve(&db, &manager, "probe", Some(target))
                    .await
                    .unwrap()
                    .values["greeting"],
                value
            );
        }
        let row = db
            .plugin_override("probe", OverrideTarget::User(user))
            .await
            .unwrap()
            .unwrap();
        crate::service::plugin_configuration::save(
            &db,
            &manager,
            &actor,
            "probe",
            OverrideTarget::User(user),
            OverridePatch {
                expected_id: Some(row.id),
                expected_revision: row.revision,
                config_version: 1,
                remove_config: vec!["greeting".into()],
                ..Default::default()
            },
        )
        .await
        .unwrap();
        let snapshot = manager.read_latest("probe").await.unwrap();
        manager
            .set_values_as(
                root,
                "probe",
                HashMap::from([("greeting".into(), ConfigValue::String("new-global".into()))]),
                &snapshot.revision,
            )
            .await
            .unwrap();
        assert_eq!(
            crate::service::plugin_configuration::resolve(
                &db,
                &manager,
                "probe",
                Some(OverrideTarget::User(user))
            )
            .await
            .unwrap()
            .values["greeting"],
            "new-global"
        );
        assert_eq!(
            crate::service::plugin_configuration::resolve(
                &db,
                &manager,
                "probe",
                Some(OverrideTarget::Group(group.id))
            )
            .await
            .unwrap()
            .values["greeting"],
            "group"
        );
        db.shutdown().await.unwrap();
    }
}

#[cfg(test)]
mod resource_tests {
    use super::*;
    #[tokio::test]
    async fn structured_resource_edits_validate_increment_and_reset_without_revision_reuse() {
        use nuono_app::configuration_resources::{
            ConfigurationResource, ConfigurationResourceDefinition,
        };
        let db = nuono_storage::test_support::test_db(2).await;
        let manager = PluginConfigManager::with_database(
            std::env::temp_dir().join(format!("resources-{}", uuid::Uuid::new_v4())),
            db.clone(),
        );
        let resource = ConfigurationResourceDefinition {
            key: "rules".into(),
            label: "Rules".into(),
            description: String::new(),
            version: 1,
            default: serde_json::json!([]),
            max_bytes: 4096,
            validator: |v| {
                if v.as_array()
                    .is_some_and(|v| v.iter().all(serde_json::Value::is_string))
                {
                    Ok(())
                } else {
                    Err("string array required".into())
                }
            },
        };
        manager
            .register_resources("probe", vec![resource.clone()])
            .await
            .unwrap();
        manager
            .register(
                "probe",
                plugin_fields_to_schema(&[resource.field().unwrap()]).unwrap(),
            )
            .await
            .unwrap();
        let snapshot = manager.read_latest("probe").await.unwrap();
        let edit = |revision, value| {
            HashMap::from([(
                "rules".into(),
                ConfigValue::String(
                    serde_json::to_string(&ConfigurationResource {
                        revision,
                        version: 1,
                        value,
                    })
                    .unwrap(),
                ),
            )])
        };
        assert!(
            manager
                .set_values_if_revision(
                    "probe",
                    edit(0, serde_json::json!([3])),
                    Some(&snapshot.revision)
                )
                .await
                .is_err()
        );
        manager
            .set_values_if_revision(
                "probe",
                edit(0, serde_json::json!(["ok"])),
                Some(&snapshot.revision),
            )
            .await
            .unwrap();
        let saved = manager.read_latest("probe").await.unwrap();
        let record: ConfigurationResource =
            serde_json::from_str(saved.values["rules"].as_str().unwrap()).unwrap();
        assert_eq!(record.revision, 1);
        assert!(
            manager
                .set_values_if_revision(
                    "probe",
                    edit(0, serde_json::json!([])),
                    Some(&saved.revision)
                )
                .await
                .is_err()
        );
        manager
            .reset_fields("probe", &["rules".into()], &saved.revision)
            .await
            .unwrap();
        let saved = manager.read_latest("probe").await.unwrap();
        let record: ConfigurationResource =
            serde_json::from_str(saved.values["rules"].as_str().unwrap()).unwrap();
        assert_eq!(record.revision, 2);
        assert_eq!(record.value, serde_json::json!([]));
        let restarted = PluginConfigManager::with_database(manager.config_dir.clone(), db);
        restarted
            .register_resources("probe", vec![resource.clone()])
            .await
            .unwrap();
        restarted
            .register(
                "probe",
                plugin_fields_to_schema(&[resource.field().unwrap()]).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            restarted.read_latest("probe").await.unwrap().values,
            saved.values
        );
    }

    #[tokio::test]
    async fn management_snapshot_reports_overridden_business_keys_without_secrets() {
        use crate::config::schema::ConfigValue;
        use crate::core::config_schema::PluginConfigField;

        let db = crate::test_support::test_db(2).await;
        let root = std::env::temp_dir().join(format!("nuono-overridden-{}", uuid::Uuid::new_v4()));
        let manager = PluginConfigManager::with_database(root.clone(), db.clone());
        let schema = plugin_fields_to_schema(&[
            PluginConfigField::text("greeting", "Greeting", "", "hello").with_section("基础"),
            PluginConfigField::number("timeout", "Timeout", "", 30).with_section("基础"),
            PluginConfigField::password("api_key", "API key", "").with_section("凭据"),
        ])
        .unwrap();
        manager.register("override-demo", schema).await.unwrap();

        let initial = manager.management_snapshot("override-demo").await.unwrap();
        assert!(initial.overridden_keys.is_empty());

        manager
            .set_values_if_revision(
                "override-demo",
                HashMap::from([
                    ("greeting".into(), ConfigValue::String("hi".into())),
                    ("api_key".into(), ConfigValue::String("top-secret".into())),
                ]),
                initial.status.revision.as_deref(),
            )
            .await
            .unwrap();

        let after_save = manager.management_snapshot("override-demo").await.unwrap();
        assert_eq!(after_save.overridden_keys, vec!["greeting".to_string()]);

        manager
            .reset_fields(
                "override-demo",
                &["greeting".into()],
                after_save.status.revision.as_deref().unwrap(),
            )
            .await
            .unwrap();
        let after_reset = manager.management_snapshot("override-demo").await.unwrap();
        assert!(after_reset.overridden_keys.is_empty());

        db.shutdown().await.unwrap();
        let _ = std::fs::remove_dir_all(root);
    }
}
