use super::*;

#[test]
fn invalid_declarations_report_errors_instead_of_silent_defaults() {
    for source in [
        "struct Config { #[config(defalt = 3)] value: i64 }",
        "struct Config { #[config(default = 3, default = 4)] value: i64 }",
        "struct Config { value: Vec<bool> }",
        "struct Config { value: Option<String> }",
        "struct Config { #[serde(rename = \"other\")] value: String }",
        "#[serde(rename_all = \"camelCase\")] struct Config { value: String }",
        "#[config(section = \"基本\")] struct Config { value: String }",
        "#[config(unknown = true)] struct Config { value: String }",
    ] {
        assert!(
            expand(&syn::parse_str::<ItemStruct>(source).unwrap()).is_err(),
            "{source}"
        );
    }
}

#[test]
fn advanced_declarations_accept_rust_expressions_and_flags() {
    let input = syn::parse_str::<ItemStruct>(r#"
        struct Config {
            #[config(default = true, widget = "switch", section = "基本", order = 2 + 1, group_scoped = false)]
            enabled: bool,
            #[config(default = "a", widget = "select", options = [("甲", "a"), ("乙", "b")], required, visible_when(key = "enabled", value = true))]
            mode: String,
            #[config(password, placeholder = "密钥", required = false)]
            token: String,
            #[config(default = 2_u16, min = 1, max = 30, unit = "秒", step = 0.5)]
            delay: u16,
            #[config(default = vec!["a".into()], options = resolver_options(), editor = "multiselect", encoding = "json_string", allow_custom)]
            platforms: Vec<String>,
        }
    "#).unwrap();
    let expanded = expand(&input).unwrap().to_string();
    assert!(expanded.contains("PluginConfigField :: select"));
    assert!(expanded.contains("PluginConfigField :: json_string_list"));
    assert!(expanded.contains("normalize_json_string_lists"));
    assert!(expanded.contains("decode_json_string_lists"));
    assert!(expanded.contains("with_visible_when"));
    syn::parse2::<syn::File>(expand(&input).unwrap()).unwrap();
}

#[test]
fn incompatible_attributes_and_unknown_values_fail_during_expansion() {
    for source in [
        r#"struct Config { #[config(widget = "password")] value: bool }"#,
        r#"struct Config { #[config(widget = "text")] value: i64 }"#,
        r#"struct Config { #[config(widget = "checkbox")] value: bool }"#,
        r#"struct Config { #[config(widget = "select")] value: String }"#,
        r#"struct Config { #[config(editor = "tags")] value: String }"#,
        r#"struct Config { #[config(editor = "object_list")] value: Vec<String> }"#,
        r#"struct Config { #[config(encoding = "json_string")] value: String }"#,
        r#"struct Config { #[config(encoding = "json")] value: Vec<String> }"#,
        r#"struct Config { #[config(options = ["a"])] value: bool }"#,
        r#"struct Config { #[config(allow_custom = false)] value: String }"#,
        r#"struct Config { #[config(unit = "秒")] value: String }"#,
        r#"struct Config { #[config(step = 0.5)] value: bool }"#,
        r#"struct Config { #[config(required = 1)] value: String }"#,
        r#"struct Config { #[config(password = true)] value: String }"#,
        r#"struct Config { #[config(password, widget = "password")] value: String }"#,
        r#"struct Config { #[config(section = "一")] #[config(section = "二")] value: String }"#,
    ] {
        assert!(
            expand(&syn::parse_str::<ItemStruct>(source).unwrap()).is_err(),
            "{source}"
        );
    }
}

#[test]
fn invalid_visibility_conditions_report_the_declaration_error() {
    for source in [
        r#"struct Config { #[config(visible_when(key = "missing", value = true))] value: String }"#,
        r#"struct Config { enabled: bool, #[config(visible_when(key = "enabled"))] value: String }"#,
        r#"struct Config { enabled: bool, #[config(visible_when(value = true))] value: String }"#,
        r#"struct Config { enabled: bool, #[config(visible_when(key = "enabled", key = "enabled", value = true))] value: String }"#,
        r#"struct Config { enabled: bool, #[config(visible_when(key = "enabled", value = true, value = false))] value: String }"#,
        r#"struct Config { enabled: bool, #[config(visible_when(key = "enabled", val = true))] value: String }"#,
        r#"struct Config { enabled: bool, #[config(visible_when(key = "enabled", value = "yes"))] value: String }"#,
        r#"struct Config { mode: String, #[config(visible_when(key = "mode", value = true))] value: String }"#,
        r#"struct Config { count: u8, #[config(visible_when(key = "count", value = 0.5))] value: String }"#,
        r#"struct Config { enabled: bool, #[config(visible_when(key = "enabled", value = true), visible_when(key = "enabled", value = false))] value: String }"#,
    ] {
        assert!(
            expand(&syn::parse_str::<ItemStruct>(source).unwrap()).is_err(),
            "{source}"
        );
    }
}

#[test]
fn plain_fields_keep_default_decoding_and_integer_width_limits() {
    let input = syn::parse_str::<ItemStruct>(
        r#"
        struct Config { #[config(default = 4, min = 1, max = 30)] value: u8, names: Vec<String> }
    "#,
    )
    .unwrap();
    let expanded = expand(&input).unwrap().to_string();
    assert!(expanded.contains("u8 > :: MIN"));
    assert!(expanded.contains("u8 > :: MAX"));
    assert!(!expanded.contains("normalize_json_string_lists"));
    assert!(!expanded.contains("decode_value"));
}
