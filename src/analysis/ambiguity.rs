//! Detects an ambiguous prefix-operator shape: a prefix operator whose direct operand is itself a
//! looser-precedence prefix operator whose own subtree eventually reaches an infix operator. A
//! different reading could plausibly attach that infix operator to the outer prefix instead of the
//! inner one, so the expression looks ambiguous even though it always parses one particular way.
//!
//! A chain of several looser prefixes (`exists d: D . mu X . A && B`) is the identical shape,
//! found by unwinding through more than one of them; see [`swallows_infix`].
//!
//! `StateFrmKind::DataValExprRightMult` (`StateFrm * DataValExpr`) and
//! `PresExprKind::RightConstantMultiply` are `Fixity::Postfix` per the grammar, but sit in the
//! middle of their precedence table and genuinely compete for the root the same way `&&`/`=>` do
//! (unlike a true postfix suffix like `Application`/`Update`, always tightest, never competing).
//! `statefrm_is_infix`/`presexpr_is_infix` count them as infix on top of `Fixity`, so e.g.
//! `[a(true)]mu X . X * val(1)` is flagged the same as `[a(true)]mu X . true && X`.
//!
//! [`PrefixShape`] and [`IsInfix`] describe each kind's own precedence as plain data, and
//! [`check_prefix_shape`] is the one general check run against every node of every kind.

use std::ops::ControlFlow;

use merc_syntax::ActFrmKind;
use merc_syntax::DataExpr;
use merc_syntax::Fixity;
use merc_syntax::MixedNode;
use merc_syntax::Operator;
use merc_syntax::PbesExpr;
use merc_syntax::PbesExprKind;
use merc_syntax::PresExpr;
use merc_syntax::PresExprKind;
use merc_syntax::ProcessExpr;
use merc_syntax::ProcessExprKind;
use merc_syntax::SourceMap;
use merc_syntax::Span;
use merc_syntax::Spanned;
use merc_syntax::StateFrm;
use merc_syntax::StateFrmKind;
use merc_syntax::TakeRecursiveChildren;
use merc_syntax::Traverse;
use merc_syntax::UntypedDataSpecification;
use merc_syntax::UntypedPbes;
use merc_syntax::UntypedPres;
use merc_syntax::UntypedProcessSpecification;
use merc_syntax::UntypedStateFrmSpec;

use crate::analysis::convert;

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

/// Every process declaration's body, `init`, and the data specification's equations. `text`/
/// `sources` are the root document's text and [`SourceMap`] (empty for a plain parse), letting
/// [`is_already_parenthesized`] resolve spans that came from an `%import`ed document.
pub fn find_in_process_specification(
    spec: &UntypedProcessSpecification,
    text: &str,
    sources: &SourceMap,
) -> Vec<AmbiguousPrefixConflict> {
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
pub fn find_in_pbes_specification(
    spec: &UntypedPbes,
    text: &str,
    sources: &SourceMap,
) -> Vec<AmbiguousPrefixConflict> {
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
pub fn find_in_pres_specification(
    spec: &UntypedPres,
    text: &str,
    sources: &SourceMap,
) -> Vec<AmbiguousPrefixConflict> {
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
pub fn find_in_modal_specification(
    spec: &UntypedStateFrmSpec,
    text: &str,
    sources: &SourceMap,
) -> Vec<AmbiguousPrefixConflict> {
    let mut hits = Vec::new();
    walk_state_frm(&spec.formula, text, sources, &mut hits);
    walk_data_specification(&spec.data_specification, text, sources, &mut hits);
    hits
}

/// `node`'s prefix precedence level and the child it wraps, or `None` if it isn't a prefix op.
type PrefixShape<K> = fn(&K) -> Option<(u8, &Spanned<K>)>;

/// Whether `node`'s own outermost connective is a genuine infix operator.
type IsInfix<K> = fn(&K) -> bool;

/// Generic [`PrefixShape`] for any [`Operator`]-implementing kind.
fn prefix_shape<K: Operator>(kind: &K) -> Option<(u8, &Spanned<K>)> {
    match kind.fixity() {
        Fixity::Prefix(level) => kind.operand().map(|operand| (level, operand)),
        _ => None,
    }
}

/// Generic [`IsInfix`] for any [`Operator`]-implementing kind.
fn is_infix<K: Operator>(kind: &K) -> bool {
    matches!(kind.fixity(), Fixity::Infix(..))
}

fn statefrm_is_infix(kind: &StateFrmKind) -> bool {
    is_infix(kind) || matches!(kind, StateFrmKind::DataValExprRightMult(..))
}

fn presexpr_is_infix(kind: &PresExprKind) -> bool {
    // As `statefrm_is_infix`.
    is_infix(kind) || matches!(kind, PresExprKind::RightConstantMultiply { .. })
}

/// Whether `node`'s subtree reaches a genuine infix application through prefix operators alone.
fn swallows_infix<K: TakeRecursiveChildren>(
    node: &Spanned<K>,
    prefix_shape: PrefixShape<K>,
    is_infix: IsInfix<K>,
) -> bool {
    is_infix(&node.node)
        || prefix_shape(&node.node)
            .is_some_and(|(_, child)| swallows_infix(child, prefix_shape, is_infix))
}

/// The one check shared by every node kind: is `node` a prefix application whose operand is also a
/// strictly looser prefix application that swallows an infix operator, and isn't already
/// parenthesized? If so, `hits` gets a new [`AmbiguousPrefixConflict`].
fn check_prefix_shape<K: TakeRecursiveChildren>(
    node: &Spanned<K>,
    prefix_shape: PrefixShape<K>,
    is_infix: IsInfix<K>,
    text: &str,
    sources: &SourceMap,
    hits: &mut Vec<AmbiguousPrefixConflict>,
) {
    let Some((outer_level, child)) = prefix_shape(&node.node) else {
        return;
    };

    let Some((inner_level, _)) = prefix_shape(&child.node) else {
        return;
    };

    // Strictly smaller level — same-level prefixes aren't an issue.
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
/// are transparent to the parsed tree, so this is the only way to recover whether the author
/// already disambiguated by hand.
fn is_already_parenthesized(text: &str, sources: &SourceMap, span: &Span) -> bool {
    let (text, span) = convert::local_text_and_span(text, sources, span);
    text[..span.start].trim_end().ends_with('(') && text[span.end..].trim_start().starts_with(')')
}

/// Finds every occurrence of the shape in one [`DataExpr`] subtree, appending to `hits`.
fn find_in_dataexpr(
    expr: &DataExpr,
    text: &str,
    sources: &SourceMap,
    hits: &mut Vec<AmbiguousPrefixConflict>,
) {
    expr.visit::<(), _>(|node| {
        check_prefix_shape(node, prefix_shape, is_infix, text, sources, hits);
        ControlFlow::Continue(())
    });
}

/// As [`walk_pbes_expr`], for a [`ProcessExpr`] tree (`Sum`/`Dist` are prefix, `Choice`/`Parallel`/
/// `Sequence`/... are infix, `Condition`'s guard is the same shape as elsewhere).
fn walk_process_expr(
    expr: &ProcessExpr,
    text: &str,
    sources: &SourceMap,
    hits: &mut Vec<AmbiguousPrefixConflict>,
) {
    expr.visit::<(), _>(|node| {
        check_prefix_shape(node, prefix_shape, is_infix, text, sources, hits);
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
            ProcessExprKind::Dist { expr: weight, .. } => {
                find_in_dataexpr(weight, text, sources, hits)
            }
            ProcessExprKind::Condition { condition, .. } => {
                find_in_dataexpr(condition, text, sources, hits)
            }
            ProcessExprKind::At { operand, .. } => find_in_dataexpr(operand, text, sources, hits),
            _ => {}
        }
        ControlFlow::Continue(())
    });
}

/// As [`walk_process_expr`], for a [`PbesExpr`] tree.
fn walk_pbes_expr(
    expr: &PbesExpr,
    text: &str,
    sources: &SourceMap,
    hits: &mut Vec<AmbiguousPrefixConflict>,
) {
    expr.visit::<(), _>(|node| {
        check_prefix_shape(node, prefix_shape, is_infix, text, sources, hits);
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
fn walk_pres_expr(
    expr: &PresExpr,
    text: &str,
    sources: &SourceMap,
    hits: &mut Vec<AmbiguousPrefixConflict>,
) {
    expr.visit::<(), _>(|node| {
        check_prefix_shape(node, prefix_shape, presexpr_is_infix, text, sources, hits);
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

/// As [`walk_pbes_expr`], for a modal (mu-calculus) state formula.
fn walk_state_frm(
    formula: &StateFrm,
    text: &str,
    sources: &SourceMap,
    hits: &mut Vec<AmbiguousPrefixConflict>,
) {
    formula.visit_mixed::<()>(|node| {
        match node {
            MixedNode::StateFrm(node) => {
                check_prefix_shape(node, prefix_shape, statefrm_is_infix, text, sources, hits);
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
                    StateFrmKind::DataValExprLeftMult(constant, _)
                    | StateFrmKind::DataValExprRightMult(_, constant) => {
                        find_in_dataexpr(constant, text, sources, hits)
                    }
                    StateFrmKind::FixedPoint { variable, .. } => {
                        for argument in &variable.arguments {
                            find_in_dataexpr(&argument.expr, text, sources, hits);
                        }
                    }
                    _ => {}
                }
            }
            MixedNode::ActFrm(node) => {
                check_prefix_shape(node, prefix_shape, is_infix, text, sources, hits);
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
            }
            _ => {}
        }
        ControlFlow::Continue(())
    });
}

/// Every equation's LHS/RHS/condition, in `data`'s own `var ... eqn ...` blocks.
fn walk_data_specification(
    data: &UntypedDataSpecification,
    text: &str,
    sources: &SourceMap,
    hits: &mut Vec<AmbiguousPrefixConflict>,
) {
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
        // Minus (tight) wrapping Lambda (loose), not `!`/`exists`.
        let text =
            "sort D;\nmap f: (D -> Nat) -> Nat;\ninit (f(-lambda d: D . d + d) == 0) -> delta;";
        assert_eq!(hits(text).len(), 1);
    }

    #[test]
    fn flags_through_a_chain_of_same_level_looser_prefixes_before_the_infix() {
        // `!` sees through `exists`/`lambda` (same level, so not a second hit) to the `&&` `lambda`
        // wraps directly.
        let text =
            "sort D;\nmap q: Bool;\ninit (!exists d: D . lambda e: D . d == e && q) -> delta;";
        assert_eq!(hits(text).len(), 1);
    }

    #[test]
    fn does_not_flag_a_quantifier_with_no_infix_body() {
        // No infix body (`d`) or a unary one (`!e`) — neither creates the competing shape.
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
        // `Minus`/`Negation` share DataExpr's tight prefix level.
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
        // `Modality` (tighter) directly wrapping `FixedPoint` (loosest) whose body reaches `&&`.
        let text = "act a: Bool;\nform [a(true)]mu X . true && X;";
        assert_eq!(modal_hits(text).len(), 1);
    }

    #[test]
    fn does_not_flag_a_bare_chain_of_same_level_quantifiers_with_no_enclosing_prefix() {
        // `exists` is the unique loosest StateFrm connective, so with nothing tighter enclosing it,
        // it always wins the root competition against `&&` unambiguously.
        let text =
            "sort D;\nmap q: Bool;\ninit (exists d: D . lambda e: D . d == e && q) -> delta;";
        assert!(hits(text).is_empty());
    }

    #[test]
    fn flags_a_state_formula_modality_over_a_fixed_point_reaching_a_right_constant_multiply() {
        // `DataValExprRightMult` is `Fixity::Postfix` per the grammar but a genuine infix
        // competitor in practice (see the module doc comment) — `statefrm_is_infix` must count it.
        let text = "act a: Bool;\nform [a(true)]mu X . X * val(1);";
        assert_eq!(modal_hits(text).len(), 1);
    }

    #[test]
    fn flags_a_pres_negation_over_a_bound_reaching_a_right_constant_multiply() {
        // As above, for `PresExpr`'s `RightConstantMultiply`. PRES negation is `-`, not `!`.
        let text = "pres mu X = -sup n: Nat . X * val(n); init X;";
        let spec = merc_syntax::UntypedPres::parse(text).expect("fixture should parse");
        assert_eq!(
            find_in_pres_specification(&spec, text, &SourceMap::new()).len(),
            1
        );
    }

    #[test]
    fn flags_a_process_condition_wrapping_a_sum_reaching_a_choice() {
        // `Condition` (level 4) directly wrapping `Sum` (level 1, loosest) whose body reaches `+`.
        let text = "act a: Bool;\ninit true -> sum d: Bool . a(d) . delta + delta;";
        assert_eq!(hits(text).len(), 1);
    }
}
