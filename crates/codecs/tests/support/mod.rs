//! Shared support code of the cross-protocol test matrix.
//!
//! * [`validators`] — structural validators for each vendor's requests,
//!   responses and streams, written from the vendor rules and independent of
//!   the codecs;
//! * [`scenarios`] — native client requests and canned upstream responses for
//!   each of the four protocols;
//! * [`harness`] — the translation pipeline as the gateway runs it, built
//!   from nothing but the [`switchyard_core::Codec`] trait.
//!
//! Every integration test binary includes this module and uses a different
//! part of it.
#![allow(dead_code)]

pub mod harness;
pub mod scenarios;
pub mod validators;
pub mod views;
