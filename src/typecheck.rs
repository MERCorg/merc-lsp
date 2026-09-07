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
//!
//! [`typecheck_pbes`]/[`typecheck_pres`]/[`typecheck_modal`] are the PBES/PRES/modal-formula
//! counterparts, each checking their own whole specification (`glob`/equations/`init` for PBES and
//! PRES; `act` declarations and the formula itself for a modal specification) the same way.

use merc_syntax::UntypedPbes;
use merc_syntax::UntypedPres;
use merc_syntax::UntypedProcessSpecification;
use merc_syntax::UntypedStateFrmSpec;
use merc_typecheck::ModalError;
use merc_typecheck::ModalSpecification;
use merc_typecheck::PbesError;
use merc_typecheck::PbesSpecification;
use merc_typecheck::PresError;
use merc_typecheck::PresSpecification;
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

/// As [`TypecheckOutcome`], for a PRES document.
pub enum PresTypecheckOutcome {
    Ok(Box<PresSpecification>),
    Error(PresError),
    Internal(String),
}

/// As [`TypecheckOutcome`], for a modal (mu-calculus) formula document.
pub enum ModalTypecheckOutcome {
    Ok(Box<ModalSpecification>),
    Error(ModalError),
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

/// Type checks `spec`, off the async executor.
///
/// Takes the already-parsed `UntypedPbes` by value, same as [`typecheck`] — `UntypedPbes` now
/// derives `Clone` upstream, so the caller can hand this a cheap clone of the copy
/// `document.parsed` holds (for `symbols`/`semantic_tokens`) instead of re-parsing `text` here.
pub async fn typecheck_pbes(spec: UntypedPbes) -> PbesTypecheckOutcome {
    match tokio::task::spawn_blocking(move || PbesSpecification::from_untyped(spec)).await {
        Ok(Ok(checked)) => PbesTypecheckOutcome::Ok(Box::new(checked)),
        Ok(Err(error)) => PbesTypecheckOutcome::Error(error),
        Err(join_error) => {
            log::error!("pbes typecheck task failed to join: {join_error}");
            PbesTypecheckOutcome::Internal(format!("internal error: typecheck task did not complete ({join_error})"))
        }
    }
}

/// As [`typecheck_pbes`], for a PRES — same reasoning throughout, just against
/// [`PresSpecification::from_untyped`].
pub async fn typecheck_pres(spec: UntypedPres) -> PresTypecheckOutcome {
    match tokio::task::spawn_blocking(move || PresSpecification::from_untyped(spec)).await {
        Ok(Ok(checked)) => PresTypecheckOutcome::Ok(Box::new(checked)),
        Ok(Err(error)) => PresTypecheckOutcome::Error(error),
        Err(join_error) => {
            log::error!("pres typecheck task failed to join: {join_error}");
            PresTypecheckOutcome::Internal(format!("internal error: typecheck task did not complete ({join_error})"))
        }
    }
}

/// As [`typecheck_pbes`], for a modal (mu-calculus) formula — same reasoning, just against
/// [`ModalSpecification::from_untyped`].
pub async fn typecheck_modal(spec: UntypedStateFrmSpec) -> ModalTypecheckOutcome {
    match tokio::task::spawn_blocking(move || ModalSpecification::from_untyped(spec)).await {
        Ok(Ok(checked)) => ModalTypecheckOutcome::Ok(Box::new(checked)),
        Ok(Err(error)) => ModalTypecheckOutcome::Error(error),
        Err(join_error) => {
            log::error!("modal typecheck task failed to join: {join_error}");
            ModalTypecheckOutcome::Internal(format!("internal error: typecheck task did not complete ({join_error})"))
        }
    }
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

    async fn pbes_specification_for(text: &str) -> UntypedPbes {
        match parse(SpecKind::Pbes, text.to_string()).await {
            ParseOutcome::Ok(Specification::Pbes(spec)) => *spec,
            _ => panic!("fixture failed to parse"),
        }
    }

    async fn pres_specification_for(text: &str) -> UntypedPres {
        match parse(SpecKind::Pres, text.to_string()).await {
            ParseOutcome::Ok(Specification::Pres(spec)) => *spec,
            _ => panic!("fixture failed to parse"),
        }
    }

    async fn modal_specification_for(text: &str) -> UntypedStateFrmSpec {
        match parse(SpecKind::Modal, text.to_string()).await {
            ParseOutcome::Ok(Specification::Modal(spec)) => *spec,
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
        let spec = pbes_specification_for("pbes mu X = true;\ninit X;").await;
        match typecheck_pbes(spec).await {
            PbesTypecheckOutcome::Ok(_) => {}
            PbesTypecheckOutcome::Error(error) => panic!("unexpected type error: {error}"),
            PbesTypecheckOutcome::Internal(message) => panic!("unexpected internal error: {message}"),
        }
    }

    #[tokio::test]
    async fn reports_type_error_for_ill_typed_pbes() {
        // `Y` is never declared as a propositional-variable equation.
        let spec = pbes_specification_for("pbes mu X = Y;\ninit X;").await;
        match typecheck_pbes(spec).await {
            PbesTypecheckOutcome::Ok(_) => panic!("expected a type error"),
            PbesTypecheckOutcome::Error(_) => {}
            PbesTypecheckOutcome::Internal(message) => panic!("unexpected internal error: {message}"),
        }
    }

    #[tokio::test]
    async fn typechecks_well_formed_pres() {
        let spec = pres_specification_for("pres mu X = true;\ninit X;").await;
        match typecheck_pres(spec).await {
            PresTypecheckOutcome::Ok(_) => {}
            PresTypecheckOutcome::Error(error) => panic!("unexpected type error: {error}"),
            PresTypecheckOutcome::Internal(message) => panic!("unexpected internal error: {message}"),
        }
    }

    #[tokio::test]
    async fn reports_type_error_for_ill_typed_pres() {
        // `Y` is never declared as a propositional-variable equation.
        let spec = pres_specification_for("pres mu X = Y;\ninit X;").await;
        match typecheck_pres(spec).await {
            PresTypecheckOutcome::Ok(_) => panic!("expected a type error"),
            PresTypecheckOutcome::Error(_) => {}
            PresTypecheckOutcome::Internal(message) => panic!("unexpected internal error: {message}"),
        }
    }

    #[tokio::test]
    async fn typechecks_well_formed_modal_formula() {
        let spec = modal_specification_for("act a: Nat;\nform nu X . [a(0)]X;").await;
        match typecheck_modal(spec).await {
            ModalTypecheckOutcome::Ok(_) => {}
            ModalTypecheckOutcome::Error(error) => panic!("unexpected type error: {error}"),
            ModalTypecheckOutcome::Internal(message) => panic!("unexpected internal error: {message}"),
        }
    }

    #[tokio::test]
    async fn reports_type_error_for_ill_typed_modal_formula() {
        // `b` is never declared as an action.
        let spec = modal_specification_for("act a: Nat;\nform nu X . [b(0)]X;").await;
        match typecheck_modal(spec).await {
            ModalTypecheckOutcome::Ok(_) => panic!("expected a type error"),
            ModalTypecheckOutcome::Error(_) => {}
            ModalTypecheckOutcome::Internal(message) => panic!("unexpected internal error: {message}"),
        }
    }
}
