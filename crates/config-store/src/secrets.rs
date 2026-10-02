//! Hiding secrets from the dashboard and restoring them on the way back.
//!
//! [`mask_config`] produces the copy of a configuration that is safe to send
//! to the dashboard; [`unmask_into`] is its inverse for configurations the
//! dashboard sends back, where an untouched secret arrives as its mask (or as
//! an empty string) and must be replaced by the stored value.

use sha2::{Digest, Sha256};
use switchyard_core::Config;
use switchyard_core::config::{ConfigIssue, CredentialConfig, ProviderConfig, is_secret_reference};
use switchyard_core::util::mask_secret;

/// A stable, non-reversible identifier for a client key: `key_` followed by
/// the first 12 hexadecimal digits of the SHA-256 of the key (surrounding
/// whitespace ignored). Safe to show in dashboards, logs and usage records.
pub fn client_key_id(key: &str) -> String {
    let digest = Sha256::digest(key.trim().as_bytes());
    let mut id = String::with_capacity(16);
    id.push_str("key_");
    for byte in digest.iter().take(6) {
        id.push_str(&format!("{byte:02x}"));
    }
    id
}

/// How a secret is shown to the dashboard: references to environment
/// variables are not secret and stay as written, literals are masked.
fn mask_value(value: &str) -> String {
    if is_secret_reference(value) {
        value.to_string()
    } else {
        mask_secret(value.trim())
    }
}

/// Whether a header carries a credential, judging by its name
/// (`Authorization`, `X-Api-Key`, `Cookie`, …).
fn is_credential_header(name: &str) -> bool {
    const HINTS: [&str; 8] = [
        "auth",
        "key",
        "token",
        "secret",
        "cookie",
        "password",
        "credential",
        "signature",
    ];
    let name = name.to_ascii_lowercase();
    HINTS.iter().any(|hint| name.contains(hint))
}

/// Splits a URL (a proxy, an endpoint) around the secret in its user-info
/// part: what precedes the secret, the secret, what follows. `None` when the
/// URL carries no credentials.
///
/// The secret is the password of `scheme://user:PASSWORD@host…`. When there
/// is no password — `scheme://TOKEN@host…`, `scheme://TOKEN:@host…` — the
/// user name is the secret: that is how services that authenticate with an
/// API key alone are addressed.
fn split_proxy_password(url: &str) -> Option<(&str, &str, &str)> {
    let authority_start = url.find("://")? + 3;
    let rest = &url[authority_start..];
    let authority = &rest[..rest.find(['/', '?', '#']).unwrap_or(rest.len())];
    let at = authority.rfind('@')?;
    let (start, end) = match authority[..at].find(':') {
        Some(colon) if colon + 1 < at => (colon + 1, at),
        Some(colon) => (0, colon),
        None => (0, at),
    };
    let (start, end) = (authority_start + start, authority_start + end);
    (start < end).then(|| (&url[..start], &url[start..end], &url[end..]))
}

/// A proxy setting with the password of its URL, if any, masked.
fn mask_proxy(value: &str) -> String {
    match split_proxy_password(value) {
        Some((before, password, after)) => {
            format!("{before}{}{after}", mask_secret(password))
        }
        None => value.to_string(),
    }
}

/// Puts the stored password back into a proxy URL whose password came back
/// masked.
///
/// `own` is the stored proxy of the very setting being restored (the same
/// provider, the same credential). Its password is used whenever its mask is
/// the one that came back, whatever else of the URL was edited: short
/// passwords all mask alike, so the mask alone cannot tell two proxies apart,
/// but the place can.
///
/// Only when the setting's own proxy does not fit (the URL was copied from
/// another setting) are the other stored proxies consulted: one that reads the
/// same around the password, or else one with the same scheme and user. Should
/// those disagree about the password, nothing is restored rather than guess.
///
/// Returns false when the masked password cannot be resolved.
fn restore_proxy(value: &mut String, own: Option<&str>, stored: &[&str]) -> bool {
    let Some((before, password, after)) = split_proxy_password(value) else {
        return true;
    };
    if !looks_masked(password) {
        return true;
    }
    if let Some((_, own_password, _)) = own.and_then(split_proxy_password)
        && mask_secret(own_password) == password
    {
        *value = format!("{before}{own_password}{after}");
        return true;
    }
    let candidates: Vec<(&str, &str, &str)> = stored
        .iter()
        .filter_map(|s| split_proxy_password(s))
        .filter(|(_, stored_password, _)| mask_secret(stored_password) == password)
        .collect();
    let identical: Vec<&str> = candidates
        .iter()
        .filter(|(b, _, a)| *b == before && *a == after)
        .map(|(_, p, _)| *p)
        .collect();
    let same_user: Vec<&str> = candidates
        .iter()
        .filter(|(b, _, _)| *b == before)
        .map(|(_, p, _)| *p)
        .collect();
    let tier = if identical.is_empty() {
        same_user
    } else {
        identical
    };
    match tier.first() {
        Some(first) if tier.iter().all(|p| p == first) => {
            *value = format!("{before}{first}{after}");
            true
        }
        _ => false,
    }
}

/// Every proxy setting of a configuration.
fn proxies(config: &Config) -> Vec<&str> {
    let mut all = vec![config.upstream.proxy.as_str()];
    for provider in &config.providers {
        all.push(provider.proxy.as_str());
        all.extend(provider.credentials.iter().map(|c| c.proxy.as_str()));
    }
    all
}

/// A copy of `config` that is safe to send to the dashboard.
///
/// Every literal secret — `admin.secret`, `auth.keys[].key`,
/// `providers[].api_keys[]`, `providers[].credentials[].api_key` and the
/// values of provider headers whose name looks credential-like — is replaced
/// by [`mask_secret`]. Secret *references* (`env:NAME`, `${NAME}`) are kept as
/// written: they name a variable, they do not contain the secret.
///
/// Passwords inside URLs — the proxies (`upstream.proxy`,
/// `providers[].proxy`, `providers[].credentials[].proxy`) and a provider's
/// `base_url` written as `https://user:password@host/…` — are masked as
/// well; the rest of the URL stays readable. A URL with a user name but no
/// password (`http://API_KEY@host`) has its user name masked: without a
/// password, the name is the credential.
pub fn mask_config(config: &Config) -> Config {
    let mut masked = config.clone();
    masked.admin.secret = mask_value(&masked.admin.secret);
    masked.upstream.proxy = mask_proxy(&masked.upstream.proxy);
    for key in &mut masked.auth.keys {
        key.key = mask_value(&key.key);
    }
    for provider in &mut masked.providers {
        provider.proxy = mask_proxy(&provider.proxy);
        provider.base_url = mask_proxy(&provider.base_url);
        for key in &mut provider.api_keys {
            *key = mask_value(key);
        }
        for credential in &mut provider.credentials {
            credential.api_key = mask_value(&credential.api_key);
            credential.proxy = mask_proxy(&credential.proxy);
        }
        for (name, value) in provider.headers.iter_mut() {
            if is_credential_header(name) {
                *value = mask_value(value);
            }
        }
    }
    masked
}

/// Whether a value can only be the output of [`mask_secret`]: bullets only,
/// or containing the ellipsis that stands for the hidden middle part.
fn looks_masked(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty()
        && !is_secret_reference(value)
        && (value.chars().all(|c| c == '•') || value.contains('…'))
}

const NO_COUNTERPART: &str = "is masked and does not match any stored secret; enter the full value";

/// A secret field of the incoming configuration.
struct Slot<'a> {
    value: &'a mut String,
    /// Label of the entry the secret belongs to (client key name, credential
    /// label); empty when there is none.
    name: &'a str,
    /// What identifies the entry besides its secret and its label: a
    /// credential's service-account file. Empty when there is none.
    anchor: &'a str,
}

/// A stored secret the slot may correspond to.
struct Candidate<'a> {
    secret: &'a str,
    name: &'a str,
    anchor: &'a str,
}

fn same(a: &str, b: &str) -> bool {
    a.trim() == b.trim()
}

/// The one index for which `matches` holds, if there is exactly one.
fn only(count: usize, matches: impl Fn(usize) -> bool) -> Option<usize> {
    let mut found = (0..count).filter(|&c| matches(c));
    match (found.next(), found.next()) {
        (Some(c), None) => Some(c),
        _ => None,
    }
}

/// Fills the slots that ask for "the current value" (empty or masked) from
/// `own`, the list the slots correspond to, and — for masked values only —
/// from `other`, a second list in which the same secret may now live.
///
/// Masked fields are resolved before empty ones. A mask is evidence of which
/// stored secret is meant, so it wins over an empty field that merely happens
/// to sit where that secret used to be; otherwise deleting or moving an entry
/// would hand its secret to a neighbour that never had one.
///
/// Returns the indices of masked slots that matched nothing.
fn restore(slots: &mut [Slot<'_>], own: &[Candidate<'_>], other: &[&str]) -> Vec<usize> {
    #[derive(Clone, Copy, PartialEq)]
    enum Need {
        Nothing,
        Empty,
        Masked,
    }
    let mut claimed = vec![false; own.len()];
    let masks: Vec<String> = own.iter().map(|c| mask_value(c.secret)).collect();

    // A secret that came back in full keeps its own entry, so that an empty
    // field elsewhere cannot be filled with the same secret a second time.
    // (This also covers a stored secret that happens to look like a mask.)
    let mut needs: Vec<Need> = Vec::with_capacity(slots.len());
    for slot in slots.iter() {
        let value = slot.value.trim();
        let need = if value.is_empty() {
            Need::Empty
        } else if let Some(c) = (0..own.len()).find(|&c| !claimed[c] && same(own[c].secret, value))
        {
            claimed[c] = true;
            Need::Nothing
        } else if looks_masked(value) {
            Need::Masked
        } else {
            Need::Nothing
        };
        needs.push(need);
    }

    let assign = |slot: &mut Slot<'_>, need: &mut Need, c: usize, claimed: &mut [bool]| {
        *slot.value = own[c].secret.to_string();
        *need = Need::Nothing;
        claimed[c] = true;
    };

    // -- masked fields -----------------------------------------------------

    // 1. Same label and mask: survives reordering and deletion of others.
    for (slot, need) in slots.iter_mut().zip(needs.iter_mut()) {
        if *need != Need::Masked || slot.name.trim().is_empty() {
            continue;
        }
        let found = (0..own.len()).find(|&c| {
            !claimed[c] && same(own[c].name, slot.name) && masks[c] == slot.value.trim()
        });
        if let Some(c) = found {
            assign(slot, need, c, &mut claimed);
        }
    }

    // 2. Same position and mask.
    for (i, (slot, need)) in slots.iter_mut().zip(needs.iter_mut()).enumerate() {
        if *need == Need::Masked && i < own.len() && !claimed[i] && masks[i] == slot.value.trim() {
            assign(slot, need, i, &mut claimed);
        }
    }

    // 3. Same mask anywhere in the list: the entry moved.
    for (slot, need) in slots.iter_mut().zip(needs.iter_mut()) {
        if *need != Need::Masked {
            continue;
        }
        if let Some(c) = (0..own.len()).find(|&c| !claimed[c] && masks[c] == slot.value.trim()) {
            assign(slot, need, c, &mut claimed);
        }
    }

    // 4. Same mask in the sibling list: the entry changed its form
    //    (`api_keys` shorthand <-> `credentials` entry).
    let mut other_claimed = vec![false; other.len()];
    for (slot, need) in slots.iter_mut().zip(needs.iter_mut()) {
        if *need != Need::Masked {
            continue;
        }
        if let Some(c) = (0..other.len())
            .find(|&c| !other_claimed[c] && mask_value(other[c]) == slot.value.trim())
        {
            *slot.value = other[c].to_string();
            *need = Need::Nothing;
            other_claimed[c] = true;
        }
    }

    // -- empty fields ------------------------------------------------------
    //
    // An empty field carries no evidence of its own. What it keeps is the
    // secret of the stored entry it *is*, which may well be no secret at all
    // (a service-account credential has no API key).

    // The stored entry each incoming entry *is*, judging by its identity: its
    // service-account file, else its label — when exactly one incoming and
    // exactly one stored entry carry it. (Two entries with the same label
    // identify nothing.)
    let partners: Vec<Option<usize>> = slots
        .iter()
        .map(|slot| {
            if !slot.anchor.trim().is_empty() {
                only(slots.len(), |j| same(slots[j].anchor, slot.anchor))
                    .and_then(|_| only(own.len(), |c| same(own[c].anchor, slot.anchor)))
            } else if !slot.name.trim().is_empty() {
                only(slots.len(), |j| same(slots[j].name, slot.name))
                    .and_then(|_| only(own.len(), |c| same(own[c].name, slot.name)))
            } else {
                None
            }
        })
        .collect();

    // 5. An entry with a known identity keeps the secret of the stored entry
    //    it is. Such a field is settled even when nothing is restored: it is
    //    not whatever happens to sit at the same position. An entry that
    //    names a service-account file no stored entry has is new, and settled
    //    too.
    let mut settled = vec![false; slots.len()];
    for (i, (slot, need)) in slots.iter_mut().zip(needs.iter_mut()).enumerate() {
        if *need != Need::Empty {
            continue;
        }
        settled[i] = partners[i].is_some()
            || (!slot.anchor.trim().is_empty() && !own.iter().any(|c| same(c.anchor, slot.anchor)));
        if let Some(c) = partners[i]
            && !claimed[c]
            && same(own[c].anchor, slot.anchor)
        {
            assign(slot, need, c, &mut claimed);
        }
    }

    // 6. Same position — unless the stored entry there is of another kind
    //    (it has a service-account file, the field's entry has none) or is
    //    recognisably some other incoming entry.
    for i in 0..slots.len().min(own.len()) {
        if needs[i] != Need::Empty || settled[i] || claimed[i] {
            continue;
        }
        let spoken_for = (0..slots.len()).any(|j| j != i && partners[j] == Some(i));
        if same(own[i].anchor, slots[i].anchor) && !spoken_for {
            assign(&mut slots[i], &mut needs[i], i, &mut claimed);
        }
    }

    needs
        .iter()
        .enumerate()
        .filter(|(_, need)| **need == Need::Masked)
        .map(|(i, _)| i)
        .collect()
}

/// Restores a single secret that has exactly one possible counterpart.
/// Returns false when the value is masked but is not the mask of `current`.
fn restore_single(value: &mut String, current: Option<&str>) -> bool {
    let wants_current = value.trim().is_empty()
        || current.is_some_and(|c| !is_secret_reference(c) && mask_value(c) == value.trim());
    if wants_current {
        if let Some(current) = current {
            *value = current.to_string();
        }
        return true;
    }
    // A stored secret that itself looks like a mask may come back verbatim.
    current.is_some_and(|c| same(c, value)) || !looks_masked(value)
}

/// The stored credential an incoming one continues, for the settings that
/// are restored by place (its proxy): the one with the same key, else the
/// same service-account file, else the same label, else the same position.
fn stored_credential<'a>(
    incoming: &CredentialConfig,
    index: usize,
    stored: &'a [CredentialConfig],
) -> Option<&'a CredentialConfig> {
    let by = |wanted: &str, field: &dyn Fn(&CredentialConfig) -> &str| {
        if wanted.trim().is_empty() {
            return None;
        }
        only(stored.len(), |c| same(field(&stored[c]), wanted)).map(|c| &stored[c])
    };
    by(&incoming.api_key, &|c| c.api_key.as_str())
        .or_else(|| {
            by(&incoming.service_account_file, &|c| {
                c.service_account_file.as_str()
            })
        })
        .or_else(|| by(&incoming.label, &|c| c.label.as_str()))
        .or_else(|| stored.get(index))
}

/// Puts stored secrets back into a configuration that came from the
/// dashboard.
///
/// The dashboard only ever sees masked secrets ([`mask_config`]), so a secret
/// field that is **empty** or **equal to the mask of the stored secret** means
/// "unchanged" and is replaced by the stored value from `current`. Anything
/// else (a new literal, a secret reference) is kept as sent.
///
/// Correspondence between incoming and stored entries:
///
/// * `admin.secret` — the one stored secret.
/// * providers — by `name`. A provider whose name is new continues the stored
///   provider at the same index only when that is recognisably the same
///   provider under a new name: its old name is no longer used, and `kind`
///   and `base_url` are unchanged. Any other provider with a new name is a
///   new provider and inherits nothing: its empty fields stay empty and its
///   masked ones are reported. (Otherwise "delete A, add B" in one save would
///   hand A's keys to B's server.)
/// * `api_keys[]` and `credentials[].api_key` within a provider. **Masked**
///   values are resolved first: the entry at the same position if its mask
///   matches (credentials: preferring the same `label`), otherwise any entry
///   of the list with that mask, and finally an entry of the provider's other
///   list — so reordering entries, deleting some, or turning an `api_keys`
///   entry into a `credentials` entry never attaches the wrong key. **Empty**
///   values come second and only take what the masked ones left: a credential
///   is the stored credential with the same `service_account_file`, else the
///   same `label`, and keeps *that* credential's key (possibly none); without
///   such an identity it takes the entry at its position, unless that entry
///   belongs to a credential of another kind or to another incoming entry. An
///   empty field that corresponds to nothing stays empty.
/// * `auth.keys[].key` — masked: the key with the same mask and `name`, then
///   the one at the same position, then any key with the same mask. Empty:
///   the key with the same `name`, else the one at the same position.
/// * credential-like provider headers — by header name. A masked value that
///   comes back under a name that is not credential-like (the header was
///   renamed) is reported, not restored: it would be shown in clear from then
///   on.
/// * passwords in proxy URLs — the password of the same setting (the same
///   provider, the same credential) when its mask is the one that came back,
///   so the host, port or user can be edited without retyping the password;
///   otherwise the stored proxy with the same scheme and user whose password
///   has that mask, preferring one whose URL is otherwise identical. When
///   those candidates disagree, the password is not guessed.
/// * a password in a provider's `base_url` — that of the same provider.
///
/// Limits that follow from the protocol: secrets of up to 11 characters all
/// mask alike, so two short keys in one list can only be told apart by label
/// or position; and an empty field cannot say "delete this secret" — remove
/// the entry instead.
///
/// A masked value that matches no stored secret cannot be resolved; each one
/// is reported as an issue and `update` should then be discarded.
pub fn unmask_into(update: &mut Config, current: &Config) -> Result<(), Vec<ConfigIssue>> {
    let mut issues = Vec::new();
    let mut issue = |path: String| {
        issues.push(ConfigIssue {
            path,
            message: NO_COUNTERPART.to_string(),
        });
    };

    if !restore_single(&mut update.admin.secret, Some(&current.admin.secret)) {
        issue("admin.secret".to_string());
    }

    let stored_proxies = proxies(current);
    if !restore_proxy(
        &mut update.upstream.proxy,
        Some(&current.upstream.proxy),
        &stored_proxies,
    ) {
        issue("upstream.proxy".to_string());
    }

    {
        let own: Vec<Candidate<'_>> = current
            .auth
            .keys
            .iter()
            .map(|k| Candidate {
                secret: &k.key,
                name: &k.name,
                anchor: "",
            })
            .collect();
        let mut slots: Vec<Slot<'_>> = update
            .auth
            .keys
            .iter_mut()
            .map(|k| Slot {
                value: &mut k.key,
                name: &k.name,
                anchor: "",
            })
            .collect();
        for i in restore(&mut slots, &own, &[]) {
            issue(format!("auth.keys[{i}].key"));
        }
    }

    let incoming_names: Vec<String> = update.providers.iter().map(|p| p.name.clone()).collect();
    for (i, provider) in update.providers.iter_mut().enumerate() {
        let stored = stored_provider(current, &incoming_names, i, provider);
        if !restore_proxy(
            &mut provider.proxy,
            stored.map(|p| p.proxy.as_str()),
            &stored_proxies,
        ) {
            issue(format!("providers[{i}].proxy"));
        }
        // An endpoint has no other place its password could come from.
        if !restore_proxy(
            &mut provider.base_url,
            stored.map(|p| p.base_url.as_str()),
            &[],
        ) {
            issue(format!("providers[{i}].base_url"));
        }
        let (stored_keys, stored_credentials): (Vec<&str>, Vec<Candidate<'_>>) = match stored {
            Some(p) => (
                p.api_keys.iter().map(String::as_str).collect(),
                p.credentials
                    .iter()
                    .map(|c| Candidate {
                        secret: &c.api_key,
                        name: &c.label,
                        anchor: &c.service_account_file,
                    })
                    .collect(),
            ),
            None => (Vec::new(), Vec::new()),
        };
        let stored_credential_keys: Vec<&str> =
            stored_credentials.iter().map(|c| c.secret).collect();

        {
            let own: Vec<Candidate<'_>> = stored_keys
                .iter()
                .map(|k| Candidate {
                    secret: k,
                    name: "",
                    anchor: "",
                })
                .collect();
            let mut slots: Vec<Slot<'_>> = provider
                .api_keys
                .iter_mut()
                .map(|k| Slot {
                    value: k,
                    name: "",
                    anchor: "",
                })
                .collect();
            for j in restore(&mut slots, &own, &stored_credential_keys) {
                issue(format!("providers[{i}].api_keys[{j}]"));
            }
        }
        {
            let mut slots: Vec<Slot<'_>> = provider
                .credentials
                .iter_mut()
                .map(|c| Slot {
                    value: &mut c.api_key,
                    name: &c.label,
                    anchor: &c.service_account_file,
                })
                .collect();
            for j in restore(&mut slots, &stored_credentials, &stored_keys) {
                issue(format!("providers[{i}].credentials[{j}].api_key"));
            }
        }
        // After the keys: a credential is recognised by its restored key.
        for (j, credential) in provider.credentials.iter_mut().enumerate() {
            let own = stored
                .and_then(|p| stored_credential(credential, j, &p.credentials))
                .map(|c| c.proxy.as_str());
            if !restore_proxy(&mut credential.proxy, own, &stored_proxies) {
                issue(format!("providers[{i}].credentials[{j}].proxy"));
            }
        }
        for (name, value) in provider.headers.iter_mut() {
            if !is_credential_header(name) {
                // The mask of one of this provider's credential headers under
                // a name that is not masked: the header was renamed. Putting
                // the secret back would show it in clear from now on, and
                // keeping the mask would send bullets upstream.
                let moved_mask = looks_masked(value)
                    && stored.is_some_and(|p| {
                        p.headers.iter().any(|(stored_name, stored_value)| {
                            is_credential_header(stored_name)
                                && mask_value(stored_value) == value.trim()
                        })
                    });
                if moved_mask {
                    issue(format!("providers[{i}].headers.{name}"));
                }
                continue;
            }
            let stored_value = stored.and_then(|p| {
                p.headers.get(name).or_else(|| {
                    p.headers
                        .iter()
                        .find(|(n, _)| n.eq_ignore_ascii_case(name))
                        .map(|(_, v)| v)
                })
            });
            if !restore_single(value, stored_value.map(String::as_str)) {
                issue(format!("providers[{i}].headers.{name}"));
            }
        }
    }

    if issues.is_empty() {
        Ok(())
    } else {
        Err(issues)
    }
}

/// The stored provider an incoming provider corresponds to: the one with
/// its name, or — for a provider that was renamed where it stands — the one
/// at its index.
///
/// The second rule needs care. "Provider A deleted, provider B added" in one
/// save puts B at A's index under a name nobody stored, exactly like a
/// rename. Taking B for A would fill B's empty key rows with A's secrets and
/// have the gateway send them to B's server. So the provider at the index
/// only counts when it is recognisably the same one: no incoming provider
/// goes by its name, and it is of the same kind and talks to the same
/// endpoint. A provider that is renamed *and* pointed elsewhere in one save
/// has no counterpart; its secrets must be entered again.
fn stored_provider<'a>(
    current: &'a Config,
    incoming_names: &[String],
    index: usize,
    incoming: &ProviderConfig,
) -> Option<&'a ProviderConfig> {
    if let Some(provider) = current.provider(&incoming.name) {
        return Some(provider);
    }
    current.providers.get(index).filter(|stored| {
        !incoming_names.contains(&stored.name)
            && stored.kind == incoming.kind
            && same_endpoint(&incoming.base_url, &stored.base_url)
    })
}

/// Whether an incoming `base_url` names the stored endpoint: the same text,
/// or the stored URL as the dashboard shows it (its password masked). An
/// empty URL on both sides is the kind's default endpoint.
fn same_endpoint(incoming: &str, stored: &str) -> bool {
    let plain = |url: &str| url.trim().trim_end_matches('/').to_string();
    let incoming = plain(incoming);
    incoming == plain(stored) || incoming == plain(&mask_proxy(stored))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use switchyard_core::config::{ClientKey, ProviderKind};

    const ADMIN: &str = "admin-secret-0123456789abcdef";
    const CLIENT_A: &str = "sy-client-aaaaaaaaaaaaaaaaaaaaaaaa";
    const CLIENT_B: &str = "sy-client-bbbbbbbbbbbbbbbbbbbbbbbb";
    const KEY_1: &str = "sk-openai-1111111111111111111111";
    const KEY_2: &str = "sk-openai-2222222222222222222222";
    const KEY_3: &str = "sk-openai-3333333333333333333333";
    const CRED_1: &str = "sk-team-44444444444444444444444444";
    const CRED_2: &str = "sk-team-55555555555555555555555555";
    const HEADER: &str = "Bearer header-secret-6666666666666666";
    const SHORT: &str = "tiny";
    const PROXY_PASSWORD: &str = "proxy-password-7777777777";
    const PROXY_PASSWORD_2: &str = "hunter2";

    fn client(key: &str, name: &str) -> ClientKey {
        ClientKey {
            key: key.to_string(),
            name: name.to_string(),
            enabled: true,
            models: Vec::new(),
            rate_limit_rpm: None,
        }
    }

    fn credential(key: &str, label: &str) -> CredentialConfig {
        CredentialConfig {
            api_key: key.to_string(),
            label: label.to_string(),
            ..CredentialConfig::default()
        }
    }

    fn sample() -> Config {
        let mut config = Config::default();
        config.admin.secret = ADMIN.to_string();
        config.auth.keys = vec![
            client(CLIENT_A, "laptop"),
            client(CLIENT_B, "ci"),
            client("env:CLIENT_KEY", "from-env"),
        ];
        let mut openai = ProviderConfig::new("openai", ProviderKind::Openai);
        openai.api_keys = vec![
            KEY_1.to_string(),
            KEY_2.to_string(),
            "env:OPENAI_API_KEY".to_string(),
            KEY_3.to_string(),
        ];
        openai.credentials = vec![credential(CRED_1, "team"), credential(CRED_2, "backup")];
        openai
            .headers
            .insert("Authorization".to_string(), HEADER.to_string());
        openai
            .headers
            .insert("X-Title".to_string(), "switchyard".to_string());
        let mut local = ProviderConfig::new("local", ProviderKind::OpenaiCompat);
        local.base_url = "http://127.0.0.1:11434/v1".to_string();
        local.api_keys = vec![SHORT.to_string(), "${LOCAL_KEY}".to_string()];
        local
            .headers
            .insert("x-api-key".to_string(), "env:HEADER_KEY".to_string());
        openai.proxy = format!("http://corp:{PROXY_PASSWORD}@proxy.internal:3128");
        openai.credentials[1].proxy = format!("socks5://team:{PROXY_PASSWORD_2}@10.0.0.1:1080/");
        config.upstream.proxy = format!("socks5h://ops:{PROXY_PASSWORD_2}@gate.internal:1080");
        config.providers = vec![openai, local];
        config
    }

    const LITERALS: [&str; 11] = [
        ADMIN,
        CLIENT_A,
        CLIENT_B,
        KEY_1,
        KEY_2,
        KEY_3,
        CRED_1,
        CRED_2,
        HEADER,
        PROXY_PASSWORD,
        PROXY_PASSWORD_2,
    ];

    #[test]
    fn client_key_ids_are_stable_and_opaque() {
        // First six bytes of SHA-256("sy-test").
        let id = client_key_id("sy-test");
        assert_eq!(id.len(), 16);
        assert!(id.starts_with("key_"));
        assert!(
            id[4..]
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
        );
        assert_eq!(id, client_key_id("sy-test"));
        assert_eq!(id, client_key_id("  sy-test\n"));
        assert_ne!(id, client_key_id("sy-test2"));
        assert!(!id.contains("sy-test"));
        // Known vector: SHA-256("abc") = ba7816bf8f01cfea…
        assert_eq!(client_key_id("abc"), "key_ba7816bf8f01");
    }

    #[test]
    fn mask_config_never_returns_a_literal_secret() {
        let config = sample();
        let masked = mask_config(&config);
        let json = serde_json::to_string(&masked).unwrap();
        for secret in LITERALS {
            assert!(!json.contains(secret), "{secret} leaked: {json}");
        }
        // A short secret is hidden entirely.
        assert_eq!(masked.providers[1].api_keys[0], "••••");
        // The TOML rendering is just as clean.
        let toml = masked.to_toml().unwrap();
        for secret in LITERALS {
            assert!(!toml.contains(secret), "{secret} leaked: {toml}");
        }
    }

    #[test]
    fn mask_config_keeps_references_and_everything_else() {
        let config = sample();
        let masked = mask_config(&config);
        assert_eq!(masked.auth.keys[2].key, "env:CLIENT_KEY");
        assert_eq!(masked.providers[0].api_keys[2], "env:OPENAI_API_KEY");
        assert_eq!(masked.providers[1].api_keys[1], "${LOCAL_KEY}");
        assert_eq!(masked.providers[1].headers["x-api-key"], "env:HEADER_KEY");
        assert_eq!(masked.providers[0].headers["X-Title"], "switchyard");
        assert_eq!(masked.providers[0].api_keys[0], mask_secret(KEY_1));
        assert_eq!(masked.admin.secret, mask_secret(ADMIN));

        // Only secrets differ.
        let mut restored = masked.clone();
        unmask_into(&mut restored, &config).unwrap();
        assert_eq!(restored, config);
    }

    #[test]
    fn empty_admin_secret_stays_empty_when_none_is_stored() {
        let current = Config::default();
        let mut update = mask_config(&current);
        assert_eq!(update.admin.secret, "");
        unmask_into(&mut update, &current).unwrap();
        assert_eq!(update, current);
    }

    #[test]
    fn empty_fields_take_the_value_at_their_position() {
        let config = sample();
        let mut update = mask_config(&config);
        update.admin.secret.clear();
        update.auth.keys[1].key.clear();
        update.providers[0].api_keys[1].clear();
        update.providers[0].credentials[0].api_key.clear();
        update.providers[0].headers["Authorization"].clear();
        unmask_into(&mut update, &config).unwrap();
        assert_eq!(update, config);
    }

    #[test]
    fn new_literals_and_references_are_kept() {
        let config = sample();
        let mut update = mask_config(&config);
        update.admin.secret = "a-brand-new-admin-secret".to_string();
        update.auth.keys[0].key = "sy-rotated-cccccccccccccccccccccc".to_string();
        update.providers[0].api_keys[0] = "env:ROTATED".to_string();
        update.providers[0].credentials[1].api_key = "sk-new-77777777777777777777".to_string();
        update.providers[0].headers["Authorization"] = "Bearer new-header-88888888".to_string();
        unmask_into(&mut update, &config).unwrap();

        assert_eq!(update.admin.secret, "a-brand-new-admin-secret");
        assert_eq!(update.auth.keys[0].key, "sy-rotated-cccccccccccccccccccccc");
        assert_eq!(update.auth.keys[1].key, CLIENT_B);
        assert_eq!(update.providers[0].api_keys[0], "env:ROTATED");
        assert_eq!(update.providers[0].api_keys[1], KEY_2);
        assert_eq!(update.providers[0].credentials[0].api_key, CRED_1);
        assert_eq!(
            update.providers[0].credentials[1].api_key,
            "sk-new-77777777777777777777"
        );
        assert_eq!(
            update.providers[0].headers["Authorization"],
            "Bearer new-header-88888888"
        );
    }

    #[test]
    fn reordering_keeps_each_key_with_its_mask() {
        let config = sample();
        let mut update = mask_config(&config);
        update.providers[0].api_keys.reverse();
        update.providers[0].credentials.reverse();
        update.providers[1].api_keys.reverse();
        update.auth.keys.reverse();
        update.providers.reverse();
        unmask_into(&mut update, &config).unwrap();

        let openai = &update.providers[1];
        assert_eq!(openai.name, "openai");
        assert_eq!(
            openai.api_keys,
            vec![KEY_3, "env:OPENAI_API_KEY", KEY_2, KEY_1]
        );
        assert_eq!(openai.credentials[0].api_key, CRED_2);
        assert_eq!(openai.credentials[0].label, "backup");
        assert_eq!(openai.credentials[1].api_key, CRED_1);
        // A fully hidden short key is found by its mask too.
        assert_eq!(update.providers[0].api_keys, vec!["${LOCAL_KEY}", SHORT]);
        assert_eq!(update.auth.keys[0].key, "env:CLIENT_KEY");
        assert_eq!(update.auth.keys[1].key, CLIENT_B);
        assert_eq!(update.auth.keys[2].key, CLIENT_A);
    }

    #[test]
    fn deleting_entries_does_not_shift_secrets() {
        let config = sample();
        let mut update = mask_config(&config);
        // Drop the first API key, the first credential, the first client key
        // and the first provider.
        update.providers[0].api_keys.remove(0);
        update.providers[0].credentials.remove(0);
        update.auth.keys.remove(0);
        unmask_into(&mut update, &config).unwrap();
        assert_eq!(
            update.providers[0].api_keys,
            vec![KEY_2, "env:OPENAI_API_KEY", KEY_3]
        );
        assert_eq!(update.providers[0].credentials[0].api_key, CRED_2);
        assert_eq!(update.auth.keys[0].key, CLIENT_B);
        assert_eq!(update.auth.keys[0].name, "ci");

        let mut update = mask_config(&config);
        update.providers.remove(0);
        unmask_into(&mut update, &config).unwrap();
        assert_eq!(update.providers[0].api_keys, vec![SHORT, "${LOCAL_KEY}"]);
    }

    #[test]
    fn new_entries_sit_beside_restored_ones() {
        let config = sample();
        let mut update = mask_config(&config);
        update.providers[0]
            .api_keys
            .insert(0, "sk-inserted-99999999999999999999".to_string());
        update
            .auth
            .keys
            .insert(1, client("sy-new-dddddddddddddddddddddddd", "phone"));
        let mut added = ProviderConfig::new("anthropic", ProviderKind::Anthropic);
        added.api_keys = vec!["sk-ant-eeeeeeeeeeeeeeeeeeeeeeee".to_string()];
        update.providers.insert(0, added);
        unmask_into(&mut update, &config).unwrap();

        assert_eq!(
            update.providers[0].api_keys,
            vec!["sk-ant-eeeeeeeeeeeeeeeeeeeeeeee"]
        );
        assert_eq!(
            update.providers[1].api_keys,
            vec![
                "sk-inserted-99999999999999999999",
                KEY_1,
                KEY_2,
                "env:OPENAI_API_KEY",
                KEY_3
            ]
        );
        assert_eq!(update.providers[2].api_keys, vec![SHORT, "${LOCAL_KEY}"]);
        let keys: Vec<&str> = update.auth.keys.iter().map(|k| k.key.as_str()).collect();
        assert_eq!(
            keys,
            vec![
                CLIENT_A,
                "sy-new-dddddddddddddddddddddddd",
                CLIENT_B,
                "env:CLIENT_KEY"
            ]
        );
    }

    #[test]
    fn masked_value_without_counterpart_is_an_error() {
        let config = sample();
        let mut update = mask_config(&config);
        update.admin.secret = "abc…xyz".to_string();
        update.auth.keys[0].key = "sy-…zzz".to_string();
        update.providers[0].api_keys[0] = "sk-…000".to_string();
        update.providers[0].credentials[0].api_key = "••••••".to_string();
        update.providers[0].headers["Authorization"] = "Bea…000".to_string();
        let mut unknown = ProviderConfig::new("fresh", ProviderKind::Openai);
        unknown.api_keys = vec![mask_secret(KEY_1)];
        update.providers.push(unknown);

        let issues = unmask_into(&mut update, &config).unwrap_err();
        let paths: Vec<&str> = issues.iter().map(|i| i.path.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                "admin.secret",
                "auth.keys[0].key",
                "providers[0].api_keys[0]",
                "providers[0].credentials[0].api_key",
                "providers[0].headers.Authorization",
                "providers[2].api_keys[0]",
            ]
        );
        for issue in &issues {
            assert_eq!(issue.message, NO_COUNTERPART);
            for secret in LITERALS {
                assert!(!issue.message.contains(secret));
            }
        }
    }

    #[test]
    fn a_mask_is_used_at_most_once() {
        // Two stored keys share a mask; sending that mask three times cannot
        // be satisfied.
        let mut config = Config::default();
        let mut p = ProviderConfig::new("p", ProviderKind::Openai);
        p.api_keys = vec!["short-a".to_string(), "short-b".to_string()];
        config.providers = vec![p];
        let mask = mask_secret("short-a");
        assert_eq!(mask, mask_secret("short-b"));

        let mut update = mask_config(&config);
        unmask_into(&mut update, &config).unwrap();
        assert_eq!(update, config);

        let mut update = mask_config(&config);
        update.providers[0].api_keys.push(mask.clone());
        let issues = unmask_into(&mut update, &config).unwrap_err();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].path, "providers[0].api_keys[2]");
    }

    #[test]
    fn retyped_secret_is_not_duplicated_into_an_empty_field() {
        let mut config = Config::default();
        let mut p = ProviderConfig::new("p", ProviderKind::Openai);
        p.api_keys = vec![KEY_1.to_string()];
        config.providers = vec![p];

        let mut update = config.clone();
        update.providers[0].api_keys = vec![String::new(), KEY_1.to_string()];
        unmask_into(&mut update, &config).unwrap();
        assert_eq!(update.providers[0].api_keys, vec!["", KEY_1]);
    }

    #[test]
    fn key_moving_between_shorthand_and_credentials_is_followed() {
        let config = sample();
        let mut update = mask_config(&config);
        // The dashboard turns the first `api_keys` entry into a full
        // credential so it can carry a label.
        let moved = update.providers[0].api_keys.remove(0);
        update.providers[0]
            .credentials
            .push(credential(&moved, "promoted"));
        unmask_into(&mut update, &config).unwrap();
        assert_eq!(
            update.providers[0].api_keys,
            vec![KEY_2, "env:OPENAI_API_KEY", KEY_3]
        );
        assert_eq!(update.providers[0].credentials[2].api_key, KEY_1);
        assert_eq!(update.providers[0].credentials[2].label, "promoted");
    }

    #[test]
    fn client_keys_follow_their_name() {
        let mut config = Config::default();
        // Same mask, different names.
        config.auth.keys = vec![client("same-aaaa", "alice"), client("same-bbbb", "bob")];
        assert_eq!(mask_secret("same-aaaa"), mask_secret("same-bbbb"));

        let mut update = mask_config(&config);
        update.auth.keys.swap(0, 1);
        unmask_into(&mut update, &config).unwrap();
        assert_eq!(update.auth.keys[0].name, "bob");
        assert_eq!(update.auth.keys[0].key, "same-bbbb");
        assert_eq!(update.auth.keys[1].key, "same-aaaa");

        // An empty key with a known name is restored by name even after the
        // list shrank.
        let mut update = mask_config(&config);
        update.auth.keys.remove(0);
        update.auth.keys[0].key.clear();
        unmask_into(&mut update, &config).unwrap();
        assert_eq!(update.auth.keys[0].key, "same-bbbb");
    }

    #[test]
    fn renamed_provider_keeps_its_secrets() {
        let config = sample();
        let mut update = mask_config(&config);
        update.providers[0].name = "openai-main".to_string();
        unmask_into(&mut update, &config).unwrap();
        assert_eq!(update.providers[0].api_keys[0], KEY_1);
        assert_eq!(update.providers[0].credentials[1].api_key, CRED_2);
        assert_eq!(update.providers[0].headers["Authorization"], HEADER);
    }

    /// A rename is recognised by what the provider talks to, so a URL with
    /// a masked password must still count as the same endpoint.
    #[test]
    fn renamed_provider_with_a_password_in_its_endpoint_keeps_its_secrets() {
        let mut config = sample();
        config.providers[1].base_url = format!("https://svc:{PROXY_PASSWORD}@llm.internal/v1/");
        let mut update = mask_config(&config);
        assert!(!update.providers[1].base_url.contains(PROXY_PASSWORD));
        update.providers[1].name = "local-2".to_string();
        unmask_into(&mut update, &config).unwrap();
        assert_eq!(update.providers[1].api_keys[0], SHORT);
        assert_eq!(update.providers[1].base_url, config.providers[1].base_url);
    }

    /// A new name *and* another endpoint or kind: not the stored provider.
    /// Its masked fields have no counterpart and its empty ones stay empty —
    /// the stored keys must not be sent to a different server.
    #[test]
    fn a_provider_renamed_and_pointed_elsewhere_inherits_nothing() {
        let config = sample();

        let mut update = mask_config(&config);
        update.providers[1].name = "elsewhere".to_string();
        update.providers[1].base_url = "http://203.0.113.9:8000/v1".to_string();
        let issues = unmask_into(&mut update, &config).unwrap_err();
        let paths: Vec<&str> = issues.iter().map(|i| i.path.as_str()).collect();
        assert_eq!(paths, vec!["providers[1].api_keys[0]"]);
        assert!(issues.iter().all(|i| !i.message.contains(SHORT)));

        let mut update = mask_config(&config);
        update.providers[1].name = "elsewhere".to_string();
        update.providers[1].base_url = "http://203.0.113.9:8000/v1".to_string();
        update.providers[1].api_keys = vec![String::new()];
        unmask_into(&mut update, &config).unwrap();
        assert_eq!(update.providers[1].api_keys, vec![String::new()]);

        // Same endpoint text, another kind.
        let mut update = mask_config(&config);
        update.providers[0].name = "not-openai".to_string();
        update.providers[0].kind = ProviderKind::Anthropic;
        update.providers[0].api_keys = vec![String::new()];
        update.providers[0].credentials.clear();
        update.providers[0].headers.clear();
        update.providers[0].proxy.clear();
        unmask_into(&mut update, &config).unwrap();
        assert_eq!(update.providers[0].api_keys, vec![String::new()]);

        // The stored name is still in use by another incoming provider: the
        // one at its index is not a rename of it.
        let mut update = mask_config(&config);
        let mut copy = update.providers[1].clone();
        copy.name = "local-copy".to_string();
        copy.api_keys = vec![String::new()];
        update.providers.insert(1, copy);
        unmask_into(&mut update, &config).unwrap();
        assert_eq!(update.providers[1].api_keys, vec![String::new()]);
        assert_eq!(update.providers[2].api_keys[0], SHORT);
    }

    /// A credential header renamed to a name that is not masked: restoring
    /// the secret would publish it, keeping the mask would send bullets.
    #[test]
    fn a_masked_header_under_a_harmless_name_is_reported() {
        let config = sample();
        let mut update = mask_config(&config);
        let masked = update.providers[0]
            .headers
            .shift_remove("Authorization")
            .unwrap();
        update.providers[0]
            .headers
            .insert("X-Forwarded-For-Fun".to_string(), masked);
        let issues = unmask_into(&mut update, &config).unwrap_err();
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].path, "providers[0].headers.X-Forwarded-For-Fun");
        assert!(!issues[0].message.contains(HEADER));

        // An ordinary header whose text merely looks like a mask is not one.
        let mut update = mask_config(&config);
        update.providers[0]
            .headers
            .insert("X-Title".to_string(), "switch…yard".to_string());
        unmask_into(&mut update, &config).unwrap();
        assert_eq!(update.providers[0].headers["X-Title"], "switch…yard");
    }

    #[test]
    fn proxy_passwords_are_masked_and_restored() {
        let config = sample();
        let masked = mask_config(&config);
        assert_eq!(
            masked.providers[0].proxy,
            format!(
                "http://corp:{}@proxy.internal:3128",
                mask_secret(PROXY_PASSWORD)
            )
        );
        assert_eq!(
            masked.upstream.proxy,
            "socks5h://ops:•••••••@gate.internal:1080"
        );
        assert_eq!(
            masked.providers[0].credentials[1].proxy,
            "socks5://team:•••••••@10.0.0.1:1080/"
        );

        // Untouched: restored exactly.
        let mut update = masked.clone();
        unmask_into(&mut update, &config).unwrap();
        assert_eq!(update, config);

        // The host is edited, the password is not retyped.
        let mut update = masked.clone();
        update.providers[0].proxy = update.providers[0]
            .proxy
            .replace("proxy.internal:3128", "proxy2.internal:8080");
        // The credential moves to another position; its proxy follows it.
        update.providers[0].credentials.swap(0, 1);
        unmask_into(&mut update, &config).unwrap();
        assert_eq!(
            update.providers[0].proxy,
            format!("http://corp:{PROXY_PASSWORD}@proxy2.internal:8080")
        );
        assert_eq!(
            update.providers[0].credentials[0].proxy,
            format!("socks5://team:{PROXY_PASSWORD_2}@10.0.0.1:1080/")
        );
        assert_eq!(update.upstream.proxy, config.upstream.proxy);

        // A new password, a proxy without one, and special values pass through.
        let mut update = masked.clone();
        update.upstream.proxy = "http://ops:new-password@gate.internal:1080".to_string();
        update.providers[0].proxy = "direct".to_string();
        update.providers[1].proxy = "http://plain.internal:3128".to_string();
        unmask_into(&mut update, &config).unwrap();
        assert_eq!(
            update.upstream.proxy,
            "http://ops:new-password@gate.internal:1080"
        );
        assert_eq!(update.providers[0].proxy, "direct");
        assert_eq!(update.providers[1].proxy, "http://plain.internal:3128");

        // A masked password nobody stored is an error.
        let mut update = masked.clone();
        update.upstream.proxy = "http://someone:••••@gate.internal:1080".to_string();
        update.providers[1].proxy = "http://ops:abc…xyz@gate.internal:1080".to_string();
        let issues = unmask_into(&mut update, &config).unwrap_err();
        let paths: Vec<&str> = issues.iter().map(|i| i.path.as_str()).collect();
        assert_eq!(paths, vec!["upstream.proxy", "providers[1].proxy"]);
    }

    #[test]
    fn proxy_password_splitting() {
        assert_eq!(
            split_proxy_password("http://u:p@h:1"),
            Some(("http://u:", "p", "@h:1"))
        );
        assert_eq!(
            split_proxy_password("socks5://u:p:with:colons@h/path?x=a@b"),
            Some(("socks5://u:", "p:with:colons", "@h/path?x=a@b"))
        );
        // Without a password the user name is the credential (an API key
        // used as the user name).
        assert_eq!(
            split_proxy_password("http://token@h:3128"),
            Some(("http://", "token", "@h:3128"))
        );
        assert_eq!(
            split_proxy_password("http://token:@h:3128/x"),
            Some(("http://", "token", ":@h:3128/x"))
        );
        for no_credentials in [
            "",
            "direct",
            "http://h:3128",
            "http://@h:3128",
            "http://:@h:3128",
            "http://h/path:with@signs",
            "not a url",
        ] {
            assert_eq!(
                split_proxy_password(no_credentials),
                None,
                "{no_credentials}"
            );
            assert_eq!(mask_proxy(no_credentials), no_credentials);
        }
    }

    #[test]
    fn an_api_key_used_as_a_proxy_user_name_is_masked_and_restored() {
        let token = "zyte-api-key-0123456789abcdef";
        let mut config = Config::default();
        config.upstream.proxy = format!("http://{token}:@proxy.example:8011");
        let mut provider = ProviderConfig::new("p", ProviderKind::Mock);
        provider.proxy = format!("http://{token}@proxy.example:8011");
        config.providers = vec![provider];

        let masked = mask_config(&config);
        let json = serde_json::to_string(&masked).unwrap();
        assert!(!json.contains(token), "{json}");
        assert_eq!(
            masked.upstream.proxy,
            format!("http://{}:@proxy.example:8011", mask_secret(token))
        );

        let mut update = masked.clone();
        update.providers[0].proxy = update.providers[0].proxy.replace(":8011", ":8012");
        unmask_into(&mut update, &config).unwrap();
        assert_eq!(update.upstream.proxy, config.upstream.proxy);
        assert_eq!(
            update.providers[0].proxy,
            format!("http://{token}@proxy.example:8012")
        );
    }

    #[test]
    fn credential_header_names() {
        for name in [
            "Authorization",
            "x-api-key",
            "X-Goog-Api-Key",
            "Cookie",
            "X-Auth-Token",
            "Proxy-Authorization",
            "X-Secret",
        ] {
            assert!(is_credential_header(name), "{name}");
        }
        for name in ["X-Title", "HTTP-Referer", "User-Agent", "anthropic-beta"] {
            assert!(!is_credential_header(name), "{name}");
        }
    }
}
