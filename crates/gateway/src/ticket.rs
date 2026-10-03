//! Single-use tickets for client WebSockets.
//!
//! A browser cannot set headers on a WebSocket, so without a ticket the only
//! place it could put a client key is the URL (`?key=`), where proxies,
//! browser history and the developer console all see it. Instead the page
//! buys a ticket with the key in a header (`POST /v1/ws-ticket`) and opens
//! the socket with `?ticket=`. A ticket is good for one request, for
//! [`TICKET_TTL`], and stands for the key that bought it: it is resolved to
//! that key again when it is used, so a key disabled or removed in between
//! takes its tickets with it.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use parking_lot::Mutex;
use rand::RngCore;
use std::collections::HashMap;
use std::fmt;
use std::time::{Duration, Instant};

/// How long a ticket is valid.
pub(crate) const TICKET_TTL: Duration = Duration::from_secs(30);

/// Tickets one holder may have outstanding; minting another drops the one
/// closest to expiry. Keeps the store bounded (by the number of configured
/// keys) without one client being able to spoil another's tickets.
pub(crate) const TICKETS_PER_HOLDER: usize = 32;

/// Who a ticket was bought by.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Holder {
    /// A configured client key, by its id.
    Key(String),
    /// An anonymous client (`auth.required = false`).
    Anonymous,
}

struct Live {
    holder: Holder,
    expires: Instant,
}

/// The tickets that have been minted and not used or expired yet.
#[derive(Default)]
pub(crate) struct Tickets {
    live: Mutex<HashMap<String, Live>>,
}

impl fmt::Debug for Tickets {
    // How many, never which.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Tickets")
            .field("outstanding", &self.live.lock().len())
            .finish()
    }
}

impl Tickets {
    /// A new ticket for `holder`: 32 random bytes, URL-safe base64, valid
    /// for [`TICKET_TTL`] from `now`. Expired tickets are dropped first.
    pub(crate) fn issue(&self, holder: Holder, now: Instant) -> String {
        let mut bytes = [0u8; 32];
        rand::rng().fill_bytes(&mut bytes);
        let ticket = URL_SAFE_NO_PAD.encode(bytes);

        let mut live = self.live.lock();
        live.retain(|_, entry| entry.expires > now);
        let mut held: Vec<(Instant, String)> = live
            .iter()
            .filter(|(_, entry)| entry.holder == holder)
            .map(|(ticket, entry)| (entry.expires, ticket.clone()))
            .collect();
        if held.len() >= TICKETS_PER_HOLDER {
            held.sort();
            let surplus = held.len() + 1 - TICKETS_PER_HOLDER;
            for (_, oldest) in held.into_iter().take(surplus) {
                live.remove(&oldest);
            }
        }
        live.insert(
            ticket.clone(),
            Live {
                holder,
                expires: now + TICKET_TTL,
            },
        );
        ticket
    }

    /// Uses a ticket up: who bought it, when it existed and had not expired
    /// at `now`. A ticket is gone after this, whatever the answer.
    pub(crate) fn redeem(&self, ticket: &str, now: Instant) -> Option<Holder> {
        let entry = self.live.lock().remove(ticket)?;
        (entry.expires > now).then_some(entry.holder)
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.live.lock().len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(id: &str) -> Holder {
        Holder::Key(id.to_string())
    }

    #[test]
    fn tickets_are_single_use() {
        let tickets = Tickets::default();
        let now = Instant::now();
        let a = tickets.issue(key("k1"), now);
        let b = tickets.issue(Holder::Anonymous, now);
        assert_ne!(a, b);
        // 32 bytes, URL-safe, nothing that needs escaping in a query.
        assert_eq!(a.len(), 43);
        assert!(
            a.bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_')
        );
        assert_eq!(tickets.redeem(&a, now), Some(key("k1")));
        assert_eq!(tickets.redeem(&a, now), None, "used up");
        assert_eq!(tickets.redeem(&b, now), Some(Holder::Anonymous));
        assert_eq!(tickets.redeem("made-up", now), None);
        assert_eq!(tickets.redeem("", now), None);
        assert_eq!(tickets.len(), 0);
    }

    #[test]
    fn tickets_expire_after_thirty_seconds() {
        let tickets = Tickets::default();
        let now = Instant::now();
        let fresh = tickets.issue(key("k"), now);
        assert_eq!(
            tickets.redeem(&fresh, now + TICKET_TTL - Duration::from_millis(1)),
            Some(key("k"))
        );
        let stale = tickets.issue(key("k"), now);
        assert_eq!(tickets.redeem(&stale, now + TICKET_TTL), None);
        // And an expired ticket is not kept around.
        let forgotten = tickets.issue(key("k"), now);
        tickets.issue(key("other"), now + TICKET_TTL + Duration::from_secs(1));
        assert_eq!(tickets.len(), 1);
        assert_eq!(tickets.redeem(&forgotten, now), None);
    }

    #[test]
    fn each_holder_has_a_bounded_number_of_tickets_of_its_own() {
        let tickets = Tickets::default();
        let now = Instant::now();
        let theirs = tickets.issue(key("other"), now);
        let first = tickets.issue(key("busy"), now);
        let mut last = String::new();
        for i in 1..=TICKETS_PER_HOLDER as u64 {
            last = tickets.issue(key("busy"), now + Duration::from_millis(i));
        }
        assert_eq!(tickets.len(), TICKETS_PER_HOLDER + 1);
        // The busy holder's oldest made room; nobody else's did.
        assert_eq!(tickets.redeem(&first, now), None);
        assert_eq!(tickets.redeem(&theirs, now), Some(key("other")));
        assert_eq!(tickets.redeem(&last, now), Some(key("busy")));
    }

    #[test]
    fn debug_output_never_shows_tickets() {
        let tickets = Tickets::default();
        let ticket = tickets.issue(key("k"), Instant::now());
        let shown = format!("{tickets:?}");
        assert!(!shown.contains(&ticket), "{shown}");
        assert!(shown.contains("outstanding: 1"), "{shown}");
    }
}
