//! `textDocument/inlayHint`: shows the name or sort of every call-argument expression (action
//! instance, process instantiation, mapping/function application, structured-sort constructor
//! application) and every equation-LHS pattern variable.
//!
//! One rule covers every case: if the callee names that position (a process parameter's name, a
//! struct field's name), show a `name: ` hint *before* the argument. Otherwise — no name is
//! available, only a sort (action arguments, mapping arguments, an equation's LHS pattern
//! variable) — show a `: Sort` hint *after* it. See `PLAN.md`'s worked examples.
//!
//! `merc_syntax::Traverse` doesn't cross between node types (a `ProcessExpr` traversal doesn't
//! descend into the `DataExpr`s inside its actions/conditions/`dist` weights — see `symbols.rs`'s
//! module doc comment), so this walks a process body's `ProcessExpr` tree by hand for its
//! `DataExpr`-bearing fields, then hands each one to [`walk_applications`], which *does* use
//! `Traverse` (`DataExpr` recurses fully into itself) to find every nested mapping/struct
//! application within it, including the field's own top level.

use std::ops::ControlFlow;

use lsp_types::InlayHint;
use lsp_types::InlayHintKind;
use lsp_types::InlayHintLabel;
use lsp_types::Position;
use lsp_types::Range;
use merc_syntax::DataExpr;
use merc_syntax::DataExprKind;
use merc_syntax::ProcessExpr;
use merc_syntax::ProcessExprKind;
use merc_syntax::Span;
use merc_syntax::SortExpression;
use merc_syntax::SortDecl;
use merc_syntax::SortExpressionKind;
use merc_syntax::Traverse;
use merc_typecheck::ProcessSpecification;
use merc_typecheck::TypingInfo;

use crate::convert::LineIndex;

/// Everything threaded unchanged through every helper below, bundled so none of them has to spell
/// out five parameters just to pass them on. `sort_declarations` is the *raw*, un-type-checked
/// specification's own sort declarations — see
/// [`crate::document::Document::parsed_process_specification`] for why struct field names have to
/// come from there rather than from `spec` itself. `hints` is the accumulator every emitting
/// helper pushes into.
struct Ctx<'a> {
    text: &'a str,
    line_index: &'a LineIndex,
    spec: &'a ProcessSpecification,
    sort_declarations: &'a [SortDecl],
    typing_info: &'a TypingInfo,
    hints: Vec<InlayHint>,
}

/// Builds every inlay hint for `spec` that falls within `range`. See [`Ctx`] for the other
/// parameters.
pub fn inlay_hints(
    text: &str,
    line_index: &LineIndex,
    spec: &ProcessSpecification,
    sort_declarations: &[SortDecl],
    typing_info: &TypingInfo,
    range: Range,
) -> Vec<InlayHint> {
    let mut ctx = Ctx {
        text,
        line_index,
        spec,
        sort_declarations,
        typing_info,
        hints: Vec::new(),
    };

    for decl in spec.process_declarations() {
        walk_process_expr(&decl.body, &mut ctx);
    }
    if let Some(init) = spec.init() {
        walk_process_expr(init, &mut ctx);
    }

    for eqn_spec in &spec.data_specification().data_specification().equation_declarations {
        for eqn in &eqn_spec.node.equations {
            walk_applications(&eqn.lhs, &mut ctx);
            walk_applications(&eqn.rhs, &mut ctx);
            if let Some(condition) = &eqn.condition {
                walk_applications(condition, &mut ctx);
            }
        }
    }

    ctx.hints.retain(|hint| within_range(hint.position, range));
    ctx.hints
}

/// Walks every `ProcessExpr` in `expr`'s subtree, picking out each node's `DataExpr`-bearing
/// fields — the ones `Traverse` itself won't reach — and hinting them.
fn walk_process_expr(expr: &ProcessExpr, ctx: &mut Ctx) {
    expr.visit::<(), _>(|node| {
        match &node.node {
            ProcessExprKind::Action(name, args) => emit_call_hints(&name.node, args, ctx),
            ProcessExprKind::Id(_, assignments) => {
                // The assignment's key (`n` in `P(n = x)`) is already an explicit name in the
                // source; only its value can contain a nested call worth hinting.
                for assignment in assignments {
                    walk_applications(&assignment.node.expr, ctx);
                }
            }
            ProcessExprKind::Dist { expr: weight, .. } => walk_applications(weight, ctx),
            ProcessExprKind::Condition { condition, .. } => walk_applications(condition, ctx),
            ProcessExprKind::At { operand, .. } => walk_applications(operand, ctx),
            _ => {}
        }
        ControlFlow::Continue(())
    });
}

/// Hints `args`, the arguments of an action instance or a positional process instantiation named
/// `name` — a `name: ` prefix per argument when `name` is a process with that many parameters
/// (their names), otherwise a `: Sort` suffix (an action never names its arguments). Then recurses
/// into each argument for any nested mapping/struct application of its own.
fn emit_call_hints(name: &str, args: &[DataExpr], ctx: &mut Ctx) {
    let param_names = process_param_names(ctx.spec, name, args.len());
    for (i, argument) in args.iter().enumerate() {
        let field_name = param_names.as_ref().and_then(|names| names.get(i).copied());
        push_hint(argument, field_name, sort_of(ctx.typing_info, &argument.span), ctx);
        walk_applications(argument, ctx);
    }
}

/// Finds every mapping/struct-constructor application anywhere in `expr`'s subtree (including
/// `expr` itself) and hints its arguments — a `name: ` prefix per argument when the applied
/// function is a struct constructor naming that field, otherwise a `: Sort` suffix (a mapping
/// never names its arguments). Covers an equation's LHS pattern the same way: `f(x)` on the left
/// of `eqn f(x) = ...;` is itself an application `walk_applications` finds like any other.
fn walk_applications(expr: &DataExpr, ctx: &mut Ctx) {
    expr.visit::<(), _>(|node| {
        if let DataExprKind::Application { function, arguments } = &node.node {
            let callee = match &function.node {
                DataExprKind::Id(name) => Some(name.as_str()),
                DataExprKind::Resolved(name, _) => Some(name.as_str()),
                _ => None,
            };
            let field_names = callee.and_then(|name| struct_field_names(ctx.sort_declarations, name, arguments.len()));
            for (i, argument) in arguments.iter().enumerate() {
                let field_name = field_names.as_ref().and_then(|names| names.get(i).copied().flatten());
                push_hint(argument, field_name, sort_of(ctx.typing_info, &argument.span), ctx);
            }
        }
        ControlFlow::Continue(())
    });
}

/// The parameter names of the `proc` declaration named `name` with exactly `arity` parameters, if
/// one exists — `None` for an action (or anything else that isn't such a process), which falls
/// back to the sort-only hint.
fn process_param_names<'a>(spec: &'a ProcessSpecification, name: &str, arity: usize) -> Option<Vec<&'a str>> {
    spec.process_declarations()
        .iter()
        .find(|decl| decl.identifier == name && decl.params.len() == arity)
        .map(|decl| decl.params.iter().map(|param| param.identifier.as_str()).collect())
}

/// The field names of the struct constructor named `name` with exactly `arity` fields, if one is
/// declared — each entry `None` for an unnamed field, which falls back to the sort-only hint for
/// just that one argument (a struct can be partially named).
fn struct_field_names<'a>(sort_declarations: &'a [SortDecl], name: &str, arity: usize) -> Option<Vec<Option<&'a str>>> {
    sort_declarations.iter().find_map(|decl| {
        let SortExpressionKind::Struct { inner } = &decl.expr.as_ref()?.node else {
            return None;
        };
        inner
            .iter()
            .find(|constructor| constructor.name == name && constructor.args.len() == arity)
            .map(|constructor| constructor.args.iter().map(|(field, _)| field.as_deref()).collect())
    })
}

/// The sort [`TypingInfo`] recorded for the node whose span is exactly `span` — an argument's own
/// span always has one recorded exactly, since every checked argument is a whole `DataExpr` node
/// in its own right.
fn sort_of<'a>(typing_info: &'a TypingInfo, span: &Span) -> Option<&'a SortExpression> {
    typing_info
        .nodes()
        .iter()
        .find(|node| node.span.start == span.start && node.span.end == span.end)
        .and_then(|node| node.sort.as_ref())
}

/// Emits one hint for `argument`: a `name: ` prefix at its start when `field_name` is known,
/// otherwise a `: Sort` suffix at its end when `sort` is known. Emits nothing when neither is
/// available (an argument `TypingInfo` has no node for at all — shouldn't arise for a checked
/// specification, but degrading to no hint is safer than a wrong one).
fn push_hint(argument: &DataExpr, field_name: Option<&str>, sort: Option<&SortExpression>, ctx: &mut Ctx) {
    let hint = match field_name {
        Some(name) => {
            let position = ctx.line_index.position(ctx.text, argument.span.start);
            build_hint(position, format!("{name}:"), InlayHintKind::PARAMETER, false, true)
        }
        None => {
            let Some(sort) = sort else { return };
            let position = ctx.line_index.position(ctx.text, argument.span.end);
            build_hint(position, format!(": {sort}"), InlayHintKind::TYPE, true, false)
        }
    };
    ctx.hints.push(hint);
}

fn build_hint(position: Position, label: String, kind: InlayHintKind, padding_left: bool, padding_right: bool) -> InlayHint {
    InlayHint {
        position,
        label: InlayHintLabel::String(label),
        kind: Some(kind),
        text_edits: None,
        tooltip: None,
        padding_left: Some(padding_left),
        padding_right: Some(padding_right),
        data: None,
    }
}

fn within_range(position: Position, range: Range) -> bool {
    let position = (position.line, position.character);
    let start = (range.start.line, range.start.character);
    let end = (range.end.line, range.end.character);
    start <= position && position <= end
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::ParseOutcome;
    use crate::parse::SpecKind;
    use crate::parse::Specification;
    use crate::parse::parse;
    use crate::typecheck::TypecheckOutcome;
    use crate::typecheck::typecheck;

    async fn hints_for(text: &str) -> (Vec<InlayHint>, LineIndex) {
        let spec = match parse(SpecKind::Process, text.to_string()).await {
            ParseOutcome::Ok(Specification::Process(spec)) => *spec,
            _ => panic!("fixture failed to parse"),
        };
        // Struct field names only live on the raw, pre-typecheck AST (see
        // `Document::parsed_process_specification`'s doc comment) — cloned out before `spec` is
        // moved into `typecheck` below.
        let sort_declarations = spec.data_specification.sort_declarations.clone();
        let mut checked = match typecheck(spec).await {
            TypecheckOutcome::Ok(checked) => checked,
            TypecheckOutcome::Error(error) => panic!("fixture failed to typecheck: {error}"),
            TypecheckOutcome::Internal(message) => panic!("internal error typechecking fixture: {message}"),
        };
        let line_index = LineIndex::new(text);
        let typing_info = checked.typing_info();
        let whole_document = Range {
            start: Position { line: 0, character: 0 },
            end: Position { line: u32::MAX, character: u32::MAX },
        };
        let hints = inlay_hints(text, &line_index, &checked, &sort_declarations, &typing_info, whole_document);
        (hints, line_index)
    }

    fn label(hint: &InlayHint) -> &str {
        let InlayHintLabel::String(label) = &hint.label else {
            panic!("expected a string label");
        };
        label
    }

    #[tokio::test]
    async fn action_argument_gets_a_type_suffix_hint() {
        let text = "act a: Nat;\nproc P(n: Nat) = a(n);\ninit P(1);";
        let (hints, line_index) = hints_for(text).await;

        let n_end = text.find("n);").unwrap() + 1;
        let expected = line_index.position(text, n_end);
        let hint = hints.iter().find(|h| h.position == expected).expect("expected a hint after 'n'");
        assert_eq!(label(hint), ": Nat");
        assert_eq!(hint.kind, Some(InlayHintKind::TYPE));
    }

    #[tokio::test]
    async fn process_instantiation_argument_gets_a_name_prefix_hint() {
        let text = "proc P(n: Nat) = delta;\ninit P(1);";
        let (hints, line_index) = hints_for(text).await;

        let one_start = text.rfind('1').unwrap();
        let expected = line_index.position(text, one_start);
        let hint = hints.iter().find(|h| h.position == expected).expect("expected a hint before '1'");
        assert_eq!(label(hint), "n:");
        assert_eq!(hint.kind, Some(InlayHintKind::PARAMETER));
    }

    #[tokio::test]
    async fn struct_field_gets_a_name_prefix_hint() {
        let text = "sort S = struct s(n: Nat);\nact a: S;\nproc P = a(s(5));\ninit P;";
        let (hints, line_index) = hints_for(text).await;

        let five_start = text.find('5').unwrap();
        let expected = line_index.position(text, five_start);
        let hint = hints.iter().find(|h| h.position == expected).expect("expected a hint before '5'");
        assert_eq!(label(hint), "n:");
        assert_eq!(hint.kind, Some(InlayHintKind::PARAMETER));
    }

    #[tokio::test]
    async fn equation_lhs_pattern_variable_gets_a_type_suffix_hint() {
        let text = "sort D;\nmap f: D -> D;\nvar x: D;\neqn f(x) = x;\ninit delta;";
        let (hints, line_index) = hints_for(text).await;

        let x_end = text.find("x) = x;").unwrap() + 1;
        let expected = line_index.position(text, x_end);
        let hint = hints.iter().find(|h| h.position == expected).expect("expected a hint after the LHS 'x'");
        assert_eq!(label(hint), ": D");
        assert_eq!(hint.kind, Some(InlayHintKind::TYPE));
    }
}
