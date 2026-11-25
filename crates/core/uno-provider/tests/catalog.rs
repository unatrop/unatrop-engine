use uno_provider::{ContextProfile, ModelCatalog, ModelInfo};

#[test]
fn context_profile_uses_explicit_catalog_limits() {
    let mut info = ModelInfo::new("openai", "any-model-name");
    info.max_input_tokens = Some(250_000);
    info.max_output_tokens = Some(32_000);
    let profile = info.context_profile().expect("profile");
    assert_eq!(profile.max_input_tokens, 250_000);
    assert_eq!(profile.max_output_tokens, 32_000);
    assert_eq!(profile.compact_at_tokens, 200_000);
}

#[test]
fn unknown_model_does_not_guess_by_name() {
    for id in [
        "some-2027-flagship",
        "gemini-9-ultra",
        "claude-99",
        "gpt-42",
    ] {
        assert!(
            ModelInfo::new("test", id).context_profile().is_none(),
            "must not infer a profile for {id}"
        );
    }
}

#[test]
fn catalog_resolves_profile_for_known_model_only() {
    let mut info = ModelInfo::new("antigravity", "dynamic-model");
    info.max_input_tokens = Some(1_000_000);
    let catalog = ModelCatalog::from_models(vec![info]);
    let profile = catalog
        .context_profile_for("dynamic-model")
        .expect("profile");
    assert_eq!(profile.max_input_tokens, 1_000_000);
    assert_eq!(
        profile.max_output_tokens,
        ContextProfile::DEFAULT.max_output_tokens
    );
    assert!(catalog.context_profile_for("missing").is_none());
}
