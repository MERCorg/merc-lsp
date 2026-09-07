//! An off-executor wrapper around `merc_syntax`'s parse entry points.
//!
//! Parsing is synchronous and CPU-bound, so it is dispatched onto a blocking thread rather than
//! run inline on the async runtime, keeping a large or pathological input from stalling other
//! documents' requests. No panic guard around the parse itself: `merc_syntax`'s AST-building layer
//! used to carry `unwrap`/`expect`/`unreachable!` sites reachable from a malformed-but-
//! grammatically-valid input, which this module used to catch with `catch_unwind` so a parser bug
//! degraded to an "internal error" diagnostic instead of taking a request down; that class of bug
//! is fixed upstream now, so a panic here is a genuine, reproducible bug in this server, not
//! something a client retry (or a user re-typing the same input) can route around — see
//! `ParseOutcome::Internal`'s doc comment for what still happens if one occurs anyway, and
//! `vscode-client`'s `merc-lsp.restartServer` command for recovering the running server without
//! reloading the whole editor window.
//!
//! Four document kinds are supported ([`SpecKind`]): plain mCRL2 process specifications, PBES
//! (parameterised boolean equation systems), PRES (parameterised real equation systems), and
//! modal (mu-calculus) state formulas (`.mcf`). The kind is decided purely from the document's
//! file extension (see [`SpecKind::from_uri`]) — `merc_syntax` exposes four distinct,
//! structurally incompatible grammar entry points (`MCRL2Spec`/`PbesSpec`/`PresSpec`/
//! `StateFrmSpec`) with no reliable way to tell them apart from content alone short of trying all
//! four and guessing from whichever parse succeeds, which would make parse errors on a genuinely
//! broken file misleading (which grammar's error should be shown?).

use lsp_types::Url;
use merc_syntax::UntypedPbes;
use merc_syntax::UntypedPres;
use merc_syntax::UntypedProcessSpecification;
use merc_syntax::UntypedStateFrmSpec;
use merc_typecheck::disambiguate_process_specification;
use merc_utilities::MercError;

/// Which of `merc_syntax`'s three top-level grammar entry points a document should be parsed
/// with, decided from its file extension.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SpecKind {
    /// A plain `.mcrl2` process specification — the default for any extension other than `.pbes`/
    /// `.pres`/`.mcf` (including no extension at all, e.g. an unsaved buffer), since this is the
    /// kind every existing feature (type checking, hover, goto-def, semantic tokens) is built for.
    Process,
    Pbes,
    Pres,
    /// A `.mcf` modal (mu-calculus) state formula.
    Modal,
}

impl SpecKind {
    /// Picks a [`SpecKind`] from `uri`'s file extension: `.pbes`, `.pres`, and `.mcf`
    /// (case-sensitively, matching the extensions registered in `vscode-client/package.json`)
    /// select PBES/PRES/Modal; everything else falls back to [`SpecKind::Process`].
    pub fn from_uri(uri: &Url) -> SpecKind {
        let path = uri.path();
        if path.ends_with(".pbes") {
            SpecKind::Pbes
        } else if path.ends_with(".pres") {
            SpecKind::Pres
        } else if path.ends_with(".mcf") {
            SpecKind::Modal
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
    Modal(Box<UntypedStateFrmSpec>),
}

impl Specification {
    /// The process specification, if `self` is [`Specification::Process`].
    pub fn as_process(&self) -> Option<&UntypedProcessSpecification> {
        match self {
            Specification::Process(spec) => Some(spec),
            Specification::Pbes(_) | Specification::Pres(_) | Specification::Modal(_) => None,
        }
    }

    /// As [`Self::as_process`], for [`Specification::Pbes`] — used the same way, by
    /// [`crate::document::Document::parsed_pbes_specification`]'s struct-field-name lookup for
    /// PBES inlay hints.
    pub fn as_pbes(&self) -> Option<&UntypedPbes> {
        match self {
            Specification::Pbes(spec) => Some(spec),
            Specification::Process(_) | Specification::Pres(_) | Specification::Modal(_) => None,
        }
    }

    /// As [`Self::as_pbes`], for [`Specification::Pres`] — used by
    /// [`crate::document::Document::parsed_pres_specification`]'s struct-field-name lookup for
    /// PRES inlay hints.
    pub fn as_pres(&self) -> Option<&UntypedPres> {
        match self {
            Specification::Pres(spec) => Some(spec),
            Specification::Process(_) | Specification::Pbes(_) | Specification::Modal(_) => None,
        }
    }

    /// As [`Self::as_pbes`], for [`Specification::Modal`] — used by
    /// [`crate::document::Document::parsed_modal_specification`]'s struct-field-name lookup for
    /// modal-formula inlay hints.
    pub fn as_modal(&self) -> Option<&UntypedStateFrmSpec> {
        match self {
            Specification::Modal(spec) => Some(spec),
            Specification::Process(_) | Specification::Pbes(_) | Specification::Pres(_) => None,
        }
    }
}

/// The result of attempting to parse a document: either the AST, a normal parse error, or an
/// internal error.
pub enum ParseOutcome {
    Ok(Specification),
    ParseError(MercError),
    /// The blocking task doing the parse didn't complete — it panicked (a genuine bug; see this
    /// module's doc comment) or the runtime is shutting down. Carries a message suitable for a
    /// diagnostic. `tokio::task::spawn_blocking` isolates the panic to that one task: it does not
    /// take the rest of the server down, so this document just goes quiet (this diagnostic, no
    /// hover/goto-def/inlay-hints on it) rather than the whole connection dying.
    Internal(String),
}

/// Parses `text` as `kind`, off the async executor.
///
/// Takes `text` by value (rather than borrowing) because the work is moved onto a blocking
/// thread via [`tokio::task::spawn_blocking`], which requires a `'static` closure. Also gets
/// parsing off the runtime's async worker threads, so a large or pathological input can't stall
/// other documents' requests.
pub async fn parse(kind: SpecKind, text: String) -> ParseOutcome {
    let outcome = match kind {
        SpecKind::Process => tokio::task::spawn_blocking(move || {
            UntypedProcessSpecification::parse(&text).map(|mut spec| {
                // Reconstructs process-algebra structure the grammar mis-parsed as a data
                // expression (a long `cond -> (...) + cond -> (...) + ...` chain being the
                // motivating case — see `disambiguate_process_specification`'s own doc comment)
                // using only declared action/process names, before anything downstream (semantic
                // tokens, inlay hints, completion, and — via `typecheck::typecheck`, which
                // re-disambiguates idempotently — type checking itself) ever sees this AST.
                // Applied here rather than separately in each consumer so every feature agrees on
                // the same corrected tree.
                disambiguate_process_specification(&mut spec);
                Specification::Process(Box::new(spec))
            })
        })
        .await,
        SpecKind::Pbes => tokio::task::spawn_blocking(move || UntypedPbes::parse(&text).map(|spec| Specification::Pbes(Box::new(spec)))).await,
        SpecKind::Pres => tokio::task::spawn_blocking(move || UntypedPres::parse(&text).map(|spec| Specification::Pres(Box::new(spec)))).await,
        SpecKind::Modal => {
            tokio::task::spawn_blocking(move || UntypedStateFrmSpec::parse(&text).map(|spec| Specification::Modal(Box::new(spec)))).await
        }
    };
    match outcome {
        Ok(Ok(spec)) => ParseOutcome::Ok(spec),
        Ok(Err(error)) => ParseOutcome::ParseError(error),
        Err(join_error) => {
            log::error!("parse task failed to join: {join_error}");
            ParseOutcome::Internal(format!("internal error: parser task did not complete ({join_error})"))
        }
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
        let text = "pres mu X = true;\ninit X;".to_string();
        match parse(SpecKind::Pres, text).await {
            ParseOutcome::Ok(Specification::Pres(_)) => {}
            ParseOutcome::Ok(_) => panic!("expected a Pres specification"),
            ParseOutcome::ParseError(error) => panic!("unexpected parse error: {error}"),
            ParseOutcome::Internal(message) => panic!("unexpected internal error: {message}"),
        }
    }

    #[tokio::test]
    async fn parses_well_formed_modal_formula() {
        let text = "act a: Nat;\nform nu X . [a(0)]X;".to_string();
        match parse(SpecKind::Modal, text).await {
            ParseOutcome::Ok(Specification::Modal(_)) => {}
            ParseOutcome::Ok(_) => panic!("expected a Modal specification"),
            ParseOutcome::ParseError(error) => panic!("unexpected parse error: {error}"),
            ParseOutcome::Internal(message) => panic!("unexpected internal error: {message}"),
        }
    }

    #[test]
    fn spec_kind_from_uri_extension() {
        assert_eq!(SpecKind::from_uri(&"file:///a/b.mcrl2".parse().unwrap()), SpecKind::Process);
        assert_eq!(SpecKind::from_uri(&"file:///a/b.pbes".parse().unwrap()), SpecKind::Pbes);
        assert_eq!(SpecKind::from_uri(&"file:///a/b.pres".parse().unwrap()), SpecKind::Pres);
        assert_eq!(SpecKind::from_uri(&"file:///a/b.mcf".parse().unwrap()), SpecKind::Modal);
        assert_eq!(SpecKind::from_uri(&"file:///a/b".parse().unwrap()), SpecKind::Process);
        assert_eq!(SpecKind::from_uri(&"untitled:Untitled-1".parse().unwrap()), SpecKind::Process);
    }
}
