//! `textDocument/completion`: a flat list of every name a document declares (sorts, constructors,
//! mappings, actions/processes or propositional-variable equations, global/equation variables),
//! plus mCRL2's reserved keywords and built-in sort names. Covers all three grammars this server
//! parses — process specifications, PBES, and PRES — unlike hover/goto-definition/inlay-hints,
//! which stay mCRL2/PBES-only: none of those three needs a *checked* specification (see below),
//! so PRES having no type checker upstream yet doesn't block it here the way it does everywhere
//! else.
//!
//! Deliberately unscoped, the same simplification [`crate::semantic_tokens`] already makes for
//! the same reason (see its module docs): real lexical scoping needs binder-stack bookkeeping
//! nothing upstream exposes for process/PBES bodies, and a flat "everything the document declares"
//! list is still useful — an editor's own fuzzy-match/prefix filtering does the rest. Built off the
//! raw parsed AST, not a checked specification, so — unlike hover/goto-definition/inlay-hints —
//! completions keep working while a document is transiently ill-typed or mid-edit.

use lsp_types::CompletionItem;
use lsp_types::CompletionItemKind;
use merc_syntax::UntypedDataSpecification;
use merc_syntax::UntypedPbes;
use merc_syntax::UntypedPres;
use merc_syntax::UntypedProcessSpecification;

/// mCRL2's built-in sort names — the basic sorts plus the parameterized container sorts. Not
/// discovered from the AST (unlike every other completion item here): a system sort is never
/// *declared*, so there is no declaration list to walk the way there is for a user's own `sort`s —
/// mirrors [`crate::semantic_tokens`]'s [`SortExpressionKind::Simple`](merc_syntax::SortExpressionKind::Simple)/
/// [`Complex`](merc_syntax::SortExpressionKind::Complex) handling, just as a static name list
/// instead of an AST-node match, since there's no node to match here — only a label to offer.
const SYSTEM_SORTS: &[&str] = &["Bool", "Pos", "Nat", "Int", "Real", "List", "Set", "Bag", "FSet", "FBag"];

/// Builds the completion list for a process specification.
pub fn completions(spec: &UntypedProcessSpecification) -> Vec<CompletionItem> {
    let mut items = base_items();
    push_data_specification_items(&spec.data_specification, &mut items);

    for decl in &spec.global_variables {
        items.push(item(&decl.identifier, CompletionItemKind::VARIABLE, Some(decl.sort.to_string())));
    }
    for decl in &spec.action_declarations {
        items.push(item(&decl.identifier, CompletionItemKind::EVENT, sort_list_detail(&decl.args)));
    }
    for decl in &spec.process_declarations {
        items.push(item(&decl.identifier, CompletionItemKind::METHOD, id_decl_list_detail(&decl.params)));
    }

    items
}

/// As [`completions`], for a PBES: no `act`/`proc`, but a propositional-variable equation is the
/// same "named, callable, with typed parameters" shape a process declaration is, so it gets the
/// same [`CompletionItemKind::METHOD`] treatment.
pub fn pbes_completions(spec: &UntypedPbes) -> Vec<CompletionItem> {
    let mut items = base_items();
    push_data_specification_items(&spec.data_specification, &mut items);

    for decl in &spec.global_variables {
        items.push(item(&decl.identifier, CompletionItemKind::VARIABLE, Some(decl.sort.to_string())));
    }
    for eqn in &spec.equations {
        items.push(item(&eqn.variable.identifier, CompletionItemKind::METHOD, id_decl_list_detail(&eqn.variable.parameters)));
    }

    items
}

/// As [`pbes_completions`], for a PRES — [`UntypedPres`] has the identical shape one level down
/// (`data_specification`/`global_variables`/`equations`/`init`; only each equation's own
/// `formula` type differs, and completion never walks a formula at all), so this is the same
/// function with a different parameter type, not merely similar code.
pub fn pres_completions(spec: &UntypedPres) -> Vec<CompletionItem> {
    let mut items = base_items();
    push_data_specification_items(&spec.data_specification, &mut items);

    for decl in &spec.global_variables {
        items.push(item(&decl.identifier, CompletionItemKind::VARIABLE, Some(decl.sort.to_string())));
    }
    for eqn in &spec.equations {
        items.push(item(&eqn.variable.identifier, CompletionItemKind::METHOD, id_decl_list_detail(&eqn.variable.parameters)));
    }

    items
}

/// The `sort`/`cons`/`map`/`eqn` part of the completion list, shared by [`completions`],
/// [`pbes_completions`], and [`pres_completions`] — a process specification, a PBES, and a PRES
/// all have the identical `UntypedDataSpecification` subtree.
fn push_data_specification_items(data: &UntypedDataSpecification, items: &mut Vec<CompletionItem>) {
    for decl in &data.sort_declarations {
        items.push(item(&decl.identifier, CompletionItemKind::STRUCT, decl.expr.as_ref().map(ToString::to_string)));
    }
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

/// The keyword and built-in-sort items every completion list starts with, regardless of kind.
fn base_items() -> Vec<CompletionItem> {
    let keywords = crate::semantic_tokens::KEYWORDS
        .iter()
        .map(|&keyword| item(keyword, CompletionItemKind::KEYWORD, None));
    let system_sorts = SYSTEM_SORTS
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
            ParseOutcome::Ok(Specification::Process(spec)) => completions(&spec),
            _ => panic!("fixture failed to parse"),
        }
    }

    async fn pbes_completions_for(text: &str) -> Vec<CompletionItem> {
        match parse(SpecKind::Pbes, text.to_string()).await {
            ParseOutcome::Ok(Specification::Pbes(spec)) => pbes_completions(&spec),
            _ => panic!("fixture failed to parse"),
        }
    }

    fn find<'a>(items: &'a [CompletionItem], label: &str) -> &'a CompletionItem {
        items.iter().find(|item| item.label == label).unwrap_or_else(|| panic!("no completion item labelled '{label}'"))
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
    async fn keywords_and_system_sorts_are_always_offered() {
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
        let text = "pres mu X(n: Bool) = 0;\ninit X(true);";
        let items = match parse(SpecKind::Pres, text.to_string()).await {
            ParseOutcome::Ok(Specification::Pres(spec)) => pres_completions(&spec),
            _ => panic!("fixture failed to parse"),
        };
        let equation = find(&items, "X");
        assert_eq!(equation.kind, Some(CompletionItemKind::METHOD));
        assert_eq!(equation.detail.as_deref(), Some("n: Bool"));
    }
}
