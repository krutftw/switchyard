//! Regression test (review finding SY-BIN-2): `import-cliproxy` must not
//! rewrite secrets that look like numbers.
//!
//! A YAML reader that types every plain scalar (integer, float, hex), with
//! an importer that turns the number back into text, changes a key or
//! password written without quotes: leading zeros are dropped, long digit
//! strings become floats in exponent notation, hex and exponent spellings
//! are evaluated. CLIProxyAPI itself decodes these fields as strings and
//! keeps the text exactly as written (go-yaml hands a string field the
//! scalar's original text), so the imported gateway would silently refuse
//! the clients and the admin password that worked before, and send the
//! wrong key upstream.
//!
//! The importer therefore reads the document verbatim
//! (`switchyard::yaml::parse_verbatim`): the values of text settings are
//! imported as written.

use switchyard::import::convert;

#[test]
fn numeric_looking_secrets_are_imported_as_written() {
    let yaml = r#"
port: 8317
remote-management:
  secret-key: 012345
api-keys:
  - 00998877
  - 123456789012345678901234567890
  - 1e5
  - 0x1F
gemini-api-key:
  - api-key: 0123456789
"#;
    let imported = convert(yaml, "config.yaml").expect("the document converts");
    let config = &imported.config;

    assert_eq!(
        config.admin.secret, "012345",
        "the management password lost its leading zero"
    );

    let client_keys: Vec<&str> = config.auth.keys.iter().map(|k| k.key.as_str()).collect();
    assert_eq!(
        client_keys,
        ["00998877", "123456789012345678901234567890", "1e5", "0x1F"],
        "client keys must be the text of the YAML scalars"
    );

    let gemini = config
        .providers
        .iter()
        .find(|p| p.name == "gemini")
        .expect("the gemini provider");
    assert_eq!(
        gemini.api_keys,
        ["0123456789"],
        "the upstream key lost its leading zero"
    );
}

/// The same for the text settings that are not secrets but are matched
/// literally: a model alias written as `1.50` is not `1.5`.
#[test]
fn numeric_looking_names_are_imported_as_written() {
    let yaml = r#"
port: 8317
openai-compatibility:
  - name: 007
    base-url: "https://llm.example.com/v1"
    models:
      - name: 1.50
        alias: 0042
"#;
    let imported = convert(yaml, "config.yaml").expect("the document converts");
    let provider = imported
        .config
        .providers
        .first()
        .expect("the provider is imported");
    assert_eq!(provider.name, "007");
    assert_eq!(provider.models.len(), 1);
    assert_eq!(provider.models[0].id, "1.50");
    assert_eq!(provider.models[0].alias, "0042");
}
