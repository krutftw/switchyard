//! Mutable runtime state: per-credential health and counters, rotation
//! cursors and session bindings. Kept across registry rebuilds for
//! credentials whose id is unchanged.

use crate::clock::Ms;
use crate::types::{CredentialId, LastError};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use switchyard_core::config::CooldownConfig;
use switchyard_core::util::{mask_secret, truncate_chars};
use switchyard_core::{FailureClass, UpstreamError};

/// Weight of a new latency sample in the moving average.
const LATENCY_ALPHA: f64 = 0.3;
/// Longest upstream-requested wait that is honoured. Guards against absurd
/// `Retry-After` values (and arithmetic overflow) while leaving room for
/// genuine weekly quota resets.
const MAX_RETRY_HINT_MS: u64 = 7 * 24 * 3600 * 1000;
/// Characters of an upstream message kept in `last_error`.
const ERROR_MESSAGE_CHARS: usize = 200;
/// Rotation cursors kept before the table is cleared.
const MAX_ROTATION_KEYS: usize = 4096;
/// Session keys longer than this are replaced by their hash.
const MAX_SESSION_KEY_BYTES: usize = 256;
/// Default cap on remembered session bindings.
pub(crate) const AFFINITY_CAPACITY: usize = 16_384;
/// How long a failed attempt on a model is remembered once its cooldown (if
/// any) has run out. It only has to outlast one client request: the memory
/// tells `pick` which alias targets a credential was already attempted on.
pub(crate) const FAILURE_MEMORY_MS: Ms = 3_600_000;
/// Under `least-latency`, how long a credential that just failed on a model
/// ranks behind the others when the failure started no (or a shorter)
/// cooldown. Matches the default transient cooldown.
pub(crate) const PROBATION_MS: Ms = 60_000;

/// A rest period and what caused it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Cooldown {
    pub until: Ms,
    pub reason: FailureClass,
}

impl Cooldown {
    fn active(self, now: Ms) -> bool {
        self.until > now
    }
}

/// A failed attempt: when it happened and its position among all failures
/// of the credential (strictly increasing, so two failures inside the same
/// millisecond still have an order).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FailureMark {
    pub seq: u64,
    pub at: Ms,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct ModelState {
    pub cooldown: Option<Cooldown>,
    /// Consecutive rate-limit windows without a success in between; drives
    /// the exponential backoff.
    pub rate_limit_streak: u32,
    /// The most recent failed attempt that no success has followed. Recorded
    /// whether or not the failure started a cooldown.
    pub last_failure: Option<FailureMark>,
    /// One-line description of that failure (`"429 slow down"`), scrubbed.
    pub last_failure_summary: String,
}

/// Health and counters of one credential.
#[derive(Clone, Debug, Default)]
pub(crate) struct CredState {
    pub runtime_disabled: bool,
    /// Why the credential cannot be used, as found out at runtime by the
    /// gateway (see `Scheduler::set_unusable`): something the configuration
    /// alone cannot tell, such as a service-account file that is missing.
    pub runtime_unusable: Option<String>,
    /// Rest period covering every model.
    pub cooldown: Option<Cooldown>,
    /// The failure behind `cooldown`: when it happened and its one-line
    /// description. `last_error` cannot serve here, a later failure of
    /// another kind replaces it.
    pub cooldown_cause: Option<(Ms, String)>,
    /// Per upstream model id.
    pub models: HashMap<String, ModelState>,
    pub requests: u64,
    pub successes: u64,
    pub failures: u64,
    pub consecutive_failures: u32,
    pub latency_ms: Option<f64>,
    pub last_used: Option<Ms>,
    pub last_error: Option<LastError>,
    /// Number of failures recorded so far; the source of [`FailureMark::seq`].
    pub failure_seq: u64,
    /// Value of `failure_seq` when the credential was last handed to a
    /// request that had not tried it yet. Failures numbered above it
    /// happened since that request began using the credential.
    pub request_floor: u64,
}

impl CredState {
    /// The credential-wide cooldown, if still running.
    pub fn credential_cooldown(&self, now: Ms) -> Option<Cooldown> {
        self.cooldown.filter(|c| c.active(now))
    }

    /// The cooldown of one model, if still running.
    pub fn model_cooldown(&self, model: &str, now: Ms) -> Option<Cooldown> {
        self.models
            .get(model)
            .and_then(|m| m.cooldown)
            .filter(|c| c.active(now))
    }

    /// Whether the credential may not serve `model` right now. When both the
    /// credential and the model are resting, the later deadline is returned:
    /// that is when the pair becomes usable again.
    pub fn blocked(&self, model: &str, now: Ms) -> Option<Cooldown> {
        match (
            self.credential_cooldown(now),
            self.model_cooldown(model, now),
        ) {
            (Some(a), Some(b)) => Some(if b.until > a.until { b } else { a }),
            (a, b) => a.or(b),
        }
    }

    /// The failure that explains why `model` rests on the credential right
    /// now — time and one-line description — following the same rule as
    /// [`CredState::blocked`]: the rest that ends last is the one that
    /// counts. `None` when nothing rests.
    pub fn rest_cause(&self, model: &str, now: Ms) -> Option<(Ms, &str)> {
        let whole = self.credential_cooldown(now);
        let own = self.model_cooldown(model, now);
        let model_is_the_cause = match (whole, own) {
            (Some(whole), Some(own)) => own.until > whole.until,
            (None, Some(_)) => true,
            (Some(_), None) => false,
            (None, None) => return None,
        };
        if model_is_the_cause {
            let state = self.models.get(model)?;
            let mark = state.last_failure?;
            Some((mark.at, state.last_failure_summary.as_str()))
        } else {
            self.cooldown_cause
                .as_ref()
                .map(|(at, summary)| (*at, summary.as_str()))
        }
    }

    /// The last failed attempt on `model` that no success has followed, while
    /// it is remembered: as long as the model rests, and for
    /// [`FAILURE_MEMORY_MS`] after the failure otherwise.
    pub fn recent_failure(&self, model: &str, now: Ms) -> Option<FailureMark> {
        let state = self.models.get(model)?;
        let mark = state.last_failure?;
        let remembered = state.cooldown.is_some_and(|c| c.active(now))
            || now.saturating_sub(mark.at) < FAILURE_MEMORY_MS;
        remembered.then_some(mark)
    }

    /// Whether the last attempt on `model` failed less than [`PROBATION_MS`]
    /// ago (and no success followed).
    pub fn on_probation(&self, model: &str, now: Ms) -> bool {
        self.recent_failure(model, now)
            .is_some_and(|mark| now.saturating_sub(mark.at) < PROBATION_MS)
    }

    /// Whether the credential's last failure on `earlier` is remembered and
    /// came after its last failure on `later` (or `later` has none).
    pub fn failed_more_recently(&self, earlier: &str, later: &str, now: Ms) -> bool {
        let Some(mark) = self.recent_failure(earlier, now) else {
            return false;
        };
        self.recent_failure(later, now)
            .is_none_or(|other| mark.seq > other.seq)
    }

    /// Notes that the credential is being handed to a request that has not
    /// tried it before: whatever failed up to now is not that request's.
    pub fn begin_request(&mut self) {
        self.request_floor = self.failure_seq;
    }

    /// Whether `model` failed on the credential since it was last handed to
    /// a request that had not tried it (see [`CredState::begin_request`]).
    pub fn failed_since_request_began(&self, model: &str, now: Ms) -> bool {
        self.recent_failure(model, now)
            .is_some_and(|mark| mark.seq > self.request_floor)
    }

    /// Clears every cooldown and the failure streaks. Counters stay.
    pub fn clear_cooldowns(&mut self) {
        self.cooldown = None;
        self.cooldown_cause = None;
        self.models.clear();
        self.consecutive_failures = 0;
    }

    /// Drops bookkeeping for cooldowns that have run out.
    pub fn prune(&mut self, now: Ms) {
        if self.cooldown.is_none_or(|c| !c.active(now)) {
            self.cooldown = None;
            self.cooldown_cause = None;
        }
        self.models.retain(|_, m| {
            if m.cooldown.is_some_and(|c| !c.active(now)) {
                m.cooldown = None;
            }
            if m.cooldown.is_none()
                && m.last_failure
                    .is_some_and(|f| now.saturating_sub(f.at) >= FAILURE_MEMORY_MS)
            {
                m.last_failure = None;
                m.last_failure_summary = String::new();
            }
            m.cooldown.is_some() || m.rate_limit_streak > 0 || m.last_failure.is_some()
        });
    }

    pub fn record_success(&mut self, model: &str, latency_ms: u64, now: Ms) {
        self.requests += 1;
        self.successes += 1;
        self.consecutive_failures = 0;
        self.last_used = Some(now);
        // A credential-wide rest (bad key, exhausted quota) is not lifted by
        // a straggler that was already in flight; the model's own state is.
        self.models.remove(model);
        let sample = latency_ms as f64;
        self.latency_ms = Some(match self.latency_ms {
            Some(average) => LATENCY_ALPHA * sample + (1.0 - LATENCY_ALPHA) * average,
            None => sample,
        });
    }

    /// Records a failed attempt and, when `config.enabled`, starts the
    /// cooldown its class calls for. `secret` is scrubbed from the stored
    /// message.
    pub fn record_failure(
        &mut self,
        model: &str,
        error: &UpstreamError,
        config: &CooldownConfig,
        secret: &str,
        now: Ms,
    ) {
        self.requests += 1;
        self.last_used = Some(now);
        if error.class == FailureClass::Request {
            // The request was at fault, not the credential.
            return;
        }
        self.failures += 1;
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        let last_error = LastError {
            status: error.status,
            class: error.class,
            message: scrub(&error.info.message, secret),
            at: now,
            model: model.to_string(),
        };
        let summary = last_error.summary();
        self.last_error = Some(last_error);
        self.failure_seq = self.failure_seq.saturating_add(1);
        let state = self.models.entry(model.to_string()).or_default();
        state.last_failure = Some(FailureMark {
            seq: self.failure_seq,
            at: now,
        });
        state.last_failure_summary = summary.clone();
        if !config.enabled {
            return;
        }

        // A hint of zero carries no information. Only the upstream's wish is
        // capped; the operator's own durations are used as written.
        let hint = error
            .retry_after_ms
            .filter(|ms| *ms > 0)
            .map(|ms| ms.min(MAX_RETRY_HINT_MS));
        let secs = |s: u64| s.saturating_mul(1000);
        match error.class {
            FailureClass::Request => {}
            FailureClass::RateLimit => {
                let state = self.models.entry(model.to_string()).or_default();
                // Requests that were already in flight when the first 429
                // arrived fail inside the same window; they must not each
                // double the backoff.
                let window_open = state
                    .cooldown
                    .is_some_and(|c| c.active(now) && c.reason == FailureClass::RateLimit);
                if !window_open {
                    state.rate_limit_streak = state.rate_limit_streak.saturating_add(1);
                }
                let wait = hint.unwrap_or_else(|| {
                    rate_limit_backoff_ms(
                        config.rate_limit_base_secs,
                        config.rate_limit_max_secs,
                        state.rate_limit_streak,
                    )
                });
                extend(&mut state.cooldown, wait, FailureClass::RateLimit, now);
            }
            // The two credential-wide classes rest the credential for at
            // least the configured time: a short `Retry-After` on a rejected
            // key or an exhausted account would only make the gateway hammer
            // a credential that cannot work. A longer upstream wait (a quota
            // reset time) is honoured.
            FailureClass::Quota => {
                let wait = secs(config.quota_secs).max(hint.unwrap_or(0));
                if extend(&mut self.cooldown, wait, FailureClass::Quota, now) {
                    self.cooldown_cause = Some((now, summary));
                }
            }
            FailureClass::Auth => {
                let wait = secs(config.auth_secs).max(hint.unwrap_or(0));
                if extend(&mut self.cooldown, wait, FailureClass::Auth, now) {
                    self.cooldown_cause = Some((now, summary));
                }
            }
            // Per-model classes: the upstream's own wait wins outright.
            FailureClass::ModelNotFound => {
                let wait = hint.unwrap_or_else(|| secs(config.model_not_found_secs));
                let state = self.models.entry(model.to_string()).or_default();
                extend(&mut state.cooldown, wait, FailureClass::ModelNotFound, now);
            }
            FailureClass::Server | FailureClass::Transport => {
                let wait = hint.unwrap_or_else(|| secs(config.transient_secs));
                let state = self.models.entry(model.to_string()).or_default();
                extend(&mut state.cooldown, wait, error.class, now);
            }
        }
    }
}

/// Starts or extends a cooldown. A running cooldown is never shortened.
/// Returns whether the slot was written, i.e. whether this failure is now
/// the one that determines the rest.
fn extend(slot: &mut Option<Cooldown>, wait_ms: u64, reason: FailureClass, now: Ms) -> bool {
    if wait_ms == 0 {
        return false;
    }
    let until = now.saturating_add(Ms::try_from(wait_ms).unwrap_or(Ms::MAX));
    match slot {
        Some(current) if current.active(now) && current.until >= until => false,
        _ => {
            *slot = Some(Cooldown { until, reason });
            true
        }
    }
}

/// `base * 2^(streak-1)` seconds, capped at `max`, in milliseconds.
pub(crate) fn rate_limit_backoff_ms(base_secs: u64, max_secs: u64, streak: u32) -> u64 {
    let doublings = streak.saturating_sub(1).min(63);
    let wait = base_secs
        .checked_shl(doublings)
        .filter(|w| w >> doublings == base_secs)
        .unwrap_or(u64::MAX)
        .min(max_secs);
    wait.saturating_mul(1000)
}

/// Shortens an upstream message for display (it is shown in the dashboard
/// and quoted to clients in "cooling down" errors) and removes key material:
/// the credential's own key should the upstream have echoed it, and anything
/// else shaped like a vendor key or token.
pub(crate) fn scrub(message: &str, secret: &str) -> String {
    let without_own = redact_own_secret(message, secret);
    let one_line = without_own.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_chars(&mask_key_like(&one_line), ERROR_MESSAGE_CHARS)
}

/// A character a key or token can be made of.
fn is_key_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '-' | '_')
}

/// Replaces every occurrence of the credential's own secret. A long secret
/// is removed wherever it appears. A short one (self-hosted servers use keys
/// like `sk-1234`) could be part of an ordinary word, so it is removed only
/// where it stands as a token of its own. Placeholders of up to three
/// characters (`x`, `key`) protect nothing and are left alone, so they do
/// not blank out ordinary words of the message.
fn redact_own_secret(message: &str, secret: &str) -> String {
    const REDACTED: &str = "[redacted]";
    const ANYWHERE_MIN_BYTES: usize = 8;
    const TOKEN_MIN_BYTES: usize = 4;
    let secret = secret.trim();
    if secret.len() < TOKEN_MIN_BYTES {
        return message.to_string();
    }
    if secret.len() >= ANYWHERE_MIN_BYTES {
        return message.replace(secret, REDACTED);
    }
    let mut out = String::with_capacity(message.len());
    let mut rest = message;
    let mut before: Option<char> = None;
    while let Some(found) = rest.find(secret) {
        let (head, tail) = rest.split_at(found);
        let after = &tail[secret.len()..];
        let starts_token = head
            .chars()
            .next_back()
            .or(before)
            .is_none_or(|c| !is_key_char(c));
        let ends_token = after.chars().next().is_none_or(|c| !is_key_char(c));
        out.push_str(head);
        if starts_token && ends_token {
            out.push_str(REDACTED);
            before = secret.chars().next_back();
            rest = after;
        } else {
            // Not a token of its own: keep one character and look again, so
            // overlapping occurrences are still examined.
            let step = tail.chars().next().map_or(tail.len(), char::len_utf8);
            out.push_str(&tail[..step]);
            before = tail[..step].chars().next_back();
            rest = &tail[step..];
        }
    }
    out.push_str(rest);
    out
}

/// Masks everything in `text` that looks like an API key or access token: a
/// run of key characters that starts with a well-known vendor prefix at a
/// word boundary and is long enough to be a real key, or whatever follows
/// `Bearer`. Every such run is masked, however many share one word (compact
/// JSON has no spaces). Keys the vendor already masked (`sk-…****abcd`) and
/// short look-alikes (`sk-invalid`) are left as they are.
fn mask_key_like(text: &str) -> String {
    /// OpenAI / Anthropic / OpenRouter / DeepSeek (`sk-`), Google API keys
    /// (`AIza`), Google access tokens (`ya29.`), then prefixes of vendors
    /// commonly reached as `openai-compat`: ElevenLabs, Groq, Hugging Face,
    /// Fireworks, Replicate. Prefixes that double as model-name prefixes
    /// (`xai-`, `pplx-`) are left out: masking a model name would make the
    /// message useless, and those vendors mask keys in their own errors.
    const PREFIXES: [&str; 8] = ["sk-", "sk_", "AIza", "ya29.", "gsk_", "hf_", "fw_", "r8_"];
    const MIN_KEY_CHARS: usize = 20;
    const BEARER: &str = "bearer ";

    // Length of the key-shaped run at the start of `rest`. Dots are part of
    // a token (`ya29.…`, JWTs) but a sentence-ending one is punctuation.
    let run_len = |rest: &str| -> usize {
        let end = rest
            .find(|c: char| !(is_key_char(c) || c == '.'))
            .unwrap_or(rest.len());
        rest[..end].trim_end_matches('.').len()
    };

    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    // The character before `index`; a key only starts at a word boundary
    // ("task-oriented" is not a key). ASCII only: in text without spaces
    // (CJK) a key follows a letter of the sentence directly.
    let mut previous: Option<char> = None;
    while index < text.len() {
        let rest = &text[index..];
        let at_boundary = previous.is_none_or(|c| !c.is_ascii_alphanumeric());
        if at_boundary {
            let bearer = rest
                .get(..BEARER.len())
                .is_some_and(|head| head.eq_ignore_ascii_case(BEARER));
            // After `Bearer ` anything of key length is a credential.
            let (lead, key_len) = if bearer {
                (BEARER.len(), run_len(&rest[BEARER.len()..]))
            } else if PREFIXES.iter().any(|p| rest.starts_with(p)) {
                (0, run_len(rest))
            } else {
                (0, 0)
            };
            if key_len >= MIN_KEY_CHARS {
                let key = &rest[lead..lead + key_len];
                out.push_str(&rest[..lead]);
                out.push_str(&mask_secret(key));
                previous = key.chars().next_back();
                index += lead + key_len;
                continue;
            }
        }
        let Some(c) = rest.chars().next() else {
            break;
        };
        out.push(c);
        previous = Some(c);
        index += c.len_utf8();
    }
    out
}

// ---------------------------------------------------------------------------
// Rotation
// ---------------------------------------------------------------------------

/// Rotation scope: one client-facing model (lower-cased) at one priority.
pub(crate) type RotationKey = (String, i32);

/// Cursors of the rotating strategies.
#[derive(Debug, Default)]
pub(crate) struct Rotation {
    /// Round-robin: the credential picked last. Rotation is by identity, not
    /// by index, so it stays fair while the candidate list shrinks and grows
    /// with cooldowns.
    last: HashMap<RotationKey, CredentialId>,
    /// Smooth weighted round-robin accumulators.
    weighted: HashMap<RotationKey, HashMap<CredentialId, i64>>,
}

impl Rotation {
    /// Picks the candidate after the one picked last. `candidates` are
    /// `(config order, id)` pairs sorted by order; `order_of` maps an id to
    /// its config order. Returns an index into `candidates`.
    pub fn round_robin(
        &mut self,
        key: RotationKey,
        candidates: &[(usize, &str)],
        order_of: impl Fn(&str) -> Option<usize>,
    ) -> usize {
        let last_order = self.last.get(&key).and_then(|id| order_of(id));
        let index = last_order
            .and_then(|last| candidates.iter().position(|(order, _)| *order > last))
            .unwrap_or(0);
        if self.last.len() >= MAX_ROTATION_KEYS && !self.last.contains_key(&key) {
            self.last.clear();
        }
        if let Some((_, id)) = candidates.get(index) {
            self.last.insert(key, (*id).to_string());
        }
        index
    }

    /// Smooth weighted round-robin over `(id, weight)` candidates with
    /// non-zero weights. Returns an index into `candidates`.
    pub fn weighted(&mut self, key: RotationKey, candidates: &[(&str, u32)]) -> usize {
        if self.weighted.len() >= MAX_ROTATION_KEYS && !self.weighted.contains_key(&key) {
            self.weighted.clear();
        }
        let current = self.weighted.entry(key).or_default();
        let mut total: i64 = 0;
        let mut best: Option<(usize, i64)> = None;
        for (index, (id, weight)) in candidates.iter().enumerate() {
            let weight = i64::from(*weight);
            total = total.saturating_add(weight);
            let value = current.entry((*id).to_string()).or_insert(0);
            *value = value.saturating_add(weight);
            // Strictly greater: the first candidate wins ties.
            if best.is_none_or(|(_, top)| *value > top) {
                best = Some((index, *value));
            }
        }
        let index = best.map(|(index, _)| index).unwrap_or(0);
        if let Some((id, _)) = candidates.get(index)
            && let Some(value) = current.get_mut(*id)
        {
            *value = value.saturating_sub(total);
        }
        index
    }

    /// Forgets weighted accumulators (weights may have changed) and cursors
    /// that point at credentials that no longer exist.
    pub fn reset_after_rebuild(&mut self, exists: impl Fn(&str) -> bool) {
        self.weighted.clear();
        self.last.retain(|_, id| exists(id));
    }
}

// ---------------------------------------------------------------------------
// Session affinity
// ---------------------------------------------------------------------------

/// What a session is bound to: a credential *and* the model it served. A
/// provider's prompt cache belongs to one model, so a binding says nothing
/// about the same credential serving another alias target.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Bound {
    pub credential: CredentialId,
    /// Client-facing name of the served model (the alias target's name for
    /// alias requests).
    pub model: String,
}

#[derive(Debug)]
struct Binding {
    bound: Bound,
    touched: Ms,
    /// Position in the eviction order.
    seq: u64,
}

/// Bounded map from session key to the credential and model that served it
/// last. When full, the binding that was refreshed longest ago is evicted.
#[derive(Debug)]
pub(crate) struct Affinity {
    bindings: HashMap<String, Binding>,
    order: BTreeMap<u64, String>,
    next_seq: u64,
    capacity: usize,
}

impl Default for Affinity {
    fn default() -> Self {
        Affinity::with_capacity(AFFINITY_CAPACITY)
    }
}

impl Affinity {
    pub fn with_capacity(capacity: usize) -> Self {
        Affinity {
            bindings: HashMap::new(),
            order: BTreeMap::new(),
            next_seq: 0,
            capacity: capacity.max(1),
        }
    }

    /// Map key for a session and the client-facing model it talks to. One
    /// conversation may use several models (a main and a helper model); each
    /// keeps its own binding so they do not evict each other.
    pub fn key(session: &str, model: &str) -> String {
        let session = session.trim();
        let model = model.to_lowercase();
        if session.len() > MAX_SESSION_KEY_BYTES {
            let digest: String = Sha256::digest(session.as_bytes())
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            format!("sha256:{digest}\u{0}{model}")
        } else {
            format!("{session}\u{0}{model}")
        }
    }

    /// What `key` is bound to, unless the binding has been idle for `ttl_ms`
    /// or longer (it is then dropped).
    pub fn get(&mut self, key: &str, now: Ms, ttl_ms: Ms) -> Option<Bound> {
        let binding = self.bindings.get(key)?;
        if now.saturating_sub(binding.touched) >= ttl_ms {
            self.remove(key);
            return None;
        }
        Some(binding.bound.clone())
    }

    /// Binds (or re-binds) `key` and marks it as just used.
    pub fn bind(&mut self, key: &str, credential: &str, model: &str, now: Ms) {
        if let Some(old) = self.bindings.remove(key) {
            self.order.remove(&old.seq);
        }
        while self.bindings.len() >= self.capacity {
            let Some((_, oldest)) = self.order.pop_first() else {
                break;
            };
            self.bindings.remove(&oldest);
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        self.order.insert(seq, key.to_string());
        self.bindings.insert(
            key.to_string(),
            Binding {
                bound: Bound {
                    credential: credential.to_string(),
                    model: model.to_string(),
                },
                touched: now,
                seq,
            },
        );
    }

    /// Whether `key` is bound to `credential` — serving `model`, when one is
    /// given.
    fn points_at(&self, key: &str, credential: &str, model: Option<&str>) -> bool {
        self.bindings.get(key).is_some_and(|b| {
            b.bound.credential == credential && model.is_none_or(|m| b.bound.model == m)
        })
    }

    /// Extends the binding's life, but only if it still points at
    /// `credential` serving `model`.
    pub fn refresh_if(&mut self, key: &str, credential: &str, model: &str, now: Ms) {
        if self.points_at(key, credential, Some(model)) {
            self.bind(key, credential, model, now);
        }
    }

    /// Drops the binding, but only if it still points at `credential` —
    /// serving `model`, when one is given; whatever it serves otherwise.
    pub fn unbind_if(&mut self, key: &str, credential: &str, model: Option<&str>) {
        if self.points_at(key, credential, model) {
            self.remove(key);
        }
    }

    fn remove(&mut self, key: &str) {
        if let Some(old) = self.bindings.remove(key) {
            self.order.remove(&old.seq);
        }
    }

    pub fn len(&self) -> usize {
        self.bindings.len()
    }

    /// Forgets every binding.
    pub fn clear(&mut self) {
        self.bindings.clear();
        self.order.clear();
    }

    /// Drops bindings to credentials that no longer exist.
    pub fn retain_credentials(&mut self, exists: impl Fn(&str) -> bool) {
        let order = &mut self.order;
        self.bindings.retain(|_, binding| {
            let keep = exists(&binding.bound.credential);
            if !keep {
                order.remove(&binding.seq);
            }
            keep
        });
    }
}

/// Everything that changes while the gateway runs.
#[derive(Debug, Default)]
pub(crate) struct State {
    pub credentials: HashMap<CredentialId, CredState>,
    pub rotation: Rotation,
    pub affinity: Affinity,
}

#[cfg(test)]
mod tests {
    use super::*;
    use switchyard_core::UpstreamErrorInfo;

    fn error(class: FailureClass, status: u16, retry_after_ms: Option<u64>) -> UpstreamError {
        UpstreamError {
            status,
            class,
            info: UpstreamErrorInfo {
                message: "boom".into(),
                ..UpstreamErrorInfo::default()
            },
            retry_after_ms,
            body: None,
            content_type: None,
        }
    }

    #[test]
    fn backoff_doubles_and_caps() {
        assert_eq!(rate_limit_backoff_ms(1, 1800, 1), 1_000);
        assert_eq!(rate_limit_backoff_ms(1, 1800, 2), 2_000);
        assert_eq!(rate_limit_backoff_ms(1, 1800, 3), 4_000);
        assert_eq!(rate_limit_backoff_ms(1, 1800, 11), 1_024_000);
        assert_eq!(rate_limit_backoff_ms(1, 1800, 12), 1_800_000);
        assert_eq!(rate_limit_backoff_ms(1, 1800, 500), 1_800_000);
        assert_eq!(rate_limit_backoff_ms(5, 60, 1), 5_000);
        assert_eq!(rate_limit_backoff_ms(5, 60, 4), 40_000);
        assert_eq!(rate_limit_backoff_ms(5, 60, 5), 60_000);
        assert_eq!(rate_limit_backoff_ms(0, 60, 9), 0);
        // A streak of zero behaves like the first failure.
        assert_eq!(rate_limit_backoff_ms(3, 60, 0), 3_000);
        assert_eq!(rate_limit_backoff_ms(u64::MAX, u64::MAX, 3), u64::MAX);
    }

    #[test]
    fn cooldowns_never_shrink() {
        let mut slot = None;
        extend(&mut slot, 10_000, FailureClass::Server, 0);
        extend(&mut slot, 1_000, FailureClass::Transport, 5);
        assert_eq!(
            slot,
            Some(Cooldown {
                until: 10_000,
                reason: FailureClass::Server
            })
        );
        extend(&mut slot, 20_000, FailureClass::Transport, 5);
        assert_eq!(slot.unwrap().until, 20_005);
        // An expired cooldown is simply replaced, even by a shorter one.
        extend(&mut slot, 1_000, FailureClass::Server, 30_000);
        assert_eq!(slot.unwrap().until, 31_000);
        // Zero wait starts nothing.
        let mut empty = None;
        extend(&mut empty, 0, FailureClass::Server, 0);
        assert_eq!(empty, None);
    }

    #[test]
    fn blocked_reports_the_later_deadline() {
        let mut state = CredState::default();
        let config = CooldownConfig::default();
        state.record_failure("m", &error(FailureClass::Server, 500, None), &config, "", 0);
        state.record_failure("m", &error(FailureClass::Auth, 401, None), &config, "", 0);
        let blocked = state.blocked("m", 1).unwrap();
        assert_eq!(blocked.reason, FailureClass::Auth);
        assert_eq!(blocked.until, 1_800_000);
        // Another model is blocked by the credential-wide cooldown only.
        assert_eq!(
            state.blocked("other", 1).unwrap().reason,
            FailureClass::Auth
        );
        assert!(state.blocked("m", 1_800_000).is_none());
    }

    #[test]
    fn rest_cause_follows_the_rest_that_ends_last() {
        let mut state = CredState::default();
        let config = CooldownConfig::default();
        assert_eq!(state.rest_cause("m", 0), None);
        state.record_failure("m", &error(FailureClass::Server, 500, None), &config, "", 0);
        assert_eq!(state.rest_cause("m", 1), Some((0, "500 boom")));
        assert_eq!(state.rest_cause("other", 1), None);

        // The whole credential rests longer than `m` does.
        state.record_failure("m", &error(FailureClass::Auth, 401, None), &config, "", 10);
        assert_eq!(state.rest_cause("m", 11), Some((10, "401 boom")));
        assert_eq!(state.rest_cause("other", 11), Some((10, "401 boom")));

        // `n` rests longer still, for its own reason.
        state.record_failure(
            "n",
            &error(FailureClass::ModelNotFound, 404, None),
            &config,
            "",
            20,
        );
        assert_eq!(state.rest_cause("n", 21), Some((20, "404 boom")));

        // A later failure of another kind replaces `last_error`, not the
        // reason the credential rests.
        state.record_failure(
            "other",
            &error(FailureClass::Transport, 0, None),
            &config,
            "",
            30,
        );
        assert_eq!(state.last_error.as_ref().unwrap().status, 0);
        assert_eq!(state.rest_cause("other", 31), Some((10, "401 boom")));
        assert_eq!(state.rest_cause("m", 31), Some((10, "401 boom")));

        // A failure that does not lengthen the rest does not become its cause …
        let short = CooldownConfig {
            quota_secs: 1,
            ..CooldownConfig::default()
        };
        state.record_failure("m", &error(FailureClass::Quota, 402, None), &short, "", 40);
        assert_eq!(state.rest_cause("x", 41), Some((10, "401 boom")));
        // … one that does, is.
        state.record_failure("m", &error(FailureClass::Quota, 402, None), &config, "", 50);
        assert_eq!(state.rest_cause("x", 51), Some((50, "402 boom")));

        // When the credential is back only the models' own rests remain.
        let later = 50 + 3_600_000;
        assert_eq!(state.rest_cause("x", later), None);
        assert_eq!(state.rest_cause("m", later), None);
        assert_eq!(state.rest_cause("n", later), Some((20, "404 boom")));
        state.prune(later);
        assert!(state.cooldown_cause.is_none());
        state.clear_cooldowns();
        assert_eq!(state.rest_cause("n", later), None);
    }

    #[test]
    fn absurd_retry_hints_are_capped() {
        let mut state = CredState::default();
        let config = CooldownConfig::default();
        state.record_failure(
            "m",
            &error(FailureClass::RateLimit, 429, Some(u64::MAX)),
            &config,
            "",
            1_000,
        );
        let until = state.blocked("m", 1_000).unwrap().until;
        assert_eq!(until, 1_000 + MAX_RETRY_HINT_MS as Ms);
    }

    #[test]
    fn prune_drops_expired_bookkeeping() {
        let mut state = CredState::default();
        let config = CooldownConfig::default();
        state.record_failure("a", &error(FailureClass::Server, 500, None), &config, "", 0);
        state.record_failure(
            "b",
            &error(FailureClass::RateLimit, 429, None),
            &config,
            "",
            0,
        );
        state.record_failure("c", &error(FailureClass::Quota, 402, None), &config, "", 0);
        state.prune(70_000);
        // The cooldowns of `a` and `b` are over; the failures themselves are
        // still remembered, and `b` keeps its streak for the backoff.
        assert!(state.models["a"].cooldown.is_none());
        assert!(state.models["a"].last_failure.is_some());
        assert_eq!(state.models["b"].rate_limit_streak, 1);
        assert!(state.models["b"].cooldown.is_none());
        assert!(state.cooldown.is_some());
        state.prune(FAILURE_MEMORY_MS);
        // An hour on nothing is left of `a` and `c`; only the streak of `b`.
        assert!(!state.models.contains_key("a"));
        assert!(!state.models.contains_key("c"));
        assert_eq!(state.models["b"].rate_limit_streak, 1);
        assert!(state.models["b"].last_failure.is_none());
        assert!(state.cooldown.is_none());
    }

    #[test]
    fn configured_durations_are_not_capped_but_upstream_hints_are() {
        let month = 30 * 24 * 3600;
        let config = CooldownConfig {
            auth_secs: month,
            quota_secs: month,
            model_not_found_secs: month,
            transient_secs: month,
            rate_limit_base_secs: month,
            rate_limit_max_secs: month,
            ..CooldownConfig::default()
        };
        let month_ms = (month * 1000) as Ms;
        for (class, status) in [
            (FailureClass::Auth, 401),
            (FailureClass::Quota, 402),
            (FailureClass::ModelNotFound, 404),
            (FailureClass::Server, 500),
            (FailureClass::Transport, 0),
            (FailureClass::RateLimit, 429),
        ] {
            let mut state = CredState::default();
            state.record_failure("m", &error(class, status, None), &config, "", 0);
            assert_eq!(state.blocked("m", 1).unwrap().until, month_ms, "{class:?}");
        }
        // A duration too large for the clock saturates instead of wrapping.
        let forever = CooldownConfig {
            auth_secs: u64::MAX,
            ..CooldownConfig::default()
        };
        let mut state = CredState::default();
        state.record_failure(
            "m",
            &error(FailureClass::Auth, 401, None),
            &forever,
            "",
            5_000,
        );
        assert_eq!(state.blocked("m", 5_000).unwrap().until, Ms::MAX);
        // The upstream's own wish is still bounded, also for the credential-wide classes.
        let mut state = CredState::default();
        state.record_failure(
            "m",
            &error(FailureClass::Quota, 429, Some(u64::MAX)),
            &CooldownConfig::default(),
            "",
            0,
        );
        assert_eq!(
            state.blocked("m", 1).unwrap().until,
            MAX_RETRY_HINT_MS as Ms
        );
    }

    #[test]
    fn failures_are_remembered_in_order_until_a_success() {
        let mut state = CredState::default();
        // No cooldowns at all: the memory must not depend on them.
        let off = CooldownConfig {
            enabled: false,
            ..CooldownConfig::default()
        };
        let boom = error(FailureClass::Server, 500, None);
        assert!(state.recent_failure("a", 0).is_none());
        state.record_failure("a", &boom, &off, "", 1_000);
        // Same millisecond, later failure: the sequence still orders them.
        state.record_failure("b", &boom, &off, "", 1_000);
        assert!(state.blocked("a", 1_000).is_none());
        assert!(state.failed_more_recently("b", "a", 1_000));
        assert!(!state.failed_more_recently("a", "b", 1_000));
        // Against a model without a failure any remembered failure is newer.
        assert!(state.failed_more_recently("a", "c", 1_000));
        assert!(!state.failed_more_recently("c", "a", 1_000));

        // Both happened since the (implicit) start; a request that is newly
        // given the credential now owns neither, only what fails afterwards.
        assert!(state.failed_since_request_began("a", 1_000));
        assert!(state.failed_since_request_began("b", 1_000));
        state.begin_request();
        assert!(!state.failed_since_request_began("a", 1_000));
        assert!(!state.failed_since_request_began("b", 1_000));
        assert!(!state.failed_since_request_began("c", 1_000));
        state.record_failure("b", &boom, &off, "", 1_000);
        assert!(state.failed_since_request_began("b", 1_000));
        assert!(!state.failed_since_request_began("a", 1_000));
        assert!(state.failed_more_recently("b", "a", 1_000));

        assert!(state.on_probation("a", 1_000 + PROBATION_MS - 1));
        assert!(!state.on_probation("a", 1_000 + PROBATION_MS));
        assert!(
            state
                .recent_failure("a", 1_000 + FAILURE_MEMORY_MS - 1)
                .is_some()
        );
        assert!(
            state
                .recent_failure("a", 1_000 + FAILURE_MEMORY_MS)
                .is_none()
        );

        // A request fault is the client's: not remembered.
        state.record_failure(
            "d",
            &error(FailureClass::Request, 400, None),
            &off,
            "",
            1_000,
        );
        assert!(state.recent_failure("d", 1_000).is_none());

        state.record_success("a", 10, 2_000);
        assert!(state.recent_failure("a", 2_000).is_none());
        assert!(state.recent_failure("b", 2_000).is_some());

        // While a model rests its failure is remembered however long that takes.
        let mut resting = CredState::default();
        resting.record_failure(
            "m",
            &error(FailureClass::ModelNotFound, 404, None),
            &CooldownConfig::default(),
            "",
            0,
        );
        assert!(
            resting
                .recent_failure("m", 10 * FAILURE_MEMORY_MS)
                .is_some()
        );
        assert!(!resting.on_probation("m", 10 * FAILURE_MEMORY_MS));
    }

    #[test]
    fn messages_are_scrubbed_and_shortened() {
        assert_eq!(
            scrub(
                "Incorrect key\n  sk-supersecret123 given",
                "sk-supersecret123"
            ),
            "Incorrect key [redacted] given"
        );
        assert_eq!(scrub("short ok", "ok"), "short ok");
        // The own key is removed wherever it appears, also inside JSON-ish text.
        assert_eq!(
            scrub("{\"key\":\"sk-supersecret123\"}", "sk-supersecret123"),
            "{\"key\":\"[redacted]\"}"
        );
        assert_eq!(
            scrub(&"x".repeat(500), "").chars().count(),
            ERROR_MESSAGE_CHARS + 1
        );
    }

    #[test]
    fn foreign_keys_and_tokens_are_masked() {
        // A key other than the credential's own (another account's, or a
        // differently trimmed copy).
        assert_eq!(
            scrub(
                "Incorrect API key provided: sk-proj-abcdefghijklmnopqrstuvwxyz.",
                ""
            ),
            "Incorrect API key provided: sk-pro…wxyz."
        );
        assert_eq!(
            scrub("API key not valid: 'AIzaSyA-abcdefghijklmnopqrstuvwx'", ""),
            "API key not valid: 'AIzaSy…uvwx'"
        );
        assert_eq!(
            scrub("token ya29.a0AfH6SMBxxxxxxxxxxxxxxxxxxxxxxx expired", ""),
            "token ya29.a…xxxx expired"
        );
        // Already masked by the vendor, too short to be a key, or simply a
        // word that contains the letters: untouched.
        for text in [
            "Incorrect API key provided: sk-proj-****abcd.",
            "use sk-test please",
            "a task-oriented-and-rather-long-hyphenated-word",
            "risk-management-framework-version-two",
            "rate limit reached for gpt-5.5 in organization org-abcdefghijklmnopqrstuvwx",
        ] {
            assert_eq!(scrub(text, ""), text);
        }
        // Multi-byte text around a key never panics, and a key that follows
        // a non-ASCII letter directly is still found.
        assert_eq!(
            scrub("密钥«sk-abcdefghijklmnopqrstuvwxyz0123»无效", ""),
            "密钥«sk-abc…0123»无效"
        );
        assert_eq!(
            scrub("密钥sk-abcdefghijklmnopqrstuvwxyz0123无效", ""),
            "密钥sk-abc…0123无效"
        );
    }

    #[test]
    fn every_key_in_a_word_is_masked() {
        let a = "sk-proj-AAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        let b = "sk-ant-api03-BBBBBBBBBBBBBBBBBBBBBBBB";
        // Compact JSON: one "word", two keys.
        assert_eq!(
            scrub(&format!(r#"{{"received":"{a}","expected":["{b}"]}}"#), ""),
            r#"{"received":"sk-pro…AAAA","expected":["sk-ant…BBBB"]}"#
        );
        // A short look-alike in front does not shield the real key behind it.
        assert_eq!(
            scrub(&format!(r#"{{"code":"sk-invalid","key":"{a}"}}"#), ""),
            r#"{"code":"sk-invalid","key":"sk-pro…AAAA"}"#
        );
        // Separated by punctuation only.
        assert_eq!(
            scrub(&format!("keys={a},{b};"), ""),
            "keys=sk-pro…AAAA,sk-ant…BBBB;"
        );
        // Other vendors' prefixes and bearer tokens.
        assert_eq!(
            scrub("bad key gsk_abcdefghijklmnopqrstuvwxyz012345", ""),
            "bad key gsk_ab…2345"
        );
        assert_eq!(
            scrub(
                "got header Authorization: Bearer abcdefghijklmnopqrstuvwxyz0123456789",
                ""
            ),
            "got header Authorization: Bearer abcdef…6789"
        );
        assert_eq!(
            scrub(
                "bearer eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.c2lnbmF0dXJl.",
                ""
            ),
            "bearer eyJhbG…dXJl."
        );
        // Prose after "Bearer" is not a token.
        for text in [
            "Bearer authentication is required",
            "expected a bearer token-in-header",
            "unknown model xai-grok-4-fast-reasoning-latest",
        ] {
            assert_eq!(scrub(text, ""), text);
        }
    }

    #[test]
    fn short_own_keys_are_removed_as_whole_tokens() {
        assert_eq!(
            scrub("Received API Key = sk-9x7Q", "sk-9x7Q"),
            "Received API Key = [redacted]"
        );
        assert_eq!(
            scrub(r#"{"key":"sk-1234","again":"sk-1234"}"#, "sk-1234"),
            r#"{"key":"[redacted]","again":"[redacted]"}"#
        );
        assert_eq!(scrub("key=k3y!; retry", "k3y!"), "key=[redacted]; retry");
        // Inside a longer token or an ordinary word it is not the key.
        assert_eq!(
            scrub("abcde xabcd abcd-1 0abcd", "abcd"),
            "abcde xabcd abcd-1 0abcd"
        );
        assert_eq!(scrub("task 1234567 failed", "1234"), "task 1234567 failed");
        assert_eq!(scrub("task 1234 failed", "1234"), "task [redacted] failed");
        // Overlapping candidates are each examined.
        assert_eq!(scrub("aaaaa aaaa", "aaaa"), "aaaaa [redacted]");
        // Multi-byte neighbours are boundaries, and never split.
        assert_eq!(scrub("密钥abcd无效", "abcd"), "密钥[redacted]无效");
        assert_eq!(
            scrub("clé «ñandú» refusée", "ñandú"),
            "clé «[redacted]» refusée"
        );
        // Placeholders of up to three characters are not secrets.
        assert_eq!(scrub("invalid key x", "x"), "invalid key x");
        assert_eq!(scrub("invalid key", "key"), "invalid key");
        // An empty secret (keyless credential) changes nothing.
        assert_eq!(scrub("nothing to hide", ""), "nothing to hide");
        assert_eq!(scrub("nothing to hide", "  "), "nothing to hide");
    }

    #[test]
    fn round_robin_rotates_by_identity() {
        let mut rotation = Rotation::default();
        let order = |id: &str| ["a", "b", "c"].iter().position(|x| *x == id);
        let key = || ("m".to_string(), 0);
        let all = [(0, "a"), (1, "b"), (2, "c")];
        assert_eq!(rotation.round_robin(key(), &all, order), 0);
        assert_eq!(rotation.round_robin(key(), &all, order), 1);
        // `c` is unavailable: wrap to `a` rather than re-seat an index.
        let without_c = [(0, "a"), (1, "b")];
        assert_eq!(rotation.round_robin(key(), &without_c, order), 0);
        assert_eq!(rotation.round_robin(key(), &all, order), 1);
        assert_eq!(rotation.round_robin(key(), &all, order), 2);
        assert_eq!(rotation.round_robin(key(), &all, order), 0);
        // A cursor pointing at a vanished credential restarts the rotation.
        rotation.reset_after_rebuild(|_| false);
        assert_eq!(rotation.round_robin(key(), &all, order), 0);
    }

    #[test]
    fn weighted_rotation_is_smooth_and_exact() {
        let mut rotation = Rotation::default();
        let candidates = [("a", 5), ("b", 1), ("c", 1)];
        let picks: Vec<usize> = (0..7)
            .map(|_| rotation.weighted(("m".into(), 0), &candidates))
            .collect();
        // The classic smooth sequence for 5/1/1: a a b a c a a.
        assert_eq!(picks, vec![0, 0, 1, 0, 2, 0, 0]);
    }

    #[test]
    fn rotation_tables_are_bounded() {
        let mut rotation = Rotation::default();
        for i in 0..(MAX_ROTATION_KEYS * 2 + 10) {
            rotation.round_robin((format!("m{i}"), 0), &[(0, "a")], |_| Some(0));
            rotation.weighted((format!("m{i}"), 0), &[("a", 1)]);
        }
        assert!(rotation.last.len() <= MAX_ROTATION_KEYS);
        assert!(rotation.weighted.len() <= MAX_ROTATION_KEYS);
    }

    #[test]
    fn affinity_expires_and_refreshes() {
        let bound = |credential: &str, model: &str| {
            Some(Bound {
                credential: credential.into(),
                model: model.into(),
            })
        };
        let mut affinity = Affinity::with_capacity(8);
        affinity.bind("s", "cred-a", "m", 0);
        assert_eq!(affinity.get("s", 999, 1000), bound("cred-a", "m"));
        assert_eq!(affinity.get("s", 1000, 1000), None);
        assert_eq!(affinity.len(), 0);

        affinity.bind("s", "cred-a", "m", 0);
        affinity.refresh_if("s", "cred-b", "m", 900); // not bound to b: ignored
        assert_eq!(affinity.get("s", 1000, 1000), None);
        affinity.bind("s", "cred-a", "m", 0);
        affinity.refresh_if("s", "cred-a", "other", 900); // not serving that model: ignored
        assert_eq!(affinity.get("s", 1000, 1000), None);
        affinity.bind("s", "cred-a", "m", 0);
        affinity.refresh_if("s", "cred-a", "m", 900);
        assert_eq!(affinity.get("s", 1500, 1000), bound("cred-a", "m"));

        affinity.unbind_if("s", "cred-b", None);
        affinity.unbind_if("s", "cred-a", Some("other"));
        assert_eq!(affinity.len(), 1);
        affinity.unbind_if("s", "cred-a", Some("m"));
        assert_eq!(affinity.len(), 0);
        affinity.bind("s", "cred-a", "m", 0);
        affinity.unbind_if("s", "cred-a", None);
        assert_eq!(affinity.len(), 0);
    }

    #[test]
    fn affinity_is_bounded_and_evicts_the_stalest() {
        let mut affinity = Affinity::with_capacity(3);
        affinity.bind("s1", "a", "m", 1);
        affinity.bind("s2", "a", "m", 2);
        affinity.bind("s3", "a", "m", 3);
        // Touching s1 makes s2 the stalest.
        affinity.bind("s1", "a", "m", 4);
        affinity.bind("s4", "b", "m", 5);
        assert_eq!(affinity.len(), 3);
        assert_eq!(affinity.get("s2", 5, 1_000_000), None);
        assert!(affinity.get("s1", 5, 1_000_000).is_some());
        assert!(affinity.get("s3", 5, 1_000_000).is_some());
        assert!(affinity.get("s4", 5, 1_000_000).is_some());
        assert_eq!(affinity.order.len(), affinity.bindings.len());

        affinity.retain_credentials(|id| id == "b");
        assert_eq!(affinity.len(), 1);
        assert_eq!(affinity.order.len(), 1);
    }

    #[test]
    fn affinity_keys_separate_models_and_bound_long_sessions() {
        assert_ne!(Affinity::key("s", "gpt-x"), Affinity::key("s", "claude-y"));
        assert_eq!(Affinity::key(" s ", "GPT-X"), Affinity::key("s", "gpt-x"));
        let long = "k".repeat(10_000);
        let key = Affinity::key(&long, "m");
        assert!(key.len() < 100);
        assert_eq!(key, Affinity::key(&long, "m"));
        assert_ne!(key, Affinity::key(&"j".repeat(10_000), "m"));
    }
}
