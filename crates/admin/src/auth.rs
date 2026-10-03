//! Who may use the admin API: the secret, the loopback rule, the lockout
//! after repeated failures, and the single-use tickets of the live-event
//! WebSocket.
//!
//! Decision order for every `/admin/api` request ([`guard`]):
//!
//! 1. admin switched off, or no secret configured → 404, as if the API did
//!    not exist;
//! 2. a remote peer while remote access is off → 403;
//! 3. `GET /ws` → the `ticket` query parameter must be a live ticket (401);
//! 4. every other route: the peer's address is locked out → 429; no secret
//!    presented → 401; a wrong secret → 401, and the fifth in a row starts a
//!    lockout (429).
//!
//! # Whose failures are whose
//!
//! Failed sign-ins are counted per address of the TCP peer, with one
//! distinction: what a reverse proxy on this machine relays is counted apart
//! from what connects to the gateway directly on loopback ([`LockoutKey`]).
//! Otherwise anybody on the internet could, with five wrong secrets every
//! half hour sent through the proxy, keep the operator who signs in on the
//! machine itself locked out. The address a proxy *reports* in its headers
//! is not used: unless the proxy overwrites what the client sent, the client
//! chooses it, and a guesser could give every attempt a bucket of its own.
//!
//! # What counts as remote
//!
//! The address of the TCP peer decides, never a header: a client can write
//! anything into `X-Forwarded-For`. The headers are used in one direction
//! only — a loopback peer that sends `X-Forwarded-For`, `Forwarded` or
//! `X-Real-IP` is a reverse proxy on this machine relaying somebody else, and
//! is treated as remote. Without that, putting nginx in front of the gateway
//! would make every client on the internet look local.

use crate::error::ApiFailure;
use crate::{Access, Shared};
use axum::extract::{ConnectInfo, Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use http::HeaderMap;
use http::header::{AUTHORIZATION, CACHE_CONTROL, HeaderName, HeaderValue};
use rand::RngCore;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::{Duration, Instant};
use subtle::ConstantTimeEq;

/// Consecutive wrong secrets from one address that start a lockout.
pub(crate) const MAX_FAILURES: u32 = 5;
/// How long a locked-out address is refused.
pub(crate) const LOCKOUT: Duration = Duration::from_secs(30 * 60);
/// Addresses remembered at most; only unlocked entries may be forgotten.
pub(crate) const LOCKOUT_CAPACITY: usize = 10_000;
/// An address that is not locked out and has not failed for this long is
/// forgotten.
const LOCKOUT_IDLE: Duration = Duration::from_secs(2 * 3600);
/// The table is swept at most this often.
const LOCKOUT_SWEEP: Duration = Duration::from_secs(60);

/// Lifetime of a WebSocket ticket.
pub(crate) const TICKET_TTL: Duration = Duration::from_secs(30);
/// Tickets outstanding at most; the one closest to expiry is dropped first.
pub(crate) const TICKET_CAPACITY: usize = 1024;

const X_ADMIN_SECRET: HeaderName = HeaderName::from_static("x-admin-secret");
const X_CONTENT_TYPE_OPTIONS: HeaderName = HeaderName::from_static("x-content-type-options");

/// Headers a reverse proxy adds when it relays a request.
const FORWARDING_HEADERS: [&str; 3] = ["x-forwarded-for", "forwarded", "x-real-ip"];

// ---------------------------------------------------------------------------
// Peer
// ---------------------------------------------------------------------------

/// The other end of the connection a request arrived on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Peer {
    /// Address of the TCP peer (IPv4-mapped IPv6 addresses as IPv4). `None`
    /// when the server was started without connection info, which is then
    /// treated as remote.
    pub ip: Option<IpAddr>,
    /// Not this machine, or relayed by a proxy on this machine.
    pub remote: bool,
    /// A loopback peer that sent a forwarding header: a reverse proxy on
    /// this machine passing on somebody else's request.
    pub relayed: bool,
}

impl Peer {
    pub fn classify(addr: Option<SocketAddr>, headers: &HeaderMap) -> Peer {
        let ip = addr.map(|addr| addr.ip().to_canonical());
        let forwarded = FORWARDING_HEADERS
            .iter()
            .any(|name| headers.contains_key(*name));
        let local = ip.is_some_and(|ip| ip.is_loopback());
        // Only a loopback peer can be the local proxy. On any other
        // connection the header is the client's own text and changes
        // nothing — in particular not the bucket its failures are counted
        // in, or sending the header would buy a second set of guesses.
        let relayed = local && forwarded;
        Peer {
            ip,
            remote: !local || relayed,
            relayed,
        }
    }

    /// What failed attempts are counted under: the address for IPv4, the
    /// /64 network for IPv6 (one host there commands 2^64 addresses), and
    /// whether the request was relayed by a proxy on this machine.
    pub fn lockout_key(&self) -> LockoutKey {
        let ip = match self.ip {
            Some(IpAddr::V4(ip)) => IpAddr::V4(ip),
            Some(IpAddr::V6(ip)) => {
                let network = u128::from(ip) & !u128::from(u64::MAX);
                IpAddr::V6(Ipv6Addr::from(network))
            }
            None => IpAddr::V4(Ipv4Addr::UNSPECIFIED),
        };
        LockoutKey {
            ip,
            relayed: self.relayed,
        }
    }
}

/// The bucket failed sign-ins are counted in.
///
/// Requests a local reverse proxy relays all arrive from the proxy's
/// loopback address. They get a bucket of their own, so that whatever the
/// outside world sends through the proxy is never held against somebody
/// who connects to the gateway directly on this machine.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) struct LockoutKey {
    /// The peer's address (IPv6: its /64 network).
    pub ip: IpAddr,
    /// Relayed by a proxy on this machine rather than sent by the peer
    /// itself.
    pub relayed: bool,
}

impl std::fmt::Display for LockoutKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.relayed {
            write!(f, "{} (relayed by a local proxy)", self.ip)
        } else {
            write!(f, "{}", self.ip)
        }
    }
}

/// What the guard learned about a request, for the handlers behind it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct AuthContext {
    pub peer: Peer,
    /// Digest of the secret that was in force when the request was admitted.
    pub secret_digest: [u8; 32],
}

// ---------------------------------------------------------------------------
// Lockouts
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
struct Entry {
    failures: u32,
    locked_until: Option<Instant>,
    last_seen: Instant,
}

/// Failed sign-in attempts per address. Bounded; see [`LOCKOUT_CAPACITY`].
#[derive(Debug, Default)]
pub(crate) struct Lockouts {
    entries: HashMap<LockoutKey, Entry>,
    last_sweep: Option<Instant>,
}

impl Lockouts {
    /// How much longer `key` is locked out, if it is.
    pub fn locked(&mut self, key: LockoutKey, now: Instant) -> Option<Duration> {
        if !self.entries.contains_key(&key) && self.entries.len() >= LOCKOUT_CAPACITY {
            self.sweep(now);
            if self.entries.len() >= LOCKOUT_CAPACITY
                && self
                    .entries
                    .values()
                    .all(|entry| entry.locked_until.is_some_and(|until| until > now))
            {
                return self
                    .entries
                    .values()
                    .filter_map(|entry| entry.locked_until)
                    .min()
                    .map(|until| until - now);
            }
        }
        let until = self.entries.get(&key)?.locked_until?;
        if until > now {
            return Some(until - now);
        }
        // Served its time: the slate is clean.
        self.entries.remove(&key);
        None
    }

    /// Counts a wrong secret. Returns the lockout that this failure started,
    /// if it was the fifth in a row.
    pub fn failure(&mut self, key: LockoutKey, now: Instant) -> Option<Duration> {
        if let Some(wait) = self.locked(key, now) {
            return Some(wait);
        }
        self.sweep(now);
        if !self.entries.contains_key(&key) && self.entries.len() >= LOCKOUT_CAPACITY {
            self.evict_oldest();
        }
        let entry = self.entries.entry(key).or_insert(Entry {
            failures: 0,
            locked_until: None,
            last_seen: now,
        });
        entry.last_seen = now;
        entry.failures += 1;
        if entry.failures < MAX_FAILURES {
            return None;
        }
        entry.failures = 0;
        entry.locked_until = Some(now + LOCKOUT);
        Some(LOCKOUT)
    }

    /// The right secret: forget earlier failures.
    pub fn success(&mut self, key: LockoutKey) {
        self.entries.remove(&key);
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Drops expired lockouts and addresses that have been quiet for a
    /// while. Runs at most once a minute, or whenever the table is full.
    fn sweep(&mut self, now: Instant) {
        let due = self
            .last_sweep
            .is_none_or(|last| now.saturating_duration_since(last) >= LOCKOUT_SWEEP);
        if !due && self.entries.len() < LOCKOUT_CAPACITY {
            return;
        }
        self.last_sweep = Some(now);
        self.entries.retain(|_, entry| match entry.locked_until {
            Some(until) => until > now,
            None => now.saturating_duration_since(entry.last_seen) < LOCKOUT_IDLE,
        });
    }

    fn evict_oldest(&mut self) {
        let oldest = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.locked_until.is_none())
            .min_by_key(|(_, entry)| entry.last_seen)
            .map(|(key, _)| *key);
        if let Some(key) = oldest {
            self.entries.remove(&key);
        }
    }
}

// ---------------------------------------------------------------------------
// Tickets
// ---------------------------------------------------------------------------

/// Single-use tickets for `GET /ws`. Browsers cannot set headers on a
/// WebSocket, so the dashboard buys a ticket with the secret and presents
/// the ticket in the URL instead of the secret.
#[derive(Debug, Default)]
pub(crate) struct Tickets {
    live: HashMap<String, (Instant, [u8; 32])>,
}

impl Tickets {
    /// A new ticket: 32 random bytes, URL-safe, valid for [`TICKET_TTL`].
    pub fn issue(&mut self, secret_digest: [u8; 32], now: Instant) -> String {
        self.live.retain(|_, (expires, issued_digest)| {
            *expires > now && *issued_digest == secret_digest
        });
        while self.live.len() >= TICKET_CAPACITY {
            let soonest = self
                .live
                .iter()
                .min_by_key(|(_, (expires, _))| *expires)
                .map(|(ticket, _)| ticket.clone());
            match soonest {
                Some(ticket) => self.live.remove(&ticket),
                None => break,
            };
        }
        let mut bytes = [0u8; 32];
        rand::rng().fill_bytes(&mut bytes);
        let ticket = URL_SAFE_NO_PAD.encode(bytes);
        self.live
            .insert(ticket.clone(), (now + TICKET_TTL, secret_digest));
        ticket
    }

    /// Uses a ticket up. True when it existed and had not expired.
    pub fn redeem(&mut self, ticket: &str, secret_digest: &[u8; 32], now: Instant) -> bool {
        self.live
            .remove(ticket)
            .is_some_and(|(expires, issued_digest)| {
                expires > now && bool::from(issued_digest.ct_eq(secret_digest))
            })
    }

    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.live.len()
    }
}

// ---------------------------------------------------------------------------
// The secret
// ---------------------------------------------------------------------------

pub(crate) fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// The secrets a request presents, as raw bytes (a secret need not be
/// ASCII; the dashboard sends its UTF-8 bytes): the token of
/// `Authorization: Bearer <secret>` — or the whole value when there is no
/// scheme — and `x-admin-secret`.
fn presented(headers: &HeaderMap) -> Vec<&[u8]> {
    let mut candidates = Vec::with_capacity(2);
    if let Some(value) = headers.get(AUTHORIZATION) {
        let bytes = value.as_bytes().trim_ascii();
        let token = match bytes.split_at_checked(6) {
            Some((scheme, rest))
                if scheme.eq_ignore_ascii_case(b"bearer")
                    && rest.first().is_none_or(u8::is_ascii_whitespace) =>
            {
                rest.trim_ascii()
            }
            _ => bytes,
        };
        if !token.is_empty() {
            candidates.push(token);
        }
    }
    if let Some(value) = headers.get(X_ADMIN_SECRET) {
        let bytes = value.as_bytes().trim_ascii();
        if !bytes.is_empty() {
            candidates.push(bytes);
        }
    }
    candidates
}

/// Whether any candidate is the secret. Digests are compared, so the time
/// taken says nothing about the secret's length or content, and every
/// candidate is looked at.
fn any_matches(candidates: &[&[u8]], secret_digest: &[u8; 32]) -> bool {
    let mut matched = subtle::Choice::from(0);
    for candidate in candidates {
        matched |= digest(candidate).ct_eq(secret_digest);
    }
    matched.into()
}

// ---------------------------------------------------------------------------
// The guard
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct TicketQuery {
    #[serde(default)]
    ticket: String,
}

impl crate::routes::QueryParams for TicketQuery {
    fn validate(_name: &str, _value: &str) -> Result<(), ApiFailure> {
        // Ticket validity is checked when it is redeemed below.
        Ok(())
    }
}

fn not_found() -> ApiFailure {
    ApiFailure::not_found("not found")
}

/// Whether the request is for the live-event WebSocket, the one route that
/// is authenticated by ticket. The guard sits inside the `/admin/api` nest,
/// where the prefix is already stripped.
fn is_ws_route(path: &str) -> bool {
    path == "/ws"
}

/// Admits or refuses one `/admin/api` request.
fn admit(state: &Shared, access: &Access, request: &Request) -> Result<AuthContext, ApiFailure> {
    let Some(secret) = access.secret() else {
        return Err(not_found());
    };
    let secret_digest = digest(secret.as_bytes());
    let addr = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .map(|info| info.0);
    let peer = Peer::classify(addr, request.headers());
    if peer.remote && !access.allow_remote {
        return Err(ApiFailure::forbidden("remote admin access is disabled"));
    }
    let now = Instant::now();
    let context = AuthContext {
        peer,
        secret_digest,
    };

    if is_ws_route(request.uri().path()) {
        let ticket =
            crate::routes::parse_query::<TicketQuery>(request.uri().query().unwrap_or_default())?
                .ticket;
        if ticket.is_empty() || !state.tickets.lock().redeem(&ticket, &secret_digest, now) {
            return Err(ApiFailure::unauthorized(
                "the ticket is missing, expired or already used",
            ));
        }
        return Ok(context);
    }

    let key = peer.lockout_key();
    // One lock from the lockout check to the count: a burst of parallel
    // guesses cannot all slip in before the first failure is registered.
    let mut lockouts = state.lockouts.lock();
    if let Some(wait) = lockouts.locked(key, now) {
        return Err(ApiFailure::too_many_attempts(ceil_secs(wait)));
    }
    let candidates = presented(request.headers());
    if candidates.is_empty() {
        // Nothing was guessed, so nothing is held against the address.
        return Err(ApiFailure::unauthorized("the admin secret is required"));
    }
    if any_matches(&candidates, &secret_digest) {
        lockouts.success(key);
        crate::routes::validate_query(request.uri().query().unwrap_or_default())?;
        return Ok(context);
    }
    match lockouts.failure(key, now) {
        Some(lockout) => {
            tracing::warn!(
                peer = %key,
                "admin API locked for this address after {MAX_FAILURES} wrong secrets"
            );
            Err(ApiFailure::too_many_attempts(ceil_secs(lockout)))
        }
        None => Err(ApiFailure::unauthorized("the admin secret is not correct")),
    }
}

fn ceil_secs(wait: Duration) -> u64 {
    u64::try_from(wait.as_millis().div_ceil(1000))
        .unwrap_or(u64::MAX)
        .max(1)
}

/// Middleware in front of every `/admin/api` route.
pub(crate) async fn guard(
    State(state): State<Shared>,
    mut request: Request,
    next: Next,
) -> Response {
    let access = state.access();
    let mut response = match admit(&state, &access, &request) {
        Ok(context) => {
            request.extensions_mut().insert(context);
            next.run(request).await
        }
        Err(failure) => failure.into_response(),
    };
    // Admin answers describe the gateway's configuration: nothing on the
    // way may keep a copy, and nothing may guess a content type.
    let headers = response.headers_mut();
    if response_is_upgrade(headers) {
        return response;
    }
    headers
        .entry(CACHE_CONTROL)
        .or_insert(HeaderValue::from_static("no-store"));
    headers.insert(X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    response
}

fn response_is_upgrade(headers: &HeaderMap) -> bool {
    headers.contains_key(http::header::UPGRADE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.insert(
                HeaderName::from_static(name),
                HeaderValue::from_bytes(value.as_bytes()).unwrap(),
            );
        }
        map
    }

    fn addr(text: &str) -> Option<SocketAddr> {
        Some(text.parse().unwrap())
    }

    /// The bucket of a peer that connected by itself.
    fn direct(ip: &str) -> LockoutKey {
        LockoutKey {
            ip: ip.parse().unwrap(),
            relayed: false,
        }
    }

    #[test]
    fn loopback_peers_are_local_everything_else_is_remote() {
        let none = HeaderMap::new();
        for local in [
            "127.0.0.1:9",
            "127.8.9.1:9",
            "[::1]:9",
            "[::ffff:127.0.0.1]:9",
        ] {
            assert!(!Peer::classify(addr(local), &none).remote, "{local}");
        }
        for remote in [
            "10.0.0.5:9",
            "192.168.1.2:9",
            "8.8.8.8:9",
            "[fe80::1]:9",
            "[2001:db8::1]:9",
            "[::ffff:10.0.0.5]:9",
            "0.0.0.0:9",
        ] {
            assert!(Peer::classify(addr(remote), &none).remote, "{remote}");
        }
        // No connection info at all: refuse rather than guess.
        assert!(Peer::classify(None, &none).remote);
    }

    #[test]
    fn a_relaying_proxy_on_loopback_is_remote() {
        for name in FORWARDING_HEADERS {
            let relayed = headers(&[(name, "203.0.113.9")]);
            assert!(
                Peer::classify(addr("127.0.0.1:9"), &relayed).remote,
                "{name}"
            );
        }
        // A forwarding header never makes a remote peer local.
        let spoofed = headers(&[("x-forwarded-for", "127.0.0.1")]);
        assert!(Peer::classify(addr("203.0.113.9:9"), &spoofed).remote);
    }

    #[test]
    fn relayed_requests_are_counted_apart_from_direct_ones() {
        let none = HeaderMap::new();
        let operator = Peer::classify(addr("127.0.0.1:9"), &none);
        assert!(!operator.relayed);
        assert_eq!(operator.lockout_key(), direct("127.0.0.1"));

        // Whatever a proxy on this machine passes on shares one bucket,
        // which is not the bucket of a direct loopback connection — and
        // not one the client can pick by what it writes into the header.
        for (name, value) in [
            ("x-forwarded-for", "203.0.113.9"),
            ("x-forwarded-for", "198.51.100.7, 203.0.113.9"),
            ("forwarded", "for=198.51.100.4"),
            ("x-real-ip", "192.0.2.1"),
        ] {
            let peer = Peer::classify(addr("127.0.0.1:9"), &headers(&[(name, value)]));
            assert!(peer.relayed, "{name}: {value}");
            assert_eq!(
                peer.lockout_key(),
                LockoutKey {
                    ip: "127.0.0.1".parse().unwrap(),
                    relayed: true,
                },
                "{name}: {value}"
            );
            assert_ne!(peer.lockout_key(), operator.lockout_key());
        }

        // On a connection that is not loopback the header is the client's
        // own text: sending it must not open a second bucket.
        let plain = Peer::classify(addr("203.0.113.9:9"), &none);
        let dressed = Peer::classify(
            addr("203.0.113.9:9"),
            &headers(&[("x-forwarded-for", "10.0.0.1")]),
        );
        assert!(!dressed.relayed);
        assert_eq!(plain.lockout_key(), dressed.lockout_key());
        assert_eq!(plain.lockout_key(), direct("203.0.113.9"));
    }

    #[test]
    fn a_lockout_of_the_relayed_bucket_leaves_the_direct_one_alone() {
        let start = Instant::now();
        let mut table = Lockouts::default();
        let relayed = LockoutKey {
            relayed: true,
            ..direct("127.0.0.1")
        };
        for _ in 0..5 {
            table.failure(relayed, start);
        }
        assert_eq!(table.locked(relayed, start), Some(LOCKOUT));
        assert_eq!(table.locked(direct("127.0.0.1"), start), None);
        // And the other way round.
        let mut table = Lockouts::default();
        for _ in 0..5 {
            table.failure(direct("127.0.0.1"), start);
        }
        assert_eq!(table.locked(relayed, start), None);

        assert_eq!(direct("127.0.0.1").to_string(), "127.0.0.1");
        assert_eq!(relayed.to_string(), "127.0.0.1 (relayed by a local proxy)");
    }

    #[test]
    fn mapped_addresses_are_canonical_and_ipv6_is_keyed_by_network() {
        let peer = Peer::classify(addr("[::ffff:10.1.2.3]:9"), &HeaderMap::new());
        assert_eq!(peer.ip, Some("10.1.2.3".parse().unwrap()));
        assert_eq!(peer.lockout_key(), direct("10.1.2.3"));

        let a = Peer::classify(addr("[2001:db8:1:2:aaaa::1]:9"), &HeaderMap::new());
        let b = Peer::classify(addr("[2001:db8:1:2:bbbb::2]:9"), &HeaderMap::new());
        let c = Peer::classify(addr("[2001:db8:1:3::1]:9"), &HeaderMap::new());
        assert_eq!(a.lockout_key(), b.lockout_key());
        assert_eq!(a.lockout_key(), direct("2001:db8:1:2::"));
        assert_ne!(a.lockout_key(), c.lockout_key());
    }

    #[test]
    fn the_fifth_failure_locks_and_a_success_resets() {
        let key = direct("10.0.0.1");
        let start = Instant::now();
        let mut table = Lockouts::default();
        for _ in 0..4 {
            assert_eq!(table.failure(key, start), None);
        }
        table.success(key);
        assert_eq!(table.len(), 0);
        for _ in 0..4 {
            assert_eq!(table.failure(key, start), None);
        }
        assert_eq!(table.failure(key, start), Some(LOCKOUT));
        assert_eq!(table.locked(key, start), Some(LOCKOUT));
        let later = start + Duration::from_secs(600);
        assert_eq!(
            table.locked(key, later),
            Some(LOCKOUT - Duration::from_secs(600))
        );
        // Other addresses are not affected.
        assert_eq!(table.locked(direct("10.0.0.2"), start), None);
    }

    #[test]
    fn a_lockout_ends_after_thirty_minutes_with_a_clean_slate() {
        let key = direct("10.0.0.1");
        let start = Instant::now();
        let mut table = Lockouts::default();
        for _ in 0..5 {
            table.failure(key, start);
        }
        let end = start + LOCKOUT;
        assert!(table.locked(key, end - Duration::from_millis(1)).is_some());
        assert_eq!(table.locked(key, end), None);
        assert_eq!(table.len(), 0);
        // Four more failures are allowed again before the next lockout.
        for _ in 0..4 {
            assert_eq!(table.failure(key, end), None);
        }
        assert_eq!(table.failure(key, end), Some(LOCKOUT));
    }

    #[test]
    fn idle_entries_are_purged_and_the_table_is_bounded() {
        let start = Instant::now();
        let mut table = Lockouts::default();
        let quiet = direct("10.9.9.9");
        let locked = direct("10.9.9.8");
        table.failure(quiet, start);
        for _ in 0..5 {
            table.failure(locked, start);
        }
        // Twenty minutes later: the quiet one is still remembered (idle
        // entries last two hours), the locked one still locked.
        let other = direct("10.9.9.7");
        table.failure(other, start + Duration::from_secs(20 * 60));
        assert_eq!(table.len(), 3);
        // Past both: only the newcomer of this sweep remains.
        let late = start + LOCKOUT_IDLE + Duration::from_secs(21 * 60);
        table.failure(direct("10.9.9.6"), late);
        assert_eq!(table.len(), 1);

        // Capacity: the least recently seen address makes room.
        let numbered = |i: u32| LockoutKey {
            ip: IpAddr::V4(Ipv4Addr::from(0x0b00_0000 + i)),
            relayed: false,
        };
        let mut table = Lockouts::default();
        for i in 0..LOCKOUT_CAPACITY as u32 {
            table.failure(numbered(i), start + Duration::from_millis(u64::from(i)));
        }
        assert_eq!(table.len(), LOCKOUT_CAPACITY);
        let first = numbered(0);
        let second = numbered(1);
        assert!(table.entries.contains_key(&first));
        table.failure(direct("10.1.1.1"), start + Duration::from_secs(30));
        assert_eq!(table.len(), LOCKOUT_CAPACITY);
        assert!(!table.entries.contains_key(&first));
        assert!(table.entries.contains_key(&second));
    }

    #[test]
    fn tickets_are_single_use_and_expire() {
        let start = Instant::now();
        let mut tickets = Tickets::default();
        let a = tickets.issue([7; 32], start);
        let b = tickets.issue([7; 32], start);
        assert_ne!(a, b);
        // 32 bytes, URL-safe, no padding.
        assert_eq!(a.len(), 43);
        assert!(
            a.bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
        );

        assert!(tickets.redeem(&a, &[7; 32], start + Duration::from_secs(1)));
        assert!(!tickets.redeem(&a, &[7; 32], start + Duration::from_secs(1)));
        assert!(!tickets.redeem("made-up", &[7; 32], start));
        // Thirty seconds on, the other one is worthless.
        assert!(!tickets.redeem(&b, &[7; 32], start + TICKET_TTL));
        assert_eq!(tickets.len(), 0);

        let c = tickets.issue([7; 32], start);
        assert!(tickets.redeem(&c, &[7; 32], start + TICKET_TTL - Duration::from_millis(1)));
    }

    #[test]
    fn tickets_are_bound_to_the_secret_that_issued_them() {
        let start = Instant::now();
        let mut tickets = Tickets::default();
        let old = tickets.issue([1; 32], start);
        assert!(!tickets.redeem(&old, &[2; 32], start));
        assert!(!tickets.redeem(&old, &[1; 32], start));
        let old = tickets.issue([1; 32], start);
        let current = tickets.issue([2; 32], start);
        assert!(!tickets.redeem(&old, &[1; 32], start));
        assert!(tickets.redeem(&current, &[2; 32], start));
    }

    #[test]
    fn active_lockouts_are_never_evicted_to_admit_new_addresses() {
        let now = Instant::now();
        let mut table = Lockouts::default();
        for i in 0..LOCKOUT_CAPACITY as u32 {
            let key = LockoutKey {
                ip: IpAddr::V4(Ipv4Addr::from(0x0b00_0000 + i)),
                relayed: false,
            };
            table.entries.insert(
                key,
                Entry {
                    failures: 0,
                    locked_until: Some(now + LOCKOUT),
                    last_seen: now,
                },
            );
        }
        let newcomer = direct("10.0.0.1");
        assert_eq!(table.locked(newcomer, now), Some(LOCKOUT));
        assert_eq!(table.failure(newcomer, now), Some(LOCKOUT));
        assert_eq!(table.len(), LOCKOUT_CAPACITY);
        assert!(!table.entries.contains_key(&newcomer));
        assert_eq!(table.locked(newcomer, now + LOCKOUT), None);
    }

    #[test]
    fn the_ticket_store_is_bounded() {
        let start = Instant::now();
        let mut tickets = Tickets::default();
        let first = tickets.issue([7; 32], start);
        for i in 1..=TICKET_CAPACITY as u64 {
            tickets.issue([7; 32], start + Duration::from_millis(i));
        }
        assert_eq!(tickets.len(), TICKET_CAPACITY);
        // The oldest outstanding ticket made room.
        assert!(!tickets.redeem(&first, &[7; 32], start + Duration::from_secs(1)));
        // Expired tickets are dropped when the next one is issued.
        tickets.issue([7; 32], start + TICKET_TTL + Duration::from_secs(5));
        assert_eq!(tickets.len(), 1);
    }

    #[test]
    fn secrets_are_read_from_both_headers() {
        let map = headers(&[
            ("authorization", "Bearer  s3cret "),
            ("x-admin-secret", "other"),
        ]);
        assert_eq!(
            presented(&map),
            vec![b"s3cret".as_slice(), b"other".as_slice()]
        );
        let map = headers(&[("authorization", "bEaReR s3cret")]);
        assert_eq!(presented(&map), vec![b"s3cret".as_slice()]);
        // No scheme: the whole value.
        let map = headers(&[("authorization", "s3cret")]);
        assert_eq!(presented(&map), vec![b"s3cret".as_slice()]);
        let map = headers(&[("authorization", "Bearer ")]);
        assert!(presented(&map).is_empty());
        assert!(presented(&HeaderMap::new()).is_empty());
    }

    #[test]
    fn secrets_are_compared_as_bytes() {
        let secret = "pässword-ünïcode-🔐";
        let wanted = digest(secret.as_bytes());
        assert!(any_matches(&[secret.as_bytes()], &wanted));
        assert!(any_matches(&[b"wrong", secret.as_bytes()], &wanted));
        assert!(!any_matches(&[b"wrong"], &wanted));
        assert!(!any_matches(&[], &wanted));
        // A prefix or an extension of the secret is not the secret.
        assert!(!any_matches(&[&secret.as_bytes()[..5]], &wanted));
        let longer = format!("{secret}x");
        assert!(!any_matches(&[longer.as_bytes()], &wanted));
    }

    #[test]
    fn waits_round_up_to_whole_seconds() {
        assert_eq!(ceil_secs(Duration::from_millis(1)), 1);
        assert_eq!(ceil_secs(Duration::from_millis(1000)), 1);
        assert_eq!(ceil_secs(Duration::from_millis(1001)), 2);
        assert_eq!(ceil_secs(Duration::ZERO), 1);
    }
}
