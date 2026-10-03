//! Client authentication: the key table built from `[auth]`, the identity a
//! request runs under, and the per-key rate limit.

use crate::ticket::Holder;
use crate::types::PresentedCredentials;
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};
use subtle::ConstantTimeEq;
use switchyard_core::ApiError;
use switchyard_core::config::{Config, resolve_secret};
use switchyard_core::util::wildcard_match;

/// The window of the per-key rate limit.
const RATE_WINDOW: Duration = Duration::from_secs(60);

/// Scope name of requests made without a client key.
const ANONYMOUS_SCOPE: &str = "anonymous";

/// Name (and scope) of the admin playground's built-in client.
const DASHBOARD_NAME: &str = "dashboard";

/// A sliding one-minute window of request times for one client key.
pub(crate) struct RateWindow {
    hits: Mutex<VecDeque<Instant>>,
}

impl RateWindow {
    fn new() -> Self {
        RateWindow {
            hits: Mutex::new(VecDeque::new()),
        }
    }

    /// Admits a request at `now` if fewer than `limit` were admitted in the
    /// minute before it; otherwise says how long until the oldest of them
    /// leaves the window.
    pub(crate) fn try_acquire(&self, limit: u32, now: Instant) -> Result<(), Duration> {
        let mut hits = self.hits.lock();
        while hits
            .front()
            .is_some_and(|first| now.saturating_duration_since(*first) >= RATE_WINDOW)
        {
            hits.pop_front();
        }
        let limit = usize::try_from(limit).unwrap_or(usize::MAX);
        if hits.len() >= limit {
            let wait = match hits.front() {
                Some(first) => RATE_WINDOW.saturating_sub(now.saturating_duration_since(*first)),
                // A limit of zero admits nothing; there is no moment to
                // wait for, so advertise the whole window.
                None => RATE_WINDOW,
            };
            return Err(wait);
        }
        hits.push_back(now);
        Ok(())
    }
}

impl fmt::Debug for RateWindow {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RateWindow")
            .field("in_window", &self.hits.lock().len())
            .finish()
    }
}

/// Who a request is made by. Obtained from
/// [`crate::Gateway::authenticate`] or
/// [`crate::Gateway::dashboard_identity`]; carries the key's model
/// allow-list and rate limit, never the key itself.
#[derive(Clone, Debug)]
pub struct ClientIdentity {
    /// Stable, non-reversible id of the client key. `None` for anonymous
    /// and internal identities.
    pub key_id: Option<String>,
    /// The key's configured name.
    pub key_name: Option<String>,
    /// No key was matched and `auth.required` is off.
    pub anonymous: bool,
    /// A built-in client of the gateway itself (the admin playground).
    pub internal: bool,
    /// Wildcard patterns of models the key may use; empty means all.
    models: Arc<[String]>,
    /// Requests per minute; `None` means unlimited.
    rpm: Option<u32>,
    window: Option<Arc<RateWindow>>,
}

impl ClientIdentity {
    pub(crate) fn anonymous() -> Self {
        ClientIdentity {
            key_id: None,
            key_name: None,
            anonymous: true,
            internal: false,
            models: Arc::from(Vec::new()),
            rpm: None,
            window: None,
        }
    }

    pub(crate) fn dashboard() -> Self {
        ClientIdentity {
            key_id: None,
            key_name: Some(DASHBOARD_NAME.to_string()),
            anonymous: false,
            internal: true,
            models: Arc::from(Vec::new()),
            rpm: None,
            window: None,
        }
    }

    /// Whether this identity may use `model`, a client-facing model name
    /// without a reasoning suffix. Keys without an allow-list may use every
    /// model.
    pub fn allows_model(&self, model: &str) -> bool {
        self.models.is_empty()
            || self
                .models
                .iter()
                .any(|pattern| wildcard_match(pattern.trim(), model))
    }

    /// The key's requests-per-minute limit, if it has one.
    pub fn rate_limit_rpm(&self) -> Option<u32> {
        self.rpm
    }

    pub(crate) fn has_model_restrictions(&self) -> bool {
        !self.models.is_empty()
    }

    pub(crate) fn same_permissions(&self, other: &Self) -> bool {
        self.models == other.models && self.rpm == other.rpm
    }

    /// Counts one request against the key's rate limit.
    pub(crate) fn check_rate(&self, now: Instant) -> Result<(), ApiError> {
        let (Some(limit), Some(window)) = (self.rpm, &self.window) else {
            return Ok(());
        };
        window.try_acquire(limit, now).map_err(|wait| {
            ApiError::rate_limit(format!(
                "rate limit exceeded for this API key: {limit} requests per minute"
            ))
            .with_code("rate_limit_exceeded")
            .with_retry_after(ceil_secs(wait))
        })
    }

    /// Who a WebSocket ticket bought by this identity stands for. `None`
    /// for the gateway's built-in clients, which never need one.
    pub(crate) fn ticket_holder(&self) -> Option<Holder> {
        match (&self.key_id, self.internal) {
            (_, true) => None,
            (Some(id), false) => Some(Holder::Key(id.clone())),
            (None, false) => Some(Holder::Anonymous),
        }
    }

    /// The key under which per-client state (remembered reasoning, session
    /// bindings) is kept, so one client never sees another's.
    pub(crate) fn scope(&self) -> &str {
        match (&self.key_id, self.internal) {
            (Some(id), _) => id,
            (None, true) => DASHBOARD_NAME,
            (None, false) => ANONYMOUS_SCOPE,
        }
    }
}

/// `wait` rounded up to whole seconds, at least one.
fn ceil_secs(wait: Duration) -> Duration {
    Duration::from_secs(
        u64::try_from(wait.as_millis().div_ceil(1000))
            .unwrap_or(u64::MAX)
            .max(1),
    )
}

struct KeyEntry {
    /// SHA-256 of the resolved key. Comparing digests keeps the comparison
    /// constant-time whatever the lengths of the two keys are.
    digest: [u8; 32],
    id: String,
    name: Option<String>,
    models: Arc<[String]>,
    rpm: Option<u32>,
    window: Arc<RateWindow>,
}

/// The client keys of one configuration, ready for matching.
pub(crate) struct KeyTable {
    required: bool,
    entries: Vec<KeyEntry>,
}

impl fmt::Debug for KeyTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KeyTable")
            .field("required", &self.required)
            .field("keys", &self.entries.len())
            .finish()
    }
}

fn digest_of(key: &str) -> [u8; 32] {
    Sha256::digest(key.as_bytes()).into()
}

impl KeyTable {
    /// Builds the table for `config`. Disabled keys and keys whose secret
    /// reference cannot be resolved are left out. `key_id` derives a key's
    /// stable id from its resolved value. Rate-limit windows of keys that
    /// were already in `previous` are carried over, so a reload does not
    /// reset anyone's count.
    pub(crate) fn build(
        config: &Config,
        previous: Option<&KeyTable>,
        key_id: impl Fn(&str) -> String,
    ) -> KeyTable {
        let mut windows: HashMap<&str, &Arc<RateWindow>> = HashMap::new();
        if let Some(previous) = previous {
            for entry in &previous.entries {
                windows.insert(entry.id.as_str(), &entry.window);
            }
        }
        let mut entries = Vec::with_capacity(config.auth.keys.len());
        for (index, key) in config.auth.keys.iter().enumerate() {
            if !key.enabled {
                continue;
            }
            let resolved = match resolve_secret(&key.key) {
                Ok(value) if !value.is_empty() => value,
                Ok(_) => continue,
                Err(variable) => {
                    tracing::warn!(
                        key = index,
                        variable = %variable,
                        "client key refers to an environment variable that is not set; the key is ignored"
                    );
                    continue;
                }
            };
            let id = key_id(&resolved);
            let window = windows
                .get(id.as_str())
                .map(|window| Arc::clone(window))
                .unwrap_or_else(|| Arc::new(RateWindow::new()));
            let name = key.name.trim();
            entries.push(KeyEntry {
                digest: digest_of(&resolved),
                id,
                name: (!name.is_empty()).then(|| name.to_string()),
                models: key
                    .models
                    .iter()
                    .map(|pattern| pattern.trim().to_string())
                    .filter(|pattern| !pattern.is_empty())
                    .collect(),
                rpm: key.rate_limit_rpm,
                window,
            });
        }
        KeyTable {
            required: config.auth.required,
            entries,
        }
    }

    /// The entry whose key is `candidate`. Every entry is compared, so the
    /// time taken does not depend on where (or whether) a match is found.
    fn find(&self, candidate: &str) -> Option<&KeyEntry> {
        let digest = digest_of(candidate);
        let mut found = None;
        for entry in &self.entries {
            if bool::from(entry.digest.ct_eq(&digest)) {
                found = Some(entry);
            }
        }
        found
    }

    /// The identity of a matched key.
    fn identity(entry: &KeyEntry) -> ClientIdentity {
        ClientIdentity {
            key_id: Some(entry.id.clone()),
            key_name: entry.name.clone(),
            anonymous: false,
            internal: false,
            models: Arc::clone(&entry.models),
            rpm: entry.rpm,
            window: Some(Arc::clone(&entry.window)),
        }
    }

    /// The identity a WebSocket ticket stands for, under this table: the
    /// key that bought it, if that key is (still) enabled; the anonymous
    /// identity, if anonymous clients are (still) admitted.
    fn ticket_identity(&self, holder: &Holder) -> Option<ClientIdentity> {
        match holder {
            Holder::Key(id) => self
                .entries
                .iter()
                .find(|entry| entry.id == *id)
                .map(KeyTable::identity),
            Holder::Anonymous => (!self.required).then(ClientIdentity::anonymous),
        }
    }

    /// Re-checks a previously issued identity against the current key table.
    /// Long-lived clients must not retain removed keys or old restrictions.
    pub(crate) fn refresh(&self, identity: &ClientIdentity) -> Result<ClientIdentity, ApiError> {
        if identity.internal {
            return Ok(ClientIdentity::dashboard());
        }
        identity
            .ticket_holder()
            .and_then(|holder| self.ticket_identity(&holder))
            .ok_or_else(|| {
                ApiError::authentication("client access was revoked").with_code("invalid_api_key")
            })
    }

    /// Identifies the client without looking at tickets. Tests only.
    #[cfg(test)]
    pub(crate) fn authenticate(
        &self,
        presented: &PresentedCredentials,
    ) -> Result<ClientIdentity, ApiError> {
        self.authenticate_with(presented, |_| None)
    }

    /// Identifies the client. See [`crate::Gateway::authenticate`].
    /// `redeem` uses up a presented WebSocket ticket and says who bought
    /// it; it is called only when no key matched.
    pub(crate) fn authenticate_with(
        &self,
        presented: &PresentedCredentials,
        redeem: impl FnOnce(&str) -> Option<Holder>,
    ) -> Result<ClientIdentity, ApiError> {
        let candidates = [
            presented.authorization.as_deref().map(bearer_token),
            presented.x_api_key.as_deref().map(str::trim),
            presented.x_goog_api_key.as_deref().map(str::trim),
            presented.query_key.as_deref().map(str::trim),
        ];
        let mut any = false;
        for candidate in candidates.into_iter().flatten() {
            if candidate.is_empty() {
                continue;
            }
            any = true;
            if let Some(entry) = self.find(candidate) {
                return Ok(KeyTable::identity(entry));
            }
        }
        if let Some(ticket) = presented
            .ws_ticket
            .as_deref()
            .map(str::trim)
            .filter(|ticket| !ticket.is_empty())
        {
            any = true;
            if let Some(identity) = redeem(ticket).and_then(|holder| self.ticket_identity(&holder))
            {
                return Ok(identity);
            }
        }
        if !self.required {
            return Ok(ClientIdentity::anonymous());
        }
        Err(if any {
            ApiError::authentication("invalid API key").with_code("invalid_api_key")
        } else {
            ApiError::authentication("missing API key").with_code("missing_api_key")
        })
    }
}

/// The credential inside an `Authorization` header: the token of a `Bearer`
/// scheme (case-insensitive), otherwise the whole value.
fn bearer_token(header: &str) -> &str {
    let header = header.trim();
    match header.split_once(char::is_whitespace) {
        Some((scheme, token)) if scheme.eq_ignore_ascii_case("bearer") => token.trim(),
        _ => header,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ticket::Tickets;
    use switchyard_core::ErrorKind;
    use switchyard_core::config::ClientKey;

    fn key(value: &str, name: &str) -> ClientKey {
        ClientKey {
            key: value.to_string(),
            name: name.to_string(),
            enabled: true,
            models: Vec::new(),
            rate_limit_rpm: None,
        }
    }

    fn table(keys: Vec<ClientKey>, required: bool) -> KeyTable {
        let mut config = Config::default();
        config.auth.required = required;
        config.auth.keys = keys;
        KeyTable::build(&config, None, |k| format!("id-{}", k.len()))
    }

    fn presented(
        authorization: Option<&str>,
        x_api_key: Option<&str>,
        x_goog: Option<&str>,
        query: Option<&str>,
    ) -> PresentedCredentials {
        PresentedCredentials {
            authorization: authorization.map(str::to_string),
            x_api_key: x_api_key.map(str::to_string),
            x_goog_api_key: x_goog.map(str::to_string),
            query_key: query.map(str::to_string),
            ws_ticket: None,
        }
    }

    fn ticket(value: &str) -> PresentedCredentials {
        PresentedCredentials {
            ws_ticket: Some(value.to_string()),
            ..PresentedCredentials::default()
        }
    }

    #[test]
    fn a_ticket_stands_for_the_key_that_bought_it() {
        let mut limited = key("k", "limited");
        limited.models = vec!["gpt-*".into()];
        limited.rate_limit_rpm = Some(1);
        let t = table(vec![limited, key("other-key", "other")], true);
        let bought = t
            .authenticate(&presented(None, Some("k"), None, None))
            .unwrap();
        let holder = bought.ticket_holder().unwrap();

        let tickets = Tickets::default();
        let now = Instant::now();
        let issued = tickets.issue(holder, now);
        let redeem = |value: &str| tickets.redeem(value, now);
        let identity = t.authenticate_with(&ticket(&issued), redeem).unwrap();
        assert_eq!(identity.key_name.as_deref(), Some("limited"));
        assert_eq!(identity.key_id, bought.key_id);
        // The key's allow-list and its rate-limit window come along.
        assert!(identity.allows_model("gpt-5"));
        assert!(!identity.allows_model("claude-opus-4-5"));
        bought.check_rate(now).unwrap();
        assert!(identity.check_rate(now).is_err(), "one window per key");

        // Used up: the second use is a wrong key.
        let again = t
            .authenticate_with(&ticket(&issued), |value| tickets.redeem(value, now))
            .unwrap_err();
        assert_eq!(again.status, 401);
        assert_eq!(again.message, "invalid API key");
        assert_eq!(again.code.as_deref(), Some("invalid_api_key"));
    }

    #[test]
    fn a_ticket_is_looked_at_only_when_no_key_matched() {
        let t = table(vec![key("k", "named")], true);
        let mut both = presented(None, Some("k"), None, None);
        both.ws_ticket = Some("some-ticket".into());
        let identity = t
            .authenticate_with(&both, |_| panic!("a matching key does not use the ticket"))
            .unwrap();
        assert_eq!(identity.key_name.as_deref(), Some("named"));
        // Blank tickets are not presented at all.
        let err = t
            .authenticate_with(&ticket("  "), |_| panic!("not looked at"))
            .unwrap_err();
        assert_eq!(err.message, "missing API key");
    }

    #[test]
    fn a_ticket_dies_with_its_key_or_with_anonymous_access() {
        let mut config = Config::default();
        config.auth.keys = vec![key("k", "named")];
        let before = KeyTable::build(&config, None, |k| format!("id-{k}"));
        let holder = before
            .authenticate(&presented(None, Some("k"), None, None))
            .unwrap()
            .ticket_holder()
            .unwrap();
        // The key is disabled before the ticket is used.
        config.auth.keys[0].enabled = false;
        let after = KeyTable::build(&config, Some(&before), |k| format!("id-{k}"));
        let err = after
            .authenticate_with(&ticket("t"), |_| Some(holder.clone()))
            .unwrap_err();
        assert_eq!(err.message, "invalid API key");

        // Bought anonymously; then keys became required.
        let open = table(vec![], false);
        let anonymous = open
            .authenticate(&PresentedCredentials::default())
            .unwrap()
            .ticket_holder()
            .unwrap();
        assert_eq!(anonymous, Holder::Anonymous);
        assert!(
            open.authenticate_with(&ticket("t"), |_| Some(Holder::Anonymous))
                .unwrap()
                .anonymous
        );
        let closed = table(vec![key("k", "named")], true);
        let err = closed
            .authenticate_with(&ticket("t"), |_| Some(Holder::Anonymous))
            .unwrap_err();
        assert_eq!(err.status, 401);

        // The playground's built-in client never holds tickets.
        assert_eq!(ClientIdentity::dashboard().ticket_holder(), None);
    }

    #[test]
    fn bearer_parsing() {
        assert_eq!(bearer_token("Bearer abc"), "abc");
        assert_eq!(bearer_token("bearer   abc  "), "abc");
        assert_eq!(bearer_token("BEARER\tabc"), "abc");
        assert_eq!(bearer_token("sk-raw"), "sk-raw");
        assert_eq!(bearer_token("Basic xyz"), "Basic xyz");
        assert_eq!(bearer_token("  sk-raw  "), "sk-raw");
    }

    #[test]
    fn every_location_authenticates() {
        let t = table(vec![key("sy-secret-key", "laptop")], true);
        for p in [
            presented(Some("Bearer sy-secret-key"), None, None, None),
            presented(Some("sy-secret-key"), None, None, None),
            presented(None, Some("sy-secret-key"), None, None),
            presented(None, None, Some("sy-secret-key"), None),
            presented(None, None, None, Some("sy-secret-key")),
        ] {
            let identity = t.authenticate(&p).unwrap();
            assert_eq!(identity.key_name.as_deref(), Some("laptop"));
            assert_eq!(identity.key_id.as_deref(), Some("id-13"));
            assert!(!identity.anonymous && !identity.internal);
            assert_eq!(identity.scope(), "id-13");
        }
    }

    #[test]
    fn the_first_matching_candidate_wins_not_the_first_present() {
        let t = table(vec![key("right-key", "a"), key("other-key", "b")], true);
        let identity = t
            .authenticate(&presented(
                Some("Bearer wrong"),
                Some("right-key"),
                None,
                Some("other-key"),
            ))
            .unwrap();
        assert_eq!(identity.key_name.as_deref(), Some("a"));
        // Order of candidates, not of configured keys.
        let identity = t
            .authenticate(&presented(
                Some("Bearer other-key"),
                Some("right-key"),
                None,
                None,
            ))
            .unwrap();
        assert_eq!(identity.key_name.as_deref(), Some("b"));
    }

    #[test]
    fn missing_and_invalid_are_told_apart() {
        let t = table(vec![key("right-key", "")], true);
        let missing = t
            .authenticate(&PresentedCredentials::default())
            .unwrap_err();
        assert_eq!(missing.status, 401);
        assert_eq!(missing.kind, ErrorKind::Authentication);
        assert_eq!(missing.message, "missing API key");
        let blank = t
            .authenticate(&presented(Some("  "), Some(""), None, None))
            .unwrap_err();
        assert_eq!(blank.message, "missing API key");
        let invalid = t
            .authenticate(&presented(Some("Bearer nope"), None, None, None))
            .unwrap_err();
        assert_eq!(invalid.status, 401);
        assert_eq!(invalid.message, "invalid API key");
        // A prefix or a longer string is not the key.
        for almost in ["right-ke", "right-key2", "Right-Key"] {
            assert!(
                t.authenticate(&presented(None, Some(almost), None, None))
                    .is_err()
            );
        }
    }

    #[test]
    fn disabled_keys_do_not_authenticate() {
        let mut disabled = key("off-key", "off");
        disabled.enabled = false;
        let t = table(vec![disabled, key("on-key", "on")], true);
        let err = t
            .authenticate(&presented(None, Some("off-key"), None, None))
            .unwrap_err();
        assert_eq!(err.message, "invalid API key");
        assert!(
            t.authenticate(&presented(None, Some("on-key"), None, None))
                .is_ok()
        );
    }

    #[test]
    fn optional_auth_admits_anonymous_clients() {
        let t = table(vec![key("right-key", "named")], false);
        let anonymous = t.authenticate(&PresentedCredentials::default()).unwrap();
        assert!(anonymous.anonymous);
        assert_eq!(anonymous.key_id, None);
        assert_eq!(anonymous.scope(), "anonymous");
        // A wrong key is anonymous too, a right one is still recognised.
        assert!(
            t.authenticate(&presented(None, Some("wrong"), None, None))
                .unwrap()
                .anonymous
        );
        let named = t
            .authenticate(&presented(None, Some("right-key"), None, None))
            .unwrap();
        assert!(!named.anonymous);
        assert_eq!(named.key_name.as_deref(), Some("named"));
    }

    #[test]
    fn unresolvable_secret_references_are_skipped() {
        let t = table(
            vec![
                key("env:SWITCHYARD_GATEWAY_TEST_UNSET_KEY", "ghost"),
                key("real-key", "real"),
            ],
            true,
        );
        assert_eq!(t.entries.len(), 1);
        // The reference text itself is not a key.
        assert!(
            t.authenticate(&presented(
                None,
                Some("env:SWITCHYARD_GATEWAY_TEST_UNSET_KEY"),
                None,
                None
            ))
            .is_err()
        );
    }

    #[test]
    fn allow_lists_use_wildcards() {
        let mut limited = key("k", "limited");
        limited.models = vec!["gpt-*".into(), " claude-sonnet-4-5 ".into()];
        let t = table(vec![limited], true);
        let identity = t
            .authenticate(&presented(None, Some("k"), None, None))
            .unwrap();
        assert!(identity.allows_model("gpt-5"));
        assert!(identity.allows_model("GPT-5-mini"));
        assert!(identity.allows_model("claude-sonnet-4-5"));
        assert!(!identity.allows_model("claude-opus-4-5"));
        assert!(ClientIdentity::anonymous().allows_model("anything"));
        assert!(ClientIdentity::dashboard().allows_model("anything"));
    }

    #[test]
    fn dashboard_identity_is_internal_and_unlimited() {
        let d = ClientIdentity::dashboard();
        assert!(d.internal && !d.anonymous);
        assert_eq!(d.key_name.as_deref(), Some("dashboard"));
        assert_eq!(d.rate_limit_rpm(), None);
        assert_eq!(d.scope(), "dashboard");
        for _ in 0..1000 {
            d.check_rate(Instant::now()).unwrap();
        }
    }

    #[test]
    fn sliding_window_admits_limit_requests_per_minute() {
        let window = RateWindow::new();
        let t0 = Instant::now();
        assert_eq!(window.try_acquire(2, t0), Ok(()));
        assert_eq!(window.try_acquire(2, t0 + Duration::from_secs(10)), Ok(()));
        // Full: the oldest hit leaves the window in 40 s.
        assert_eq!(
            window.try_acquire(2, t0 + Duration::from_secs(20)),
            Err(Duration::from_secs(40))
        );
        // A refused request does not count.
        assert_eq!(
            window.try_acquire(2, t0 + Duration::from_secs(59)),
            Err(Duration::from_secs(1))
        );
        assert_eq!(window.try_acquire(2, t0 + Duration::from_secs(60)), Ok(()));
        assert_eq!(
            window.try_acquire(2, t0 + Duration::from_secs(61)),
            Err(Duration::from_secs(9))
        );
        // A limit of zero admits nothing.
        assert_eq!(
            RateWindow::new().try_acquire(0, t0),
            Err(Duration::from_secs(60))
        );
    }

    #[test]
    fn rate_limit_errors_carry_retry_after() {
        let mut limited = key("k", "limited");
        limited.rate_limit_rpm = Some(1);
        let t = table(vec![limited], true);
        let identity = t
            .authenticate(&presented(None, Some("k"), None, None))
            .unwrap();
        let now = Instant::now();
        identity.check_rate(now).unwrap();
        let err = identity
            .check_rate(now + Duration::from_millis(1500))
            .unwrap_err();
        assert_eq!(err.status, 429);
        assert_eq!(err.kind, ErrorKind::RateLimit);
        assert_eq!(err.retry_after_secs, Some(59));
        // The window belongs to the key, not to the identity value.
        let again = t
            .authenticate(&presented(None, Some("k"), None, None))
            .unwrap();
        assert!(again.check_rate(now + Duration::from_secs(2)).is_err());
    }

    #[test]
    fn rebuilding_keeps_the_windows_of_unchanged_keys() {
        let mut limited = key("k", "limited");
        limited.rate_limit_rpm = Some(1);
        let mut config = Config::default();
        config.auth.keys = vec![limited.clone()];
        let first = KeyTable::build(&config, None, |k| format!("id-{k}"));
        let now = Instant::now();
        first
            .authenticate(&presented(None, Some("k"), None, None))
            .unwrap()
            .check_rate(now)
            .unwrap();

        // Renamed, limit unchanged: the count survives.
        limited.name = "renamed".into();
        config.auth.keys = vec![limited, key("new", "new")];
        let second = KeyTable::build(&config, Some(&first), |k| format!("id-{k}"));
        let identity = second
            .authenticate(&presented(None, Some("k"), None, None))
            .unwrap();
        assert_eq!(identity.key_name.as_deref(), Some("renamed"));
        assert!(identity.check_rate(now + Duration::from_secs(1)).is_err());
        assert!(
            second
                .authenticate(&presented(None, Some("new"), None, None))
                .is_ok()
        );
    }

    #[test]
    fn debug_output_never_shows_keys() {
        let t = table(vec![key("super-secret-key-value", "n")], true);
        assert!(!format!("{t:?}").contains("super-secret"));
        let p = presented(Some("Bearer super-secret-key-value"), None, None, None);
        assert!(!format!("{p:?}").contains("super-secret"));
        let identity = t.authenticate(&p).unwrap();
        assert!(!format!("{identity:?}").contains("super-secret"));
    }

    #[test]
    fn issued_identities_take_current_permissions_and_do_not_survive_revocation() {
        let mut config = Config::default();
        config.auth.keys = vec![key("k", "named")];
        let before = KeyTable::build(&config, None, str::to_owned);
        let issued = before
            .authenticate(&presented(None, Some("k"), None, None))
            .unwrap();
        config.auth.keys[0].models = vec!["safe".into()];
        config.auth.keys[0].rate_limit_rpm = Some(1);
        let after = KeyTable::build(&config, Some(&before), str::to_owned);
        let current = after.refresh(&issued).unwrap();
        assert!(current.allows_model("safe"));
        assert!(!current.allows_model("other"));
        assert_eq!(current.rate_limit_rpm(), Some(1));
        assert!(!issued.same_permissions(&current));
        current.check_rate(Instant::now()).unwrap();
        assert!(
            after
                .refresh(&issued)
                .unwrap()
                .check_rate(Instant::now())
                .is_err()
        );

        config.auth.keys[0].enabled = false;
        let disabled = KeyTable::build(&config, Some(&after), str::to_owned);
        assert_eq!(disabled.refresh(&issued).unwrap_err().status, 401);
        config.auth.keys.clear();
        let removed = KeyTable::build(&config, Some(&disabled), str::to_owned);
        assert_eq!(removed.refresh(&issued).unwrap_err().status, 401);
        assert_eq!(
            removed
                .refresh(&ClientIdentity::anonymous())
                .unwrap_err()
                .status,
            401
        );
        assert!(
            removed
                .refresh(&ClientIdentity::dashboard())
                .unwrap()
                .internal
        );
    }
}
