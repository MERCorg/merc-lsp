//! A panic-safe, off-executor wrapper around `merc_syntax`'s parse entry points.
//!
//! `merc_syntax`'s AST-building layer (the code that runs *after* pest's own grammar match
//! succeeds) contains `unwrap`/`expect`/`unreachable!` sites that can in principle panic on a
//! malformed-but-grammatically-valid input. Since this server is a long-running process talking
//! over stdio, a panic on the async task handling a request must not be allowed to take the
//! whole server down; it also must not deadlock any lock the panicking task happened to be
//! holding.
//!
//! Parsing is also synchronous and CPU-bound, so it is dispatched onto a blocking thread rather
//! than run inline on the async runtime.
//!
//! Three document kinds are supported ([`SpecKind`]): plain mCRL2 process specifications, PBES
//! (parameterised boolean equation systems), and PRES (parameterised real equation systems). The
//! kind is decided purely from the document's file extension (see [`SpecKind::from_uri`]) —
//! `merc_syntax` exposes three distinct, structurally incompatible grammar entry points
//! (`MCRL2Spec`/`PbesSpec`/`PresSpec`) with no reliable way to tell them apart from content alone
//! short of trying all three and guessing from whichever parse succeeds, which would make parse
//! errors on a genuinely broken file misleading (which grammar's error should be shown?).
//! Type checking, hover, and go-to-definition are not extended to PBES/PRES yet: `merc_typecheck`
//! only exposes `ProcessSpecification`, nothing for `UntypedPbes`/`UntypedPres` — see `PLAN.md`.

use std::panic::AssertUnwindSafe;

use lsp_types::Url;
use merc_syntax::UntypedPbes;
use merc_syntax::UntypedPres;
use merc_syntax::UntypedProcessSpecification;
use merc_utilities::MercError;

/// Which of `merc_syntax`'s three top-level grammar entry points a document should be parsed
/// with, decided from its file extension.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpecKind {
    /// A plain `.mcrl2` process specification — the default for any extension other than `.pbes`/
    /// `.pres` (including no extension at all, e.g. an unsaved buffer), since this is the kind
    /// every existing feature (type checking, hover, goto-def, semantic tokens) is built for.
    Process,
    Pbes,
    Pres,
}

impl SpecKind {
    /// Picks a [`SpecKind`] from `uri`'s file extension: `.pbes` and `.pres` (case-sensitively,
    /// matching the extensions registered in `vscode-client/package.json`) select PBES/PRES;
    /// everything else falls back to [`SpecKind::Process`].
    pub fn from_uri(uri: &Url) -> SpecKind {
        let path = uri.path();
        if path.ends_with(".pbes") {
            SpecKind::Pbes
        } else if path.ends_with(".pres") {
            SpecKind::Pres
        } else {
            SpecKind::Process
        }
    }
}

/// A successfully parsed document, tagged by which grammar entry point produced it.
///
/// Each variant is boxed since every one of these ASTs is far larger than a bare enum
/// discriminant; without it every `ParseOutcome`/`Specification` (including the common
/// `ParseError`/`Internal` cases) would pay for the largest AST's worst-case size.
pub enum Specification {
    Process(Box<UntypedProcessSpecification>),
    Pbes(Box<UntypedPbes>),
    Pres(Box<UntypedPres>),
}

impl Specification {
    /// The process specification, if `self` is [`Specification::Process`] — every feature that
    /// isn't parse-error diagnostics or document symbols is scoped to this kind only, for now.
    pub fn as_process(&self) -> Option<&UntypedProcessSpecification> {
        match self {
            Specification::Process(spec) => Some(spec),
            Specification::Pbes(_) | Specification::Pres(_) => None,
        }
    }
}

/// The result of attempting to parse a document: either the AST, a normal parse error, or an
/// internal error (a panic in the AST-building layer).
pub enum ParseOutcome {
    Ok(Specification),
    ParseError(MercError),
    /// The parser panicked while building the AST. Carries a message suitable for a diagnostic;
    /// the panic itself has already been caught and logged.
    Internal(String),
}

/// Parses `text` as `kind`, off the async executor and guarded against panics.
///
/// Takes `text` by value (rather than borrowing) because the work is moved onto a blocking
/// thread via [`tokio::task::spawn_blocking`], which requires a `'static` closure.
pub async fn parse(kind: SpecKind, text: String) -> ParseOutcome {
    // `spawn_blocking` also gets parsing off the runtime's async worker threads, so a large or
    // pathological input can't stall other documents' requests.
    match tokio::task::spawn_blocking(move || parse_catching_panics(kind, &text)).await {
        Ok(outcome) => outcome,
        Err(join_error) => {
            // The blocking task itself panicked in a way `catch_unwind` below didn't intercept
            // (e.g. it was cancelled), or the runtime is shutting down.
            log::error!("parse task failed to join: {join_error}");
            ParseOutcome::Internal("internal error: parser task did not complete".to_string())
        }
    }
}

fn parse_catching_panics(kind: SpecKind, text: &str) -> ParseOutcome {
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| match kind {
        SpecKind::Process => UntypedProcessSpecification::parse(text).map(|spec| Specification::Process(Box::new(spec))),
        SpecKind::Pbes => UntypedPbes::parse(text).map(|spec| Specification::Pbes(Box::new(spec))),
        SpecKind::Pres => UntypedPres::parse(text).map(|spec| Specification::Pres(Box::new(spec))),
    }));
    match result {
        Ok(Ok(spec)) => ParseOutcome::Ok(spec),
        Ok(Err(error)) => ParseOutcome::ParseError(error),
        Err(panic) => {
            let message = panic_message(&panic);
            log::error!("panic while parsing document: {message}");
            ParseOutcome::Internal(format!("internal parser error: {message}"))
        }
    }
}

/// Best-effort extraction of a human-readable message from a caught panic payload.
pub(crate) fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = panic.downcast_ref::<&str>() {
        message.to_string()
    } else if let Some(message) = panic.downcast_ref::<String>() {
        message.clone()
    } else {
        "unknown panic".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn parses_well_formed_specification() {
        let text = "sort D;\ninit delta;".to_string();
        match parse(SpecKind::Process, text).await {
            ParseOutcome::Ok(_) => {}
            ParseOutcome::ParseError(error) => panic!("unexpected parse error: {error}"),
            ParseOutcome::Internal(message) => panic!("unexpected internal error: {message}"),
        }
    }

    #[tokio::test]
    async fn reports_parse_error_for_malformed_specification() {
        let text = "sort D".to_string(); // missing terminating ';'
        match parse(SpecKind::Process, text).await {
            ParseOutcome::Ok(_) => panic!("expected a parse error"),
            ParseOutcome::ParseError(_) => {}
            ParseOutcome::Internal(message) => panic!("unexpected internal error: {message}"),
        }
    }

    #[tokio::test]
    async fn parses_well_formed_pbes() {
        let text = "pbes mu X = true;\ninit X;".to_string();
        match parse(SpecKind::Pbes, text).await {
            ParseOutcome::Ok(Specification::Pbes(_)) => {}
            ParseOutcome::Ok(_) => panic!("expected a Pbes specification"),
            ParseOutcome::ParseError(error) => panic!("unexpected parse error: {error}"),
            ParseOutcome::Internal(message) => panic!("unexpected internal error: {message}"),
        }
    }

    #[tokio::test]
    async fn parses_well_formed_pres() {
        let text = "pres mu X = 0;\ninit X;".to_string();
        match parse(SpecKind::Pres, text).await {
            ParseOutcome::Ok(Specification::Pres(_)) => {}
            ParseOutcome::Ok(_) => panic!("expected a Pres specification"),
            ParseOutcome::ParseError(error) => panic!("unexpected parse error: {error}"),
            ParseOutcome::Internal(message) => panic!("unexpected internal error: {message}"),
        }
    }

    #[test]
    fn spec_kind_from_uri_extension() {
        assert_eq!(SpecKind::from_uri(&"file:///a/b.mcrl2".parse().unwrap()), SpecKind::Process);
        assert_eq!(SpecKind::from_uri(&"file:///a/b.pbes".parse().unwrap()), SpecKind::Pbes);
        assert_eq!(SpecKind::from_uri(&"file:///a/b.pres".parse().unwrap()), SpecKind::Pres);
        assert_eq!(SpecKind::from_uri(&"file:///a/b".parse().unwrap()), SpecKind::Process);
        assert_eq!(SpecKind::from_uri(&"untitled:Untitled-1".parse().unwrap()), SpecKind::Process);
    }
}
