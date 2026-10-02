//! Model registry and credential scheduling. See `docs/DESIGN.md` section 6.
//!
//! The crate answers one question for the gateway: *which credential serves
//! this model right now?*
//!
//! * [`catalog()`] — built-in metadata for well-known models;
//! * [`Scheduler`] — the registry built from the configuration (providers,
//!   credentials, client-facing model names, aliases) together with the
//!   runtime state of every credential (cooldowns, counters, latency,
//!   rotation cursors, session bindings).
//!
//! A request goes through three calls:
//!
//! 1. [`Scheduler::resolve`] turns the client's model string into the list of
//!    things that can serve it ([`Resolved`]);
//! 2. [`Scheduler::pick`] chooses a credential for one upstream attempt
//!    ([`Lease`]), skipping credentials that are resting or were already tried;
//! 3. [`Scheduler::report`] records what happened, which starts cooldowns and
//!    feeds the selection strategies.
//!
//! Nothing here performs I/O or needs an async runtime. The current time is
//! passed in explicitly (and read from an injectable [`Clock`] for the
//! introspection calls), so behaviour over time is fully deterministic in
//! tests.

mod catalog;
mod clock;
mod error;
mod registry;
mod scheduler;
mod state;
mod types;

pub use catalog::{Catalog, CatalogEntry, catalog, family_of_kind, strip_version_suffix};
pub use clock::{Clock, ManualClock, SystemClock};
pub use error::PickError;
pub use registry::SecretResolver;
pub use scheduler::Scheduler;
pub use types::{
    CredentialId, CredentialSnapshot, CredentialStatus, CredentialView, LastError, Lease,
    ModelCooldown, ModelEntry, ModelRoute, Outcome, PickRequest, ProviderSnapshot, Resolved,
    ResolvedTarget, RouteRef,
};
