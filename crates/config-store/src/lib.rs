//! The configuration store: loads, validates, watches and rewrites
//! `switchyard.toml`. See `docs/DESIGN.md` section 8.
//!
//! * [`ConfigStore`] owns the live [`Config`](switchyard_core::Config). It
//!   publishes every applied configuration to subscribers, picks up edits made
//!   to the file on disk, and persists edits made through the admin API.
//! * Typed edits ([`ConfigStore::update`]) are written back **preserving the
//!   file's formatting**: only the values that actually changed are touched,
//!   so comments, blank lines, key order and quoting survive (see [`merge`]).
//! * [`mask_config`] / [`unmask_into`] hide literal secrets from the dashboard
//!   and put them back when an edited configuration returns.
//!
//! Secret values never appear in this crate's log lines or error messages.

mod error;
pub mod merge;
mod persist;
mod secrets;
mod store;
mod validate;

pub use error::ConfigStoreError;
pub use persist::write_new;
pub use secrets::{client_key_id, mask_config, unmask_into};
pub use store::{ConfigEvent, ConfigStore, Source, WatchOptions};
pub use validate::validate_text;
