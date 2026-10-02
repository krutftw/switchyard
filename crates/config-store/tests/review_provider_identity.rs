//! Review finding: `unmask_into` hands the secrets of a provider that was
//! deleted to a *different* provider that happens to sit at its index.
//!
//! The brief: "Correspondence: providers by name; … (so reordering or
//! deleting other entries does not attach the wrong key)".
//!
//! `stored_provider` falls back from the name to the *index* whenever the
//! stored provider at that index is no longer named by any incoming provider
//! ("renamed in place"). Nothing checks that the incoming provider resembles
//! the stored one, so "delete provider A, add provider B" in one save looks
//! exactly like "rename A to B" — and every field of B that means "keep the
//! current value" (an empty key row, an empty credential-like header) is
//! filled with A's secret. B then sends A's API key, as `Authorization:
//! Bearer …`, to B's endpoint: an OpenAI key goes to whatever server the new
//! entry points at.
//!
//! The merge in this same crate takes the opposite view of the same situation
//! ("a different provider at the same index is a different provider").
//!
//! Expected: a provider whose name is new only inherits from the stored
//! provider at its index when it is recognisably the same provider (same
//! `kind` and endpoint, say); otherwise it has no counterpart — empty fields
//! stay empty, masked ones are reported.

use pretty_assertions::assert_eq;
use switchyard_config_store::{mask_config, unmask_into};
use switchyard_core::Config;
use switchyard_core::config::{CredentialConfig, ProviderConfig, ProviderKind};

const OPENAI_KEY: &str = "sk-openai-1111111111111111111111";
const OPENAI_CREDENTIAL: &str = "sk-openai-2222222222222222222222";
const OPENAI_HEADER: &str = "Bearer org-token-3333333333333333";

fn stored() -> Config {
    let mut openai = ProviderConfig::new("openai", ProviderKind::Openai);
    openai.api_keys = vec![OPENAI_KEY.to_string()];
    openai.credentials = vec![CredentialConfig {
        api_key: OPENAI_CREDENTIAL.to_string(),
        ..CredentialConfig::default()
    }];
    openai
        .headers
        .insert("X-Org-Token".to_string(), OPENAI_HEADER.to_string());
    let config = Config {
        providers: vec![openai],
        ..Config::default()
    };
    assert!(config.validate().is_empty(), "{:?}", config.validate());
    config
}

/// A self-hosted server takes the place of the OpenAI entry. Its form has one
/// (blank) key row; a local server needs no key.
fn replacement() -> ProviderConfig {
    let mut local = ProviderConfig::new("local-llm", ProviderKind::OpenaiCompat);
    local.base_url = "http://203.0.113.9:8000/v1".to_string();
    local
}

#[test]
fn a_blank_key_row_of_a_new_provider_is_not_filled_with_a_deleted_providers_key() {
    let current = stored();
    let mut update = mask_config(&current);
    let mut local = replacement();
    local.api_keys = vec![String::new()];
    update.providers = vec![local];

    // No masked value is involved, so there is nothing to refuse.
    unmask_into(&mut update, &current).unwrap();

    assert_eq!(
        update.providers[0].api_keys,
        vec![String::new()],
        "the OpenAI key was attached to `local-llm` ({})",
        update.providers[0].base_url
    );
}

#[test]
fn a_blank_credential_of_a_new_provider_is_not_filled_with_a_deleted_providers_key() {
    let current = stored();
    let mut update = mask_config(&current);
    let mut local = replacement();
    // A credential that only carries a weight (valid for `openai-compat`).
    local.credentials = vec![CredentialConfig {
        weight: Some(2),
        ..CredentialConfig::default()
    }];
    update.providers = vec![local];

    unmask_into(&mut update, &current).unwrap();

    assert_eq!(
        update.providers[0].credentials[0].api_key, "",
        "the OpenAI credential's key was attached to `local-llm`"
    );
}

#[test]
fn an_empty_header_of_a_new_provider_is_not_filled_with_a_deleted_providers_header() {
    let current = stored();
    let mut update = mask_config(&current);
    let mut local = replacement();
    local
        .headers
        .insert("X-Org-Token".to_string(), String::new());
    update.providers = vec![local];

    unmask_into(&mut update, &current).unwrap();

    assert_eq!(
        update.providers[0].headers["X-Org-Token"], "",
        "the OpenAI provider's header was attached to `local-llm`"
    );
}

/// What the fallback is for keeps working: the same provider under a new
/// name keeps its secrets.
#[test]
fn a_provider_renamed_in_place_still_keeps_its_secrets() {
    let current = stored();
    let mut update = mask_config(&current);
    update.providers[0].name = "openai-main".to_string();

    unmask_into(&mut update, &current).unwrap();

    assert_eq!(update.providers[0].api_keys, vec![OPENAI_KEY.to_string()]);
    assert_eq!(
        update.providers[0].credentials[0].api_key,
        OPENAI_CREDENTIAL
    );
    assert_eq!(update.providers[0].headers["X-Org-Token"], OPENAI_HEADER);
}
