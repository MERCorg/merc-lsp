//! `textDocument/completion`: the names a document declares (sorts, constructors, mappings,
//! actions/processes or propositional-variable equations, global/equation variables), plus
//! mCRL2's reserved keywords and built-in sort names — filtered by [`CompletionCategory`] to just
//! the kind of name expected at the cursor (see [`crate::completion_context`]). Covers all three
//! grammars this server parses — process specifications, PBES, and PRES — unlike
//! hover/goto-definition/inlay-hints, which stay mCRL2/PBES-only: none of those three needs a
//! *checked* specification (see below), so PRES having no type checker upstream yet doesn't block
//! it here the way it does everywhere else.
//!
//! Filtering by category is still not full lexical scoping: within a category, every declaration
//! of that kind document-wide is offered, the same unscoped way this module always worked (see
//! `completion_context.rs`'s module docs) — real scoping needs binder-stack bookkeeping nothing
//! upstream exposes for process/PBES bodies. Built off the raw parsed AST, not a checked
//! specification, so — unlike hover/goto-definition/inlay-hints — completions keep working while a
//! document is transiently ill-typed or mid-edit.

use lsp_types::CompletionItem;
use lsp_types::CompletionItemKind;
use merc_syntax::StateFrm;
use merc_syntax::StateFrmKind;
use merc_syntax::UntypedDataSpecification;
use merc_syntax::UntypedPbes;
use merc_syntax::UntypedPres;
use merc_syntax::UntypedProcessSpecification;
use merc_syntax::UntypedStateFrmSpec;

pub use crate::completion_context::CompletionCategory;
use crate::names::SYSTEM_SORTS;

/// Keywords worth offering inside a sort expression — none beyond the built-in sort names
/// themselves, which are offered separately (see [`SYSTEM_SORTS`]).
const SORT_KEYWORDS: &[&str] = &[];

/// Keywords that start or continue a data expression.
const DATA_KEYWORDS: &[&str] = &["true", "false", "whr", "end", "forall", "exists", "lambda"];

/// Keywords that start a process-algebra term.
const PROCESS_KEYWORDS: &[&str] = &["delta", "tau", "sum", "dist", "hide", "block", "allow", "comm", "rename"];

/// Keywords that start or continue a PBES/PRES formula.
const FORMULA_KEYWORDS: &[&str] = &["true", "false", "val", "forall", "exists"];

/// Keywords that start or continue a modal formula's action-formula position (inside a
/// `[...]`/`<...>` modality).
const ACTION_KEYWORDS: &[&str] = &["true", "false", "val", "forall", "exists"];

/// Keywords that start or continue a modal (mu-calculus) state formula.
const STATE_FORMULA_KEYWORDS: &[&str] = &["true", "false", "val", "forall", "exists", "inf", "sup", "sum", "mu", "nu", "delay", "yaled"];

/// The keywords relevant to `category` — a subset of [`crate::semantic_tokens::KEYWORDS`], except
/// for [`CompletionCategory::Unscoped`], which offers all of them (matching this module's
/// behavior before cursor context existed).
fn keywords_for(category: CompletionCategory) -> &'static [&'static str] {
    match category {
        CompletionCategory::Sort => SORT_KEYWORDS,
        CompletionCategory::Data => DATA_KEYWORDS,
        CompletionCategory::ActionOrProcess => PROCESS_KEYWORDS,
        CompletionCategory::PropositionalVariable => FORMULA_KEYWORDS,
        CompletionCategory::Action => ACTION_KEYWORDS,
        CompletionCategory::StateVariable => STATE_FORMULA_KEYWORDS,
        CompletionCategory::Unscoped => crate::semantic_tokens::KEYWORDS,
    }
}

/// The built-in sort names relevant to `category`: only [`CompletionCategory::Sort`] and
/// [`CompletionCategory::Unscoped`] have any use for one — a system sort is never a valid data
/// value, action/process name, or propositional/state variable.
fn system_sorts_for(category: CompletionCategory) -> &'static [&'static str] {
    match category {
        CompletionCategory::Sort | CompletionCategory::Unscoped => SYSTEM_SORTS,
        CompletionCategory::Data
        | CompletionCategory::ActionOrProcess
        | CompletionCategory::PropositionalVariable
        | CompletionCategory::Action
        | CompletionCategory::StateVariable => &[],
    }
}

/// Builds the completion list for a process specification, filtered to `category`.
pub fn completions(spec: &UntypedProcessSpecification, category: CompletionCategory) -> Vec<CompletionItem> {
    let mut items = base_items(category);

    if matches!(category, CompletionCategory::Sort | CompletionCategory::Unscoped) {
        push_sort_items(&spec.data_specification, &mut items);
    }
    if matches!(category, CompletionCategory::Data | CompletionCategory::Unscoped) {
        push_data_value_items(&spec.data_specification, &mut items);
        for decl in &spec.global_variables {
            items.push(item(&decl.identifier, CompletionItemKind::VARIABLE, Some(decl.sort.to_string())));
        }
    }
    if matches!(category, CompletionCategory::ActionOrProcess | CompletionCategory::Unscoped) {
        for decl in &spec.action_declarations {
            items.push(item(&decl.identifier, CompletionItemKind::EVENT, sort_list_detail(&decl.args)));
        }
        for decl in &spec.process_declarations {
            items.push(item(&decl.identifier, CompletionItemKind::METHOD, id_decl_list_detail(&decl.params)));
        }
    }

    items
}

/// As [`completions`], for a PBES: no `act`/`proc`, but a propositional-variable equation is the
/// same "named, callable, with typed parameters" shape a process declaration is, so it gets the
/// same [`CompletionItemKind::METHOD`] treatment, offered for
/// [`CompletionCategory::PropositionalVariable`] rather than [`CompletionCategory::ActionOrProcess`].
pub fn pbes_completions(spec: &UntypedPbes, category: CompletionCategory) -> Vec<CompletionItem> {
    let mut items = base_items(category);

    if matches!(category, CompletionCategory::Sort | CompletionCategory::Unscoped) {
        push_sort_items(&spec.data_specification, &mut items);
    }
    if matches!(category, CompletionCategory::Data | CompletionCategory::Unscoped) {
        push_data_value_items(&spec.data_specification, &mut items);
        for decl in &spec.global_variables {
            items.push(item(&decl.identifier, CompletionItemKind::VARIABLE, Some(decl.sort.to_string())));
        }
    }
    if matches!(category, CompletionCategory::PropositionalVariable | CompletionCategory::Unscoped) {
        for eqn in &spec.equations {
            items.push(item(&eqn.variable.identifier, CompletionItemKind::METHOD, id_decl_list_detail(&eqn.variable.parameters)));
        }
    }

    items
}

/// As [`pbes_completions`], for a PRES — [`UntypedPres`] has the identical shape one level down
/// (`data_specification`/`global_variables`/`equations`/`init`; only each equation's own
/// `formula` type differs, and completion never walks a formula at all), so this is the same
/// function with a different parameter type, not merely similar code.
pub fn pres_completions(spec: &UntypedPres, category: CompletionCategory) -> Vec<CompletionItem> {
    let mut items = base_items(category);

    if matches!(category, CompletionCategory::Sort | CompletionCategory::Unscoped) {
        push_sort_items(&spec.data_specification, &mut items);
    }
    if matches!(category, CompletionCategory::Data | CompletionCategory::Unscoped) {
        push_data_value_items(&spec.data_specification, &mut items);
        for decl in &spec.global_variables {
            items.push(item(&decl.identifier, CompletionItemKind::VARIABLE, Some(decl.sort.to_string())));
        }
    }
    if matches!(category, CompletionCategory::PropositionalVariable | CompletionCategory::Unscoped) {
        for eqn in &spec.equations {
            items.push(item(&eqn.variable.identifier, CompletionItemKind::METHOD, id_decl_list_detail(&eqn.variable.parameters)));
        }
    }

    items
}

/// As [`completions`], for a modal (mu-calculus) formula: no `proc`/PBES-style equation list, but
/// `act` declarations (offered for [`CompletionCategory::Action`] rather than
/// [`CompletionCategory::ActionOrProcess`] — see that variant's own doc comment) and fixpoint
/// (`mu`/`nu`) variables (offered for [`CompletionCategory::StateVariable`]), the latter collected
/// recursively since — unlike a PBES/PRES's `equations` — they aren't listed anywhere flat (see
/// [`push_state_variable_items`]). [`UntypedStateFrmSpec`] declares no `glob`al variables of its
/// own either, unlike a process specification/PBES/PRES.
pub fn modal_completions(spec: &UntypedStateFrmSpec, category: CompletionCategory) -> Vec<CompletionItem> {
    let mut items = base_items(category);

    if matches!(category, CompletionCategory::Sort | CompletionCategory::Unscoped) {
        push_sort_items(&spec.data_specification, &mut items);
    }
    if matches!(category, CompletionCategory::Data | CompletionCategory::Unscoped) {
        push_data_value_items(&spec.data_specification, &mut items);
    }
    if matches!(category, CompletionCategory::Action | CompletionCategory::Unscoped) {
        for decl in &spec.action_declarations {
            items.push(item(&decl.identifier, CompletionItemKind::EVENT, sort_list_detail(&decl.args)));
        }
    }
    if matches!(category, CompletionCategory::StateVariable | CompletionCategory::Unscoped) {
        push_state_variable_items(&spec.formula, &mut items);
    }

    items
}

/// The [`CompletionItemKind::METHOD`] item per `mu`/`nu` fixpoint variable declared anywhere in
/// `formula` — recursive, since (unlike a PBES/PRES's `equations`) they aren't listed anywhere
/// flat; mirrors `symbols.rs`'s own `collect_fixed_points` walk.
fn push_state_variable_items(formula: &StateFrm, items: &mut Vec<CompletionItem>) {
    match &formula.node {
        StateFrmKind::FixedPoint { variable, body, .. } => {
            let detail = (!variable.arguments.is_empty()).then(|| variable.arguments.iter().map(ToString::to_string).collect::<Vec<_>>().join(", "));
            items.push(item(&variable.identifier, CompletionItemKind::METHOD, detail));
            push_state_variable_items(body, items);
        }
        StateFrmKind::Unary { expr, .. } | StateFrmKind::Modality { expr, .. } => push_state_variable_items(expr, items),
        StateFrmKind::Binary { lhs, rhs, .. } => {
            push_state_variable_items(lhs, items);
            push_state_variable_items(rhs, items);
        }
        StateFrmKind::Quantifier { body, .. } | StateFrmKind::Bound { body, .. } => push_state_variable_items(body, items),
        StateFrmKind::DataValExprLeftMult(_, expr) | StateFrmKind::DataValExprRightMult(expr, _) => push_state_variable_items(expr, items),
        StateFrmKind::True
        | StateFrmKind::False
        | StateFrmKind::Delay(_)
        | StateFrmKind::Yaled(_)
        | StateFrmKind::Id(_, _)
        | StateFrmKind::Resolved(_, _, _)
        | StateFrmKind::DataValExpr(_) => {}
    }
}

/// The `sort` part of the completion list, shared by [`completions`], [`pbes_completions`], and
/// [`pres_completions`] — see [`push_data_value_items`] for the rest of the shared
/// `UntypedDataSpecification` subtree.
fn push_sort_items(data: &UntypedDataSpecification, items: &mut Vec<CompletionItem>) {
    for decl in &data.sort_declarations {
        items.push(item(&decl.identifier, CompletionItemKind::STRUCT, decl.expr.as_ref().map(ToString::to_string)));
    }
}

/// The `cons`/`map`/`eqn`-variable part of the completion list — a data specification's
/// declarations usable as a data *value*, as opposed to [`push_sort_items`]'s sort names.
fn push_data_value_items(data: &UntypedDataSpecification, items: &mut Vec<CompletionItem>) {
    for decl in &data.constructor_declarations {
        items.push(item(&decl.identifier, CompletionItemKind::ENUM_MEMBER, Some(decl.sort.to_string())));
    }
    for decl in &data.map_declarations {
        items.push(item(&decl.identifier, CompletionItemKind::FUNCTION, Some(decl.sort.to_string())));
    }
    for eqn_spec in &data.equation_declarations {
        for decl in &eqn_spec.variables {
            items.push(item(&decl.identifier, CompletionItemKind::VARIABLE, Some(decl.sort.to_string())));
        }
    }
}

/// The keyword and built-in-sort items relevant to `category` — see [`keywords_for`]/
/// [`system_sorts_for`].
fn base_items(category: CompletionCategory) -> Vec<CompletionItem> {
    let keywords = keywords_for(category).iter().map(|&keyword| item(keyword, CompletionItemKind::KEYWORD, None));
    let system_sorts = system_sorts_for(category)
        .iter()
        .map(|&sort| item(sort, CompletionItemKind::STRUCT, Some("built-in sort".to_string())));
    keywords.chain(system_sorts).collect()
}

/// `", "`-joined `Display` of each declared parameter (`SortExpression`'s own `Display` for an
/// unnamed one, `IdDecl`'s for a named one), or `None` for an empty list — mirrors `symbols.rs`'s
/// own `detail` formatting for the same declaration kinds, so hover-over-completion-item and the
/// outline read the same way.
fn id_decl_list_detail<Id>(params: &[merc_syntax::IdDecl<Id>]) -> Option<String> {
    (!params.is_empty()).then(|| params.iter().map(ToString::to_string).collect::<Vec<_>>().join(", "))
}

/// As [`id_decl_list_detail`], for an action's unnamed argument-sort list (`#`-joined, matching
/// mCRL2's own product-sort notation — see `symbols.rs`'s identical formatting).
fn sort_list_detail(args: &[merc_syntax::SortExpression]) -> Option<String> {
    (!args.is_empty()).then(|| args.iter().map(ToString::to_string).collect::<Vec<_>>().join(" # "))
}

fn item(label: &str, kind: CompletionItemKind, detail: Option<String>) -> CompletionItem {
    CompletionItem {
        label: label.to_string(),
        kind: Some(kind),
        detail,
        ..CompletionItem::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::ParseOutcome;
    use crate::parse::SpecKind;
    use crate::parse::Specification;
    use crate::parse::parse;

    async fn completions_for(text: &str) -> Vec<CompletionItem> {
        match parse(SpecKind::Process, text.to_string()).await {
            ParseOutcome::Ok(Specification::Process(spec)) => completions(&spec, CompletionCategory::Unscoped),
            _ => panic!("fixture failed to parse"),
        }
    }

    async fn pbes_completions_for(text: &str) -> Vec<CompletionItem> {
        match parse(SpecKind::Pbes, text.to_string()).await {
            ParseOutcome::Ok(Specification::Pbes(spec)) => pbes_completions(&spec, CompletionCategory::Unscoped),
            _ => panic!("fixture failed to parse"),
        }
    }

    fn find<'a>(items: &'a [CompletionItem], label: &str) -> &'a CompletionItem {
        items.iter().find(|item| item.label == label).unwrap_or_else(|| panic!("no completion item labelled '{label}'"))
    }

    fn contains(items: &[CompletionItem], label: &str) -> bool {
        items.iter().any(|item| item.label == label)
    }

    #[tokio::test]
    async fn every_declaration_kind_gets_a_completion_item() {
        let text = "sort D;\ncons c: D;\nmap f: D -> D;\nvar x: D;\neqn f(x) = x;\nglob g: D;\nact a: D;\nproc P(n: D) = a(n);\ninit P(g);";
        let items = completions_for(text).await;

        assert_eq!(find(&items, "D").kind, Some(CompletionItemKind::STRUCT));
        assert_eq!(find(&items, "c").kind, Some(CompletionItemKind::ENUM_MEMBER));
        assert_eq!(find(&items, "f").kind, Some(CompletionItemKind::FUNCTION));
        assert_eq!(find(&items, "x").kind, Some(CompletionItemKind::VARIABLE));
        assert_eq!(find(&items, "g").kind, Some(CompletionItemKind::VARIABLE));
        assert_eq!(find(&items, "a").kind, Some(CompletionItemKind::EVENT));
        let process = find(&items, "P");
        assert_eq!(process.kind, Some(CompletionItemKind::METHOD));
        assert_eq!(process.detail.as_deref(), Some("n: D"));
    }

    #[tokio::test]
    async fn keywords_and_system_sorts_are_always_offered_when_unscoped() {
        let items = completions_for("init delta;").await;
        assert_eq!(find(&items, "proc").kind, Some(CompletionItemKind::KEYWORD));
        assert_eq!(find(&items, "Nat").kind, Some(CompletionItemKind::STRUCT));
    }

    #[tokio::test]
    async fn completions_are_still_offered_for_an_ill_typed_document() {
        // `undeclared` makes this fail to type check, but completion works off the raw parse, not
        // a checked specification — `f` should still show up.
        let text = "map f: Bool;\neqn f = undeclared;\ninit delta;";
        let items = completions_for(text).await;
        assert_eq!(find(&items, "f").kind, Some(CompletionItemKind::FUNCTION));
    }

    #[tokio::test]
    async fn pbes_equation_gets_a_completion_item() {
        let text = "pbes mu X(n: Bool) = val(n);\ninit X(true);";
        let items = pbes_completions_for(text).await;
        let equation = find(&items, "X");
        assert_eq!(equation.kind, Some(CompletionItemKind::METHOD));
        assert_eq!(equation.detail.as_deref(), Some("n: Bool"));
    }

    #[tokio::test]
    async fn pres_equation_gets_a_completion_item() {
        let text = "pres mu X(n: Bool) = true;\ninit X(true);";
        let items = match parse(SpecKind::Pres, text.to_string()).await {
            ParseOutcome::Ok(Specification::Pres(spec)) => pres_completions(&spec, CompletionCategory::Unscoped),
            _ => panic!("fixture failed to parse"),
        };
        let equation = find(&items, "X");
        assert_eq!(equation.kind, Some(CompletionItemKind::METHOD));
        assert_eq!(equation.detail.as_deref(), Some("n: Bool"));
    }

    #[tokio::test]
    async fn sort_category_only_offers_sorts_and_built_ins() {
        let text = "sort D;\ncons c: D;\nact a: D;\nproc P(n: D) = a(n);\ninit P(c);";
        let outcome = parse(SpecKind::Process, text.to_string()).await;
        let ParseOutcome::Ok(Specification::Process(spec)) = outcome else { panic!("fixture failed to parse") };
        let items = completions(&spec, CompletionCategory::Sort);

        assert!(contains(&items, "D"));
        assert!(contains(&items, "Nat"));
        assert!(!contains(&items, "c"), "a constructor is not a valid sort");
        assert!(!contains(&items, "a"), "an action is not a valid sort");
        assert!(!contains(&items, "P"), "a process is not a valid sort");
        assert!(!contains(&items, "proc"), "a section keyword does not belong in a sort expression");
    }

    #[tokio::test]
    async fn data_category_excludes_sorts_actions_and_processes() {
        let text = "sort D;\ncons c: D;\nact a: D;\nproc P(n: D) = a(n);\ninit P(c);";
        let outcome = parse(SpecKind::Process, text.to_string()).await;
        let ParseOutcome::Ok(Specification::Process(spec)) = outcome else { panic!("fixture failed to parse") };
        let items = completions(&spec, CompletionCategory::Data);

        assert!(contains(&items, "c"));
        assert!(contains(&items, "true"), "a data keyword belongs in a data expression");
        assert!(!contains(&items, "D"), "a sort name is not a valid data value");
        assert!(!contains(&items, "Nat"), "a built-in sort is not a valid data value");
        assert!(!contains(&items, "a"), "an action name is not a valid data value");
        assert!(!contains(&items, "P"), "a process name is not a valid data value");
    }

    #[tokio::test]
    async fn action_or_process_category_excludes_sorts_and_data_values() {
        let text = "sort D;\ncons c: D;\nact a: D;\nproc P(n: D) = a(n);\ninit P(c);";
        let outcome = parse(SpecKind::Process, text.to_string()).await;
        let ParseOutcome::Ok(Specification::Process(spec)) = outcome else { panic!("fixture failed to parse") };
        let items = completions(&spec, CompletionCategory::ActionOrProcess);

        assert!(contains(&items, "a"));
        assert!(contains(&items, "P"));
        assert!(contains(&items, "delta"), "a process keyword belongs in a process term");
        assert!(!contains(&items, "c"), "a constructor is not an action or process name");
        assert!(!contains(&items, "D"), "a sort name is not an action or process name");
    }

    #[tokio::test]
    async fn propositional_variable_category_excludes_everything_else() {
        let text = "pbes mu X(n: Bool) = val(n);\ninit X(true);";
        let outcome = parse(SpecKind::Pbes, text.to_string()).await;
        let ParseOutcome::Ok(Specification::Pbes(spec)) = outcome else { panic!("fixture failed to parse") };
        let items = pbes_completions(&spec, CompletionCategory::PropositionalVariable);

        assert!(contains(&items, "X"));
        assert!(contains(&items, "val"), "a formula keyword belongs in a formula");
        assert!(!contains(&items, "Bool"), "a built-in sort is not a propositional variable");
    }

    async fn modal_completions_for(text: &str, category: CompletionCategory) -> Vec<CompletionItem> {
        match parse(SpecKind::Modal, text.to_string()).await {
            ParseOutcome::Ok(Specification::Modal(spec)) => modal_completions(&spec, category),
            _ => panic!("fixture failed to parse"),
        }
    }

    #[tokio::test]
    async fn modal_action_gets_a_completion_item() {
        let text = "act a: Nat;\nform nu X . [a(1)]X;";
        let items = modal_completions_for(text, CompletionCategory::Unscoped).await;
        let action = find(&items, "a");
        assert_eq!(action.kind, Some(CompletionItemKind::EVENT));
        assert_eq!(action.detail.as_deref(), Some("Nat"));
    }

    #[tokio::test]
    async fn modal_nested_fixed_point_gets_a_completion_item() {
        let text = "form mu X . (nu Y(n: Nat = 0) . X) && true;";
        let items = modal_completions_for(text, CompletionCategory::Unscoped).await;
        assert_eq!(find(&items, "X").kind, Some(CompletionItemKind::METHOD));
        let inner = find(&items, "Y");
        assert_eq!(inner.kind, Some(CompletionItemKind::METHOD));
        assert!(inner.detail.is_some(), "expected 'Y' to show its own parameter");
    }

    #[tokio::test]
    async fn action_category_excludes_state_variables_and_data_values() {
        let text = "act a: Nat;\nform nu X . [a(1)]X;";
        let items = modal_completions_for(text, CompletionCategory::Action).await;

        assert!(contains(&items, "a"));
        assert!(!contains(&items, "X"), "a fixpoint variable is not an action name");
        assert!(!contains(&items, "Nat"), "a built-in sort is not an action name");
    }

    #[tokio::test]
    async fn state_variable_category_excludes_actions_and_data_values() {
        let text = "act a: Nat;\nform nu X . [a(1)]X;";
        let items = modal_completions_for(text, CompletionCategory::StateVariable).await;

        assert!(contains(&items, "X"));
        assert!(contains(&items, "nu"), "a state-formula keyword belongs in a state formula");
        assert!(!contains(&items, "a"), "an action name is not a fixpoint variable");
    }
}
