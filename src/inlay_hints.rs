//! `textDocument/inlayHint`: shows the name or sort of every call-argument expression (action
//! instance, process instantiation, PBES/PRES propositional-variable instantiation, structured-sort
//! constructor application).
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
//!   (a process/PBES instantiation that doesn't resolve to a matching declared name). Action
//!   arguments get no sort hint at all: an action never names its arguments, and showing a sort
//!   suffix there would be noise that the declaration already conveys. Nor does an equation's own
//!   left-hand-side pattern variable (`x` in `eqn f(x) = ...;`) get one — its sort is already a
//!   click away on hover, and repeating it on every equation would just be clutter. A plain
//!   function/equation application or an infix/prefix operator found *inside* an argument — `a`
//!   and `l` inside `a |> l`, or `x` inside `g(x)` used as an argument — never gets one either:
//!   showing it there would just repeat what the argument's own hint (or its declaration) already
//!   says. Nor does an infix/prefix operator expression get one when it *is* the whole argument —
//!   `x |> l` used as a top-level action argument shows no `: List(...)` after it either, since
//!   the operator's own operands already make its shape visible; see [`push_hint`].
//!
//! An equation's condition (`eqn ... = ... when b;`) is never hinted at all — `b` is a boolean
//! guard, not a value worth annotating.
//!
//! `merc_syntax::Traverse` doesn't cross between node types (a `ProcessExpr`/`PbesExpr` traversal
//! doesn't descend into the `DataExpr`s inside its actions/conditions/`dist` weights/`val(...)`
//! expressions — see `symbols.rs`'s module doc comment), so [`inlay_hints`]/[`pbes_inlay_hints`]
//! each walk their own tree by hand for its `DataExpr`-bearing fields, then hand each one to
//! [`walk_struct_applications`], which *does* use `Traverse` (`DataExpr` recurses fully into
//! itself) to find every nested struct-constructor application within it, including the field's
//! own top level — an equation's LHS pattern is walked the same way as everywhere else. Its
//! arguments are never sort-suffixed; see the module doc comment above and [`push_hint`].
//! [`emit_call_hints`] is the part shared by both trees: a process's `Action`/`Id` and a PBES's
//! `PropVarInst` are the same "named callee, positional `DataExpr` arguments" shape one level
//! down, so both feed it the same way, just with a different (spec-specific) parameter-name
//! lookup — see [`resolved_process_param_names`]/[`propvarinst_param_names`].

use std::ops::ControlFlow;

use lsp_types::InlayHint;
use lsp_types::InlayHintKind;
use lsp_types::InlayHintLabel;
use lsp_types::Position;
use lsp_types::Range;
use merc_syntax::ActFrm;
use merc_syntax::ActFrmKind;
use merc_syntax::DataExpr;
use merc_syntax::DataExprKind;
use merc_syntax::PbesExpr;
use merc_syntax::PbesExprKind;
use merc_syntax::PresExpr;
use merc_syntax::PresExprKind;
use merc_syntax::ProcessExpr;
use merc_syntax::ProcessExprKind;
use merc_syntax::RegFrm;
use merc_syntax::RegFrmKind;
use merc_syntax::SortDecl;
use merc_syntax::SortExpression;
use merc_syntax::SortExpressionKind;
use merc_syntax::SourceMap;
use merc_syntax::Span;
use merc_syntax::StateFrm;
use merc_syntax::StateFrmKind;
use merc_syntax::StateVarDecl;
use merc_syntax::Traverse;
use merc_typecheck::ModalSpecification;
use merc_typecheck::PbesSpecification;
use merc_typecheck::PresSpecification;
use merc_typecheck::ProcessSpecification;
use merc_typecheck::ResolvedName;
use merc_typecheck::TypingInfo;

use crate::convert::LineIndex;

/// Everything threaded unchanged through every helper below, bundled so none of them has to spell
/// out four parameters just to pass them on. Deliberately holds neither a `ProcessSpecification`
/// nor a `PbesSpecification` — the one piece that differs between [`inlay_hints`] and
/// [`pbes_inlay_hints`] — so every helper here is shared by both; each entry point resolves a
/// callee's parameter names itself (see [`resolved_process_param_names`]/[`propvarinst_param_names`])
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
    /// The document's whole-project [`SourceMap`], used only to keep [`push_hint`] from placing a
    /// hint against a span that belongs to an `%import`ed file rather than the root document being
    /// hinted — see [`push_hint`]'s doc comment. Empty (`file_count() == 0`) in the unit tests
    /// below, which parse a fixture standalone rather than through a real `%import` resolution.
    sources: &'a SourceMap,
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
    sources: &SourceMap,
    range: Range,
) -> Vec<InlayHint> {
    let mut ctx = Ctx {
        text,
        line_index,
        sort_declarations,
        typing_info,
        sources,
        hints: Vec::new(),
    };

    for decl in spec.process_declarations() {
        walk_process_expr(&decl.body, spec, &mut ctx);
    }
    if let Some(init) = spec.init() {
        walk_process_expr(init, spec, &mut ctx);
    }

    for eqn_spec in &spec
        .data_specification()
        .data_specification()
        .equation_declarations
    {
        for eqn in &eqn_spec.node.equations {
            walk_struct_applications(&eqn.lhs, &mut ctx);
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
    sources: &SourceMap,
    range: Range,
) -> Vec<InlayHint> {
    let mut ctx = Ctx {
        text,
        line_index,
        sort_declarations,
        typing_info,
        sources,
        hints: Vec::new(),
    };

    for eqn in spec.equations() {
        walk_pbes_expr(&eqn.formula, spec, &mut ctx);
    }
    let init = spec.init();
    let param_names =
        propvarinst_param_names(spec, &init.node.identifier, init.node.arguments.len());
    emit_call_hints(
        &init.node.arguments,
        param_names.as_deref(),
        false,
        &mut ctx,
    );

    for eqn_spec in &spec
        .data_specification()
        .data_specification()
        .equation_declarations
    {
        for eqn in &eqn_spec.node.equations {
            walk_struct_applications(&eqn.lhs, &mut ctx);
            walk_struct_applications(&eqn.rhs, &mut ctx);
            // The condition (`when b`) is a boolean guard, not a value — never hinted.
        }
    }

    ctx.hints.retain(|hint| within_range(hint.position, range));
    ctx.hints
}

/// As [`pbes_inlay_hints`], for a PRES: [`PresSpecification`] has the identical shape one level
/// down (see `crate::completion::pres_completions`'s doc comment), so this differs only in walking
/// [`PresExpr`] instead of [`PbesExpr`] for each equation's formula.
pub fn pres_inlay_hints(
    text: &str,
    line_index: &LineIndex,
    spec: &PresSpecification,
    sort_declarations: &[SortDecl],
    typing_info: &TypingInfo,
    sources: &SourceMap,
    range: Range,
) -> Vec<InlayHint> {
    let mut ctx = Ctx {
        text,
        line_index,
        sort_declarations,
        typing_info,
        sources,
        hints: Vec::new(),
    };

    for eqn in spec.equations() {
        walk_pres_expr(&eqn.formula, spec, &mut ctx);
    }
    let init = spec.init();
    let param_names = pres_propvarinst_param_names(spec, &init.node.identifier, init.node.arguments.len());
    emit_call_hints(&init.node.arguments, param_names.as_deref(), false, &mut ctx);

    for eqn_spec in &spec
        .data_specification()
        .data_specification()
        .equation_declarations
    {
        for eqn in &eqn_spec.node.equations {
            walk_struct_applications(&eqn.lhs, &mut ctx);
            walk_struct_applications(&eqn.rhs, &mut ctx);
        }
    }

    ctx.hints.retain(|hint| within_range(hint.position, range));
    ctx.hints
}

/// As [`pbes_inlay_hints`]/[`pres_inlay_hints`], for a modal (mu-calculus) formula: no flat
/// top-level equation list to walk — a fixpoint variable's own reference/declaration is found
/// recursively instead (see [`walk_state_frm`]/[`resolved_state_var_param_names`]), the same way
/// `symbols.rs`'s own `collect_fixed_points` has to.
pub fn modal_inlay_hints(
    text: &str,
    line_index: &LineIndex,
    spec: &ModalSpecification,
    sort_declarations: &[SortDecl],
    typing_info: &TypingInfo,
    sources: &SourceMap,
    range: Range,
) -> Vec<InlayHint> {
    let mut ctx = Ctx {
        text,
        line_index,
        sort_declarations,
        typing_info,
        sources,
        hints: Vec::new(),
    };

    walk_state_frm(spec.formula(), spec, &mut ctx);

    for eqn_spec in &spec
        .data_specification()
        .data_specification()
        .equation_declarations
    {
        for eqn in &eqn_spec.node.equations {
            walk_struct_applications(&eqn.lhs, &mut ctx);
            walk_struct_applications(&eqn.rhs, &mut ctx);
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
                let param_names = resolved_process_param_names(spec, ctx, &name.span);
                emit_call_hints(args, param_names.as_deref(), false, ctx);
            }
            ProcessExprKind::Id(_, assignments) => {
                // The assignment's key (`n` in `P(n = x)`) is already an explicit name in the
                // source; only its value can contain a nested struct application worth hinting.
                for assignment in assignments {
                    walk_struct_applications(&assignment.node.expr, ctx);
                }
            }
            ProcessExprKind::Dist { expr: weight, .. } => walk_struct_applications(weight, ctx),
            ProcessExprKind::Condition { condition, .. } => {
                walk_struct_applications(condition, ctx)
            }
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
                let param_names =
                    propvarinst_param_names(spec, &inst.node.identifier, inst.node.arguments.len());
                emit_call_hints(&inst.node.arguments, param_names.as_deref(), false, ctx);
            }
            _ => {}
        }
        ControlFlow::Continue(())
    });
}

/// As [`walk_pbes_expr`], for a [`PresExpr`] tree — a `val(...)` expression and a `PropVarInst` are
/// hinted the same way; each side of a scalar multiplication
/// (`PresExprKind::RightConstantMultiply`/`LeftConstantMultiply`) carries its own `constant`, a
/// `DataExpr` `Traverse` doesn't reach on its own, so it's hinted the same way too. `Equal`'s and
/// `Condition`'s own tag fields carry no `DataExpr` at all; their `body`/`lhs`/`then`/`else_`
/// children are plain `PresExpr` nodes `Traverse` already recurses into.
fn walk_pres_expr(expr: &PresExpr, spec: &PresSpecification, ctx: &mut Ctx) {
    expr.visit::<(), _>(|node| {
        match &node.node {
            PresExprKind::DataValExpr(value) => walk_struct_applications(value, ctx),
            PresExprKind::PropVarInst(inst) => {
                let param_names = pres_propvarinst_param_names(spec, &inst.node.identifier, inst.node.arguments.len());
                emit_call_hints(&inst.node.arguments, param_names.as_deref(), false, ctx);
            }
            PresExprKind::RightConstantMultiply { constant, .. } | PresExprKind::LeftConstantMultiply { constant, .. } => {
                walk_struct_applications(constant, ctx);
            }
            _ => {}
        }
        ControlFlow::Continue(())
    });
}

/// Walks a modal (mu-calculus) state formula's own tree by hand, hinting a fixpoint-variable
/// reference's arguments the same way [`walk_process_expr`]/[`walk_pbes_expr`] hint a process/
/// propositional-variable call's — descends by hand (rather than via `Traverse::visit`, the way
/// `walk_process_expr`/`walk_pbes_expr` do) for the same reason `symbols.rs`'s own
/// `collect_fixed_points` and `semantic_tokens.rs`'s own `walk_state_frm` do: `Traverse` doesn't
/// cross into the `RegFrm`/`ActFrm` a modality carries either.
fn walk_state_frm(formula: &StateFrm, spec: &ModalSpecification, ctx: &mut Ctx) {
    match &formula.node {
        StateFrmKind::True | StateFrmKind::False => {}
        StateFrmKind::Delay(time) | StateFrmKind::Yaled(time) => {
            if let Some(time) = time {
                walk_struct_applications(time, ctx);
            }
        }
        StateFrmKind::Id(_, arguments) | StateFrmKind::Resolved(_, arguments, _) => {
            let param_names = resolved_state_var_param_names(spec, ctx, &formula.span);
            emit_call_hints(arguments, param_names.as_deref(), false, ctx);
        }
        StateFrmKind::DataValExpr(expr) => walk_struct_applications(expr, ctx),
        StateFrmKind::DataValExprLeftMult(constant, expr) => {
            walk_struct_applications(constant, ctx);
            walk_state_frm(expr, spec, ctx);
        }
        StateFrmKind::DataValExprRightMult(expr, constant) => {
            walk_state_frm(expr, spec, ctx);
            walk_struct_applications(constant, ctx);
        }
        StateFrmKind::Modality { formula: reg, expr, .. } => {
            walk_reg_frm(reg, ctx);
            walk_state_frm(expr, spec, ctx);
        }
        StateFrmKind::Unary { expr, .. } => walk_state_frm(expr, spec, ctx),
        StateFrmKind::Binary { lhs, rhs, .. } => {
            walk_state_frm(lhs, spec, ctx);
            walk_state_frm(rhs, spec, ctx);
        }
        StateFrmKind::Quantifier { body, .. } | StateFrmKind::Bound { body, .. } => walk_state_frm(body, spec, ctx),
        StateFrmKind::FixedPoint { variable, body, .. } => {
            // Each parameter's own initial value is checked in the *outer* scope (mirrors a
            // process instantiation's assignment value) — never sort-suffixed (it's not a call
            // argument), but still worth descending into for a nested struct application.
            for argument in &variable.arguments {
                walk_struct_applications(&argument.expr, ctx);
            }
            walk_state_frm(body, spec, ctx);
        }
    }
}

/// As [`walk_state_frm`], for a modality's regular formula (`[a*]X`'s `a*`).
fn walk_reg_frm(formula: &RegFrm, ctx: &mut Ctx) {
    match &formula.node {
        RegFrmKind::Action(action) => walk_act_frm(action, ctx),
        RegFrmKind::Iteration(inner) | RegFrmKind::Plus(inner) => walk_reg_frm(inner, ctx),
        RegFrmKind::Sequence { lhs, rhs } | RegFrmKind::Choice { lhs, rhs } => {
            walk_reg_frm(lhs, ctx);
            walk_reg_frm(rhs, ctx);
        }
    }
}

/// As [`walk_reg_frm`], for an action formula (`a(1) && !b`) — an action's own arguments are never
/// hinted with a name or sort suffix (see the module doc comment), only descended into for a
/// nested struct application, the same way [`walk_process_expr`]'s `ProcessExprKind::Action`
/// arguments never get a top-level hint of their own either (`emit_call_hints` isn't called here).
fn walk_act_frm(formula: &ActFrm, ctx: &mut Ctx) {
    match &formula.node {
        ActFrmKind::True | ActFrmKind::False => {}
        ActFrmKind::MultAct(multi_action) => {
            for action in &multi_action.actions {
                for argument in &action.args {
                    walk_struct_applications(argument, ctx);
                }
            }
        }
        ActFrmKind::DataExprVal(expr) => walk_struct_applications(expr, ctx),
        ActFrmKind::Negation(inner) => walk_act_frm(inner, ctx),
        ActFrmKind::Quantifier { body, .. } => walk_act_frm(body, ctx),
        ActFrmKind::Binary { lhs, rhs, .. } => {
            walk_act_frm(lhs, ctx);
            walk_act_frm(rhs, ctx);
        }
    }
}

/// Hints `args`, the positional arguments of an action instance, process instantiation, or PBES
/// propositional-variable instantiation: a `name: ` prefix per argument when `param_names` has a
/// name for that position, otherwise a `: Sort` suffix (only when `allow_sort_suffix` is true;
/// an action never names its arguments, nor does an instantiation of a variable/equation resolved
/// without a matching arity). Then recurses into each argument for any nested struct application
/// of its own — never for a nested sort suffix; see the module doc comment for why a call
/// argument's own hint is the only `: Sort` an argument should get.
fn emit_call_hints(
    args: &[DataExpr],
    param_names: Option<&[&str]>,
    allow_sort_suffix: bool,
    ctx: &mut Ctx,
) {
    for (i, argument) in args.iter().enumerate() {
        let field_name = param_names.and_then(|names| names.get(i).copied());
        if field_name.is_some() || allow_sort_suffix {
            push_hint(
                argument,
                field_name,
                sort_of(ctx.typing_info, &argument.span),
                ctx,
            );
        }
        walk_struct_applications(argument, ctx);
    }
}

/// Finds every struct-constructor application anywhere in `expr`'s subtree (including `expr`
/// itself) and hints its arguments with a `name: ` prefix per named field. A plain (non-struct)
/// application's arguments are left alone — no hint at all — since there's nothing to say about
/// them that their own declaration doesn't already say (see the module doc comment). This is used
/// for an equation's own LHS pattern too: a plain function application there gets no `: Sort`
/// hint either — its sort is a hover away, and one on every equation would be clutter.
fn walk_struct_applications(expr: &DataExpr, ctx: &mut Ctx) {
    expr.visit::<(), _>(|node| {
        if let DataExprKind::Application {
            function,
            arguments,
        } = &node.node
        {
            let callee = match &function.node {
                DataExprKind::Id(name) => Some(name.as_str()),
                DataExprKind::Resolved(name, _) => Some(name.as_str()),
                _ => None,
            };
            let field_names = callee
                .and_then(|name| struct_field_names(ctx.sort_declarations, name, arguments.len()));
            for (i, argument) in arguments.iter().enumerate() {
                let field_name = field_names
                    .as_ref()
                    .and_then(|names| names.get(i).copied().flatten());
                let Some(field_name) = field_name else {
                    continue;
                };
                push_hint(
                    argument,
                    Some(field_name),
                    sort_of(ctx.typing_info, &argument.span),
                    ctx,
                );
            }
        }
        ControlFlow::Continue(())
    });
}

/// The parameter names of the `proc` declaration that the `TypingInfo` resolved `name`'s span to —
/// i.e. the single winning overload, not just any declaration sharing a name and arity. Resolving
/// via the type checker's `ResolvedName::Process` declaration span is what lets an overloaded
/// process (same name and parameter count, different parameter sorts) pick the declaration the
/// checked call actually selected. `None` when `name`'s span resolves to no process (an action, or
/// a process reference the checker didn't record — the checked call sites this skips anyway, so a
/// `None` just falls back to the sort-only hint).
fn resolved_process_param_names<'a>(
    spec: &'a ProcessSpecification,
    ctx: &Ctx,
    callee_span: &Span,
) -> Option<Vec<&'a str>> {
    let decl_span = ctx.typing_info.nodes().iter().find_map(|node| {
        if node.span.start == callee_span.start
            && node.span.end == callee_span.end
            && let Some(ResolvedName::Process { declaration, .. }) = &node.name
        {
            declaration.clone()
        } else {
            None
        }
    })?;
    spec.process_declarations()
        .iter()
        .find(|decl| decl.identifier.span == decl_span)
        .map(|decl| {
            decl.params
                .iter()
                .map(|param| param.identifier.as_str())
                .collect()
        })
}

/// As [`resolved_process_param_names`], for a PBES propositional-variable equation — the parameter names of
/// the equation named `name` with exactly `arity` parameters, if one exists. `None` when `name`
/// resolves to nothing with a matching arity (a free/global variable used where a `PropVarInst` is
/// expected, or a genuinely undeclared name — either way, a checked specification wouldn't have
/// accepted the document, so this is defensive rather than expected in practice).
fn propvarinst_param_names<'a>(
    spec: &'a PbesSpecification,
    name: &str,
    arity: usize,
) -> Option<Vec<&'a str>> {
    spec.equations()
        .iter()
        .find(|eqn| eqn.variable.identifier.as_str() == name && eqn.variable.parameters.len() == arity)
        .map(|eqn| {
            eqn.variable
                .parameters
                .iter()
                .map(|param| param.identifier.as_str())
                .collect()
        })
}

/// As [`propvarinst_param_names`], for a PRES.
fn pres_propvarinst_param_names<'a>(spec: &'a PresSpecification, name: &str, arity: usize) -> Option<Vec<&'a str>> {
    spec.equations()
        .iter()
        .find(|eqn| eqn.variable.identifier.as_str() == name && eqn.variable.parameters.len() == arity)
        .map(|eqn| {
            eqn.variable
                .parameters
                .iter()
                .map(|param| param.identifier.as_str())
                .collect()
        })
}

/// As [`resolved_process_param_names`], for a modal formula's fixpoint-variable reference — the
/// parameter names of the `mu`/`nu` declaration the `TypingInfo` resolved `occurrence_span` (the
/// whole `Id`/`Resolved` node — see [`merc_typecheck::ResolvedName::StateVariable`]'s own doc
/// comment for why there is no narrower span) to. `None` when it resolves to nothing (shouldn't
/// arise for a checked specification, but degrading to a sort-only hint is safer than a wrong one).
fn resolved_state_var_param_names<'a>(spec: &'a ModalSpecification, ctx: &Ctx, occurrence_span: &Span) -> Option<Vec<&'a str>> {
    let decl_span = ctx.typing_info.nodes().iter().find_map(|node| {
        if node.span.start == occurrence_span.start
            && node.span.end == occurrence_span.end
            && let Some(ResolvedName::StateVariable { declaration, .. }) = &node.name
        {
            declaration.clone()
        } else {
            None
        }
    })?;
    find_state_var_decl(spec.formula(), &decl_span).map(|decl| decl.arguments.iter().map(|argument| argument.identifier.as_str()).collect())
}

/// The `StateVarDecl` whose own span is exactly `decl_span`, found by recursing through the
/// formula tree by hand — mirrors `crate::hover::find_state_var_in_formula` and `symbols.rs`'s own
/// `collect_fixed_points`: a modal formula's fixpoint variables aren't listed anywhere flat the way
/// a PBES/PRES's `equations` are.
fn find_state_var_decl<'a>(formula: &'a StateFrm, decl_span: &Span) -> Option<&'a StateVarDecl> {
    match &formula.node {
        StateFrmKind::FixedPoint { variable, body, .. } => {
            if &variable.span == decl_span {
                Some(variable)
            } else {
                find_state_var_decl(body, decl_span)
            }
        }
        StateFrmKind::Unary { expr, .. } | StateFrmKind::Modality { expr, .. } => find_state_var_decl(expr, decl_span),
        StateFrmKind::Binary { lhs, rhs, .. } => find_state_var_decl(lhs, decl_span).or_else(|| find_state_var_decl(rhs, decl_span)),
        StateFrmKind::Quantifier { body, .. } | StateFrmKind::Bound { body, .. } => find_state_var_decl(body, decl_span),
        StateFrmKind::DataValExprLeftMult(_, expr) | StateFrmKind::DataValExprRightMult(expr, _) => find_state_var_decl(expr, decl_span),
        StateFrmKind::True
        | StateFrmKind::False
        | StateFrmKind::Delay(_)
        | StateFrmKind::Yaled(_)
        | StateFrmKind::Id(_, _)
        | StateFrmKind::Resolved(_, _, _)
        | StateFrmKind::DataValExpr(_) => None,
    }
}

/// The field names of the struct constructor named `name` with exactly `arity` fields, if one is
/// declared — each entry `None` for an unnamed field, which falls back to the sort-only hint for
/// just that one argument (a struct can be partially named).
fn struct_field_names<'a>(
    sort_declarations: &'a [SortDecl],
    name: &str,
    arity: usize,
) -> Option<Vec<Option<&'a str>>> {
    sort_declarations.iter().find_map(|decl| {
        let SortExpressionKind::Struct { inner } = &decl.expr.as_ref()?.node else {
            return None;
        };
        inner
            .iter()
            .find(|constructor| constructor.name.node == name && constructor.args.len() == arity)
            .map(|constructor| {
                constructor
                    .args
                    .iter()
                    .map(|(field, _)| field.as_ref().map(|spanned| spanned.node.as_str()))
                    .collect()
            })
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
fn push_hint(
    argument: &DataExpr,
    field_name: Option<&str>,
    sort: Option<&SortExpression>,
    ctx: &mut Ctx,
) {
    // `spec.process_declarations()` (and the equivalent equation lists for PBES/PRES/modal specs)
    // include every declaration merged in from an `%import`ed file, so `argument.span` can be a
    // global offset into a file other than the one being hinted. Rendering that against `ctx.text`/
    // `ctx.line_index` (always the root document's own) would silently clamp to the end of the
    // document (see `LineIndex::position`'s doc comment) rather than the argument's real position —
    // in practice a pile of hints all stacked on the document's last line whenever it imports
    // anything. `sources.file_count() == 0` is the unit-test-fixture case below, parsed standalone
    // with no real `SourceMap`, where every span is trivially local.
    if ctx.sources.file_count() > 0 && ctx.sources.lookup(argument.span.start).value() != 0 {
        return;
    }

    let hint = match field_name {
        Some(name) => {
            if is_bare_reference_to(argument, name) {
                return;
            }
            let position = ctx.line_index.position(ctx.text, argument.span.start);
            build_hint(
                position,
                format!("{name}:"),
                InlayHintKind::PARAMETER,
                false,
                true,
            )
        }
        None => {
            if matches!(
                argument.node,
                DataExprKind::Binary { .. } | DataExprKind::Unary { .. }
            ) {
                return;
            }
            let Some(sort) = sort else { return };
            let position = ctx.line_index.position(ctx.text, argument.span.end);
            build_hint(
                position,
                format!(": {sort}"),
                InlayHintKind::TYPE,
                false,
                false,
            )
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

fn build_hint(
    position: Position,
    label: String,
    kind: InlayHintKind,
    padding_left: bool,
    padding_right: bool,
) -> InlayHint {
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
    use crate::parse::parse_ignoring_sources as parse;
    use crate::typecheck::PbesTypecheckOutcome;
    use crate::typecheck::TypecheckOutcome;
    use crate::typecheck::typecheck_ignoring_sources as typecheck;
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
            TypecheckOutcome::Internal(message) => {
                panic!("internal error typechecking fixture: {message}")
            }
        };
        let line_index = LineIndex::new(text);
        let typing_info = checked.typing_info();
        let whole_document = Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end: Position {
                line: u32::MAX,
                character: u32::MAX,
            },
        };
        let hints = inlay_hints(
            text,
            &line_index,
            &checked,
            &sort_declarations,
            &typing_info,
            &SourceMap::new(),
            whole_document,
        );
        (hints, line_index)
    }

    async fn pbes_hints_for(text: &str) -> (Vec<InlayHint>, LineIndex) {
        let spec = match parse(SpecKind::Pbes, text.to_string()).await {
            ParseOutcome::Ok(Specification::Pbes(spec)) => *spec,
            _ => panic!("fixture failed to parse"),
        };
        let sort_declarations = spec.data_specification.sort_declarations.clone();
        let mut checked = match typecheck_pbes(spec.clone()).await {
            PbesTypecheckOutcome::Ok(checked) => checked,
            PbesTypecheckOutcome::Error(error) => panic!("fixture failed to typecheck: {error}"),
            PbesTypecheckOutcome::Internal(message) => {
                panic!("internal error typechecking fixture: {message}")
            }
        };
        let line_index = LineIndex::new(text);
        let typing_info = checked.typing_info();
        let whole_document = Range {
            start: Position {
                line: 0,
                character: 0,
            },
            end: Position {
                line: u32::MAX,
                character: u32::MAX,
            },
        };
        let hints = pbes_inlay_hints(
            text,
            &line_index,
            &checked,
            &sort_declarations,
            &typing_info,
            &SourceMap::new(),
            whole_document,
        );
        (hints, line_index)
    }

    async fn modal_hints_for(text: &str) -> (Vec<InlayHint>, LineIndex) {
        let spec = match parse(SpecKind::Modal, text.to_string()).await {
            ParseOutcome::Ok(Specification::Modal(spec)) => *spec,
            _ => panic!("fixture failed to parse"),
        };
        let sort_declarations = spec.data_specification.sort_declarations.clone();
        let mut checked = match crate::typecheck::typecheck_modal_ignoring_sources(spec.clone()).await {
            crate::typecheck::ModalTypecheckOutcome::Ok(checked) => checked,
            crate::typecheck::ModalTypecheckOutcome::Error(error) => panic!("fixture failed to typecheck: {error}"),
            crate::typecheck::ModalTypecheckOutcome::Internal(message) => panic!("internal error typechecking fixture: {message}"),
        };
        let line_index = LineIndex::new(text);
        let typing_info = checked.typing_info();
        let whole_document = Range {
            start: Position { line: 0, character: 0 },
            end: Position { line: u32::MAX, character: u32::MAX },
        };
        let hints = modal_inlay_hints(text, &line_index, &checked, &sort_declarations, &typing_info, &SourceMap::new(), whole_document);
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

        assert!(
            hints.is_empty(),
            "did not expect any hint for an infix-operator argument, got {hints:?}"
        );
    }

    #[tokio::test]
    async fn action_argument_gets_no_type_suffix_hint() {
        let text = "act a: Nat;\nproc P(n: Nat) = a(n);\ninit P(1);";
        let (hints, line_index) = hints_for(text).await;

        let n_end = text.find("n);").unwrap() + 1;
        let unwanted = line_index.position(text, n_end);
        assert!(
            hints.iter().all(|h| h.position != unwanted),
            "did not expect a type suffix hint after an action argument"
        );
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
        assert!(
            hints.iter().all(|h| h.position != unwanted),
            "did not expect a hint after the nested '1'"
        );

        let g_start = text.find("g(1)").unwrap();
        let expected = line_index.position(text, g_start);
        let hint = hints
            .iter()
            .find(|h| h.position == expected)
            .expect("expected a hint before 'g(1)'");
        assert_eq!(label(hint), "n:");
    }

    #[tokio::test]
    async fn call_argument_matching_the_parameter_name_gets_no_hint() {
        let text = "glob n: Nat;\nproc P(n: Nat) = delta;\ninit P(n);";
        let (hints, line_index) = hints_for(text).await;

        let n_start = text.rfind('n').unwrap();
        let unwanted = line_index.position(text, n_start);
        assert!(
            hints.iter().all(|h| h.position != unwanted),
            "did not expect a redundant 'n:' hint before 'n'"
        );
    }

    #[tokio::test]
    async fn equation_condition_gets_no_hint() {
        let text = "sort D;\nmap f: D -> D;\nmap p: D -> Bool;\nvar x: D;\neqn p(x) -> f(x) = x;\ninit delta;";
        let (hints, _line_index) = hints_for(text).await;

        assert!(
            hints.is_empty(),
            "did not expect any hint, got {hints:?}"
        );
    }

    #[tokio::test]
    async fn equation_rhs_application_argument_gets_no_sort_hint() {
        let text =
            "sort D;\nmap f: D -> D;\nmap g: D -> D;\nvar x: D;\neqn f(x) = g(x);\ninit delta;";
        let (hints, _line_index) = hints_for(text).await;

        assert!(
            hints.is_empty(),
            "did not expect any hint, got {hints:?}"
        );
    }

    #[tokio::test]
    async fn process_instantiation_argument_gets_a_name_prefix_hint() {
        let text = "proc P(n: Nat) = delta;\ninit P(1);";
        let (hints, line_index) = hints_for(text).await;

        let one_start = text.rfind('1').unwrap();
        let expected = line_index.position(text, one_start);
        let hint = hints
            .iter()
            .find(|h| h.position == expected)
            .expect("expected a hint before '1'");
        assert_eq!(label(hint), "n:");
        assert_eq!(hint.kind, Some(InlayHintKind::PARAMETER));
    }

    #[tokio::test]
    async fn overloaded_process_argument_hints_the_resolved_overload() {
        // `P(what)` picks the second `P`, whose parameter is named `y` — not the first `P`, which
        // only shares the name and arity but takes an `S` instead of a `T`. The hint must name the
        // overload the type checker actually resolved (`y:`), not the first matching declaration
        // (`x:`).
        let text = "sort S = struct test;\nsort T = struct what;\nact a;\nproc P(x: S) = a;\nproc P(y: T) = delta;\ninit P(what);";
        let (hints, line_index) = hints_for(text).await;

        let what_start = text.rfind("what").unwrap();
        let expected = line_index.position(text, what_start);
        let hint = hints
            .iter()
            .find(|h| h.position == expected)
            .expect("expected a hint before 'what'");
        assert_eq!(label(hint), "y:");
        assert_eq!(hint.kind, Some(InlayHintKind::PARAMETER));
    }

    #[tokio::test]
    async fn struct_field_gets_a_name_prefix_hint() {
        let text = "sort S = struct s(n: Nat);\nact a: S;\nproc P = a(s(5));\ninit P;";
        let (hints, line_index) = hints_for(text).await;

        let five_start = text.find('5').unwrap();
        let expected = line_index.position(text, five_start);
        let hint = hints
            .iter()
            .find(|h| h.position == expected)
            .expect("expected a hint before '5'");
        assert_eq!(label(hint), "n:");
        assert_eq!(hint.kind, Some(InlayHintKind::PARAMETER));
    }

    #[tokio::test]
    async fn equation_lhs_pattern_variable_gets_no_type_suffix_hint() {
        let text = "sort D;\nmap f: D -> D;\nvar x: D;\neqn f(x) = x;\ninit delta;";
        let (hints, _line_index) = hints_for(text).await;

        assert!(
            hints.is_empty(),
            "did not expect any hint, got {hints:?}"
        );
    }

    #[tokio::test]
    async fn equation_lhs_struct_pattern_argument_still_gets_a_name_prefix_hint() {
        let text = "sort S = struct s(n: Nat);\nmap f: S -> S;\nvar y: Nat;\neqn f(s(y)) = s(y);\ninit delta;";
        let (hints, line_index) = hints_for(text).await;

        let lhs_y_start = text.find("s(y))").unwrap() + 2;
        let expected = line_index.position(text, lhs_y_start);
        let hint = hints
            .iter()
            .find(|h| h.position == expected)
            .expect("expected a hint before the LHS struct argument 'y'");
        assert_eq!(label(hint), "n:");
        assert_eq!(hint.kind, Some(InlayHintKind::PARAMETER));
    }

    #[tokio::test]
    async fn propvarinst_argument_gets_a_name_prefix_hint() {
        let text = "pbes mu X(n: Nat) = val(n > 0);\ninit X(1);";
        let (hints, line_index) = pbes_hints_for(text).await;

        let one_start = text.rfind('1').unwrap();
        let expected = line_index.position(text, one_start);
        let hint = hints
            .iter()
            .find(|h| h.position == expected)
            .expect("expected a hint before the init argument '1'");
        assert_eq!(label(hint), "n:");
        assert_eq!(hint.kind, Some(InlayHintKind::PARAMETER));
    }

    #[tokio::test]
    async fn val_expression_argument_gets_hinted_like_a_data_expression() {
        let text =
            "sort S = struct s(n: Nat);\nmap f: S -> Bool;\npbes mu X = val(f(s(5)));\ninit X;";
        let (hints, line_index) = pbes_hints_for(text).await;

        let five_start = text.find('5').unwrap();
        let expected = line_index.position(text, five_start);
        let hint = hints
            .iter()
            .find(|h| h.position == expected)
            .expect("expected a hint before '5' inside val(...)");
        assert_eq!(label(hint), "n:");
        assert_eq!(hint.kind, Some(InlayHintKind::PARAMETER));
    }

    #[tokio::test]
    async fn state_variable_reference_argument_gets_a_name_prefix_hint() {
        let text = "act a: Nat;\nform nu X(n: Nat = 0) . [a(n)]X(1);";
        let (hints, line_index) = modal_hints_for(text).await;

        let one_start = text.rfind('1').unwrap();
        let expected = line_index.position(text, one_start);
        let hint = hints
            .iter()
            .find(|h| h.position == expected)
            .expect("expected a hint before the reference argument '1'");
        assert_eq!(label(hint), "n:");
        assert_eq!(hint.kind, Some(InlayHintKind::PARAMETER));
    }

    #[tokio::test]
    async fn modal_action_argument_gets_no_type_suffix_hint() {
        let text = "act a: Nat;\nform nu X . [a(1)]X;";
        let (hints, line_index) = modal_hints_for(text).await;

        let one_end = text.find("1)]").unwrap() + 1;
        let unwanted = line_index.position(text, one_end);
        assert!(
            hints.iter().all(|h| h.position != unwanted),
            "did not expect a type suffix hint after a modality's action argument"
        );
    }

    /// Writes `files` (relative-path -> contents) into a fresh temp directory and returns it —
    /// mirrors `document.rs`'s own test helper of the same name.
    fn temp_project(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("should create a temp directory");
        for (name, contents) in files {
            let path = dir.path().join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("should create parent directories");
            }
            std::fs::write(path, contents).expect("should write the fixture file");
        }
        dir
    }

    #[tokio::test]
    async fn a_call_inside_an_imported_declarations_body_gets_no_hint() {
        // Regression test: `spec.process_declarations()` includes every `proc` merged in from an
        // `%import`ed file (see `syntax_tree.rs`'s `UntypedProcessSpecification::merge`), so walking
        // it used to hand `push_hint` a call-argument span that belongs to `common.mcrl2`, not
        // `main.mcrl2`. Rendered against `main.mcrl2`'s own `line_index`/text regardless, that
        // clamped to the end of `main.mcrl2` (see `LineIndex::position`'s doc comment) instead of
        // being placed correctly (or, as fixed, not placed in this document at all) — in practice a
        // pile of stray hints stacked on the last line of any file that imports another one.
        let dir = temp_project(&[
            ("main.mcrl2", "%import \"common.mcrl2\"\nproc P(m: Nat) = delta;\ninit P(1);\n"),
            ("common.mcrl2", "proc Callee(n: Nat) = delta;\nproc Caller = Callee(5);\n"),
        ]);
        let main_path = dir.path().join("main.mcrl2");
        let text = std::fs::read_to_string(&main_path).unwrap();
        let (outcome, sources) = crate::parse::parse(SpecKind::Process, text.clone(), Some(main_path)).await;
        let ParseOutcome::Ok(Specification::Process(spec)) = &outcome else {
            panic!("fixture failed to parse");
        };
        let sort_declarations = spec.data_specification.sort_declarations.clone();
        let (checked, sources) = crate::typecheck::typecheck((**spec).clone(), sources).await;
        let TypecheckOutcome::Ok(mut checked) = checked else {
            panic!("fixture failed to typecheck");
        };
        let line_index = LineIndex::new(&text);
        let typing_info = checked.typing_info();
        let whole_document = Range {
            start: Position { line: 0, character: 0 },
            end: Position { line: u32::MAX, character: u32::MAX },
        };

        let end_of_file = line_index.position(&text, text.len());
        let hints = inlay_hints(
            &text,
            &line_index,
            &checked,
            &sort_declarations,
            &typing_info,
            &sources,
            whole_document,
        );

        assert!(
            hints.iter().all(|hint| hint.position != end_of_file),
            "expected no hint clamped to the end of main.mcrl2 from common.mcrl2's own `Callee(5)` call, got {hints:?}"
        );
        // The call local to `main.mcrl2` itself must still be hinted normally.
        let one_start = text.rfind('1').unwrap();
        let expected = line_index.position(&text, one_start);
        let hint = hints
            .iter()
            .find(|h| h.position == expected)
            .expect("expected a hint before the local call's argument '1'");
        assert_eq!(label(hint), "m:");
    }
}
