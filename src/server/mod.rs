//! Protocol wiring: request routing ([`backend`]), capability negotiation ([`capabilities`]),
//! and the per-document cache ([`document`]) that precomputes diagnostics and semantic tokens for
//! every analyzed document.

pub(crate) mod backend;
pub(crate) mod capabilities;
pub(crate) mod document;
