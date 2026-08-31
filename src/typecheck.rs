//! A panic-safe, off-executor wrapper around `merc_typecheck`'s whole-process-specification entry
//! point.
//!
//! Mirrors [`crate::parse`] in shape and reasoning: type checking is synchronous and CPU-bound
//! (so it's dispatched onto a blocking thread) and, like the AST-building layer `parse.rs`
//! guards, `merc_typecheck` is not proven panic-free on arbitrary well-parsed-but-ill-typed
//! input, so a panic here must not be allowed to take the whole server down either.
//!
//! Checks the data specification *and* the `act`/`proc`/`glob`/`init` declarations built on top
//! of it (`ProcessSpecification`, not just `DataSpecification`) — so a diagnostic can come from an
//! action argument, a process instantiation, or `init` itself, not only from an equation.
//! Diagnostics from this module are still tagged with a distinct `source` (see
//! [`crate::diagnostics`]): communication sort-compatibility isn't checked yet (see the
//! `merc_typecheck` crate README), so "no errors" here is not a full guarantee.

use std::panic::AssertUnwindSafe;

use merc_syntax::UntypedProcessSpecification;
use merc_typecheck::ProcessError;
use merc_typecheck::ProcessSpecification;

use crate::parse::panic_message;

/// The result of attempting to type check a document's whole process specification.
///
/// `Ok` is boxed since `ProcessSpecification` is far larger than the other variants (it carries
/// the whole checked data specification); without it every `TypecheckOutcome` would pay for its
/// worst-case size — mirrors [`crate::parse::ParseOutcome`]'s own `Ok(Box<..>)`.
pub enum TypecheckOutcome {
    /// The checked specification — kept around (rather than discarded) since [`crate::hover`] and
    /// [`crate::goto_definition`] both need its [`DataSpecification`](merc_typecheck::DataSpecification)'s
    /// span-keyed typing info.
    Ok(Box<ProcessSpecification>),
    Error(ProcessError),
    /// The type checker panicked. Carries a message suitable for a diagnostic; the panic itself
    /// has already been caught and logged.
    Internal(String),
}

/// Type checks `spec`, off the async executor and guarded against panics.
///
/// Takes `spec` by value (rather than borrowing) because the work is moved onto a blocking thread
/// via [`tokio::task::spawn_blocking`], which requires a `'static` closure — same reason
/// [`crate::parse::parse`] takes `text: String` by value. The caller clones it out of the
/// document's parsed AST (kept separately so `symbols`/`semantic_tokens` keep working even when
/// this fails — see `PLAN.md`).
pub async fn typecheck(spec: UntypedProcessSpecification) -> TypecheckOutcome {
    match tokio::task::spawn_blocking(move || typecheck_catching_panics(spec)).await {
        Ok(outcome) => outcome,
        Err(join_error) => {
            log::error!("typecheck task failed to join: {join_error}");
            TypecheckOutcome::Internal("internal error: typecheck task did not complete".to_string())
        }
    }
}

fn typecheck_catching_panics(spec: UntypedProcessSpecification) -> TypecheckOutcome {
    match std::panic::catch_unwind(AssertUnwindSafe(|| ProcessSpecification::from_untyped(spec))) {
        Ok(Ok(checked)) => TypecheckOutcome::Ok(Box::new(checked)),
        Ok(Err(error)) => TypecheckOutcome::Error(error),
        Err(panic) => {
            let message = panic_message(&panic);
            log::error!("panic while type checking document: {message}");
            TypecheckOutcome::Internal(format!("internal type checker error: {message}"))
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
}
