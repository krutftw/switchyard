//! Regression tests from the review: `unmask_into` used to attach stored
//! secrets to the wrong entry.
//!
//! The brief: "any secret field that is empty or equals the mask of the
//! corresponding current secret is replaced by the current secret.
//! Correspondence: … api_keys / credentials by position within the provider
//! AND by mask match anywhere in that provider's list (so reordering or
//! deleting other entries does not attach the wrong key)".
//!
//! The first five tests are the reviewer's reproductions; the rest cover the
//! same family of mistakes in neighbouring cases.

use pretty_assertions::assert_eq;
use switchyard_config_store::{mask_config, unmask_into};
use switchyard_core::Config;
use switchyard_core::config::{ClientKey, CredentialConfig, ProviderConfig, ProviderKind};

const KEY_1: &str = "AIza-vertex-key-1111111111111111";
const KEY_2: &str = "AIza-vertex-key-2222222222222222";

fn keyed(key: &str) -> CredentialConfig {
    CredentialConfig {
        api_key: key.to_string(),
        ..CredentialConfig::default()
    }
}

fn service_account(file: &str) -> CredentialConfig {
    CredentialConfig {
        service_account_file: file.to_string(),
        ..CredentialConfig::default()
    }
}

fn vertex(credentials: Vec<CredentialConfig>) -> Config {
    let mut provider = ProviderConfig::new("vx", ProviderKind::Vertex);
    provider.location = "global".to_string();
    provider.credentials = credentials;
    with_providers(vec![provider])
}

fn with_providers(providers: Vec<ProviderConfig>) -> Config {
    let config = Config {
        providers,
        ..Config::default()
    };
    assert!(config.validate().is_empty(), "{:?}", config.validate());
    config
}

/// A Vertex provider with one API-key credential and one service-account
/// credential (which has no `api_key`). The dashboard deletes the API-key
/// credential and sends the remaining one back. Its `api_key` is empty, which
/// means "keep the current value" — and the current value of *that*
/// credential is empty. Instead, the key of the credential that was just
/// deleted is silently attached to the service-account credential (the empty
/// field takes whatever sits at its position in the stored list), so the
/// deleted key stays in the file and in use.
#[test]
fn deleting_a_keyed_credential_does_not_hand_its_key_to_the_keyless_one() {
    let current = vertex(vec![keyed(KEY_1), service_account("sa.json")]);

    let mut update = mask_config(&current);
    update.providers[0].credentials.remove(0);
    unmask_into(&mut update, &current).expect("nothing here is unresolvable");

    assert_eq!(
        update.providers[0].credentials,
        vec![service_account("sa.json")],
        "the deleted credential's key must not be attached to another credential"
    );
}

/// The same two credentials, merely reordered. Nothing is ambiguous: the
/// masked key matches exactly one stored key, and the service-account
/// credential never had a key. The empty field is resolved first, by
/// position, and takes the stored key; the masked field then finds its only
/// candidate already claimed and the whole update is refused.
#[test]
fn reordering_a_keyed_and_a_keyless_credential_is_not_an_error() {
    let current = vertex(vec![keyed(KEY_1), service_account("sa.json")]);

    let mut update = mask_config(&current);
    update.providers[0].credentials.swap(0, 1);
    let result = unmask_into(&mut update, &current);

    assert_eq!(result, Ok(()), "a pure reorder must be accepted");
    assert_eq!(
        update.providers[0].credentials,
        vec![service_account("sa.json"), keyed(KEY_1)]
    );
}

/// Two API-key credentials. The dashboard deletes the first and adds a new
/// service-account credential (no key) at the end. The new, empty field sits
/// at the position of the second stored key and takes it; the masked second
/// key then has no counterpart left and the update fails with "is masked and
/// does not match any stored secret".
#[test]
fn a_new_keyless_credential_does_not_steal_the_key_of_a_kept_one() {
    let current = vertex(vec![keyed(KEY_1), keyed(KEY_2)]);

    let mut update = mask_config(&current);
    update.providers[0].credentials.remove(0);
    update.providers[0]
        .credentials
        .push(service_account("new-sa.json"));
    let result = unmask_into(&mut update, &current);

    assert_eq!(result, Ok(()), "the kept key is identified by its mask");
    assert_eq!(
        update.providers[0].credentials,
        vec![keyed(KEY_2), service_account("new-sa.json")]
    );
}

/// The same defect in the `api_keys` shorthand: a blank row added after
/// deleting the first key takes the kept key's place, and the kept key's mask
/// is then reported as unknown.
#[test]
fn a_blank_api_key_row_does_not_steal_the_key_of_a_kept_one() {
    let mut provider = ProviderConfig::new("openai", ProviderKind::Openai);
    provider.api_keys = vec![
        "sk-openai-1111111111111111111111".to_string(),
        "sk-openai-2222222222222222222222".to_string(),
    ];
    let current = with_providers(vec![provider]);

    let mut update = mask_config(&current);
    update.providers[0].api_keys.remove(0);
    update.providers[0].api_keys.push(String::new());
    let result = unmask_into(&mut update, &current);

    assert_eq!(result, Ok(()), "the kept key is identified by its mask");
    assert_eq!(
        update.providers[0].api_keys[0],
        "sk-openai-2222222222222222222222"
    );
}

/// Proxy passwords (masked by `mask_config` as an extension of the brief) are
/// restored from *any* proxy in the configuration with the same scheme and
/// user, in configuration order, without preferring the proxy the field
/// itself held. Two providers use the same proxy user with different short
/// passwords (short secrets all mask to the same run of bullets). Editing the
/// port of the second provider's proxy silently gives it the first
/// provider's password.
#[test]
fn an_edited_proxy_keeps_its_own_password_not_another_providers() {
    let mut a = ProviderConfig::new("a", ProviderKind::Openai);
    a.api_keys = vec!["env:A".to_string()];
    a.proxy = "http://user:secretAAA@proxy-a.internal:3128".to_string();
    let mut b = ProviderConfig::new("b", ProviderKind::Openai);
    b.api_keys = vec!["env:B".to_string()];
    b.proxy = "http://user:secretBBB@proxy-b.internal:3128".to_string();
    let current = with_providers(vec![a, b]);

    let mut update = mask_config(&current);
    assert!(!update.providers[1].proxy.contains("secretBBB"));
    update.providers[1].proxy = update.providers[1].proxy.replace(":3128", ":8080");
    unmask_into(&mut update, &current).expect("the password is the field's own");

    assert_eq!(
        update.providers[1].proxy,
        "http://user:secretBBB@proxy-b.internal:8080"
    );
}

// ---------------------------------------------------------------------------
// The same family, neighbouring cases
// ---------------------------------------------------------------------------

fn labelled(key: &str, label: &str) -> CredentialConfig {
    CredentialConfig {
        api_key: key.to_string(),
        label: label.to_string(),
        ..CredentialConfig::default()
    }
}

/// A provider kind that accepts keyless credentials, so an empty key is a
/// valid outcome.
fn compat(credentials: Vec<CredentialConfig>) -> Config {
    let mut provider = ProviderConfig::new("local", ProviderKind::OpenaiCompat);
    provider.base_url = "http://127.0.0.1:11434/v1".to_string();
    provider.credentials = credentials;
    with_providers(vec![provider])
}

/// A new service-account credential placed *in front of* a kept key, where
/// the deleted key used to be.
#[test]
fn a_new_service_account_in_front_does_not_take_a_deleted_key() {
    let current = vertex(vec![keyed(KEY_1), keyed(KEY_2)]);

    let mut update = mask_config(&current);
    update.providers[0].credentials.remove(0);
    update.providers[0]
        .credentials
        .insert(0, service_account("new-sa.json"));
    assert_eq!(unmask_into(&mut update, &current), Ok(()));

    assert_eq!(
        update.providers[0].credentials,
        vec![service_account("new-sa.json"), keyed(KEY_2)]
    );
}

/// Empty fields are "keep the current value" of the entry they belong to,
/// and a label says which entry that is, wherever it moved.
#[test]
fn an_empty_field_follows_its_label_not_its_position() {
    let current = compat(vec![labelled(KEY_1, "team"), labelled(KEY_2, "backup")]);

    let mut update = current.clone();
    update.providers[0].credentials = vec![labelled("", "backup"), labelled("", "team")];
    assert_eq!(unmask_into(&mut update, &current), Ok(()));
    assert_eq!(
        update.providers[0].credentials,
        vec![labelled(KEY_2, "backup"), labelled(KEY_1, "team")]
    );

    // One of them deleted: the other still gets its own key.
    let mut update = current.clone();
    update.providers[0].credentials = vec![labelled("", "backup")];
    assert_eq!(unmask_into(&mut update, &current), Ok(()));
    assert_eq!(
        update.providers[0].credentials,
        vec![labelled(KEY_2, "backup")]
    );
}

/// Without any identity the position is all there is: an entry whose label
/// was edited and whose key field was left blank keeps the key at its place.
#[test]
fn a_relabelled_credential_keeps_the_key_at_its_position() {
    let current = compat(vec![labelled(KEY_1, "old name")]);
    let mut update = current.clone();
    update.providers[0].credentials = vec![labelled("", "new name")];
    assert_eq!(unmask_into(&mut update, &current), Ok(()));
    assert_eq!(
        update.providers[0].credentials,
        vec![labelled(KEY_1, "new name")]
    );
}

/// Labels that are not unique identify nothing; the position decides.
#[test]
fn duplicate_labels_fall_back_to_the_position() {
    let current = compat(vec![labelled(KEY_1, "pool"), labelled(KEY_2, "pool")]);
    let mut update = current.clone();
    update.providers[0].credentials = vec![labelled("", "pool"), labelled("", "pool")];
    assert_eq!(unmask_into(&mut update, &current), Ok(()));
    assert_eq!(update, current);

    // Masked, in the other order: each mask finds its key.
    let mut update = mask_config(&current);
    update.providers[0].credentials.swap(0, 1);
    assert_eq!(unmask_into(&mut update, &current), Ok(()));
    assert_eq!(
        update.providers[0].credentials,
        vec![labelled(KEY_2, "pool"), labelled(KEY_1, "pool")]
    );
}

/// The key of "a" is rotated (a new literal is typed) and a new keyless entry
/// "c" is added in front. The old key of "a" sits at position 0 and is no
/// longer claimed by anything — but it is recognisably a's, and must not be
/// given to "c".
#[test]
fn a_rotated_key_is_not_handed_to_a_new_entry_at_its_position() {
    let current = compat(vec![labelled(KEY_1, "a"), labelled(KEY_2, "b")]);
    let rotated = "sk-rotated-9999999999999999999999";

    let mut update = mask_config(&current);
    update.providers[0].credentials = vec![
        labelled("", "c"),
        labelled(rotated, "a"),
        update.providers[0].credentials[1].clone(),
    ];
    assert_eq!(unmask_into(&mut update, &current), Ok(()));
    assert_eq!(
        update.providers[0].credentials,
        vec![
            labelled("", "c"),
            labelled(rotated, "a"),
            labelled(KEY_2, "b")
        ]
    );
}

/// Client keys go through the same matching.
#[test]
fn a_new_client_key_row_does_not_steal_a_kept_key() {
    let client = |key: &str, name: &str| ClientKey {
        key: key.to_string(),
        name: name.to_string(),
        enabled: true,
        models: Vec::new(),
        rate_limit_rpm: None,
    };
    let mut current = Config::default();
    current.auth.keys = vec![
        client("sy-alice-aaaaaaaaaaaaaaaaaaaaaaaa", "alice"),
        client("sy-bob-bbbbbbbbbbbbbbbbbbbbbbbbbb", "bob"),
    ];

    // Alice is deleted, a blank row is added at the end.
    let mut update = mask_config(&current);
    update.auth.keys.remove(0);
    update.auth.keys.push(client("", "new"));
    assert_eq!(unmask_into(&mut update, &current), Ok(()));
    assert_eq!(update.auth.keys[0].key, "sy-bob-bbbbbbbbbbbbbbbbbbbbbbbbbb");
    assert_eq!(update.auth.keys[0].name, "bob");
    assert_eq!(
        update.auth.keys[1].key, "",
        "the blank row has no counterpart"
    );

    // Bob's key is rotated and a blank row is added in front, where alice's
    // key was — alice herself is still there, further down.
    let mut update = mask_config(&current);
    update.auth.keys.insert(0, client("", "new"));
    update.auth.keys[2].key = "sy-bob-rotated-cccccccccccccccccc".to_string();
    assert_eq!(unmask_into(&mut update, &current), Ok(()));
    let keys: Vec<&str> = update.auth.keys.iter().map(|k| k.key.as_str()).collect();
    assert_eq!(
        keys,
        vec![
            "",
            "sy-alice-aaaaaaaaaaaaaaaaaaaaaaaa",
            "sy-bob-rotated-cccccccccccccccccc"
        ]
    );
}

/// A stored secret that happens to look like a mask (it contains an
/// ellipsis) is not mistaken for one when it comes back verbatim.
#[test]
fn a_secret_that_looks_like_a_mask_survives_a_round_trip() {
    let odd = "pass…phrase with an ellipsis 1234";
    let mut provider = ProviderConfig::new("openai", ProviderKind::Openai);
    provider.api_keys = vec![odd.to_string()];
    provider
        .headers
        .insert("X-Api-Key".to_string(), odd.to_string());
    let mut current = with_providers(vec![provider]);
    current.admin.secret = odd.to_string();

    let mut update = current.clone();
    assert_eq!(unmask_into(&mut update, &current), Ok(()));
    assert_eq!(update, current);

    let mut update = mask_config(&current);
    assert_eq!(unmask_into(&mut update, &current), Ok(()));
    assert_eq!(update, current);
}

fn proxied(name: &str, proxy: &str) -> ProviderConfig {
    let mut provider = ProviderConfig::new(name, ProviderKind::Openai);
    provider.api_keys = vec![format!("env:{}", name.to_uppercase())];
    provider.proxy = proxy.to_string();
    provider
}

/// A masked proxy URL pasted into a setting that had no proxy can only be
/// resolved from the other proxies — and only when they agree.
#[test]
fn a_masked_proxy_password_is_never_guessed_between_candidates() {
    let current = with_providers(vec![
        proxied("a", "http://user:secretAAA@proxy.internal:3128"),
        proxied("b", "http://user:secretBBB@proxy.internal:3128"),
        proxied("c", ""),
    ]);
    let masked = mask_config(&current);
    assert_eq!(masked.providers[0].proxy, masked.providers[1].proxy);

    // Both candidates read exactly like the pasted URL and differ only in
    // the hidden password.
    let mut update = masked.clone();
    update.providers[2].proxy = masked.providers[0].proxy.clone();
    let issues = unmask_into(&mut update, &current).unwrap_err();
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].path, "providers[2].proxy");

    // With a single candidate there is nothing to guess.
    let current = with_providers(vec![
        proxied("a", "http://user:secretAAA@proxy.internal:3128"),
        proxied("b", "http://other:secretBBB@proxy.internal:3128"),
        proxied("c", ""),
    ]);
    let masked = mask_config(&current);
    let mut update = masked.clone();
    update.providers[2].proxy = masked.providers[1].proxy.clone();
    assert_eq!(unmask_into(&mut update, &current), Ok(()));
    assert_eq!(
        update.providers[2].proxy,
        "http://other:secretBBB@proxy.internal:3128"
    );
    assert_eq!(update.providers[0].proxy, current.providers[0].proxy);
    assert_eq!(update.providers[1].proxy, current.providers[1].proxy);
}

/// Credential proxies follow their credential when the list is reordered,
/// even when the passwords mask alike and the URLs were edited.
#[test]
fn credential_proxies_follow_their_credential() {
    let with_proxy = |key: &str, proxy: &str| CredentialConfig {
        api_key: key.to_string(),
        proxy: proxy.to_string(),
        ..CredentialConfig::default()
    };
    let mut provider = ProviderConfig::new("openai", ProviderKind::Openai);
    provider.credentials = vec![
        with_proxy(KEY_1, "socks5://user:secretAAA@10.0.0.1:1080"),
        with_proxy(KEY_2, "socks5://user:secretBBB@10.0.0.2:1080"),
    ];
    let current = with_providers(vec![provider]);

    let mut update = mask_config(&current);
    update.providers[0].credentials.swap(0, 1);
    for credential in &mut update.providers[0].credentials {
        credential.proxy = credential.proxy.replace(":1080", ":1081");
    }
    assert_eq!(unmask_into(&mut update, &current), Ok(()));
    assert_eq!(
        update.providers[0].credentials,
        vec![
            with_proxy(KEY_2, "socks5://user:secretBBB@10.0.0.2:1081"),
            with_proxy(KEY_1, "socks5://user:secretAAA@10.0.0.1:1081"),
        ]
    );
}

/// A password written into a provider's endpoint URL is a secret like any
/// other: masked on the way out, restored on the way back.
#[test]
fn a_password_in_the_base_url_is_masked_and_restored() {
    let mut provider = ProviderConfig::new("lab", ProviderKind::OpenaiCompat);
    provider.base_url = "https://gateway:endpoint-password-0123456789@lab.internal/v1".to_string();
    let current = with_providers(vec![provider]);

    let masked = mask_config(&current);
    let json = serde_json::to_string(&masked).unwrap();
    assert!(!json.contains("endpoint-password-0123456789"), "{json}");
    assert!(masked.providers[0].base_url.starts_with("https://gateway:"));
    assert!(masked.providers[0].base_url.ends_with("@lab.internal/v1"));

    // Untouched, and with the host edited.
    let mut update = masked.clone();
    assert_eq!(unmask_into(&mut update, &current), Ok(()));
    assert_eq!(update, current);
    let mut update = masked.clone();
    update.providers[0].base_url = update.providers[0]
        .base_url
        .replace("lab.internal", "lab2.internal");
    assert_eq!(unmask_into(&mut update, &current), Ok(()));
    assert_eq!(
        update.providers[0].base_url,
        "https://gateway:endpoint-password-0123456789@lab2.internal/v1"
    );

    // A masked password on a provider that has none stored is an error.
    let mut update = masked.clone();
    let mut other = ProviderConfig::new("other", ProviderKind::OpenaiCompat);
    other.base_url = masked.providers[0].base_url.clone();
    update.providers.push(other);
    let issues = unmask_into(&mut update, &current).unwrap_err();
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].path, "providers[1].base_url");
}

// ---------------------------------------------------------------------------
// Property: masking, then any mix of reordering, deleting and adding entries,
// then unmasking gives exactly the same edit applied to the real secrets.
// ---------------------------------------------------------------------------

/// Deterministic generator (xorshift64*), so failures reproduce.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }

    fn chance(&mut self, percent: u64) -> bool {
        self.next() % 100 < percent
    }
}

/// A key whose mask is unique: masks keep the first and last characters, so
/// the serial number goes at both ends.
fn unique_key(serial: usize) -> String {
    format!("{serial:05}-key-material-0123456789-{serial:05}")
}

/// A credential with a unique identity: a key (stored ones are masked on the
/// way out, new ones arrive in full) or a service-account file, and sometimes
/// a label.
fn random_credential(rng: &mut Rng, serial: usize, new: bool) -> CredentialConfig {
    let label = if rng.chance(50) {
        format!("label-{serial}")
    } else {
        String::new()
    };
    if rng.chance(35) {
        // A service-account credential has no key.
        CredentialConfig {
            service_account_file: format!("sa-{serial}.json"),
            label,
            ..CredentialConfig::default()
        }
    } else {
        CredentialConfig {
            api_key: if new {
                format!("new-{}", unique_key(serial))
            } else {
                unique_key(serial)
            },
            label,
            weight: rng.chance(30).then_some(2),
            ..CredentialConfig::default()
        }
    }
}

#[test]
fn any_edit_of_the_masked_lists_restores_exactly_the_right_secrets() {
    let mut rng = Rng(0x5EED_CAFE_F00D_0001);
    let mut serial = 0usize;

    for case in 0..1500 {
        let mut provider = ProviderConfig::new("vx", ProviderKind::Vertex);
        provider.location = "global".to_string();
        for _ in 0..rng.below(6) {
            serial += 1;
            provider
                .credentials
                .push(random_credential(&mut rng, serial, false));
        }
        for _ in 0..rng.below(5) {
            serial += 1;
            provider.api_keys.push(unique_key(serial));
        }
        let current = with_providers(vec![provider]);

        // The edit applied to the real configuration (what the result must
        // be) and, in step, to what the dashboard sees.
        let mut expected = current.clone();
        let mut update = mask_config(&current);
        for _ in 0..rng.below(5) {
            let n = expected.providers[0].credentials.len();
            let m = expected.providers[0].api_keys.len();
            match rng.below(6) {
                0 if n >= 2 => {
                    let (a, b) = (rng.below(n), rng.below(n));
                    expected.providers[0].credentials.swap(a, b);
                    update.providers[0].credentials.swap(a, b);
                }
                1 if n >= 1 => {
                    let at = rng.below(n);
                    expected.providers[0].credentials.remove(at);
                    update.providers[0].credentials.remove(at);
                }
                2 => {
                    serial += 1;
                    let at = rng.below(n + 1);
                    let new = random_credential(&mut rng, serial, true);
                    expected.providers[0].credentials.insert(at, new.clone());
                    update.providers[0].credentials.insert(at, new);
                }
                3 if m >= 2 => {
                    let (a, b) = (rng.below(m), rng.below(m));
                    expected.providers[0].api_keys.swap(a, b);
                    update.providers[0].api_keys.swap(a, b);
                }
                4 if m >= 1 => {
                    let at = rng.below(m);
                    expected.providers[0].api_keys.remove(at);
                    update.providers[0].api_keys.remove(at);
                }
                5 => {
                    serial += 1;
                    let at = rng.below(m + 1);
                    let new = format!("new-{}", unique_key(serial));
                    expected.providers[0].api_keys.insert(at, new.clone());
                    update.providers[0].api_keys.insert(at, new);
                }
                _ => {}
            }
        }
        // The dashboard may send an untouched key as an empty field instead
        // of its mask; a label then says which entry it is.
        for credential in &mut update.providers[0].credentials {
            let stored = current.providers[0]
                .credentials
                .iter()
                .any(|c| !c.label.is_empty() && c.label == credential.label);
            if stored && rng.chance(30) {
                credential.api_key.clear();
            }
        }

        let sent = update.clone();
        let result = unmask_into(&mut update, &current);
        assert_eq!(
            result,
            Ok(()),
            "case {case}\nstored: {:#?}\nsent: {:#?}",
            current.providers[0],
            sent.providers[0]
        );
        assert_eq!(
            update, expected,
            "case {case}\nstored: {:#?}\nsent: {:#?}",
            current.providers[0], sent.providers[0]
        );
    }
}
