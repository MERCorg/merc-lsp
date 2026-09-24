//! Turns document text into parsed/checked data, plus the small utilities every consumer of that
//! data shares: byte-offset/LSP-position conversion, declared-name extraction, fuzzy name
//! matching, and the parser-ambiguity lint. Nothing in this module depends on
//! [`crate::features`] or [`crate::server`].

pub(crate) mod ambiguity;
pub(crate) mod convert;
pub(crate) mod edit_distance;
pub(crate) mod names;
pub(crate) mod parse;
pub(crate) mod typecheck;
