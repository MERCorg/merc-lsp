//! An off-executor wrapper around `merc_typecheck`'s whole-process-specification entry point.
//!
//! Mirrors [`crate::parse`] in shape and reasoning: type checking is synchronous and CPU-bound,
//! so it's dispatched onto a blocking thread rather than run inline. See `parse.rs`'s module doc
//! comment for why this no longer wraps the check itself in a panic guard.
//!
//! Checks the data specification *and* the `act`/`proc`/`glob`/`init` declarations built on top
//! of it (`ProcessSpecification`, not just `DataSpecification`) — so a diagnostic can come from an
//! action argument, a process instantiation, or `init` itself, not only from an equation.
//! Diagnostics from this module are still tagged with a distinct `source` (see
//! [`crate::diagnostics`]): communication sort-compatibility isn't checked yet (see the
//! `merc_typecheck` crate README), so "no errors" here is not a full guarantee.

use merc_syntax::UntypedPbes;
use merc_syntax::UntypedProcessSpecification;
use merc_typecheck::PbesError;
use merc_typecheck::PbesSpecification;
use merc_typecheck::ProcessError;
use merc_typecheck::ProcessSpecification;

/// The result of attempting to type check a document's whole process specification.
///
/// `Ok` is boxed since `ProcessSpecification` is far larger than the other variants (it carries
/// the whole checked data specification); without it every `TypecheckOutcome` would pay for its
/// worst-case size — mirrors [`crate::parse::ParseOutcome`]'s own `Ok(Box<..>)`.
pub enum TypecheckOutcome {
    /// The checked specification — kept around (rather than discarded) since [`crate::hover`],
    /// [`crate::goto_definition`], and [`crate::inlay_hints`] all need its span-keyed typing info.
    Ok(Box<ProcessSpecification>),
    Error(ProcessError),
    /// The blocking task doing the check didn't complete — see
    /// [`crate::parse::ParseOutcome::Internal`]'s doc comment; the same reasoning applies here.
    Internal(String),
}

/// As [`TypecheckOutcome`], for a PBES document.
pub enum PbesTypecheckOutcome {
    Ok(Box<PbesSpecification>),
    Error(PbesError),
    Internal(String),
}

/// Type checks `spec`, off the async executor.
///
/// Takes `spec` by value (rather than borrowing) because the work is moved onto a blocking thread
/// via [`tokio::task::spawn_blocking`], which requires a `'static` closure — same reason
/// [`crate::parse::parse`] takes `text: String` by value. The caller clones it out of the
/// document's parsed AST (kept separately so `symbols`/`semantic_tokens` keep working even when
/// this fails — see `PLAN.md`).
pub async fn typecheck(spec: UntypedProcessSpecification) -> TypecheckOutcome {
    match tokio::task::spawn_blocking(move || ProcessSpecification::from_untyped(spec)).await {
        Ok(Ok(checked)) => TypecheckOutcome::Ok(Box::new(checked)),
        Ok(Err(error)) => TypecheckOutcome::Error(error),
        Err(join_error) => {
            log::error!("typecheck task failed to join: {join_error}");
            TypecheckOutcome::Internal(format!("internal error: typecheck task did not complete ({join_error})"))
        }
    }
}

/// Type checks `text` as a PBES, off the async executor.
///
/// Unlike [`typecheck`], this takes the document's raw `text` and re-parses it internally rather
/// than an already-parsed `UntypedPbes`: `UntypedPbes` doesn't derive `Clone` upstream (unlike
/// `UntypedProcessSpecification`), so there is no cheap way to keep the copy `document.parsed`
/// holds (for `symbols`/`semantic_tokens`) *and* hand a second one to
/// `PbesSpecification::from_untyped`, which consumes its argument. Re-parsing is the simplest way
/// around that without an upstream change — `text` has already parsed successfully once by the
/// time this is called (see `backend::analyze`), so the re-parse is not expected to fail; if it
/// somehow does, that is reported the same way a join failure is, not treated as a type error.
pub async fn typecheck_pbes(text: String) -> PbesTypecheckOutcome {
    let outcome = tokio::task::spawn_blocking(move || match UntypedPbes::parse(&text) {
        Ok(spec) => PbesSpecification::from_untyped(spec).map_err(TypecheckPbesError::Type),
        Err(error) => Err(TypecheckPbesError::Reparse(error)),
    })
    .await;
    match outcome {
        Ok(Ok(checked)) => PbesTypecheckOutcome::Ok(Box::new(checked)),
        Ok(Err(TypecheckPbesError::Type(error))) => PbesTypecheckOutcome::Error(error),
        Ok(Err(TypecheckPbesError::Reparse(error))) => {
            log::error!("PBES re-parse for type checking unexpectedly failed: {error}");
            PbesTypecheckOutcome::Internal(format!("internal error: PBES re-parse failed unexpectedly: {error}"))
        }
        Err(join_error) => {
            log::error!("pbes typecheck task failed to join: {join_error}");
            PbesTypecheckOutcome::Internal(format!("internal error: typecheck task did not complete ({join_error})"))
        }
    }
}

/// [`typecheck_pbes`]'s two failure modes, kept distinct so the re-parse case (should not happen,
/// logged loudly) is never confused with an ordinary [`PbesError`] (an expected, user-facing
/// outcome).
enum TypecheckPbesError {
    Reparse(merc_utilities::MercError),
    Type(PbesError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::ParseOutcome;
    use crate::parse::SpecKind;
    use crate::parse::Specification;
    use crate::parse::parse;

    async fn process_specification_for(text: &str) -> UntypedProcessSpecification {
        match parse(SpecKind::Process, text.to_string()).await {
            ParseOutcome::Ok(Specification::Process(spec)) => *spec,
            _ => panic!("fixture failed to parse"),
        }
    }

    #[tokio::test]
    async fn typechecks_well_formed_specification() {
        let spec = process_specification_for("sort D;\ncons c: D;\ninit delta;").await;
        match typecheck(spec).await {
            TypecheckOutcome::Ok(_) => {}
            TypecheckOutcome::Error(error) => panic!("unexpected type error: {error}"),
            TypecheckOutcome::Internal(message) => panic!("unexpected internal error: {message}"),
        }
    }

    #[tokio::test]
    async fn reports_type_error_for_ill_typed_data_specification() {
        // `undeclared` is not bound by any `var`, `map`, or `cons` declaration.
        let spec = process_specification_for("map f: Bool;\neqn f = undeclared;\ninit delta;").await;
        match typecheck(spec).await {
            TypecheckOutcome::Ok(_) => panic!("expected a type error"),
            TypecheckOutcome::Error(_) => {}
            TypecheckOutcome::Internal(message) => panic!("unexpected internal error: {message}"),
        }
    }

    #[tokio::test]
    async fn reports_type_error_for_ill_typed_process() {
        // `a` is not declared as an action anywhere.
        let spec = process_specification_for("init a;").await;
        match typecheck(spec).await {
            TypecheckOutcome::Ok(_) => panic!("expected a type error"),
            TypecheckOutcome::Error(_) => {}
            TypecheckOutcome::Internal(message) => panic!("unexpected internal error: {message}"),
        }
    }

    #[tokio::test]
    async fn typechecks_well_formed_pbes() {
        let text = "pbes mu X = true;\ninit X;".to_string();
        match typecheck_pbes(text).await {
            PbesTypecheckOutcome::Ok(_) => {}
            PbesTypecheckOutcome::Error(error) => panic!("unexpected type error: {error}"),
            PbesTypecheckOutcome::Internal(message) => panic!("unexpected internal error: {message}"),
        }
    }

    #[tokio::test]
    async fn reports_type_error_for_ill_typed_pbes() {
        // `Y` is never declared as a propositional-variable equation.
        let text = "pbes mu X = Y;\ninit X;".to_string();
        match typecheck_pbes(text).await {
            PbesTypecheckOutcome::Ok(_) => panic!("expected a type error"),
            PbesTypecheckOutcome::Error(_) => {}
            PbesTypecheckOutcome::Internal(message) => panic!("unexpected internal error: {message}"),
        }
    }
}
