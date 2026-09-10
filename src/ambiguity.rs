//! Detects an ambiguous prefix-operator shape in expression trees: a prefix
//! operator whose direct operand is itself a looser-precedence prefix
//! operator, where that operand's own subtree eventually reaches an infix
//! operator. In this shape, a different reading of the same source text could
//! plausibly attach the infix operator to the outer prefix operator instead
//! of the inner one, so the expression is ambiguous on the page even though
//! it always parses one particular way.
//!
//! A chain of several looser prefixes in a row (`exists d: D . mu X . A && B`)
//! is the identical shape, just discovered by unwinding through more than one
//! of them; see [`swallows_infix`].
//!
//! [`PrefixShape`] and [`IsInfix`] describe each expression kind's own
//! precedence as plain data, and [`check_prefix_shape`] is the one general
//! check run against every node of every kind that defines them.
//!
//! `crate::diagnostics` turns each hit into a warning, and `crate::code_action`
//! turns it into a quick fix that parenthesizes the inner operator's own span.

use std::ops::ControlFlow;

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
use merc_syntax::SourceMap;
use merc_syntax::Span;
use merc_syntax::Spanned;
use merc_syntax::StateFrm;
use merc_syntax::StateFrmKind;
use merc_syntax::Traverse;
use merc_syntax::UntypedDataSpecification;
use merc_syntax::UntypedPbes;
use merc_syntax::UntypedPres;
use merc_syntax::UntypedProcessSpecification;
use merc_syntax::UntypedStateFrmSpec;

use crate::convert;

/// One occurrence of the ambiguous shape: `outer_span` is the outer (tighter-binding) prefix
/// operator's own span.
#[derive(Debug, Clone)]
pub struct AmbiguousPrefixConflict {
    pub outer_span: Span,
    pub inner_span: Span,
}

impl AmbiguousPrefixConflict {
    /// The span covering the whole ambiguous expression — used both for the diagnostic's range and
    /// to test a requested code-action range against.
    pub fn whole_span(&self) -> Span {
        Span {
            start: self.outer_span.start,
            end: self.inner_span.end,
        }
    }
}

/// Returns `node`'s own outer-prefix precedence level and the single child it wraps, if `node` was
/// built by a *prefix* operator — `None` for a primary, an infix application, or a genuine postfix
/// one. A postfix operator's "operand" is whatever already-parsed expression precedes it, not
/// something freshly parsed as a fresh subexpression, so it can never be the *looser* half of this
/// shape, and it can never compete for the root position the way an infix connective can. Some
/// operators look postfix-shaped (a value trailing an expression) but are genuine infix competitors
/// in disguise; those are handled by [`IsInfix`] instead — see [`statefrm_is_infix`] and
/// [`presexpr_is_infix`].
///
/// One implementation per node kind. Precedence numbers only need to be *consistent within one
/// kind's own implementation* — lower means looser (binds less tightly) — they are not compared
/// across different node kinds.
type PrefixShape<K> = fn(&K) -> Option<(u8, &Spanned<K>)>;

/// Returns whether `node`'s own outermost connective is a genuine infix operator — a competitor for
/// the "root" position against an enclosing prefix operator (see the module doc comment). One
/// implementation per node kind. That's `Binary` alone for `DataExpr`/`PbesExpr`/`ActFrm`, but
/// `StateFrm`/`PresExpr` each have one more: `DataValExprRightMult`/`RightConstantMultiply` look
/// postfix-shaped (a value trailing an expression) but behave as genuine infix competitors — see
/// [`PrefixShape`]'s doc comment.
type IsInfix<K> = fn(&K) -> bool;

fn dataexpr_prefix_shape(kind: &DataExprKind) -> Option<(u8, &DataExpr)> {
    match kind {
        // `Forall`/`Exists`/`Lambda` share the loosest prefix level.
        DataExprKind::Quantifier { body, .. } | DataExprKind::Lambda { body, .. } => {
            Some((0, body))
        }
        // `Minus`/`Negation`/`Size` share the tightest prefix level.
        DataExprKind::Unary { expr, .. } => Some((1, expr)),
        _ => None,
    }
}

fn dataexpr_is_infix(kind: &DataExprKind) -> bool {
    matches!(kind, DataExprKind::Binary { .. })
}

fn statefrm_prefix_shape(kind: &StateFrmKind) -> Option<(u8, &StateFrm)> {
    match kind {
        StateFrmKind::FixedPoint { body, .. } => Some((0, body)),
        StateFrmKind::Quantifier { body, .. } | StateFrmKind::Bound { body, .. } => Some((1, body)),
        StateFrmKind::DataValExprLeftMult(_, expr) => Some((2, expr)),
        StateFrmKind::Modality { expr, .. } => Some((3, expr)),
        StateFrmKind::Unary { expr, .. } => Some((4, expr)),
        _ => None,
    }
}

fn statefrm_is_infix(kind: &StateFrmKind) -> bool {
    // `DataValExprRightMult` (`StateFrm * DataValExpr`) isn't just a suffix on an already-parsed
    // primary the way `DataExpr`'s `Update`/`Application` are (see `PrefixShape`'s doc comment) — it
    // is a genuine infix competitor for the root position, the same as `&&`/`=>`. Missing it here
    // would silently under-detect: a strictly looser prefix (`mu`/`nu`) whose body reaches a
    // `* val(...)` still competes for the root position exactly the way it would against `&&`.
    matches!(
        kind,
        StateFrmKind::Binary { .. } | StateFrmKind::DataValExprRightMult(..)
    )
}

fn pbesexpr_prefix_shape(kind: &PbesExprKind) -> Option<(u8, &PbesExpr)> {
    match kind {
        PbesExprKind::Quantifier { body, .. } => Some((0, body)),
        PbesExprKind::Negation(expr) => Some((1, expr)),
        _ => None,
    }
}

fn pbesexpr_is_infix(kind: &PbesExprKind) -> bool {
    matches!(kind, PbesExprKind::Binary { .. })
}

fn presexpr_prefix_shape(kind: &PresExprKind) -> Option<(u8, &PresExpr)> {
    match kind {
        PresExprKind::Bound { expr, .. } => Some((0, expr)),
        PresExprKind::LeftConstantMultiply { expr, .. } => Some((1, expr)),
        PresExprKind::Negation(expr) => Some((2, expr)),
        _ => None,
    }
}

fn presexpr_is_infix(kind: &PresExprKind) -> bool {
    // As `statefrm_is_infix`: `RightConstantMultiply` (`PresExpr * DataValExpr`) is a genuine infix
    // competitor for the root position, not a fixed-tightest suffix.
    matches!(
        kind,
        PresExprKind::Binary { .. } | PresExprKind::RightConstantMultiply { .. }
    )
}

fn actfrm_prefix_shape(kind: &ActFrmKind) -> Option<(u8, &ActFrm)> {
    match kind {
        ActFrmKind::Quantifier { body, .. } => Some((0, body)),
        ActFrmKind::Negation(expr) => Some((1, expr)),
        _ => None,
    }
}

fn actfrm_is_infix(kind: &ActFrmKind) -> bool {
    matches!(kind, ActFrmKind::Binary { .. })
}

/// Whether `node`'s own subtree eventually reaches a genuine infix application without first
/// escaping through anything but more prefix operators — i.e., whether *something* ends up
/// swallowed into `node`'s body the way the module doc comment describes, however many looser
/// prefixes deep it takes to reach it (`exists d: D . mu X . A && B`: `exists` doesn't touch `&&`
/// directly, `mu` does, but `exists`'s own body is still `mu X . (A && B)` — this is what lets the
/// outer `check_prefix_shape` call for `exists` see through the intervening `mu` to the `&&` it
/// would otherwise be blind to).
fn swallows_infix<K>(
    node: &Spanned<K>,
    prefix_shape: PrefixShape<K>,
    is_infix: IsInfix<K>,
) -> bool {
    is_infix(&node.node)
        || prefix_shape(&node.node)
            .is_some_and(|(_, child)| swallows_infix(child, prefix_shape, is_infix))
}

/// The one check shared by every node kind: is `node` itself a prefix application (`prefix_shape`)
/// whose immediate operand is *also* a prefix application, at a strictly looser precedence level,
/// whose own subtree swallows an infix operator (`swallows_infix`)? If so — and the looser operand
/// isn't already parenthesized by hand — that's the shape the module doc comment describes, and
/// `hits` gets a new [`AmbiguousPrefixConflict`] for it.
///
/// Deliberately `<`, not `<=`: two prefixes sharing one precedence *level* (as `DataExpr` does for
/// `Minus`/`Negation`/`Size`, or for `Forall`/`Exists`/`Lambda`) never reproduce this shape. The
/// tight end of a precedence level (`Unary`-style operators) never reaches an infix operator to
/// begin with, since its own precedence is tighter than every infix operator in the same kind. And
/// the loose end (quantifiers/binders) is always the unique loosest precedence in its kind — looser
/// than every infix operator (and, in `PresExpr`/`StateFrm`, every right-constant-multiply too, see
/// [`statefrm_is_infix`]/[`presexpr_is_infix`]) — so a same-level chain of binders always wins the
/// root competition outright and swallows the whole thing, exactly like a single one does; there is
/// no looser rival for it to lose to. So a same-level chain is a proven non-issue, not an unverified
/// one, and no `<=` is needed.
fn check_prefix_shape<K>(node: &Spanned<K>, prefix_shape: PrefixShape<K>, is_infix: IsInfix<K>, text: &str, sources: &SourceMap, hits: &mut Vec<AmbiguousPrefixConflict>) {
    let Some((outer_level, child)) = prefix_shape(&node.node) else {
        return;
    };
    let Some((inner_level, _)) = prefix_shape(&child.node) else {
        return;
    };
    if inner_level < outer_level
        && swallows_infix(child, prefix_shape, is_infix)
        && !is_already_parenthesized(text, sources, &child.span)
    {
        hits.push(AmbiguousPrefixConflict {
            outer_span: node.span.clone(),
            inner_span: child.span.clone(),
        });
    }
}

/// True when `span` is already wrapped in a matching `(...)` in its own source text. Parentheses
/// are transparent to the parsed tree — an operator the author already disambiguated by hand looks
/// identical, node-for-node, to one that wasn't — so this is the only place that distinction can
/// still be recovered, by checking the raw source text immediately around `span` instead of the
/// tree.
fn is_already_parenthesized(text: &str, sources: &SourceMap, span: &Span) -> bool {
    let (text, span) = convert::local_text_and_span(text, sources, span);
    text[..span.start].trim_end().ends_with('(') && text[span.end..].trim_start().starts_with(')')
}

/// Finds every occurrence of the shape in one [`DataExpr`] subtree, appending to `hits`.
fn find_in_dataexpr(expr: &DataExpr, text: &str, sources: &SourceMap, hits: &mut Vec<AmbiguousPrefixConflict>) {
    expr.visit::<(), _>(|node| {
        check_prefix_shape(node, dataexpr_prefix_shape, dataexpr_is_infix, text, sources, hits);
        ControlFlow::Continue(())
    });
}

/// As [`find_in_dataexpr`], across a whole process specification: every process declaration's
/// body, `init`, and the data specification's own equations. `text` is the root document's own
/// text and `sources` its [`SourceMap`] (empty for a plain, import-free parse) — together they let
/// [`is_already_parenthesized`] resolve a node's span correctly even when that node came from
/// something the document `%import`s rather than from `text` itself.
pub fn find_in_process_specification(spec: &UntypedProcessSpecification, text: &str, sources: &SourceMap) -> Vec<AmbiguousPrefixConflict> {
    let mut hits = Vec::new();
    for decl in &spec.process_declarations {
        walk_process_expr(&decl.body, text, sources, &mut hits);
    }
    if let Some(init) = &spec.init {
        walk_process_expr(init, text, sources, &mut hits);
    }
    walk_data_specification(&spec.data_specification, text, sources, &mut hits);
    hits
}

/// As [`find_in_process_specification`], for a PBES: every equation's formula, `init`'s own
/// arguments, and the data specification's equations.
pub fn find_in_pbes_specification(spec: &UntypedPbes, text: &str, sources: &SourceMap) -> Vec<AmbiguousPrefixConflict> {
    let mut hits = Vec::new();
    for eqn in &spec.equations {
        walk_pbes_expr(&eqn.formula, text, sources, &mut hits);
    }
    for argument in &spec.init.node.arguments {
        find_in_dataexpr(argument, text, sources, &mut hits);
    }
    walk_data_specification(&spec.data_specification, text, sources, &mut hits);
    hits
}

/// As [`find_in_pbes_specification`], for a PRES.
pub fn find_in_pres_specification(spec: &UntypedPres, text: &str, sources: &SourceMap) -> Vec<AmbiguousPrefixConflict> {
    let mut hits = Vec::new();
    for eqn in &spec.equations {
        walk_pres_expr(&eqn.formula, text, sources, &mut hits);
    }
    for argument in &spec.init.node.arguments {
        find_in_dataexpr(argument, text, sources, &mut hits);
    }
    walk_data_specification(&spec.data_specification, text, sources, &mut hits);
    hits
}

/// As [`find_in_process_specification`], for a modal (mu-calculus) formula.
pub fn find_in_modal_specification(spec: &UntypedStateFrmSpec, text: &str, sources: &SourceMap) -> Vec<AmbiguousPrefixConflict> {
    let mut hits = Vec::new();
    walk_state_frm(&spec.formula, text, sources, &mut hits);
    walk_data_specification(&spec.data_specification, text, sources, &mut hits);
    hits
}

/// Walks every `ProcessExpr` in `expr`'s subtree, picking out each node's `DataExpr`-bearing
/// fields — the ones a plain traversal won't reach on its own — and checking them.
/// Process-algebra operators themselves (`.`/`+`/`||`/...) are out of scope for this module, so
/// `ProcessExprKind` gets no `check_prefix_shape` call of its own here, only the `DataExpr`s nested
/// inside it.
fn walk_process_expr(expr: &ProcessExpr, text: &str, sources: &SourceMap, hits: &mut Vec<AmbiguousPrefixConflict>) {
    expr.visit::<(), _>(|node| {
        match &node.node {
            ProcessExprKind::Action(_, args) => {
                for arg in args {
                    find_in_dataexpr(arg, text, sources, hits);
                }
            }
            ProcessExprKind::Id(_, assignments) => {
                for assignment in assignments {
                    find_in_dataexpr(&assignment.node.expr, text, sources, hits);
                }
            }
            ProcessExprKind::Dist { expr: weight, .. } => find_in_dataexpr(weight, text, sources, hits),
            ProcessExprKind::Condition { condition, .. } => find_in_dataexpr(condition, text, sources, hits),
            ProcessExprKind::At { operand, .. } => find_in_dataexpr(operand, text, sources, hits),
            _ => {}
        }
        ControlFlow::Continue(())
    });
}

/// As [`walk_process_expr`], for a [`PbesExpr`] tree — `.visit()` also runs [`check_prefix_shape`]
/// on every `PbesExpr` node along the way (see the module doc comment), not just the `DataExpr`s
/// nested inside it.
fn walk_pbes_expr(expr: &PbesExpr, text: &str, sources: &SourceMap, hits: &mut Vec<AmbiguousPrefixConflict>) {
    expr.visit::<(), _>(|node| {
        check_prefix_shape(node, pbesexpr_prefix_shape, pbesexpr_is_infix, text, sources, hits);
        match &node.node {
            PbesExprKind::DataValExpr(value) => find_in_dataexpr(value, text, sources, hits),
            PbesExprKind::PropVarInst(inst) => {
                for argument in &inst.node.arguments {
                    find_in_dataexpr(argument, text, sources, hits);
                }
            }
            _ => {}
        }
        ControlFlow::Continue(())
    });
}

/// As [`walk_pbes_expr`], for a [`PresExpr`] tree.
fn walk_pres_expr(expr: &PresExpr, text: &str, sources: &SourceMap, hits: &mut Vec<AmbiguousPrefixConflict>) {
    expr.visit::<(), _>(|node| {
        check_prefix_shape(node, presexpr_prefix_shape, presexpr_is_infix, text, sources, hits);
        match &node.node {
            PresExprKind::DataValExpr(value) => find_in_dataexpr(value, text, sources, hits),
            PresExprKind::PropVarInst(inst) => {
                for argument in &inst.node.arguments {
                    find_in_dataexpr(argument, text, sources, hits);
                }
            }
            PresExprKind::RightConstantMultiply { constant, .. }
            | PresExprKind::LeftConstantMultiply { constant, .. } => {
                find_in_dataexpr(constant, text, sources, hits);
            }
            _ => {}
        }
        ControlFlow::Continue(())
    });
}

/// As [`walk_pbes_expr`], for a modal (mu-calculus) state formula — `.visit()` supplies the descent
/// into every same-typed child (a `Unary`/`Binary`/`Quantifier`/`Bound`/`FixedPoint`/`Modality`'s
/// own `StateFrm` operand) for free, including running [`check_prefix_shape`] on each one, so this
/// only has to reach into the *other*-typed fields `Traverse` won't cross into on its own: a
/// `DataExpr` (`Delay`/`Yaled`'s time, `Id`/`Resolved`'s arguments, `DataValExpr(LeftMult)`, a
/// `FixedPoint` variable's initial values) or a `RegFrm` (a `Modality`'s own formula).
fn walk_state_frm(formula: &StateFrm, text: &str, sources: &SourceMap, hits: &mut Vec<AmbiguousPrefixConflict>) {
    formula.visit::<(), _>(|node| {
        check_prefix_shape(node, statefrm_prefix_shape, statefrm_is_infix, text, sources, hits);
        match &node.node {
            StateFrmKind::Delay(time) | StateFrmKind::Yaled(time) => {
                if let Some(time) = time {
                    find_in_dataexpr(time, text, sources, hits);
                }
            }
            StateFrmKind::Id(_, arguments) | StateFrmKind::Resolved(_, arguments, _) => {
                for argument in arguments {
                    find_in_dataexpr(argument, text, sources, hits);
                }
            }
            StateFrmKind::DataValExpr(expr) => find_in_dataexpr(expr, text, sources, hits),
            StateFrmKind::DataValExprLeftMult(constant, _) => {
                find_in_dataexpr(constant, text, sources, hits)
            }
            StateFrmKind::DataValExprRightMult(_, constant) => {
                find_in_dataexpr(constant, text, sources, hits)
            }
            StateFrmKind::Modality { formula: reg, .. } => walk_reg_frm(reg, text, sources, hits),
            StateFrmKind::FixedPoint { variable, .. } => {
                for argument in &variable.arguments {
                    find_in_dataexpr(&argument.expr, text, sources, hits);
                }
            }
            _ => {}
        }
        ControlFlow::Continue(())
    });
}

/// As [`walk_state_frm`], for a modality's regular formula (`[a*]X`'s `a*`) — reaches into the
/// `ActFrm` a `RegFrmKind::Action` carries, the one `RegFrm` field `Traverse` won't cross into on
/// its own; `Iteration`/`Plus`/`Sequence`/`Choice`'s own `RegFrm` children need no walking here
/// since nothing in this module treats `RegFrmKind` as a prefix-shaped kind of its own (`*` and `+`
/// are postfix, `.`/`+` in the regular-formula sense are infix — see [`prefix_shape`]'s doc comment
/// on why postfix operators are out of scope, and there's no *prefix* regular-formula operator to
/// even compete with them in the first place).
fn walk_reg_frm(formula: &RegFrm, text: &str, sources: &SourceMap, hits: &mut Vec<AmbiguousPrefixConflict>) {
    match &formula.node {
        RegFrmKind::Action(action) => walk_act_frm(action, text, sources, hits),
        RegFrmKind::Iteration(inner) | RegFrmKind::Plus(inner) => walk_reg_frm(inner, text, sources, hits),
        RegFrmKind::Sequence { lhs, rhs } | RegFrmKind::Choice { lhs, rhs } => {
            walk_reg_frm(lhs, text, sources, hits);
            walk_reg_frm(rhs, text, sources, hits);
        }
    }
}

/// As [`walk_pbes_expr`], for an action formula (`a(1) && !b`).
fn walk_act_frm(formula: &ActFrm, text: &str, sources: &SourceMap, hits: &mut Vec<AmbiguousPrefixConflict>) {
    formula.visit::<(), _>(|node| {
        check_prefix_shape(node, actfrm_prefix_shape, actfrm_is_infix, text, sources, hits);
        match &node.node {
            ActFrmKind::MultAct(multi_action) => {
                for action in &multi_action.actions {
                    for argument in &action.args {
                        find_in_dataexpr(argument, text, sources, hits);
                    }
                }
            }
            ActFrmKind::DataExprVal(expr) => find_in_dataexpr(expr, text, sources, hits),
            _ => {}
        }
        ControlFlow::Continue(())
    });
}

/// Every equation's LHS/RHS/condition, in `data`'s own `var ... eqn ...` blocks.
fn walk_data_specification(data: &UntypedDataSpecification, text: &str, sources: &SourceMap, hits: &mut Vec<AmbiguousPrefixConflict>) {
    for eqn_spec in &data.equation_declarations {
        for eqn in &eqn_spec.node.equations {
            find_in_dataexpr(&eqn.lhs, text, sources, hits);
            find_in_dataexpr(&eqn.rhs, text, sources, hits);
            if let Some(condition) = &eqn.condition {
                find_in_dataexpr(condition, text, sources, hits);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hits(text: &str) -> Vec<AmbiguousPrefixConflict> {
        let spec =
            merc_syntax::UntypedProcessSpecification::parse(text).expect("fixture should parse");
        find_in_process_specification(&spec, text, &SourceMap::new())
    }

    fn modal_hits(text: &str) -> Vec<AmbiguousPrefixConflict> {
        let spec = merc_syntax::UntypedStateFrmSpec::parse(text).expect("fixture should parse");
        find_in_modal_specification(&spec, text, &SourceMap::new())
    }

    #[test]
    fn flags_negated_exists_with_a_binary_body() {
        let text = "sort D;\nmap q: Bool;\ninit (!exists d: D . d == d && q) -> delta;";
        let found = hits(text);
        assert_eq!(found.len(), 1);
        assert_eq!(
            &text[found[0].inner_span.start..found[0].inner_span.end],
            "exists d: D . d == d && q"
        );
    }

    #[test]
    fn flags_negated_forall_the_same_way() {
        let text = "sort D;\nmap q: Bool;\ninit (!forall d: D . d == d && q) -> delta;";
        assert_eq!(hits(text).len(), 1);
    }

    #[test]
    fn flags_a_unary_minus_wrapping_a_lambda() {
        // Same kind, a different pairing: `Minus`/`Negation`/`Size` are the tight prefix level,
        // `Lambda` the loose one — not just `!`/`exists`.
        let text =
            "sort D;\nmap f: (D -> Nat) -> Nat;\ninit (f(-lambda d: D . d + d) == 0) -> delta;";
        assert_eq!(hits(text).len(), 1);
    }

    #[test]
    fn flags_through_a_chain_of_same_level_looser_prefixes_before_the_infix() {
        // `exists` doesn't directly touch `&&` — `lambda` does — but `exists`'s own body is still
        // `lambda e: D . (d == e && q)`, so the outer `!` still sees a swallowed infix through
        // `lambda` (see `swallows_infix`'s doc comment for exactly this chained-unwrap case).
        // `exists`/`lambda` themselves aren't a *second* hit here: they share one precedence level,
        // and `check_prefix_shape` only flags a *strictly* looser child
        // (see its own doc comment) — same reasoning as
        // `does_not_flag_two_prefixes_sharing_one_precedence_level`, just on the loose end of the
        // table instead of the tight end.
        let text =
            "sort D;\nmap q: Bool;\ninit (!exists d: D . lambda e: D . d == e && q) -> delta;";
        assert_eq!(hits(text).len(), 1);
    }

    #[test]
    fn does_not_flag_a_quantifier_with_no_infix_body() {
        // `d` alone (no infix operator at all) and `!d` (a `Unary`, not `Binary`) both bound the
        // quantifier's body the same way in both grammars — see the module doc comment on why only
        // a `Binary` body creates the competing-infix shape. `d == d` would *not* belong here:
        // `==` is itself the infix operator this lint looks for, so that shape is a true positive
        // (see `flags_negated_exists_with_a_binary_body`), not a case to assert empty on.
        let text =
            "sort D;\ninit (!exists d: D . d) -> delta;\nproc Q = (!exists e: D . !e) -> delta;";
        assert!(hits(text).is_empty());
    }

    #[test]
    fn does_not_flag_an_already_parenthesized_quantifier() {
        let text = "sort D;\nmap q: Bool;\ninit (!(exists d: D . d == d && q)) -> delta;";
        assert!(hits(text).is_empty());
    }

    #[test]
    fn does_not_flag_a_bare_negation_with_no_quantifier() {
        let text = "map q: Bool;\ninit (!q && q) -> delta;";
        assert!(hits(text).is_empty());
    }

    #[test]
    fn does_not_flag_two_prefixes_sharing_one_precedence_level() {
        // `Minus` and `Negation` share DataExpr's tight prefix level — not a case this checks (see
        // `check_prefix_shape`'s doc comment on why `<`, not `<=`).
        let text = "sort D;\nmap q: Bool;\ninit (-!q == 0) -> delta;";
        assert!(hits(text).is_empty());
    }

    #[test]
    fn flags_a_state_formula_negation_over_a_quantifier() {
        let text = "act a: Bool;\nform nu X . !exists d: Bool . d && [a(d)]X;";
        let found = modal_hits(text);
        assert_eq!(found.len(), 1);
    }

    #[test]
    fn flags_a_state_formula_modality_over_a_fixed_point() {
        // `Modality` (tighter) directly wrapping `FixedPoint` (the loosest StateFrm prefix) whose
        // own body reaches `&&` — the same shape, different pair, in the same grammar.
        let text = "act a: Bool;\nform [a(true)]mu X . true && X;";
        assert_eq!(modal_hits(text).len(), 1);
    }

    #[test]
    fn does_not_flag_a_bare_chain_of_same_level_quantifiers_with_no_enclosing_prefix() {
        // Proof, not assumption (see `check_prefix_shape`'s doc comment): `exists` is already the
        // unique loosest-precedence connective among `DataExpr`'s own operators, so with nothing
        // tighter enclosing it, `exists` always wins the root competition against `&&` on its own —
        // the parse is unambiguous without any `!` to create the divergence.
        let text =
            "sort D;\nmap q: Bool;\ninit (exists d: D . lambda e: D . d == e && q) -> delta;";
        assert!(hits(text).is_empty());
    }

    #[test]
    fn flags_a_state_formula_modality_over_a_fixed_point_reaching_a_right_constant_multiply() {
        // `DataValExprRightMult` (`StateFrm * DataValExpr`) is a genuine infix competitor for the
        // root position, not a fixed-tightest suffix like `DataExpr`'s `Application`/`Update` — so
        // `statefrm_is_infix` must count it, the same way it already counts `&&`/`=>`/etc. Without
        // that, this fixture would produce zero hits even though the shape is identical to
        // `flags_a_state_formula_modality_over_a_fixed_point`, just with a `*` in place of `&&`.
        let text = "act a: Bool;\nform [a(true)]mu X . X * val(1);";
        assert_eq!(modal_hits(text).len(), 1);
    }

    #[test]
    fn flags_a_pres_negation_over_a_bound_reaching_a_right_constant_multiply() {
        // As the `StateFrm` case above, but for `PresExpr`'s `RightConstantMultiply`
        // (`PresExpr * DataValExpr`): negation is the tightest prefix (level 2), directly wrapping
        // `sup` (loosest, level 0) whose own body reaches the `* val(...)`. Unlike every other
        // expression kind here (Data/StateFrm/ActFrm/Pbes), PRES's own negation token is `-`, not
        // `!` (`PresExprNegation = { "-" }` in the grammar).
        let text = "pres mu X = -sup n: Nat . X * val(n); init X;";
        let spec = merc_syntax::UntypedPres::parse(text).expect("fixture should parse");
        assert_eq!(find_in_pres_specification(&spec, text, &SourceMap::new()).len(), 1);
    }
}
