use nuono_app::persona::*;

#[test]
fn bindings_preserve_exact_identity_and_validate_scoped_platforms() {
    let key = PersonaBindingKey {
        platform: "onebot".into(),
        bot_id: "a:b".into(),
        group_id: "c".into(),
        namespace: None,
    };
    let other = PersonaBindingKey {
        bot_id: "a".into(),
        group_id: "b:c".into(),
        ..key.clone()
    };
    assert_ne!(key.canonical_key(), other.canonical_key());
    assert_ne!(
        key.canonical_key(),
        PersonaBindingKey {
            namespace: Some("app".into()),
            ..key.clone()
        }
        .canonical_key()
    );
    assert!(
        PersonaBindingKey {
            platform: "qq_official".into(),
            ..key.clone()
        }
        .validate()
        .is_err()
    );
    assert!(
        PersonaBindingKey {
            namespace: Some("".into()),
            ..key
        }
        .validate()
        .is_err()
    );
}

#[test]
fn character_and_asset_inputs_reject_overflow_and_arbitrary_paths() {
    let mut input = PersonaInput {
        name: "小菲比".into(),
        ..Default::default()
    };
    input.validate().unwrap();
    input.system_prompt = "x".repeat(65537);
    assert!(input.validate().is_err());
    let id = "11111111-1111-4111-8111-111111111111";
    validate_asset_filename(PersonaAssetKind::Avatar, &format!("{id}.png")).unwrap();
    validate_asset_filename(PersonaAssetKind::ReferenceAudio, &format!("{id}.wav")).unwrap();
    for invalid in [
        format!("../{id}.png"),
        format!("{id}.svg"),
        format!("{id}.PNG"),
        "file:///etc/passwd".into(),
    ] {
        assert!(validate_asset_filename(PersonaAssetKind::Avatar, &invalid).is_err());
    }
}

#[tokio::test]
async fn unconfigured_speech_provider_explicitly_reports_absence() {
    let result = UnconfiguredTtsProvider
        .synthesize(TtsRequest {
            text: "你好".into(),
            reference_audio: None,
            reference_text: String::new(),
            language: "zh".into(),
            voice_id: None,
        })
        .await;
    assert!(matches!(result, Err(TtsError::NotConfigured)));
}

#[test]
fn legacy_character_history_is_isolated_by_bot_and_channel() {
    let mut first = nuono_app::agent::AgentContext::from_runtime_event(
        1,
        &serde_json::json!({
            "post_type":"message", "user_id":2, "group_id":300,
            "message_type":"group", "raw_message":"/ai hello"
        }),
    )
    .unwrap();
    let base_session = first.base_session_id();
    assert_ne!(first.session_id(), base_session);
    assert!(first.session_id().starts_with("group:"));
    first.persona = Some(PersonaRecord {
        id: "11111111-1111-4111-8111-111111111111".into(),
        name: "小菲比".into(),
        description: String::new(),
        system_prompt: String::new(),
        enabled: true,
        avatar_filename: None,
        portrait_filename: None,
        reference_audio_filename: None,
        reference_text: String::new(),
        language: "zh".into(),
        tts_provider: None,
        voice_id: None,
        revision: 1,
        updated_at: String::new(),
    });
    let mut other_bot = first.clone();
    other_bot.bot_id = 2;
    let mut other_channel = first.clone();
    other_channel.channel_id = Some("channel-a".into());
    assert_ne!(first.session_id(), other_bot.session_id());
    assert_ne!(first.session_id(), other_channel.session_id());
    assert_eq!(first.base_session_id(), other_bot.base_session_id());
    assert_eq!(first.base_session_id(), other_channel.base_session_id());
    let mut edited = first.clone();
    edited.persona.as_mut().unwrap().revision = 2;
    assert_eq!(first.session_id(), edited.session_id());
    let restored: nuono_app::agent::AgentContext =
        serde_json::from_str(&serde_json::to_string(&first).unwrap()).unwrap();
    assert_eq!(first.session_id(), restored.session_id());
}
