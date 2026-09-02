//! Finds the sort-name occurrence (if any) at a source offset, and looks up its declaring `sort`
//! block — the syntactic counterpart of [`crate::goto_definition`]'s `ResolvedName`-based lookup,
//! which doesn't cover sort references at all: a sort name never appears inside a checked
//! `DataExpr`, only in declaration signatures (`cons`/`map`/`var`/`act`/`proc`/`glob`, a sort
//! alias's own right-hand side, a `lambda`/quantifier/comprehension binder's sort, …) that
//! `merc_typecheck::TypingInfo` doesn't index at all — see its module doc comment.
//!
//! Works over the *raw*, un-type-checked [`Specification`] — like
//! [`crate::document::Document::parsed_process_specification`], since a sort reference's own span
//! survives resolution unchanged, and this way hover/goto-def for a sort keeps working even before
//! the rest of the document type checks (the raw tree is available whenever parsing succeeded, not
//! only once it type checks too).
//!
//! `merc_syntax::Traverse` doesn't cross between node types (a `SortExpression` traversal doesn't
//! descend into a `DataExpr`/`ProcessExpr`/`PbesExpr`, and vice versa — see its module doc
//! comment), so this walks each tree by hand, the same way `inlay_hints.rs` and `symbols.rs` do,
//! recursing into a [`SortExpression`] itself only via [`Traverse`], which *does* apply there.

use std::ops::ControlFlow;

use merc_syntax::DataExpr;
use merc_syntax::DataExprKind;
use merc_syntax::PbesExpr;
use merc_syntax::PbesExprKind;
use merc_syntax::ProcessExpr;
use merc_syntax::ProcessExprKind;
use merc_syntax::SortDecl;
use merc_syntax::SortExpression;
use merc_syntax::SortExpressionKind;
use merc_syntax::Span;
use merc_syntax::Traverse;
use merc_syntax::UntypedDataSpecification;
use merc_syntax::UntypedPbes;
use merc_syntax::UntypedProcessSpecification;

use crate::parse::Specification;

/// A sort-name occurrence: the name itself, and the span of just that occurrence — not the whole
/// enclosing sort expression (`D` inside `List(D)`, not `List(D)` itself).
pub(crate) struct SortRef {
    pub name: String,
    pub span: Span,
}

/// The sort-name occurrence at `offset`, if any — see the module doc comment for where this
/// looks. `None` for a [`Specification::Pres`]: no caller reaches this for a PRES document yet
/// (it has no type checker upstream at all, so neither hover nor goto-def is wired for it — see
/// `crate::parse`'s module doc comment).
pub(crate) fn sort_ref_at(spec: &Specification, offset: usize) -> Option<SortRef> {
    let (name, span) = match spec {
        Specification::Process(spec) => process_specification_sort_ref_at(spec, offset),
        Specification::Pbes(spec) => pbes_sort_ref_at(spec, offset),
        Specification::Pres(_) => None,
    }?;
    Some(SortRef { name, span })
}

/// The `sort` block declaring `name`, if the specification's data specification has one. `None`
/// for a built-in (`Bool`, `Nat`, `List`, …) or any other name with no user declaration to jump
/// to.
pub(crate) fn find_sort_declaration<'a>(spec: &'a Specification, name: &str) -> Option<&'a SortDecl> {
    let declarations = match spec {
        Specification::Process(spec) => &spec.data_specification.sort_declarations,
        Specification::Pbes(spec) => &spec.data_specification.sort_declarations,
        Specification::Pres(spec) => &spec.data_specification.sort_declarations,
    };
    declarations.iter().find(|decl| decl.identifier == name)
}

/// The innermost `Reference`/`Resolved` leaf of `sort` whose own span contains `offset`, if any.
/// `sort`'s compound kinds (`Product`, `Function`, `FlattenedFunction`, `Complex`, `Struct`) are
/// walked via [`Traverse`] until a named leaf is reached; `Simple` (`Bool`, `Nat`, …) never
/// matches — those parse straight to `Simple`, never `Reference`, so there is no identifier
/// occurrence to report for one in the first place. `Resolved` only ever occurs after type
/// checking, which never touches the raw tree this is called on in practice, but is handled all
/// the same so this stays correct if a future caller ever feeds it a checked tree.
fn leaf_sort_ref_at(sort: &SortExpression, offset: usize) -> Option<(String, Span)> {
    if sort.span.start > offset || offset > sort.span.end {
        return None;
    }
    sort.visit(|node| {
        let name = match &node.node {
            SortExpressionKind::Reference(name) => name,
            SortExpressionKind::Resolved(name, _) => name,
            _ => return ControlFlow::Continue(()),
        };
        if node.span.start <= offset && offset <= node.span.end {
            ControlFlow::Break((name.clone(), node.span.clone()))
        } else {
            ControlFlow::Continue(())
        }
    })
}

/// Every place in a data specification's own declarations where a `SortExpression` can occur:
/// `cons`/`map` signatures, a `var`-block declaration, a sort alias's own right-hand side
/// (including a desugared `struct`'s own field sorts, reached via `Traverse` inside
/// [`leaf_sort_ref_at`]), and a `lambda`/quantifier/comprehension binder inside an equation.
/// Shared by every entry point below.
fn data_specification_sort_ref_at(data: &UntypedDataSpecification, offset: usize) -> Option<(String, Span)> {
    data.sort_declarations
        .iter()
        .filter_map(|decl| decl.expr.as_ref())
        .find_map(|expr| leaf_sort_ref_at(expr, offset))
        .or_else(|| data.constructor_declarations.iter().find_map(|decl| leaf_sort_ref_at(&decl.sort, offset)))
        .or_else(|| data.map_declarations.iter().find_map(|decl| leaf_sort_ref_at(&decl.sort, offset)))
        .or_else(|| {
            data.equation_declarations
                .iter()
                .find_map(|eqn_spec| eqn_spec.variables.iter().find_map(|var| leaf_sort_ref_at(&var.sort, offset)))
        })
        .or_else(|| {
            data.equation_declarations
                .iter()
                .flat_map(|eqn_spec| &eqn_spec.equations)
                .find_map(|eqn| {
                    data_expr_sort_ref_at(&eqn.lhs, offset)
                        .or_else(|| data_expr_sort_ref_at(&eqn.rhs, offset))
                        .or_else(|| eqn.condition.as_ref().and_then(|condition| data_expr_sort_ref_at(condition, offset)))
                })
        })
}

/// Every `IdDecl`-bound variable's sort inside `expr` — a `lambda`, a quantifier, or a set/bag
/// comprehension's own binder — since [`Traverse`] doesn't descend into an `IdDecl`'s own sort,
/// only into same-typed (`DataExpr`) children.
fn data_expr_sort_ref_at(expr: &DataExpr, offset: usize) -> Option<(String, Span)> {
    expr.visit(|node| {
        let found = match &node.node {
            DataExprKind::Lambda { variables, .. } | DataExprKind::Quantifier { variables, .. } => {
                variables.iter().find_map(|var| leaf_sort_ref_at(&var.sort, offset))
            }
            DataExprKind::SetBagComp { variable, .. } => leaf_sort_ref_at(&variable.sort, offset),
            _ => None,
        };
        match found {
            Some(value) => ControlFlow::Break(value),
            None => ControlFlow::Continue(()),
        }
    })
}

/// As [`data_expr_sort_ref_at`], for a `sum`/`dist` binder inside `expr`'s `ProcessExpr` tree,
/// plus every `DataExpr` reachable from it (an action's arguments, an assignment's value, a
/// `dist` weight, a condition, an `at` time) — the same fields `inlay_hints::walk_process_expr`
/// hand-walks, for the same reason (see this module's doc comment).
fn process_expr_sort_ref_at(expr: &ProcessExpr, offset: usize) -> Option<(String, Span)> {
    expr.visit(|node| {
        let found = match &node.node {
            ProcessExprKind::Sum { variables, .. } | ProcessExprKind::Dist { variables, .. } => {
                variables.iter().find_map(|var| leaf_sort_ref_at(&var.sort, offset))
            }
            _ => None,
        }
        .or_else(|| match &node.node {
            ProcessExprKind::Action(_, args) => args.iter().find_map(|arg| data_expr_sort_ref_at(arg, offset)),
            ProcessExprKind::Id(_, assignments) => assignments.iter().find_map(|assignment| data_expr_sort_ref_at(&assignment.node.expr, offset)),
            ProcessExprKind::Dist { expr: weight, .. } => data_expr_sort_ref_at(weight, offset),
            ProcessExprKind::Condition { condition, .. } => data_expr_sort_ref_at(condition, offset),
            ProcessExprKind::At { operand, .. } => data_expr_sort_ref_at(operand, offset),
            _ => None,
        });
        match found {
            Some(value) => ControlFlow::Break(value),
            None => ControlFlow::Continue(()),
        }
    })
}

/// As [`process_expr_sort_ref_at`], for a PBES formula: a quantifier's own binder, plus every
/// `DataExpr` reachable from it (a `val(...)` expression, a propositional-variable
/// instantiation's arguments).
fn pbes_expr_sort_ref_at(expr: &PbesExpr, offset: usize) -> Option<(String, Span)> {
    expr.visit(|node| {
        let found = match &node.node {
            PbesExprKind::Quantifier { variables, .. } => variables.iter().find_map(|var| leaf_sort_ref_at(&var.sort, offset)),
            _ => None,
        }
        .or_else(|| match &node.node {
            PbesExprKind::DataValExpr(value) => data_expr_sort_ref_at(value, offset),
            PbesExprKind::PropVarInst(inst) => inst.node.arguments.iter().find_map(|arg| data_expr_sort_ref_at(arg, offset)),
            _ => None,
        });
        match found {
            Some(value) => ControlFlow::Break(value),
            None => ControlFlow::Continue(()),
        }
    })
}

/// [`sort_ref_at`]'s process-specification half: the data specification (see
/// [`data_specification_sort_ref_at`]), `glob` variables, `act` declarations' argument sorts, and
/// every `proc` declaration's parameters and body, plus `init`.
fn process_specification_sort_ref_at(spec: &UntypedProcessSpecification, offset: usize) -> Option<(String, Span)> {
    data_specification_sort_ref_at(&spec.data_specification, offset)
        .or_else(|| spec.global_variables.iter().find_map(|decl| leaf_sort_ref_at(&decl.sort, offset)))
        .or_else(|| spec.action_declarations.iter().flat_map(|decl| &decl.args).find_map(|sort| leaf_sort_ref_at(sort, offset)))
        .or_else(|| {
            spec.process_declarations.iter().find_map(|decl| {
                decl.params
                    .iter()
                    .find_map(|param| leaf_sort_ref_at(&param.sort, offset))
                    .or_else(|| process_expr_sort_ref_at(&decl.body, offset))
            })
        })
        .or_else(|| spec.init.as_ref().and_then(|init| process_expr_sort_ref_at(init, offset)))
}

/// [`sort_ref_at`]'s PBES half: the data specification, `glob` variables, and every equation's
/// parameters and formula, plus `init`'s arguments.
fn pbes_sort_ref_at(spec: &UntypedPbes, offset: usize) -> Option<(String, Span)> {
    data_specification_sort_ref_at(&spec.data_specification, offset)
        .or_else(|| spec.global_variables.iter().find_map(|decl| leaf_sort_ref_at(&decl.sort, offset)))
        .or_else(|| {
            spec.equations.iter().find_map(|eqn| {
                eqn.variable
                    .parameters
                    .iter()
                    .find_map(|param| leaf_sort_ref_at(&param.sort, offset))
                    .or_else(|| pbes_expr_sort_ref_at(&eqn.formula, offset))
            })
        })
        .or_else(|| spec.init.node.arguments.iter().find_map(|arg| data_expr_sort_ref_at(arg, offset)))
}
