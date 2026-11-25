use nuono_plugin::{ConfigFieldOption, ConfigFieldType, ConfigFieldUi, PluginConfigField};

#[derive(nuono_plugin::serde::Deserialize, nuono_plugin::ConfigSchema)]
#[serde(crate = "nuono_plugin::serde")]
struct DerivedConfig {
    #[config(default = 0.125)]
    factor: f64,
    names: Vec<String>,
}

#[test]
fn derive_preserves_float_and_native_string_list_types() {
    let fields = DerivedConfig::get_schema();
    assert_eq!(fields[0].field_type, ConfigFieldType::Float);
    assert_eq!(fields[0].default, nuono_plugin::serde_json::json!(0.125));
    assert_eq!(fields[1].field_type, ConfigFieldType::Token);
    assert_eq!(fields[1].default, nuono_plugin::serde_json::json!([]));
    assert_eq!(fields[1].ui.encoding.as_deref(), Some("native"));
    let config = DerivedConfig::from_value(&nuono_plugin::serde_json::json!({
        "names": ["alpha", "beta"]
    }))
    .unwrap();
    assert_eq!(config.factor, 0.125);
    assert_eq!(config.names, ["alpha", "beta"]);
}

#[derive(nuono_plugin::serde::Deserialize, nuono_plugin::ConfigSchema)]
#[serde(crate = "nuono_plugin::serde")]
struct OrderedConfig {
    #[config(section = "通用配置")]
    zulu: bool,
    #[config(section = "QQ音乐", order = 30)]
    alpha: bool,
    #[config(section = "QQ音乐")]
    middle: bool,
    #[config(section = "网易云音乐", order = -10 + 2)]
    beta: bool,
    #[config(section = "网易云音乐")]
    omega: bool,
}

#[test]
fn derive_exports_source_order_and_preserves_explicit_sort_overrides() {
    use nuono_plugin::serde_json::json;
    let schema = OrderedConfig::get_schema();
    assert_eq!(
        schema
            .iter()
            .map(|field| (field.key.as_str(), field.section.as_deref(), field.ui.order))
            .collect::<Vec<_>>(),
        [
            ("zulu", Some("通用配置"), 0),
            ("alpha", Some("QQ音乐"), 30),
            ("middle", Some("QQ音乐"), 2),
            ("beta", Some("网易云音乐"), -8),
            ("omega", Some("网易云音乐"), 4),
        ]
    );
    // 管理 API 的序列化仍携带默认顺序，后续表单排序无需读取源码或按键名猜测。
    let exported = nuono_plugin::serde_json::to_value(&schema).unwrap();
    assert_eq!(exported[2]["key"], "middle");
    assert_eq!(exported[2]["ui"]["order"], 2);
    assert_eq!(exported[3]["ui"]["order"], -8);
    let config = OrderedConfig::from_value(&json!({})).unwrap();
    assert_eq!(
        (
            config.zulu,
            config.alpha,
            config.middle,
            config.beta,
            config.omega
        ),
        (false, false, false, false, false)
    );
}

#[test]
fn nested_form_metadata_round_trips_and_old_declarations_still_decode() {
    let field = PluginConfigField::json_object_list(
        "models",
        "Models",
        "Available models",
        "[]",
        vec![PluginConfigField::text("name", "Name", "", "").required()],
        "name",
    )
    .with_order(3)
    .with_unit("USD")
    .with_step(0.01)
    .with_placeholder("Add a model")
    .with_visible_when("enabled", true)
    .with_options_source("providers", "name", Some("title"));
    let json = nuono_plugin::serde_json::to_value(&field).unwrap();
    let decoded: PluginConfigField = nuono_plugin::serde_json::from_value(json).unwrap();
    assert_eq!(decoded, field);
    assert!(decoded.effective_ui().fields[0].effective_ui().required);
    let mut old =
        nuono_plugin::serde_json::to_value(PluginConfigField::text("name", "Name", "", "old"))
            .unwrap();
    old.as_object_mut().unwrap().remove("ui");
    let decoded: PluginConfigField = nuono_plugin::serde_json::from_value(old).unwrap();
    assert_eq!(decoded.ui, ConfigFieldUi::default());
}

#[test]
fn legacy_options_labels_and_requirements_merge_into_ui() {
    let field = PluginConfigField::text("mode", "Mode", "", "a")
        .required()
        .with_placeholder("Choose")
        .with_options(vec![ConfigFieldOption {
            label: "Alpha".into(),
            value: "a".into(),
        }]);
    let ui = field.effective_ui();
    assert!(ui.required);
    assert_eq!(ui.placeholder.as_deref(), Some("Choose"));
    assert_eq!(ui.options[0].label, "Alpha");
}

#[test]
fn custom_options_metadata_is_explicit_and_round_trips() {
    let field = PluginConfigField::json_string_list("disabled", "Disabled", "", "[]")
        .with_editor("multiselect")
        .with_custom_options();
    let json = nuono_plugin::serde_json::to_value(&field).unwrap();
    assert_eq!(json["ui"]["allow_custom"], true);
    let decoded: PluginConfigField = nuono_plugin::serde_json::from_value(json).unwrap();
    assert!(decoded.ui.allow_custom);
    assert!(!field.with_allow_custom_options(false).ui.allow_custom);
    let default = nuono_plugin::serde_json::to_value(ConfigFieldUi::default()).unwrap();
    assert!(default.get("allow_custom").is_none());
}

#[derive(nuono_plugin::serde::Deserialize, nuono_plugin::ConfigSchema)]
#[serde(crate = "nuono_plugin::serde")]
struct BoundedConfig {
    /// 温度偏移
    #[config(default = -3, min = -10, max = 10)]
    offset: i64,
    #[config(default = -0.25)]
    factor: f64,
    #[config(default = vec!["alpha".to_string()], label = "标签")]
    tags: Vec<String>,
}

#[test]
fn defaults_and_decoding_use_the_same_declaration_and_constraints() {
    use nuono_plugin::{config_validation::config_defaults, serde_json::json};
    assert_eq!(
        BoundedConfig::default_value(),
        config_defaults(&BoundedConfig::get_schema())
    );
    let config = BoundedConfig::from_value(&json!({})).unwrap();
    assert_eq!(config.offset, -3);
    assert_eq!(config.factor, -0.25);
    assert_eq!(config.tags, ["alpha"]);
    assert!(BoundedConfig::from_value(&json!({"offset": 11})).is_err());
    assert!(BoundedConfig::from_value(&json!({"offset": "-3"})).is_err());
    let live = nuono_plugin::LiveConfig::<BoundedConfig>::default();
    assert!(
        live.bind_handle(nuono_plugin::config::PluginConfigHandle::new(
            json!({"offset": 100})
        ))
        .is_err()
    );
    assert_eq!(live.get().unwrap().offset, -3);
}

#[test]
fn declaration_errors_are_explicit_and_do_not_echo_values() {
    use nuono_plugin::{config_validation::validate_config_fields, serde_json::json};
    let field = PluginConfigField::number("count", "Count", "", 3);
    for fields in [
        vec![field.clone(), field.clone()],
        vec![field.clone().with_default(json!("private-default"))],
        vec![field.clone().with_min(4)],
        vec![field.clone().with_min(5).with_max(4)],
        vec![field.clone().with_step(0.0)],
        vec![field.clone().with_visible_when("missing", true)],
        vec![field.clone().with_options_source("missing", "id", None)],
        vec![field.with_editor("unknown")],
    ] {
        let error = validate_config_fields(&fields).unwrap_err();
        assert!(error.contains("count"), "{error}");
        assert!(!error.contains("private-default"));
    }
    assert!(validate_config_fields(&[]).is_ok());
}

#[test]
fn nested_templates_and_actual_items_have_distinct_required_checks() {
    use nuono_plugin::{
        config_validation::{validate_config_field_value, validate_config_fields},
        serde_json::json,
    };
    let field = PluginConfigField::json_object_list(
        "models",
        "Models",
        "",
        "[]",
        vec![PluginConfigField::text("id", "Name", "", "").required()],
        "id",
    );
    validate_config_fields(std::slice::from_ref(&field)).unwrap();
    validate_config_field_value("models", &json!(r#"[{"id":"alpha"}]"#), &field).unwrap();
    assert!(validate_config_field_value("models", &json!(r#"[{"id":""}]"#), &field).is_err());
    let invalid = field.with_default(json!(r#"[{"id":""}]"#));
    assert!(validate_config_fields(&[invalid]).is_err());
}

#[derive(nuono_plugin::serde::Deserialize, nuono_plugin::ConfigSchema)]
#[serde(crate = "nuono_plugin::serde")]
struct SmallIntegerConfig {
    #[config(default = 1, min = -100, max = 1000)]
    count: u8,
}

#[test]
fn derive_schema_respects_rust_integer_width_even_with_wider_requested_bounds() {
    use nuono_plugin::{config_validation::validate_config_field_value, serde_json::json};
    let fields = SmallIntegerConfig::get_schema();
    assert_eq!(fields[0].min, Some(0));
    assert_eq!(fields[0].max, Some(255));
    assert!(validate_config_field_value("count", &json!(256), &fields[0]).is_err());
    assert_eq!(
        SmallIntegerConfig::from_value(&json!({"count":255}))
            .unwrap()
            .count,
        255
    );
}

#[derive(Debug, nuono_plugin::serde::Deserialize, nuono_plugin::ConfigSchema)]
#[serde(crate = "nuono_plugin::serde")]
struct FormConfig {
    /// 是否下载视频
    #[config(
        label = "启用下载",
        section = "下载",
        order = 510,
        widget = "switch",
        default = false
    )]
    enabled: bool,
    #[config(label = "访问密钥", widget = "password", placeholder = "输入密钥")]
    secret: String,
    #[config(
        label = "下载策略",
        widget = "select",
        default = "b",
        options = [("甲", "a"), ("乙", "b")],
        required,
        visible_when(key = "enabled", value = true)
    )]
    mode: String,
    #[config(
        label = "尝试次数",
        default = 2,
        min = 1,
        max = 10,
        step = 1.0,
        unit = "次",
        group_scoped,
        visible_when(key = "mode", value = "b")
    )]
    attempts: i64,
    #[config(
        label = "可扩展列表",
        default = vec!["a".to_owned()],
        encoding = "json_string",
        editor = "multiselect",
        options = ["a", "b"],
        allow_custom
    )]
    custom_list: Vec<String>,
    #[config(
        label = "固定列表",
        default = vec!["a".to_owned()],
        encoding = "json_string",
        editor = "multiselect",
        options = [("甲", "a"), ("乙", "b")],
        allow_custom = false
    )]
    fixed_list: Vec<String>,
    #[config(default = vec!["a".to_owned()])]
    native_list: Vec<String>,
    #[config(default = "plain-text")]
    ordinary_string: String,
}

#[test]
fn advanced_derive_matches_explicit_form_declarations() {
    use nuono_plugin::config_validation::validate_config_fields;
    let expected = vec![
        PluginConfigField::switch("enabled", "启用下载", "是否下载视频", false)
            .with_section("下载")
            .with_order(510),
        PluginConfigField::password("secret", "访问密钥", "")
            .with_placeholder("输入密钥")
            .with_order(1),
        PluginConfigField::select(
            "mode",
            "下载策略",
            "",
            "b",
            vec![("甲", "a").into(), ("乙", "b").into()],
        )
        .required()
        .with_visible_when("enabled", true)
        .with_order(2),
        PluginConfigField::number("attempts", "尝试次数", "", 2)
            .with_min(1)
            .with_max(10)
            .with_step(1.0)
            .with_unit("次")
            .group_scoped()
            .with_visible_when("mode", "b")
            .with_order(3),
        PluginConfigField::json_string_list("custom_list", "可扩展列表", "", r#"["a"]"#)
            .with_editor("multiselect")
            .with_options(vec!["a".into(), "b".into()])
            .with_custom_options()
            .with_order(4),
        PluginConfigField::json_string_list("fixed_list", "固定列表", "", r#"["a"]"#)
            .with_editor("multiselect")
            .with_options(vec![("甲", "a").into(), ("乙", "b").into()])
            .with_order(5),
        PluginConfigField::string_list("native_list", "native_list", "", vec!["a".into()])
            .with_order(6),
        PluginConfigField::text("ordinary_string", "ordinary_string", "", "plain-text")
            .with_order(7),
    ];
    let actual = FormConfig::get_schema();
    assert_eq!(actual, expected);
    validate_config_fields(&actual).unwrap();
    assert_eq!(FormConfig::default_value()["custom_list"], r#"["a"]"#);
    assert_eq!(FormConfig::default_value()["fixed_list"], r#"["a"]"#);
    assert_eq!(
        FormConfig::default_value()["native_list"],
        nuono_plugin::serde_json::json!(["a"])
    );
}

#[test]
fn encoded_lists_decode_defaults_legacy_strings_and_native_input() {
    use nuono_plugin::serde_json::json;
    let defaults = FormConfig::from_value(&json!({})).unwrap();
    assert!(!defaults.enabled);
    assert!(defaults.secret.is_empty());
    assert_eq!(defaults.mode, "b");
    assert_eq!(defaults.attempts, 2);
    assert_eq!(defaults.custom_list, ["a"]);
    assert_eq!(defaults.fixed_list, ["a"]);
    assert_eq!(defaults.native_list, ["a"]);
    assert_eq!(defaults.ordinary_string, "plain-text");

    let legacy = FormConfig::from_value(&json!({
        "secret": "nonempty-secret",
        "custom_list": "[\"b\",\"extended\"]",
        "fixed_list": "[\"b\"]",
        "ordinary_string": "[\"leave-as-a-string\"]"
    }))
    .unwrap();
    assert_eq!(legacy.secret, "nonempty-secret");
    assert_eq!(legacy.custom_list, ["b", "extended"]);
    assert_eq!(legacy.fixed_list, ["b"]);
    assert_eq!(legacy.ordinary_string, r#"["leave-as-a-string"]"#);

    let native = FormConfig::from_value(&json!({
        "custom_list": ["b", "extended"],
        "fixed_list": ["b"],
        "native_list": ["native"]
    }))
    .unwrap();
    assert_eq!(native.custom_list, ["b", "extended"]);
    assert_eq!(native.fixed_list, ["b"]);
    assert_eq!(native.native_list, ["native"]);

    let normalized = <FormConfig as nuono_plugin::ConfigSchema>::normalize_value(json!({
        "custom_list": ["b"],
        "native_list": ["native"],
        "ordinary_string": "[\"plain\"]",
        "unknown": ["preserved"]
    }))
    .unwrap();
    assert_eq!(normalized["custom_list"], r#"["b"]"#);
    assert_eq!(normalized["native_list"], json!(["native"]));
    assert_eq!(normalized["ordinary_string"], r#"["plain"]"#);
    assert_eq!(normalized["unknown"], json!(["preserved"]));
}

#[test]
fn encoded_lists_and_advanced_constraints_reject_invalid_values_without_echoing_them() {
    use nuono_plugin::serde_json::json;
    for value in [
        json!({ "custom_list": ["private-value", 1] }),
        json!({ "custom_list": "[\"private-value\",1]" }),
        json!({ "custom_list": "private-value" }),
        json!({ "custom_list": { "private-value": true } }),
        json!({ "fixed_list": ["private-value"] }),
        json!({ "mode": "private-value" }),
        json!({ "mode": "" }),
        json!({ "attempts": 11 }),
        json!({ "attempts": "private-value" }),
        json!({ "native_list": "[\"private-value\"]" }),
        json!({ "ordinary_string": ["private-value"] }),
    ] {
        let error = FormConfig::from_value(&value).unwrap_err().to_string();
        assert!(!error.contains("private-value"), "{error}");
    }
}

#[test]
fn encoded_lists_live_config_reads_valid_updates_and_preserves_binding_on_failure() {
    use nuono_plugin::{LiveConfig, config::PluginConfigHandle, serde_json::json};
    let live = LiveConfig::<FormConfig>::default();
    assert_eq!(live.get().unwrap().custom_list, ["a"]);
    let handle = PluginConfigHandle::new(json!({ "custom_list": "[\"b\"]" }));
    live.bind_handle(handle.clone()).unwrap();
    assert_eq!(live.get().unwrap().custom_list, ["b"]);
    handle.publish(json!({ "enabled": true, "custom_list": ["custom"] }));
    assert!(live.get().unwrap().enabled);
    assert_eq!(live.get().unwrap().custom_list, ["custom"]);
    assert!(
        live.bind_handle(PluginConfigHandle::new(json!({ "fixed_list": ["other"] })))
            .is_err()
    );
    assert_eq!(live.get().unwrap().custom_list, ["custom"]);
}

#[derive(nuono_plugin::serde::Deserialize)]
#[serde(crate = "nuono_plugin::serde")]
struct HandwrittenConfig {
    raw: String,
    list: Vec<String>,
}

impl nuono_plugin::ConfigSchema for HandwrittenConfig {
    fn get_schema() -> Vec<PluginConfigField> {
        vec![
            PluginConfigField::text("raw", "Raw", "", r#"["string"]"#),
            PluginConfigField::string_list("list", "List", "", vec!["default".into()]),
        ]
    }
}

#[test]
fn handwritten_schema_uses_ordinary_serde_without_implicit_string_conversion() {
    use nuono_plugin::{ConfigSchema, serde_json::json};
    let defaults = HandwrittenConfig::from_value(&json!({})).unwrap();
    assert_eq!(defaults.raw, r#"["string"]"#);
    assert_eq!(defaults.list, ["default"]);
    assert!(HandwrittenConfig::from_value(&json!({ "raw": ["string"] })).is_err());
    assert!(HandwrittenConfig::from_value(&json!({ "list": "[\"default\"]" })).is_err());
}

#[test]
fn option_shorthands_preserve_label_and_value() {
    assert_eq!(
        ConfigFieldOption::from("a"),
        ConfigFieldOption {
            label: "a".into(),
            value: "a".into(),
        }
    );
    assert_eq!(ConfigFieldOption::from(String::from("a")), "a".into());
    assert_eq!(
        ConfigFieldOption::from((String::from("Alpha"), "a")),
        ConfigFieldOption {
            label: "Alpha".into(),
            value: "a".into(),
        }
    );
}

const BORROWED_OPTIONS: &[&str] = &["a", "b"];
const BORROWED_LABEL_OPTIONS: &[(&str, &str)] = &[("甲", "a"), ("乙", "b")];

fn borrowed_label_options() -> &'static [(&'static str, &'static str)] {
    BORROWED_LABEL_OPTIONS
}

fn borrowed_field_options() -> std::slice::Iter<'static, ConfigFieldOption> {
    static OPTIONS: std::sync::LazyLock<Vec<ConfigFieldOption>> =
        std::sync::LazyLock::new(|| vec![("甲", "a").into(), ("乙", "b").into()]);
    OPTIONS.iter()
}

fn borrowed_string_options() -> std::slice::Iter<'static, String> {
    static OPTIONS: std::sync::LazyLock<Vec<String>> =
        std::sync::LazyLock::new(|| vec!["a".into(), "b".into()]);
    OPTIONS.iter()
}

#[derive(nuono_plugin::serde::Deserialize, nuono_plugin::ConfigSchema)]
#[serde(crate = "nuono_plugin::serde")]
struct BorrowedOptionsConfig {
    #[config(default = "a", widget = "select", options = BORROWED_OPTIONS)]
    slice: String,
    #[config(default = vec!["a".into()], options = borrowed_label_options())]
    tuples: Vec<String>,
    #[config(default = "b", widget = "select", options = borrowed_field_options())]
    fields: String,
    #[config(default = vec!["b".into()], options = borrowed_string_options())]
    strings: Vec<String>,
}

#[test]
fn derive_accepts_constant_option_slices_and_borrowed_option_iterators() {
    use nuono_plugin::{config_validation::validate_config_fields, serde_json::json};
    let schema = BorrowedOptionsConfig::get_schema();
    assert_eq!(
        schema,
        vec![
            PluginConfigField::select("slice", "slice", "", "a", vec!["a".into(), "b".into()])
                .with_order(0),
            PluginConfigField::string_list("tuples", "tuples", "", vec!["a".into()])
                .with_options(vec![("甲", "a").into(), ("乙", "b").into()])
                .with_order(1),
            PluginConfigField::select(
                "fields",
                "fields",
                "",
                "b",
                vec![("甲", "a").into(), ("乙", "b").into()]
            )
            .with_order(2),
            PluginConfigField::string_list("strings", "strings", "", vec!["b".into()])
                .with_options(vec!["a".into(), "b".into()])
                .with_order(3),
        ]
    );
    validate_config_fields(&schema).unwrap();
    let config = BorrowedOptionsConfig::from_value(&json!({})).unwrap();
    assert_eq!(config.slice, "a");
    assert_eq!(config.tuples, ["a"]);
    assert_eq!(config.fields, "b");
    assert_eq!(config.strings, ["b"]);
    assert!(BorrowedOptionsConfig::from_value(&json!({ "strings": ["unknown"] })).is_err());
}
