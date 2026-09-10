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

use std::path::Path;

use lsp_types::CompletionItem;
use lsp_types::CompletionItemKind;
use lsp_types::CompletionTextEdit;
use lsp_types::Position;
use lsp_types::Range;
use lsp_types::TextEdit;
use merc_syntax::StateFrm;
use merc_syntax::StateFrmKind;
use merc_syntax::UntypedDataSpecification;
use merc_syntax::UntypedPbes;
use merc_syntax::UntypedPres;
use merc_syntax::UntypedProcessSpecification;
use merc_syntax::UntypedStateFrmSpec;

pub use crate::completion_context::CompletionCategory;
use crate::completion_context;
use crate::convert::LineIndex;
use crate::names::SYSTEM_SORTS;

/// Completion items listing the `.mcrl2` files (and subdirectories) available at the `%import`
/// path the cursor is currently sitting in.
pub fn import_path_completions(text: &str, line_index: &LineIndex, doc_path: Option<&Path>, position: Position) -> Option<Vec<CompletionItem>> {
    let doc_dir = doc_path?.parent().unwrap_or_else(|| Path::new("."));
    let offset = line_index.offset(text, position)?;
    let typed = completion_context::import_path_prefix(text, offset)?;

    // Splits the already-typed path at its last '/', if any: everything up to and including it
    // names a subdirectory to list (possibly several levels deep, e.g. "a/b/"), everything after
    // is the partial filename this completion replaces — so completing "sub/fo" only replaces
    // "fo", leaving "sub/" (and the surrounding quotes) untouched.
    let (sub_dir, partial) = match typed.rfind('/') {
        Some(index) => (&typed[..=index], &typed[index + 1..]),
        None => ("", typed),
    };
    let entries = std::fs::read_dir(doc_dir.join(sub_dir)).ok()?;

    // Only the partial filename segment gets replaced; `offset - partial.len()` stays a valid
    // char boundary since `partial` is always a suffix of `text` split at an ASCII '/' or the
    // directive's own path-span start.
    let edit_range = Range {
        start: line_index.position(text, offset - partial.len()),
        end: line_index.position(text, offset),
    };

    let mut items: Vec<CompletionItem> = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            let file_type = entry.file_type().ok()?;
            let name = entry.file_name().to_str()?.to_string();
            let (label, kind) = if file_type.is_dir() {
                (format!("{name}/"), CompletionItemKind::FOLDER)
            } else if name.ends_with(".mcrl2") {
                (name, CompletionItemKind::FILE)
            } else {
                return None;
            };
            Some(CompletionItem {
                text_edit: Some(CompletionTextEdit::Edit(TextEdit {
                    range: edit_range,
                    new_text: label.clone(),
                })),
                // Editors that locally re-filter a still-open completion list as the user keeps
                // typing (rather than waiting for the next server round trip) compare that typed
                // text against `filterText`.
                filter_text: Some(format!("{sub_dir}{label}")),
                kind: Some(kind),
                label,
                ..CompletionItem::default()
            })
        })
        .collect();
    items.sort_by(|a, b| a.label.cmp(&b.label));
    Some(items)
}

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
    use crate::parse::parse_ignoring_sources as parse;

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

    #[test]
    fn import_path_completion_lists_mcrl2_files_and_subdirectories() {
        let dir = tempfile::tempdir().expect("should create a temp directory");
        std::fs::write(dir.path().join("common.mcrl2"), "").unwrap();
        std::fs::write(dir.path().join("notes.txt"), "").unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let main_path = dir.path().join("main.mcrl2");
        // `scan_imports` requires a non-empty quoted path to recognize the line as a directive at
        // all (see `merc_syntax::imports::parse_import_line`), so this starts with one character
        // already typed — server-side filtering by that prefix is left to the client, the same as
        // every other completion category in this module, so every entry is still offered here.
        let text = "%import \"c\"\ninit delta;\n".to_string();
        std::fs::write(&main_path, &text).unwrap();

        let line_index = LineIndex::new(&text);
        let offset = text.find("c\"").unwrap() + 1;
        let position = line_index.position(&text, offset);
        let items = import_path_completions(&text, &line_index, Some(main_path.as_path()), position).expect("should offer import completions");

        assert!(items.iter().any(|item| item.label == "common.mcrl2"), "expected common.mcrl2 among {items:?}");
        assert!(items.iter().any(|item| item.label == "sub/"), "expected the sub directory among {items:?}");
        assert!(!items.iter().any(|item| item.label == "notes.txt"), "a non-mcrl2 file should not be offered");
    }

    #[test]
    fn import_path_completion_is_none_outside_an_import_directive() {
        let text = "init delta;".to_string();
        let line_index = LineIndex::new(&text);
        let position = line_index.position(&text, 0);
        assert!(import_path_completions(&text, &line_index, Some(Path::new("/tmp/main.mcrl2")), position).is_none());
    }

    #[test]
    fn import_path_completion_only_replaces_the_partial_segment_after_the_last_slash() {
        let dir = tempfile::tempdir().expect("should create a temp directory");
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub").join("common.mcrl2"), "").unwrap();
        let main_path = dir.path().join("main.mcrl2");
        let text = "%import \"sub/co\"\ninit delta;\n".to_string();
        std::fs::write(&main_path, &text).unwrap();

        let line_index = LineIndex::new(&text);
        let offset = text.find("co\"").unwrap() + "co".len();
        let position = line_index.position(&text, offset);
        let items = import_path_completions(&text, &line_index, Some(main_path.as_path()), position).expect("should offer import completions");

        let item = items.iter().find(|item| item.label == "common.mcrl2").expect("expected common.mcrl2 among the sub-directory's contents");
        let Some(CompletionTextEdit::Edit(edit)) = &item.text_edit else {
            panic!("expected a plain TextEdit, got: {:?}", item.text_edit);
        };
        assert_eq!(edit.new_text, "common.mcrl2");
        let start_offset = line_index.offset(&text, edit.range.start).unwrap();
        let end_offset = line_index.offset(&text, edit.range.end).unwrap();
        assert_eq!(&text[start_offset..end_offset], "co", "should only replace the partial filename, not 'sub/'");
    }

    #[test]
    fn import_path_completion_sets_filter_text_to_the_full_typed_path() {
        // Regression test: a `./`-prefixed path used to fail to complete in editors (e.g.
        // VS Code) that locally re-filter an already-open completion list against `filterText`
        // (which defaults to the bare `label`) as the user keeps typing, instead of always
        // waiting for a fresh request. `../` happened to still work because its second `.`
        // immediately empties that local list (no label has two `.`s), closing the stale session
        // before the next keystroke — but `./`'s single `.` still fuzzy-matches the `.` inside
        // `common.mcrl2`, keeping the stale session open long enough for the following `/` to
        // wipe it out locally, racing the fresh (correct) results this directory listing computes.
        // `filterText` must include `sub_dir` (here "./"), not just the bare label, so every
        // character the user has actually typed remains a real prefix of it.
        let dir = tempfile::tempdir().expect("should create a temp directory");
        std::fs::write(dir.path().join("common.mcrl2"), "").unwrap();
        let main_path = dir.path().join("main.mcrl2");
        let text = "%import \"./\"\ninit delta;\n".to_string();
        std::fs::write(&main_path, &text).unwrap();

        let line_index = LineIndex::new(&text);
        let offset = text.find("./\"").unwrap() + "./".len();
        let position = line_index.position(&text, offset);
        let items = import_path_completions(&text, &line_index, Some(main_path.as_path()), position).expect("should offer import completions");

        let item = items.iter().find(|item| item.label == "common.mcrl2").expect("expected common.mcrl2 among {items:?}");
        assert_eq!(item.filter_text.as_deref(), Some("./common.mcrl2"));
    }
}
