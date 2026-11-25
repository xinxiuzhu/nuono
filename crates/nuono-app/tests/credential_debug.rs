use nuono_app::{agent::store::ProviderModel, ai::AiConfig};

#[test]
fn debug_redacts_ai_credentials_and_arbitrary_configuration() {
    let config = AiConfig {
        api_key: "secret-key-marker".into(),
        base_url: "https://example.com/v1?token=secret-endpoint-marker".into(),
        system_prompt: Some("private-prompt-marker".into()),
        extra: [("authorization".into(), "secret-extra-marker".into())].into(),
        ..Default::default()
    };
    let debug = format!("{config:?}");
    for secret in [
        "secret-key-marker",
        "secret-endpoint-marker",
        "private-prompt-marker",
        "secret-extra-marker",
    ] {
        assert!(!debug.contains(secret), "Debug exposed {secret}");
    }
    assert!(
        debug.contains(&config.model),
        "safe model metadata remains useful"
    );
}

#[test]
fn debug_redacts_stored_provider_credentials() {
    let model = ProviderModel {
        id: 7,
        name: "test model".into(),
        provider: "custom".into(),
        provider_type: "openai_compatible".into(),
        model_id: "test-model".into(),
        api_key: Some("secret-key-marker".into()),
        base_url: "https://example.com/v1?token=secret-endpoint-marker".into(),
        is_default: false,
        enabled: true,
        created_at: None,
        updated_at: None,
    };
    let debug = format!("{model:?}");
    assert!(!debug.contains("secret-key-marker"));
    assert!(!debug.contains("secret-endpoint-marker"));
    assert!(debug.contains(&model.model_id));
}
