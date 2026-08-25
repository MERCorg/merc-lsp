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

use std::panic::AssertUnwindSafe;

use merc_syntax::UntypedProcessSpecification;
use merc_utilities::MercError;

/// The result of attempting to parse a document: either the AST, a normal parse error, or an
/// internal error (a panic in the AST-building layer).
///
/// `Ok` is boxed since `UntypedProcessSpecification` is far larger than the other variants;
/// without it every `ParseOutcome` (including the common `ParseError`/`Internal` cases) would pay
/// for the AST's worst-case size.
pub enum ParseOutcome {
    Ok(Box<UntypedProcessSpecification>),
    ParseError(MercError),
    /// The parser panicked while building the AST. Carries a message suitable for a diagnostic;
    /// the panic itself has already been caught and logged.
    Internal(String),
}

/// Parses `text` as a full mCRL2 process specification, off the async executor and guarded
/// against panics.
///
/// Takes `text` by value (rather than borrowing) because the work is moved onto a blocking
/// thread via [`tokio::task::spawn_blocking`], which requires a `'static` closure.
pub async fn parse(text: String) -> ParseOutcome {
    // `spawn_blocking` also gets parsing off the runtime's async worker threads, so a large or
    // pathological input can't stall other documents' requests.
    match tokio::task::spawn_blocking(move || parse_catching_panics(&text)).await {
        Ok(outcome) => outcome,
        Err(join_error) => {
            // The blocking task itself panicked in a way `catch_unwind` below didn't intercept
            // (e.g. it was cancelled), or the runtime is shutting down.
            log::error!("parse task failed to join: {join_error}");
            ParseOutcome::Internal("internal error: parser task did not complete".to_string())
        }
    }
}

fn parse_catching_panics(text: &str) -> ParseOutcome {
    match std::panic::catch_unwind(AssertUnwindSafe(|| UntypedProcessSpecification::parse(text))) {
        Ok(Ok(spec)) => ParseOutcome::Ok(Box::new(spec)),
        Ok(Err(error)) => ParseOutcome::ParseError(error),
        Err(panic) => {
            let message = panic_message(&panic);
            log::error!("panic while parsing document: {message}");
            ParseOutcome::Internal(format!("internal parser error: {message}"))
        }
    }
}

/// Best-effort extraction of a human-readable message from a caught panic payload.
fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
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
        match parse(text).await {
            ParseOutcome::Ok(_) => {}
            ParseOutcome::ParseError(error) => panic!("unexpected parse error: {error}"),
            ParseOutcome::Internal(message) => panic!("unexpected internal error: {message}"),
        }
    }

    #[tokio::test]
    async fn reports_parse_error_for_malformed_specification() {
        let text = "sort D".to_string(); // missing terminating ';'
        match parse(text).await {
            ParseOutcome::Ok(_) => panic!("expected a parse error"),
            ParseOutcome::ParseError(_) => {}
            ParseOutcome::Internal(message) => panic!("unexpected internal error: {message}"),
        }
    }
}
