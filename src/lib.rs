//! `portable-opentunnel`: expose one local service through OpenTunnel.
//!
//! The binary is a thin supervisor around the pinned SDK. Library modules are
//! separated so each rule (argument parsing, state ownership, forwarding,
//! cleanup, output) can be tested on its own.

pub mod bridge;
pub mod budget;
pub mod cleanup;
pub mod cli;
pub mod directory;
pub mod error;
pub mod forward;
pub mod layout;
pub mod random;
pub mod redact;
pub mod remote;
pub mod remove;
pub mod signals;
pub mod state;
pub mod trust;
pub mod url;

#[cfg(test)]
pub mod test_support;
