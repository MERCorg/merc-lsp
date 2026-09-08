//! Classifies a completion request's cursor offset against the raw parsed AST, so
//! [`crate::completion`] can offer only the kind of name actually expected there — a declared
//! sort while completing a sort expression, an action or process name in a process body, and so
//! on — instead of every name the document declares regardless of where the cursor sits.
//!
//! Deliberately *not* full lexical scoping (see `completion.rs`'s module docs on why that stays
//! out of scope): this only asks "what **kind** of name is expected here", by hand-walking down
//! whichever child span contains the cursor offset until none does, never "which binders are in
//! scope at this exact point". A [`CompletionCategory`] still names every declaration of that
//! kind document-wide, the same unscoped way [`crate::completion`] already did before this module
//! existed — just no longer *every* kind at once.
//!
//! Written as plain recursive functions rather than `merc_syntax::Traverse` (which every other
//! AST-walking module in this crate uses): `Traverse::visit`'s callback runs behind a closure
//! whose node reference cannot outlive a single call (it descends by re-invoking the callback
//! itself, not by handing back a value the caller can keep), so it cannot report back *which*
//! node was innermost — only whether one was found. This module needs the innermost node's own
//! shape (which variant, which fields) to decide the category, so it descends by hand instead.

use merc_syntax::ActFrm;
use merc_syntax::ActFrmKind;
use merc_syntax::ActionName;
use merc_syntax::DataExpr;
use merc_syntax::IdDecl;
use merc_syntax::MultiAction;
use merc_syntax::PbesExpr;
use merc_syntax::PbesExprKind;
use merc_syntax::PresExpr;
use merc_syntax::PresExprKind;
use merc_syntax::ProcessExpr;
use merc_syntax::ProcessExprKind;
use merc_syntax::PropVarInst;
use merc_syntax::PropVarInstData;
use merc_syntax::RegFrm;
use merc_syntax::RegFrmKind;
use merc_syntax::Span;
use merc_syntax::StateFrm;
use merc_syntax::StateFrmKind;
use merc_syntax::UntypedDataSpecification;
use merc_syntax::UntypedPbes;
use merc_syntax::UntypedPres;
use merc_syntax::UntypedProcessSpecification;
use merc_syntax::UntypedStateFrmSpec;
use merc_syntax::scan_imports;

/// What kind of declared name, if any, a completion request's cursor sits where one is expected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionCategory {
    /// A sort expression: the right-hand side of `sort D = ...`, a `cons`/`map`/`glob`/`var`
    /// declaration's sort, an action's argument-sort list, or a process/equation parameter's sort.
    Sort,
    /// A data value: an equation's left- or right-hand side, an action or process-instantiation
    /// argument, a `val(...)`/quantifier condition, and so on.
    Data,
    /// An action or process name: the callee of a process-instantiation term (`a(1)`, `P(g)`) in
    /// a process body or `init`.
    ActionOrProcess,
    /// A propositional-variable name: the callee of a `PropVarInst` in a PBES/PRES formula or
    /// `init`.
    PropositionalVariable,
    /// An action name: inside a modal formula's `[...]`/`<...>` modality (its action-formula
    /// position) or one of its own `act` declarations' argument sorts — a modal specification
    /// has no processes to disambiguate against the way [`Self::ActionOrProcess`] does for a
    /// process specification, so this is its own category rather than reusing that one.
    Action,
    /// A fixpoint (`mu`/`nu`) variable name: the callee of an `Id`/`Resolved` occurrence in a
    /// modal (mu-calculus) state formula — the state-formula counterpart of
    /// [`Self::PropositionalVariable`].
    StateVariable,
    /// No more specific context was found — offer everything the document declares, as
    /// [`crate::completion`] always did before cursor context existed.
    Unscoped,
}

/// Whether `offset` falls within `span`, inclusive of both ends — inclusive so a cursor sitting
/// right at the start or end of a token (the common case: completion fires right after the
/// partial identifier just typed) still counts as inside it.
fn contains(span: &Span, offset: usize) -> bool {
    span.start <= offset && offset <= span.end
}

/// The part of an `%import "relative/path"` directive's own quoted path that's already been typed.
pub fn import_path_prefix(text: &str, offset: usize) -> Option<&str> {
    let directive = scan_imports(text)
        .into_iter()
        .find(|directive| (directive.node.path_span.start..=directive.node.path_span.end).contains(&offset))?;
    Some(&text[directive.node.path_span.start..offset])
}

/// Whether `offset` sits somewhere inside `expr` — a data-expression's own span always covers its
/// full syntactic extent, so this alone is enough to answer "is this a data-value position"
/// without descending any further into it.
fn data_contains(expr: &DataExpr, offset: usize) -> bool {
    contains(&expr.span, offset)
}

/// [`CompletionCategory::Sort`] if `offset` sits in one of `variables`' own sort expressions
/// (a `sum`/`dist`/quantifier/bound binder list), otherwise `fallback`.
fn binder_or(variables: &[IdDecl], offset: usize, fallback: CompletionCategory) -> CompletionCategory {
    if variables.iter().any(|variable| contains(&variable.sort.span, offset)) {
        CompletionCategory::Sort
    } else {
        fallback
    }
}

/// [`CompletionCategory::ActionOrProcess`] if `offset` sits on `name` itself, [`CompletionCategory::Data`]
/// if it sits in one of `arguments`, and [`CompletionCategory::Data`] as the default otherwise (an
/// empty or not-yet-typed argument list, which lexically only ever precedes more arguments).
fn callee_or_argument<'a>(name: &ActionName, arguments: impl IntoIterator<Item = &'a DataExpr>, offset: usize) -> CompletionCategory {
    if contains(&name.span, offset) {
        return CompletionCategory::ActionOrProcess;
    }
    for argument in arguments {
        if data_contains(argument, offset) {
            return CompletionCategory::Data;
        }
    }
    CompletionCategory::Data
}

/// The category expected at `offset` within `data`, if any of `data`'s own declarations account
/// for it — shared by [`process_category`], [`pbes_category`], and [`pres_category`], since a
/// process specification, a PBES, and a PRES all share the identical `UntypedDataSpecification`
/// subtree (see `names.rs`'s module docs).
fn data_specification_category(data: &UntypedDataSpecification, offset: usize) -> Option<CompletionCategory> {
    for decl in &data.sort_declarations {
        if let Some(expr) = &decl.expr
            && contains(&expr.span, offset)
        {
            return Some(CompletionCategory::Sort);
        }
    }
    for decl in &data.constructor_declarations {
        if contains(&decl.sort.span, offset) {
            return Some(CompletionCategory::Sort);
        }
    }
    for decl in &data.map_declarations {
        if contains(&decl.sort.span, offset) {
            return Some(CompletionCategory::Sort);
        }
    }
    for eqn_spec in &data.equation_declarations {
        if !contains(&eqn_spec.span, offset) {
            continue;
        }
        for variable in &eqn_spec.variables {
            if contains(&variable.sort.span, offset) {
                return Some(CompletionCategory::Sort);
            }
        }
        // Anywhere else inside a `var ... eqn ...` block is a data-value position: on the
        // left-/right-hand side or condition of one of its equations, or simply not attached to
        // any specific sub-expression yet (an equation still being typed).
        return Some(CompletionCategory::Data);
    }
    None
}

/// The category expected at `offset` within `expr`, a process-algebra term (a `proc` body or
/// `init`) — descends by hand into whichever child's span contains `offset`, falling back to
/// [`CompletionCategory::ActionOrProcess`] once no child does (an operator/keyword position, or a
/// leaf like `delta`/`tau`).
fn process_expr_category(expr: &ProcessExpr, offset: usize) -> CompletionCategory {
    match &expr.node {
        ProcessExprKind::Action(name, arguments) => callee_or_argument(name, arguments, offset),
        ProcessExprKind::Id(name, assignments) => callee_or_argument(name, assignments.iter().map(|a| &a.expr), offset),
        ProcessExprKind::Delta | ProcessExprKind::Tau => CompletionCategory::ActionOrProcess,
        ProcessExprKind::Sum { variables, operand } => {
            recurse_or(operand, offset, process_expr_category).unwrap_or_else(|| binder_or(variables, offset, CompletionCategory::ActionOrProcess))
        }
        ProcessExprKind::Dist { variables, expr: distribution, operand } => {
            if data_contains(distribution, offset) {
                CompletionCategory::Data
            } else {
                recurse_or(operand, offset, process_expr_category)
                    .unwrap_or_else(|| binder_or(variables, offset, CompletionCategory::ActionOrProcess))
            }
        }
        ProcessExprKind::Binary { lhs, rhs, .. } => recurse_or(lhs, offset, process_expr_category)
            .or_else(|| recurse_or(rhs, offset, process_expr_category))
            .unwrap_or(CompletionCategory::ActionOrProcess),
        ProcessExprKind::Hide { operand, .. }
        | ProcessExprKind::Rename { operand, .. }
        | ProcessExprKind::Allow { operand, .. }
        | ProcessExprKind::Block { operand, .. }
        | ProcessExprKind::Comm { operand, .. } => {
            recurse_or(operand, offset, process_expr_category).unwrap_or(CompletionCategory::ActionOrProcess)
        }
        ProcessExprKind::Condition { condition, then, else_ } => {
            if data_contains(condition, offset) {
                CompletionCategory::Data
            } else {
                recurse_or(then, offset, process_expr_category)
                    .or_else(|| else_.as_deref().and_then(|else_| recurse_or(else_, offset, process_expr_category)))
                    .unwrap_or(CompletionCategory::ActionOrProcess)
            }
        }
        ProcessExprKind::At { expr: inner, operand } => recurse_or(inner, offset, process_expr_category)
            .or_else(|| data_contains(operand, offset).then_some(CompletionCategory::Data))
            .unwrap_or(CompletionCategory::ActionOrProcess),
    }
}

/// Descends into `child` (calling `recurse`) if `offset` sits within its span, otherwise `None` —
/// the shared "is this even the right child to descend into" guard every process/PBES/PRES
/// recursive step needs before recursing.
fn recurse_or<K>(child: &merc_syntax::Spanned<K>, offset: usize, recurse: impl Fn(&merc_syntax::Spanned<K>, usize) -> CompletionCategory) -> Option<CompletionCategory> {
    contains(&child.span, offset).then(|| recurse(child, offset))
}

/// As [`process_expr_category`]'s `Action`/`Id` handling, for a PBES/PRES `PropVarInst` (`X(n)`):
/// [`CompletionCategory::PropositionalVariable`] on the name itself or with no argument matched,
/// [`CompletionCategory::Data`] inside one of its arguments.
fn prop_var_inst_category(inst: &PropVarInstData, offset: usize) -> CompletionCategory {
    if contains(&inst.identifier.span, offset) {
        return CompletionCategory::PropositionalVariable;
    }
    for argument in &inst.arguments {
        if data_contains(argument, offset) {
            return CompletionCategory::Data;
        }
    }
    CompletionCategory::PropositionalVariable
}

fn pbes_formula_category(formula: &PbesExpr, offset: usize) -> CompletionCategory {
    match &formula.node {
        PbesExprKind::DataValExpr(_) => CompletionCategory::Data,
        PbesExprKind::PropVarInst(inst) => prop_var_inst_category(inst, offset),
        PbesExprKind::Quantifier { variables, body, .. } => {
            recurse_or(body, offset, pbes_formula_category).unwrap_or_else(|| binder_or(variables, offset, CompletionCategory::PropositionalVariable))
        }
        PbesExprKind::Negation(inner) => recurse_or(inner, offset, pbes_formula_category).unwrap_or(CompletionCategory::PropositionalVariable),
        PbesExprKind::Binary { lhs, rhs, .. } => recurse_or(lhs, offset, pbes_formula_category)
            .or_else(|| recurse_or(rhs, offset, pbes_formula_category))
            .unwrap_or(CompletionCategory::PropositionalVariable),
        PbesExprKind::True | PbesExprKind::False => CompletionCategory::PropositionalVariable,
    }
}

fn pres_formula_category(formula: &PresExpr, offset: usize) -> CompletionCategory {
    match &formula.node {
        PresExprKind::DataValExpr(_) => CompletionCategory::Data,
        PresExprKind::PropVarInst(inst) => prop_var_inst_category(inst, offset),
        PresExprKind::RightConstantMultiply { expr, constant } | PresExprKind::LeftConstantMultiply { constant, expr } => {
            if data_contains(constant, offset) {
                CompletionCategory::Data
            } else {
                recurse_or(expr, offset, pres_formula_category).unwrap_or(CompletionCategory::PropositionalVariable)
            }
        }
        PresExprKind::Bound { variables, expr, .. } => {
            recurse_or(expr, offset, pres_formula_category).unwrap_or_else(|| binder_or(variables, offset, CompletionCategory::PropositionalVariable))
        }
        PresExprKind::Equal { body, .. } => recurse_or(body, offset, pres_formula_category).unwrap_or(CompletionCategory::PropositionalVariable),
        PresExprKind::Condition { lhs, then, else_, .. } => recurse_or(lhs, offset, pres_formula_category)
            .or_else(|| recurse_or(then, offset, pres_formula_category))
            .or_else(|| recurse_or(else_, offset, pres_formula_category))
            .unwrap_or(CompletionCategory::PropositionalVariable),
        PresExprKind::Negation(inner) => recurse_or(inner, offset, pres_formula_category).unwrap_or(CompletionCategory::PropositionalVariable),
        PresExprKind::Binary { lhs, rhs, .. } => recurse_or(lhs, offset, pres_formula_category)
            .or_else(|| recurse_or(rhs, offset, pres_formula_category))
            .unwrap_or(CompletionCategory::PropositionalVariable),
        PresExprKind::True | PresExprKind::False => CompletionCategory::PropositionalVariable,
    }
}

/// Classifies `offset` (a byte offset into the document `spec` was parsed from) against a process
/// specification.
pub fn process_category(spec: &UntypedProcessSpecification, offset: usize) -> CompletionCategory {
    if let Some(category) = data_specification_category(&spec.data_specification, offset) {
        return category;
    }
    for decl in &spec.global_variables {
        if contains(&decl.sort.span, offset) {
            return CompletionCategory::Sort;
        }
    }
    for decl in &spec.action_declarations {
        for arg in &decl.args {
            if contains(&arg.span, offset) {
                return CompletionCategory::Sort;
            }
        }
    }
    for decl in &spec.process_declarations {
        for param in &decl.params {
            if contains(&param.sort.span, offset) {
                return CompletionCategory::Sort;
            }
        }
        if contains(&decl.body.span, offset) {
            return process_expr_category(&decl.body, offset);
        }
    }
    if let Some(init) = &spec.init
        && contains(&init.span, offset)
    {
        return process_expr_category(init, offset);
    }
    CompletionCategory::Unscoped
}

/// As [`process_category`], for a PBES.
pub fn pbes_category(spec: &UntypedPbes, offset: usize) -> CompletionCategory {
    if let Some(category) = data_specification_category(&spec.data_specification, offset) {
        return category;
    }
    for decl in &spec.global_variables {
        if contains(&decl.sort.span, offset) {
            return CompletionCategory::Sort;
        }
    }
    for eqn in &spec.equations {
        for param in &eqn.variable.parameters {
            if contains(&param.sort.span, offset) {
                return CompletionCategory::Sort;
            }
        }
        if contains(&eqn.formula.span, offset) {
            return pbes_formula_category(&eqn.formula, offset);
        }
    }
    if contains(&spec.init.span, offset) {
        return prop_var_init_category(&spec.init, offset);
    }
    CompletionCategory::Unscoped
}

/// As [`process_category`], for a PRES.
pub fn pres_category(spec: &UntypedPres, offset: usize) -> CompletionCategory {
    if let Some(category) = data_specification_category(&spec.data_specification, offset) {
        return category;
    }
    for decl in &spec.global_variables {
        if contains(&decl.sort.span, offset) {
            return CompletionCategory::Sort;
        }
    }
    for eqn in &spec.equations {
        for param in &eqn.variable.parameters {
            if contains(&param.sort.span, offset) {
                return CompletionCategory::Sort;
            }
        }
        if contains(&eqn.formula.span, offset) {
            return pres_formula_category(&eqn.formula, offset);
        }
    }
    if contains(&spec.init.span, offset) {
        return prop_var_init_category(&spec.init, offset);
    }
    CompletionCategory::Unscoped
}

fn prop_var_init_category(init: &PropVarInst, offset: usize) -> CompletionCategory {
    prop_var_inst_category(&init.node, offset)
}

/// As [`process_category`], for a modal (mu-calculus) formula.
pub fn modal_category(spec: &UntypedStateFrmSpec, offset: usize) -> CompletionCategory {
    if let Some(category) = data_specification_category(&spec.data_specification, offset) {
        return category;
    }
    for decl in &spec.action_declarations {
        for arg in &decl.args {
            if contains(&arg.span, offset) {
                return CompletionCategory::Sort;
            }
        }
    }
    if contains(&spec.formula.span, offset) {
        return state_frm_category(&spec.formula, offset);
    }
    CompletionCategory::Unscoped
}

/// The category expected at `offset` within `formula`, a modal state formula — descends by hand
/// the same way [`process_expr_category`]/[`pbes_formula_category`] do, falling back to
/// [`CompletionCategory::StateVariable`] once no child accounts for `offset` (a fixpoint-variable
/// reference is a state formula's "callable name" concept, the same role
/// [`CompletionCategory::PropositionalVariable`] plays for a PBES/PRES).
fn state_frm_category(formula: &StateFrm, offset: usize) -> CompletionCategory {
    match &formula.node {
        StateFrmKind::True | StateFrmKind::False => CompletionCategory::StateVariable,
        StateFrmKind::Delay(time) | StateFrmKind::Yaled(time) => match time {
            Some(time) if data_contains(time, offset) => CompletionCategory::Data,
            _ => CompletionCategory::StateVariable,
        },
        StateFrmKind::Id(_, arguments) | StateFrmKind::Resolved(_, arguments, _) => {
            for argument in arguments {
                if data_contains(argument, offset) {
                    return CompletionCategory::Data;
                }
            }
            CompletionCategory::StateVariable
        }
        StateFrmKind::DataValExpr(_) => CompletionCategory::Data,
        StateFrmKind::DataValExprLeftMult(constant, expr) => {
            if data_contains(constant, offset) {
                CompletionCategory::Data
            } else {
                recurse_or(expr, offset, state_frm_category).unwrap_or(CompletionCategory::StateVariable)
            }
        }
        StateFrmKind::DataValExprRightMult(expr, constant) => {
            if data_contains(constant, offset) {
                CompletionCategory::Data
            } else {
                recurse_or(expr, offset, state_frm_category).unwrap_or(CompletionCategory::StateVariable)
            }
        }
        StateFrmKind::Modality { formula: reg, expr, .. } => {
            if contains(&reg.span, offset) {
                reg_frm_category(reg, offset)
            } else {
                recurse_or(expr, offset, state_frm_category).unwrap_or(CompletionCategory::StateVariable)
            }
        }
        StateFrmKind::Unary { expr, .. } => recurse_or(expr, offset, state_frm_category).unwrap_or(CompletionCategory::StateVariable),
        StateFrmKind::Binary { lhs, rhs, .. } => recurse_or(lhs, offset, state_frm_category)
            .or_else(|| recurse_or(rhs, offset, state_frm_category))
            .unwrap_or(CompletionCategory::StateVariable),
        StateFrmKind::Quantifier { variables, body, .. } | StateFrmKind::Bound { variables, body, .. } => {
            recurse_or(body, offset, state_frm_category).unwrap_or_else(|| binder_or(variables, offset, CompletionCategory::StateVariable))
        }
        StateFrmKind::FixedPoint { variable, body, .. } => {
            for argument in &variable.arguments {
                if contains(&argument.sort.span, offset) {
                    return CompletionCategory::Sort;
                }
                if data_contains(&argument.expr, offset) {
                    return CompletionCategory::Data;
                }
            }
            recurse_or(body, offset, state_frm_category).unwrap_or(CompletionCategory::StateVariable)
        }
    }
}

/// As [`state_frm_category`], for a modality's regular formula (`[a*]X`'s `a*`).
fn reg_frm_category(formula: &RegFrm, offset: usize) -> CompletionCategory {
    match &formula.node {
        RegFrmKind::Action(action) => act_frm_category(action, offset),
        RegFrmKind::Iteration(inner) | RegFrmKind::Plus(inner) => recurse_or(inner, offset, reg_frm_category).unwrap_or(CompletionCategory::Action),
        RegFrmKind::Sequence { lhs, rhs } | RegFrmKind::Choice { lhs, rhs } => recurse_or(lhs, offset, reg_frm_category)
            .or_else(|| recurse_or(rhs, offset, reg_frm_category))
            .unwrap_or(CompletionCategory::Action),
    }
}

/// As [`reg_frm_category`], for an action formula (`a(1) && !b`).
fn act_frm_category(formula: &ActFrm, offset: usize) -> CompletionCategory {
    match &formula.node {
        ActFrmKind::True | ActFrmKind::False => CompletionCategory::Action,
        ActFrmKind::MultAct(multi_action) => multi_action_category(multi_action, offset),
        ActFrmKind::DataExprVal(_) => CompletionCategory::Data,
        ActFrmKind::Negation(inner) => recurse_or(inner, offset, act_frm_category).unwrap_or(CompletionCategory::Action),
        ActFrmKind::Quantifier { variables, body, .. } => {
            recurse_or(body, offset, act_frm_category).unwrap_or_else(|| binder_or(variables, offset, CompletionCategory::Action))
        }
        ActFrmKind::Binary { lhs, rhs, .. } => recurse_or(lhs, offset, act_frm_category)
            .or_else(|| recurse_or(rhs, offset, act_frm_category))
            .unwrap_or(CompletionCategory::Action),
    }
}

/// As [`callee_or_argument`], for a multi-action's own actions (`a(1)|b(2)`) — the modal-formula
/// counterpart, offering [`CompletionCategory::Action`] instead of
/// [`CompletionCategory::ActionOrProcess`] (see [`CompletionCategory::Action`]'s own doc comment).
fn multi_action_category(multi_action: &MultiAction, offset: usize) -> CompletionCategory {
    for action in &multi_action.actions {
        if contains(&action.id.span, offset) {
            return CompletionCategory::Action;
        }
        for argument in &action.args {
            if data_contains(argument, offset) {
                return CompletionCategory::Data;
            }
        }
    }
    CompletionCategory::Action
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::ParseOutcome;
    use crate::parse::SpecKind;
    use crate::parse::Specification;
    use crate::parse::parse_ignoring_sources as parse;

    async fn process_spec_for(text: &str) -> UntypedProcessSpecification {
        match parse(SpecKind::Process, text.to_string()).await {
            ParseOutcome::Ok(Specification::Process(spec)) => *spec,
            _ => panic!("fixture failed to parse"),
        }
    }

    async fn pbes_spec_for(text: &str) -> UntypedPbes {
        match parse(SpecKind::Pbes, text.to_string()).await {
            ParseOutcome::Ok(Specification::Pbes(spec)) => *spec,
            _ => panic!("fixture failed to parse"),
        }
    }

    async fn modal_spec_for(text: &str) -> UntypedStateFrmSpec {
        match parse(SpecKind::Modal, text.to_string()).await {
            ParseOutcome::Ok(Specification::Modal(spec)) => *spec,
            _ => panic!("fixture failed to parse"),
        }
    }

    /// The offset of the *last* occurrence of `needle` in `text` — used to land inside a token
    /// unambiguously even when an earlier, unrelated declaration uses the same name.
    fn last_offset_of(text: &str, needle: &str) -> usize {
        text.rfind(needle).unwrap_or_else(|| panic!("'{needle}' not found in {text:?}"))
    }

    #[tokio::test]
    async fn sort_alias_expression_is_sort_context() {
        let text = "sort D = List(Nat);\ninit delta;";
        let spec = process_spec_for(text).await;
        assert_eq!(process_category(&spec, last_offset_of(text, "Nat")), CompletionCategory::Sort);
    }

    #[tokio::test]
    async fn map_signature_is_sort_context() {
        let text = "sort D;\nmap f: D -> D;\ninit delta;";
        let spec = process_spec_for(text).await;
        // Anywhere in "D -> D" is inside the map's one (function-shaped) sort expression.
        assert_eq!(process_category(&spec, last_offset_of(text, "->")), CompletionCategory::Sort);
    }

    #[tokio::test]
    async fn action_argument_sort_is_sort_context() {
        let text = "sort D;\nact a: D;\ninit delta;";
        let spec = process_spec_for(text).await;
        assert_eq!(process_category(&spec, last_offset_of(text, "D")), CompletionCategory::Sort);
    }

    #[tokio::test]
    async fn equation_right_hand_side_is_data_context() {
        let text = "sort D;\ncons c: D;\nmap f: D -> D;\nvar x: D;\neqn f(x) = c;\ninit delta;";
        let spec = process_spec_for(text).await;
        assert_eq!(process_category(&spec, last_offset_of(text, "c;")), CompletionCategory::Data);
    }

    #[tokio::test]
    async fn equation_variable_sort_is_sort_context() {
        let text = "sort D;\nmap f: D -> D;\nvar x: D;\neqn f(x) = x;\ninit delta;";
        let spec = process_spec_for(text).await;
        // The last 'D' in the text is the one in "var x: D;", the equation variable's own sort.
        assert_eq!(process_category(&spec, last_offset_of(text, "D")), CompletionCategory::Sort);
    }

    #[tokio::test]
    async fn process_instantiation_name_is_action_or_process_context() {
        let text = "act a: Bool;\nproc P(n: Bool) = a(n);\ninit P(true);";
        let spec = process_spec_for(text).await;
        assert_eq!(process_category(&spec, last_offset_of(text, "P(true)")), CompletionCategory::ActionOrProcess);
    }

    #[tokio::test]
    async fn process_instantiation_argument_is_data_context() {
        let text = "act a: Bool;\nproc P(n: Bool) = a(n);\ninit P(true);";
        let spec = process_spec_for(text).await;
        assert_eq!(process_category(&spec, last_offset_of(text, "true")), CompletionCategory::Data);
    }

    #[tokio::test]
    async fn process_parameter_sort_is_sort_context() {
        let text = "proc P(n: Bool) = delta;\ninit P(true);";
        let spec = process_spec_for(text).await;
        assert_eq!(process_category(&spec, last_offset_of(text, "Bool")), CompletionCategory::Sort);
    }

    #[tokio::test]
    async fn between_process_operators_is_action_or_process_context() {
        let text = "act a, b: Bool;\ninit a(true) . b(true);\n";
        let spec = process_spec_for(text).await;
        // Right at the '.' between the two actions: no argument, no callee name, just an operator.
        assert_eq!(process_category(&spec, last_offset_of(text, ".")), CompletionCategory::ActionOrProcess);
    }

    #[tokio::test]
    async fn sum_binder_sort_is_sort_context() {
        let text = "act a: Nat;\ninit sum n: Nat . a(n);\n";
        let spec = process_spec_for(text).await;
        assert_eq!(process_category(&spec, last_offset_of(text, "Nat .")), CompletionCategory::Sort);
    }

    #[tokio::test]
    async fn outside_every_declaration_is_unscoped() {
        let text = "sort D;\ninit delta;";
        let spec = process_spec_for(text).await;
        assert_eq!(process_category(&spec, text.len()), CompletionCategory::Unscoped);
    }

    #[tokio::test]
    async fn pbes_prop_var_inst_name_is_propositional_variable_context() {
        let text = "pbes mu X(n: Bool) = val(n);\ninit X(true);";
        let spec = pbes_spec_for(text).await;
        assert_eq!(pbes_category(&spec, last_offset_of(text, "X(true)")), CompletionCategory::PropositionalVariable);
    }

    #[tokio::test]
    async fn pbes_val_argument_is_data_context() {
        let text = "pbes mu X(n: Bool) = val(n);\ninit X(true);";
        let spec = pbes_spec_for(text).await;
        assert_eq!(pbes_category(&spec, last_offset_of(text, "val(n)")), CompletionCategory::Data);
    }

    #[tokio::test]
    async fn pbes_equation_parameter_sort_is_sort_context() {
        let text = "pbes mu X(n: Bool) = val(n);\ninit X(true);";
        let spec = pbes_spec_for(text).await;
        assert_eq!(pbes_category(&spec, last_offset_of(text, "Bool")), CompletionCategory::Sort);
    }

    #[tokio::test]
    async fn modal_action_reference_is_action_context() {
        let text = "act a: Nat;\nform nu X . [a(1)]X;";
        let spec = modal_spec_for(text).await;
        assert_eq!(modal_category(&spec, last_offset_of(text, "a(1)")), CompletionCategory::Action);
    }

    #[tokio::test]
    async fn modal_action_argument_is_data_context() {
        let text = "act a: Nat;\nform nu X . [a(1)]X;";
        let spec = modal_spec_for(text).await;
        assert_eq!(modal_category(&spec, last_offset_of(text, "1)")), CompletionCategory::Data);
    }

    #[tokio::test]
    async fn modal_fixed_point_body_reference_is_state_variable_context() {
        let text = "act a: Nat;\nform nu X . [a(1)]X;";
        let spec = modal_spec_for(text).await;
        // One past the ']': the boundary right at it is inclusively still the action formula (see
        // `contains`'s own doc comment), so this lands unambiguously on the trailing 'X' reference.
        assert_eq!(modal_category(&spec, last_offset_of(text, "]X") + 1), CompletionCategory::StateVariable);
    }

    #[tokio::test]
    async fn modal_fixed_point_parameter_sort_is_sort_context() {
        let text = "act a: Nat;\nform nu X(n: Nat = 0) . [a(n)]X(n);";
        let spec = modal_spec_for(text).await;
        assert_eq!(modal_category(&spec, last_offset_of(text, "Nat = 0")), CompletionCategory::Sort);
    }

    #[test]
    fn import_path_prefix_is_the_path_typed_so_far() {
        let text = "%import \"sub/co\"\ninit delta;\n";
        let offset = text.find("co\"").unwrap() + "co".len();
        assert_eq!(import_path_prefix(text, offset), Some("sub/co"));
    }

    #[test]
    fn import_path_prefix_is_none_outside_an_import_directives_path() {
        let text = "%import \"common.mcrl2\"\ninit delta;\n";
        assert_eq!(import_path_prefix(text, 0), None);
        assert_eq!(import_path_prefix(text, text.find("init").unwrap()), None);
    }
}
