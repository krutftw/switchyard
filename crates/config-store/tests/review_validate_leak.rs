//! Regression tests from the review: parse-error issues used to carry
//! secret values.
//!
//! `validate_text` documents "Messages never quote string values from the
//! file, so a mistyped secret cannot leak through an error", and the brief
//! says the watcher must "never log secret values". Issue messages are logged
//! by the store at `warn` level (`configuration rejected`), broadcast in
//! `ConfigEvent::Rejected`, and returned by the admin API.
//!
//! Only *quoted string* values and long unknown enum variants were redacted.
//! A secret written without quotes that happens to be numeric, and a secret
//! written where a key name goes, were echoed in full.

use switchyard_config_store::validate_text;

fn assert_no_leak(text: &str, secret: &str) {
    let issues = validate_text(text).expect_err("the text is not a valid configuration");
    for issue in &issues {
        assert!(
            !issue.message.contains(secret) && !issue.path.contains(secret),
            "the secret is echoed in the issue: {issue}"
        );
    }
}

/// A numeric admin secret / client key / API key written without quotes is a
/// TOML integer; serde reports "invalid type: integer `…`, expected a string"
/// with the whole value.
#[test]
fn an_unquoted_numeric_secret_is_not_echoed() {
    let secret = "8675309112233445";
    assert_no_leak(&format!("[admin]\nsecret = {secret}\n"), secret);
    assert_no_leak(&format!("[[auth.keys]]\nkey = {secret}\n"), secret);
    assert_no_leak(
        &format!("[[providers]]\nname = \"a\"\nkind = \"openai\"\napi_keys = [{secret}]\n"),
        secret,
    );
    // Beyond 64 bits the message is "integer `…` as i128".
    let long = "12345678901234567890123456789012345678";
    assert_no_leak(&format!("[admin]\nsecret = {long}\n"), long);
    // A float-looking secret.
    let float = "31415926.53589793";
    assert_no_leak(&format!("[admin]\nsecret = {float}\n"), float);
}

/// A key pasted where a field name goes (`sy-… = "laptop"` under
/// `[[auth.keys]]`, say) is reported as "unknown field `…`" with the whole
/// value.
#[test]
fn a_secret_written_as_a_field_name_is_not_echoed() {
    let secret = "sy-live-0123456789abcdefghijklmnop";
    assert_no_leak(&format!("[[auth.keys]]\n{secret} = \"laptop\"\n"), secret);
    assert_no_leak(&format!("[admin]\n\"{secret}\" = true\n"), secret);
}

/// The same family, swept: a key pasted into every kind of wrong place —
/// as the value of a number, a boolean, an enum, a list or a table; as a field
/// name, a section name or a root key; in text that is not even valid TOML.
/// Whatever the parser says about it, the key is not in the message.
#[test]
fn a_misplaced_secret_is_never_echoed_wherever_it_lands() {
    let secret = "sk-live-0123456789abcdefghijklmnopqrstuv";
    let number = "4155550123998877";
    let provider = "[[providers]]\nname = \"a\"\nkind = \"openai\"\n";
    let templates = [
        "[server]\nport = \"{S}\"\n".to_string(),
        "[server]\ncors = \"{S}\"\n".to_string(),
        "[server]\n{S} = 1\n".to_string(),
        "[server]\ntls = \"{S}\"\n".to_string(),
        "[routing]\nstrategy = \"{S}\"\n".to_string(),
        "[routing]\ncooldown = \"{S}\"\n".to_string(),
        "[logging]\nrequest_log = \"{S}\"\n".to_string(),
        "[usage]\nretention_days = \"{S}\"\n".to_string(),
        "[auth]\nkeys = \"{S}\"\n".to_string(),
        "[auth]\nkeys = [\"{S}\"]\n".to_string(),
        "[[auth.keys]]\nkey = \"k\"\nmodels = \"{S}\"\n".to_string(),
        "[[auth.keys]]\nkey = \"k\"\nrate_limit_rpm = \"{S}\"\n".to_string(),
        "[[auth.keys]]\nkey = \"k\"\nenabled = \"{S}\"\n".to_string(),
        "[[providers]]\nname = \"a\"\nkind = \"{S}\"\n".to_string(),
        format!("{provider}api_keys = \"{{S}}\"\n"),
        format!("{provider}headers = \"{{S}}\"\n"),
        format!("{provider}headers = {{ Authorization = [\"{{S}}\"] }}\n"),
        format!("{provider}wire_api = \"{{S}}\"\n"),
        format!("{provider}priority = \"{{S}}\"\n"),
        format!("{provider}credentials = \"{{S}}\"\n"),
        format!("{provider}credentials = [\"{{S}}\"]\n"),
        format!("{provider}[[providers.credentials]]\napi_key = [\"{{S}}\"]\n"),
        format!("{provider}[[providers.credentials]]\nweight = \"{{S}}\"\n"),
        format!("{provider}[[providers.credentials]]\n\"{{S}}\" = true\n"),
        format!(
            "{provider}[[providers.models]]\nid = \"m\"\nthinking = {{ levels = [\"{{S}}\"] }}\n"
        ),
        "[[payload.override]]\nmodels = [\"*\"]\nprotocol = \"{S}\"\nset = { a = 1 }\n".to_string(),
        "[[pricing]]\nmodel = \"m\"\ninput = \"{S}\"\n".to_string(),
        "[[aliases]]\nname = \"n\"\ntargets = \"{S}\"\n".to_string(),
        "{S} = \"x\"\n".to_string(),
        "\"{S}\" = \"x\"\n".to_string(),
        "[{S}]\nx = 1\n".to_string(),
        "[[{S}]]\nx = 1\n".to_string(),
        "[admin.{S}]\nx = 1\n".to_string(),
        // Not TOML at all.
        "[admin]\nsecret = {S}\n".to_string(),
        "[admin]\nsecret = \"{S}\n".to_string(),
        "[admin]\nsecret = '{S}\n".to_string(),
        "[admin]\nsecret = \"x\"\nsecret = \"{S}\"\n".to_string(),
        "[admin]\n{S}\n".to_string(),
        "{S}\n".to_string(),
        "[admin\nsecret = \"{S}\"\n".to_string(),
        // Numbers where text or a smaller number belongs.
        "[admin]\nsecret = {N}\n".to_string(),
        "[server]\nport = {N}\n".to_string(),
        "[server]\nhost = -{N}\n".to_string(),
        "[server]\nhost = {N}.25\n".to_string(),
        "[server]\nhost = 0x{N}\n".to_string(),
        "[[auth.keys]]\nkey = {N}\n".to_string(),
        "[[pricing]]\nmodel = {N}\n".to_string(),
        format!("{provider}api_keys = [{{N}}]\n"),
        format!("{provider}headers = {{ Authorization = {{N}} }}\n"),
        "[routing]\nstrategy = {N}\n".to_string(),
        "[usage]\nenabled = {N}\n".to_string(),
    ];
    for template in &templates {
        let (text, hidden) = if template.contains("{S}") {
            (template.replace("{S}", secret), secret)
        } else {
            (template.replace("{N}", number), number)
        };
        let issues = match validate_text(&text) {
            Err(issues) => issues,
            Ok(_) => panic!("the text is not a valid configuration:\n{text}"),
        };
        for issue in &issues {
            assert!(
                !issue.message.contains(hidden) && !issue.path.contains(hidden),
                "the secret is echoed for\n{text}\n-> {issue}"
            );
        }
        // And so is the error a caller would log or show.
        let error = switchyard_config_store::ConfigStoreError::Invalid(issues);
        assert!(!error.to_string().contains(hidden), "{error}");
        assert!(!format!("{error:?}").contains(hidden), "{error:?}");
    }
}

/// What is shown stays useful: a typo is readable, a small number too.
#[test]
fn typos_and_small_numbers_stay_readable() {
    let message = |text: &str| validate_text(text).unwrap_err().remove(0).message;
    assert!(message("[server]\nprot = 1\n").contains("`prot`"));
    assert!(message("[routing]\nstrategy = \"round-robbin\"\n").contains("`round-robbin`"));
    assert!(message("[server]\nport = 70000\n").contains("`70000`"));
    assert!(message("[server]\nport = -1\n").contains("`-1`"));
    // The names the schema expects are listed in full.
    assert!(message("[routing]\nprot = 1\n").contains("`session_affinity_ttl_secs`"));
}
