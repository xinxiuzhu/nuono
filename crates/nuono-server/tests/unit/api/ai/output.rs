use super::*;
#[test]
fn output_preferences_are_an_explicit_settings_patch_without_secrets() {
    let body: UpdateAiConfigPayload = serde_json::from_value(
        serde_json::json!({"output":{"default":{"mode":"segmented","segment_chars":300}}}),
    )
    .unwrap();
    let patch = build_ai_config_patch(body);
    assert_eq!(
        patch.settings,
        serde_json::json!({"ai":{"output":{"default":{"mode":"segmented","segment_chars":300}}}})
    );
    assert!(patch.secrets.is_empty());
    let empty: UpdateAiConfigPayload = serde_json::from_value(serde_json::json!({})).unwrap();
    assert_eq!(
        build_ai_config_patch(empty).settings,
        serde_json::json!({"ai":{}})
    );
}
