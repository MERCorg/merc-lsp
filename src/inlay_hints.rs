//! `textDocument/inlayHint`: shows the name or sort of every call-argument expression (action
//! instance, process instantiation, PBES/PRES propositional-variable instantiation, structured-sort
//! constructor application) and every equation-LHS pattern variable.
//!
//! Two kinds of hint, each restricted to where it's actually informative:
//!
//! - A `name:` hint *before* an argument, whenever the callee names that position (a process
//!   parameter's name, a propositional-variable equation's parameter name, a struct field's name).
//!   This one recurses arbitrarily deep — a struct constructor nested three calls down inside an
//!   argument still gets its fields named — but is skipped when the argument is itself a bare
//!   variable already spelled the same as the name being shown (`P(n)` where `n` is also the
//!   parameter's name would just be noise).
//! - A `: Sort` hint *after* an argument, whenever no name is available. Unlike the name hint,
//!   this one is *not* shown for every unnamed position — only for a top-level call argument
//!   (an action instance, or a process/PBES instantiation that doesn't resolve to a matching
//!   declared name) and for an equation's own left-hand-side pattern variables (the closest thing
//!   a `map` declaration has to named parameters). A plain function/equation application or an
//!   infix/prefix operator found *inside* an argument — `a` and `l` inside `a |> l`, or `x` inside
//!   `g(x)` used as an argument — never gets one: showing it there would just repeat what the
//!   argument's own hint (or its declaration) already says. Nor does an infix/prefix operator
//!   expression get one when it *is* the whole argument — `x |> l` used as a top-level action
//!   argument shows no `: List(...)` after it either, since the operator's own operands already
//!   make its shape visible; see [`push_hint`].
//!
//! An equation's condition (`eqn ... = ... when b;`) is never hinted at all — `b` is a boolean
//! guard, not a value worth annotating.
//!
//! `merc_syntax::Traverse` doesn't cross between node types (a `ProcessExpr`/`PbesExpr` traversal
//! doesn't descend into the `DataExpr`s inside its actions/conditions/`dist` weights/`val(...)`
//! expressions — see `symbols.rs`'s module doc comment), so [`inlay_hints`]/[`pbes_inlay_hints`]
//! each walk their own tree by hand for its `DataExpr`-bearing fields, then hand each one to
//! [`walk_applications`] or [`walk_struct_applications`], both of which *do* use `Traverse`
//! (`DataExpr` recurses fully into itself) to find every nested application within it, including
//! the field's own top level. [`walk_applications`] allows the `: Sort` fallback and is used only
//! for an equation's LHS pattern; [`walk_struct_applications`] never falls back to `: Sort` and is
//! used everywhere else a `DataExpr` subtree needs searching for nested struct constructors.
//! [`emit_call_hints`] is the part shared by both trees: a process's `Action`/`Id` and a PBES's
//! `PropVarInst` are the same "named callee, positional `DataExpr` arguments" shape one level
//! down, so both feed it the same way, just with a different (spec-specific) parameter-name
//! lookup — see [`process_param_names`]/[`propvarinst_param_names`].

use std::ops::ControlFlow;

use lsp_types::InlayHint;
use lsp_types::InlayHintKind;
use lsp_types::InlayHintLabel;
use lsp_types::Position;
use lsp_types::Range;
use merc_syntax::DataExpr;
use merc_syntax::DataExprKind;
use merc_syntax::PbesExpr;
use merc_syntax::PbesExprKind;
use merc_syntax::ProcessExpr;
use merc_syntax::ProcessExprKind;
use merc_syntax::Span;
use merc_syntax::SortExpression;
use merc_syntax::SortDecl;
use merc_syntax::SortExpressionKind;
use merc_syntax::Traverse;
use merc_typecheck::PbesSpecification;
use merc_typecheck::ProcessSpecification;
use merc_typecheck::TypingInfo;

use crate::convert::LineIndex;

/// Everything threaded unchanged through every helper below, bundled so none of them has to spell
/// out four parameters just to pass them on. Deliberately holds neither a `ProcessSpecification`
/// nor a `PbesSpecification` — the one piece that differs between [`inlay_hints`] and
/// [`pbes_inlay_hints`] — so every helper here is shared by both; each entry point resolves a
/// callee's parameter names itself (see [`process_param_names`]/[`propvarinst_param_names`])
/// before handing the result to the shared [`emit_call_hints`]. `sort_declarations` is the *raw*,
/// un-type-checked specification's own sort declarations — see
/// [`crate::document::Document::parsed_process_specification`] for why struct field names have to
/// come from there rather than from a checked specification. `hints` is the accumulator every
/// emitting helper pushes into.
struct Ctx<'a> {
    text: &'a str,
    line_index: &'a LineIndex,
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
    let mut ctx = Ctx { text, line_index, sort_declarations, typing_info, hints: Vec::new() };

    for decl in spec.process_declarations() {
        walk_process_expr(&decl.body, spec, &mut ctx);
    }
    if let Some(init) = spec.init() {
        walk_process_expr(init, spec, &mut ctx);
    }

    for eqn_spec in &spec.data_specification().data_specification().equation_declarations {
        for eqn in &eqn_spec.node.equations {
            walk_applications(&eqn.lhs, &mut ctx);
            walk_struct_applications(&eqn.rhs, &mut ctx);
            // The condition (`when b`) is a boolean guard, not a value — never hinted.
        }
    }

    ctx.hints.retain(|hint| within_range(hint.position, range));
    ctx.hints
}

/// As [`inlay_hints`], for a PBES: no `act`/`proc` tree to walk, just every equation's formula
/// plus `init` — both a [`PbesExpr`] tree, hand-walked the same way `inlay_hints` walks a
/// `ProcessExpr` tree, for the same reason (see the module doc comment).
pub fn pbes_inlay_hints(
    text: &str,
    line_index: &LineIndex,
    spec: &PbesSpecification,
    sort_declarations: &[SortDecl],
    typing_info: &TypingInfo,
    range: Range,
) -> Vec<InlayHint> {
    let mut ctx = Ctx { text, line_index, sort_declarations, typing_info, hints: Vec::new() };

    for eqn in spec.equations() {
        walk_pbes_expr(&eqn.formula, spec, &mut ctx);
    }
    let init = spec.init();
    let param_names = propvarinst_param_names(spec, &init.node.identifier, init.node.arguments.len());
    emit_call_hints(&init.node.arguments, param_names.as_deref(), &mut ctx);

    for eqn_spec in &spec.data_specification().data_specification().equation_declarations {
        for eqn in &eqn_spec.node.equations {
            walk_applications(&eqn.lhs, &mut ctx);
            walk_struct_applications(&eqn.rhs, &mut ctx);
            // The condition (`when b`) is a boolean guard, not a value — never hinted.
        }
    }

    ctx.hints.retain(|hint| within_range(hint.position, range));
    ctx.hints
}

/// Walks every `ProcessExpr` in `expr`'s subtree, picking out each node's `DataExpr`-bearing
/// fields — the ones `Traverse` itself won't reach — and hinting them.
fn walk_process_expr(expr: &ProcessExpr, spec: &ProcessSpecification, ctx: &mut Ctx) {
    expr.visit::<(), _>(|node| {
        match &node.node {
            ProcessExprKind::Action(name, args) => {
                let param_names = process_param_names(spec, &name.node, args.len());
                emit_call_hints(args, param_names.as_deref(), ctx);
            }
            ProcessExprKind::Id(_, assignments) => {
                // The assignment's key (`n` in `P(n = x)`) is already an explicit name in the
                // source; only its value can contain a nested struct application worth hinting.
                for assignment in assignments {
                    walk_struct_applications(&assignment.node.expr, ctx);
                }
            }
            ProcessExprKind::Dist { expr: weight, .. } => walk_struct_applications(weight, ctx),
            ProcessExprKind::Condition { condition, .. } => walk_struct_applications(condition, ctx),
            ProcessExprKind::At { operand, .. } => walk_struct_applications(operand, ctx),
            _ => {}
        }
        ControlFlow::Continue(())
    });
}

/// As [`walk_process_expr`], for a [`PbesExpr`] tree: a `val(...)` expression is itself a
/// `DataExpr` worth hinting into, and a `PropVarInst` is the PBES counterpart of an action/process
/// call — everything else (`Quantifier`/`Negation`/`Binary`/`True`/`False`) is either already
/// walked by `Traverse` itself (same-typed children) or has no `DataExpr` in it at all.
fn walk_pbes_expr(expr: &PbesExpr, spec: &PbesSpecification, ctx: &mut Ctx) {
    expr.visit::<(), _>(|node| {
        match &node.node {
            PbesExprKind::DataValExpr(value) => walk_struct_applications(value, ctx),
            PbesExprKind::PropVarInst(inst) => {
                let param_names = propvarinst_param_names(spec, &inst.node.identifier, inst.node.arguments.len());
                emit_call_hints(&inst.node.arguments, param_names.as_deref(), ctx);
            }
            _ => {}
        }
        ControlFlow::Continue(())
    });
}

/// Hints `args`, the positional arguments of an action instance, process instantiation, or PBES
/// propositional-variable instantiation: a `name: ` prefix per argument when `param_names` has a
/// name for that position, otherwise a `: Sort` suffix (an action never names its arguments, nor
/// does an instantiation of a variable/equation resolved without a matching arity). Then recurses
/// into each argument for any nested struct application of its own — never for a nested sort
/// suffix; see the module doc comment for why a call argument's own hint is the only `: Sort` an
/// argument should get.
fn emit_call_hints(args: &[DataExpr], param_names: Option<&[&str]>, ctx: &mut Ctx) {
    for (i, argument) in args.iter().enumerate() {
        let field_name = param_names.and_then(|names| names.get(i).copied());
        push_hint(argument, field_name, sort_of(ctx.typing_info, &argument.span), ctx);
        walk_struct_applications(argument, ctx);
    }
}

/// As [`walk_struct_applications`], but additionally falls back to a `: Sort` suffix for an
/// argument whose applied function isn't a struct constructor — appropriate only for an
/// equation's own LHS pattern, `f(x)` in `eqn f(x) = ...;`: a `map` declaration has nowhere else
/// to name its parameters, so this is the one place a plain function application's argument
/// deserves a sort hint. Every other caller uses [`walk_struct_applications`] instead.
fn walk_applications(expr: &DataExpr, ctx: &mut Ctx) {
    walk_applications_impl(expr, true, ctx);
}

/// Finds every struct-constructor application anywhere in `expr`'s subtree (including `expr`
/// itself) and hints its arguments with a `name: ` prefix per named field. A plain (non-struct)
/// application's arguments are left alone — no hint at all — since outside an equation's own LHS
/// pattern there's nothing to say about them that their own declaration doesn't already say (see
/// the module doc comment).
fn walk_struct_applications(expr: &DataExpr, ctx: &mut Ctx) {
    walk_applications_impl(expr, false, ctx);
}

fn walk_applications_impl(expr: &DataExpr, allow_sort_suffix: bool, ctx: &mut Ctx) {
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
                if field_name.is_none() && !allow_sort_suffix {
                    continue;
                }
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

/// As [`process_param_names`], for a PBES propositional-variable equation — the parameter names of
/// the equation named `name` with exactly `arity` parameters, if one exists. `None` when `name`
/// resolves to nothing with a matching arity (a free/global variable used where a `PropVarInst` is
/// expected, or a genuinely undeclared name — either way, a checked specification wouldn't have
/// accepted the document, so this is defensive rather than expected in practice).
fn propvarinst_param_names<'a>(spec: &'a PbesSpecification, name: &str, arity: usize) -> Option<Vec<&'a str>> {
    spec.equations()
        .iter()
        .find(|eqn| eqn.variable.identifier == name && eqn.variable.parameters.len() == arity)
        .map(|eqn| eqn.variable.parameters.iter().map(|param| param.identifier.as_str()).collect())
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
            .find(|constructor| constructor.name.node == name && constructor.args.len() == arity)
            .map(|constructor| constructor.args.iter().map(|(field, _)| field.as_ref().map(|spanned| spanned.node.as_str())).collect())
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
/// specification, but degrading to no hint is safer than a wrong one), nor when `argument` is
/// already a bare variable spelled the same as `field_name` — `P(n)` for a parameter also named
/// `n` doesn't need `n:` repeated right in front of it. The `: Sort` suffix is also skipped when
/// `argument` is itself an infix/prefix operator application (`x |> l`, `-x`) — its operands are
/// already visible right there in the source, so appending the *result*'s sort on top would just
/// be noise; a `name:` prefix is unaffected, since that names the argument, not its result.
fn push_hint(argument: &DataExpr, field_name: Option<&str>, sort: Option<&SortExpression>, ctx: &mut Ctx) {
    let hint = match field_name {
        Some(name) => {
            if is_bare_reference_to(argument, name) {
                return;
            }
            let position = ctx.line_index.position(ctx.text, argument.span.start);
            build_hint(position, format!("{name}:"), InlayHintKind::PARAMETER, false, true)
        }
        None => {
            if matches!(argument.node, DataExprKind::Binary { .. } | DataExprKind::Unary { .. }) {
                return;
            }
            let Some(sort) = sort else { return };
            let position = ctx.line_index.position(ctx.text, argument.span.end);
            build_hint(position, format!(": {sort}"), InlayHintKind::TYPE, false, false)
        }
    };
    ctx.hints.push(hint);
}

/// Whether `argument` is nothing more than a reference to a variable/constant named `name` — the
/// case where a `name: ` hint would just repeat the source text right next to it.
fn is_bare_reference_to(argument: &DataExpr, name: &str) -> bool {
    match &argument.node {
        DataExprKind::Id(id) | DataExprKind::Resolved(id, _) => id == name,
        _ => false,
    }
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
    use crate::typecheck::PbesTypecheckOutcome;
    use crate::typecheck::TypecheckOutcome;
    use crate::typecheck::typecheck;
    use crate::typecheck::typecheck_pbes;

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

    async fn pbes_hints_for(text: &str) -> (Vec<InlayHint>, LineIndex) {
        let spec = match parse(SpecKind::Pbes, text.to_string()).await {
            ParseOutcome::Ok(Specification::Pbes(spec)) => *spec,
            _ => panic!("fixture failed to parse"),
        };
        let sort_declarations = spec.data_specification.sort_declarations.clone();
        let mut checked = match typecheck_pbes(text.to_string()).await {
            PbesTypecheckOutcome::Ok(checked) => checked,
            PbesTypecheckOutcome::Error(error) => panic!("fixture failed to typecheck: {error}"),
            PbesTypecheckOutcome::Internal(message) => panic!("internal error typechecking fixture: {message}"),
        };
        let line_index = LineIndex::new(text);
        let typing_info = checked.typing_info();
        let whole_document = Range {
            start: Position { line: 0, character: 0 },
            end: Position { line: u32::MAX, character: u32::MAX },
        };
        let hints = pbes_inlay_hints(text, &line_index, &checked, &sort_declarations, &typing_info, whole_document);
        (hints, line_index)
    }

    fn label(hint: &InlayHint) -> &str {
        let InlayHintLabel::String(label) = &hint.label else {
            panic!("expected a string label");
        };
        label
    }

    #[tokio::test]
    async fn infix_operator_argument_gets_no_type_suffix_hint() {
        // `x |> l` (list cons) is itself the whole action argument here — its own `: List(Pos)`
        // would just repeat what's already visible from the operator and its operands.
        let text = "act a: List(Pos);\nproc P(x: Pos, l: List(Pos)) = a(x |> l);\ninit delta;";
        let (hints, _line_index) = hints_for(text).await;

        assert!(hints.is_empty(), "did not expect any hint for an infix-operator argument, got {hints:?}");
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
        // No leading space: `a: Nat`, not `a : Nat`.
        assert_eq!(hint.padding_left, Some(false));
    }

    #[tokio::test]
    async fn nested_call_inside_a_call_argument_gets_no_sort_hint() {
        // `g(1)` is itself an argument of `P(...)`, so it already gets its own `n:` hint from the
        // call; `1`, one level further down inside `g(...)`, isn't a top-level parameter of
        // anything and must not get a redundant `: Nat` sort hint of its own.
        let text = "map g: Nat -> Nat;\nproc P(n: Nat) = delta;\ninit P(g(1));";
        let (hints, line_index) = hints_for(text).await;

        let one_end = text.find("1))").unwrap() + 1;
        let unwanted = line_index.position(text, one_end);
        assert!(hints.iter().all(|h| h.position != unwanted), "did not expect a hint after the nested '1'");

        let g_start = text.find("g(1)").unwrap();
        let expected = line_index.position(text, g_start);
        let hint = hints.iter().find(|h| h.position == expected).expect("expected a hint before 'g(1)'");
        assert_eq!(label(hint), "n:");
    }

    #[tokio::test]
    async fn call_argument_matching_the_parameter_name_gets_no_hint() {
        let text = "glob n: Nat;\nproc P(n: Nat) = delta;\ninit P(n);";
        let (hints, line_index) = hints_for(text).await;

        let n_start = text.rfind('n').unwrap();
        let unwanted = line_index.position(text, n_start);
        assert!(hints.iter().all(|h| h.position != unwanted), "did not expect a redundant 'n:' hint before 'n'");
    }

    #[tokio::test]
    async fn equation_condition_gets_no_hint() {
        let text = "sort D;\nmap f: D -> D;\nmap p: D -> Bool;\nvar x: D;\neqn p(x) -> f(x) = x;\ninit delta;";
        let (hints, line_index) = hints_for(text).await;

        let condition_x_end = text.find("p(x)").unwrap() + 3;
        let unwanted = line_index.position(text, condition_x_end);
        assert!(
            hints.iter().all(|h| h.position != unwanted),
            "did not expect a hint after the condition's 'x'"
        );

        let lhs_x_end = text.find("f(x)").unwrap() + 3;
        let expected = line_index.position(text, lhs_x_end);
        let hint = hints.iter().find(|h| h.position == expected).expect("expected a hint after the LHS 'x'");
        assert_eq!(label(hint), ": D");
    }

    #[tokio::test]
    async fn equation_rhs_application_argument_gets_no_sort_hint() {
        let text = "sort D;\nmap f: D -> D;\nmap g: D -> D;\nvar x: D;\neqn f(x) = g(x);\ninit delta;";
        let (hints, line_index) = hints_for(text).await;

        let rhs_x_end = text.find("g(x)").unwrap() + 3;
        let unwanted = line_index.position(text, rhs_x_end);
        assert!(hints.iter().all(|h| h.position != unwanted), "did not expect a hint after the RHS 'x'");

        let lhs_x_end = text.find("f(x)").unwrap() + 3;
        let expected = line_index.position(text, lhs_x_end);
        let hint = hints.iter().find(|h| h.position == expected).expect("expected a hint after the LHS 'x'");
        assert_eq!(label(hint), ": D");
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

    #[tokio::test]
    async fn propvarinst_argument_gets_a_name_prefix_hint() {
        let text = "pbes mu X(n: Nat) = val(n > 0);\ninit X(1);";
        let (hints, line_index) = pbes_hints_for(text).await;

        let one_start = text.rfind('1').unwrap();
        let expected = line_index.position(text, one_start);
        let hint = hints.iter().find(|h| h.position == expected).expect("expected a hint before the init argument '1'");
        assert_eq!(label(hint), "n:");
        assert_eq!(hint.kind, Some(InlayHintKind::PARAMETER));
    }

    #[tokio::test]
    async fn val_expression_argument_gets_hinted_like_a_data_expression() {
        let text = "sort S = struct s(n: Nat);\nmap f: S -> Bool;\npbes mu X = val(f(s(5)));\ninit X;";
        let (hints, line_index) = pbes_hints_for(text).await;

        let five_start = text.find('5').unwrap();
        let expected = line_index.position(text, five_start);
        let hint = hints.iter().find(|h| h.position == expected).expect("expected a hint before '5' inside val(...)");
        assert_eq!(label(hint), "n:");
        assert_eq!(hint.kind, Some(InlayHintKind::PARAMETER));
    }

}
