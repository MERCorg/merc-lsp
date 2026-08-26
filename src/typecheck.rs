//! A panic-safe, off-executor wrapper around `merc_typecheck`'s data-specification entry point.
//!
//! Mirrors [`crate::parse`] in shape and reasoning: type checking is synchronous and CPU-bound
//! (so it's dispatched onto a blocking thread) and, like the AST-building layer `parse.rs`
//! guards, `merc_typecheck` is not proven panic-free on arbitrary well-parsed-but-ill-typed
//! input, so a panic here must not be allowed to take the whole server down either.
//!
//! Only the data-specification subtree is checked (see PLAN.md §4): no whole-specification
//! (`UntypedProcessSpecification`) typecheck entry point exists upstream yet, so process/action/
//! PBES bodies are currently untyped as far as `merc-lsp` is concerned. Diagnostics from this
//! module are tagged with a distinct `source` (see [`crate::diagnostics`]) precisely so that gap
//! doesn't silently read as "no errors".

use std::panic::AssertUnwindSafe;

use merc_syntax::UntypedDataSpecification;
use merc_typecheck::DataSpecification;
use merc_typecheck::WellTypedError;

use crate::parse::panic_message;

/// The result of attempting to type check a document's data specification.
pub enum TypecheckOutcome {
    Ok,
    Error(WellTypedError),
    /// The type checker panicked. Carries a message suitable for a diagnostic; the panic itself
    /// has already been caught and logged.
    Internal(String),
}

/// Type checks `data_specification`, off the async executor and guarded against panics.
///
/// Takes `data_specification` by value (rather than borrowing) because the work is moved onto a
/// blocking thread via [`tokio::task::spawn_blocking`], which requires a `'static` closure —
/// same reason [`crate::parse::parse`] takes `text: String` by value.
pub async fn typecheck(data_specification: UntypedDataSpecification) -> TypecheckOutcome {
    match tokio::task::spawn_blocking(move || typecheck_catching_panics(data_specification)).await {
        Ok(outcome) => outcome,
        Err(join_error) => {
            log::error!("typecheck task failed to join: {join_error}");
            TypecheckOutcome::Internal("internal error: typecheck task did not complete".to_string())
        }
    }
}

fn typecheck_catching_panics(data_specification: UntypedDataSpecification) -> TypecheckOutcome {
    match std::panic::catch_unwind(AssertUnwindSafe(|| DataSpecification::from_untyped(data_specification))) {
        Ok(Ok(_checked)) => TypecheckOutcome::Ok,
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
    use crate::parse::parse;

    async fn data_specification_for(text: &str) -> UntypedDataSpecification {
        match parse(text.to_string()).await {
            ParseOutcome::Ok(spec) => spec.data_specification.clone(),
            _ => panic!("fixture failed to parse"),
        }
    }

    #[tokio::test]
    async fn typechecks_well_formed_data_specification() {
        let data_specification = data_specification_for("sort D;\ncons c: D;\ninit delta;").await;
        match typecheck(data_specification).await {
            TypecheckOutcome::Ok => {}
            TypecheckOutcome::Error(error) => panic!("unexpected type error: {error}"),
            TypecheckOutcome::Internal(message) => panic!("unexpected internal error: {message}"),
        }
    }

    #[tokio::test]
    async fn reports_type_error_for_ill_typed_specification() {
        // `undeclared` is not bound by any `var`, `map`, or `cons` declaration.
        let data_specification = data_specification_for("map f: Bool;\neqn f = undeclared;").await;
        match typecheck(data_specification).await {
            TypecheckOutcome::Ok => panic!("expected a type error"),
            TypecheckOutcome::Error(_) => {}
            TypecheckOutcome::Internal(message) => panic!("unexpected internal error: {message}"),
        }
    }
}
