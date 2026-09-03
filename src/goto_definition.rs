//! `textDocument/definition`: resolves the identifier at a position to its declaration site(s).
//!
//! Covers every [`ResolvedName`] variant that carries a declaration span: a user-declared
//! constructor or mapping — including one implicitly declared by a `sort D = struct c1(a: S)?is_c1
//! | c2;` alternative, which resolves to `c1`/`a`/`is_c1`'s own name within the `struct` expression
//! (`merc_syntax::ConstructorDecl` carries a real span for each of those now, not just a top-level
//! `cons`/`map` declaration) — a variable (an equation's own `var`-block, a process/PBES parameter,
//! a `sum`/`dist`/quantifier binder), an action/process reference, a sort-name reference (`D`
//! in `map f: D -> D;`, a sort alias's own right-hand side, …) — `TypingInfo` indexes those
//! directly too now, via [`ResolvedName::Sort`], the same way as every other reference here — and a
//! bare action name inside a `hide`/`block`/`allow`/`comm`/`rename` action set
//! ([`ResolvedName::ActionSet`]), which carries every `act` declaration sharing that name rather
//! than a single span (unlike every other variant here, which resolves to exactly one overload
//! already), so this can return more than one range for it. A built-in or a symbol declared only
//! on the system-defined specification has no declaration site `merc_typecheck` exposes at all, so
//! those resolve to no ranges at all — same as a binder with no real span of its own
//! (`declaration: None`; see `ResolvedName`'s doc comment upstream). Shares [`crate::hover`]'s
//! scoping caveat: a checked specification is only available once the whole process specification
//! type checks.

use lsp_types::Position;
use lsp_types::Range;
use merc_syntax::Span;
use merc_typecheck::ResolvedName;
use merc_typecheck::TypingInfo;

use crate::convert::LineIndex;

/// The declaration range(s) for the identifier at `position` — empty if it doesn't resolve to a
/// declaration site at all, one for almost every [`ResolvedName`] variant, or more than one only
/// for a [`ResolvedName::ActionSet`] naming several `act` declarations at once (see the module
/// docs above).
pub fn definition_ranges(text: &str, line_index: &LineIndex, typing_info: &TypingInfo, position: Position) -> Vec<Range> {
    let Some(offset) = line_index.offset(text, position) else {
        return Vec::new();
    };
    let Some(node) = typing_info.at_offset(offset) else {
        return Vec::new();
    };
    let declarations: Vec<Span> = match &node.name {
        Some(ResolvedName::Constructor { declaration, .. })
        | Some(ResolvedName::Mapping { declaration, .. })
        | Some(ResolvedName::Variable { declaration, .. })
        | Some(ResolvedName::Action { declaration, .. })
        | Some(ResolvedName::Process { declaration, .. })
        | Some(ResolvedName::Sort { declaration, .. }) => declaration.iter().cloned().collect(),
        Some(ResolvedName::ActionSet { declarations, .. }) => declarations.clone(),
        _ => Vec::new(),
    };
    declarations.iter().map(|declaration| line_index.range(text, declaration)).collect()
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

    /// Asserts `ranges` resolves to exactly one range and returns it — every fixture below has a
    /// single declaration to jump to (an `ActionSet` naming several `act` declarations at once is
    /// the only case `definition_ranges` returns more than one for; no fixture here exercises
    /// that).
    fn single(ranges: Vec<Range>) -> Range {
        let [range] = ranges.as_slice() else {
            panic!("expected exactly one definition, got {ranges:?}");
        };
        *range
    }

    async fn typing_info_for(text: &str) -> TypingInfo {
        let spec = match parse(SpecKind::Process, text.to_string()).await {
            ParseOutcome::Ok(Specification::Process(spec)) => *spec,
            _ => panic!("fixture failed to parse"),
        };
        match typecheck(spec).await {
            TypecheckOutcome::Ok(mut checked) => checked.typing_info(),
            TypecheckOutcome::Error(error) => panic!("fixture failed to typecheck: {error}"),
            TypecheckOutcome::Internal(message) => panic!("internal error typechecking fixture: {message}"),
        }
    }

    #[tokio::test]
    async fn jumps_from_a_mapping_use_to_its_declaration() {
        let text = "sort D;\ncons c: D;\nmap f: D -> D;\nvar x: D;\neqn f(x) = x;\ninit delta;";
        let typing_info = typing_info_for(text).await;
        let line_index = LineIndex::new(text);

        let use_offset = text.find("f(x) = x").unwrap();
        let position = line_index.position(text, use_offset);
        let range = single(definition_ranges(text, &line_index, &typing_info, position));

        let declaration_offset = text.find("map f").unwrap() + "map ".len();
        let expected = line_index.position(text, declaration_offset);
        assert_eq!(range.start, expected);
    }

    #[tokio::test]
    async fn jumps_from_an_action_argument_to_the_process_parameter_declaring_it() {
        let text = "act a: Nat;\nproc P(n: Nat) = a(n);\ninit P(1);";
        let typing_info = typing_info_for(text).await;
        let line_index = LineIndex::new(text);

        let use_offset = text.find("n);").unwrap();
        let position = line_index.position(text, use_offset);
        let range = single(definition_ranges(text, &line_index, &typing_info, position));

        let declaration_offset = text.find("n: Nat)").unwrap();
        let expected = line_index.position(text, declaration_offset);
        assert_eq!(range.start, expected);
    }

    #[tokio::test]
    async fn jumps_when_the_cursor_sits_right_after_the_use_s_last_character() {
        let text = "sort D;\ncons c: D;\nmap f: D -> D;\nvar x: D;\neqn f(x) = x;\ninit delta;";
        let typing_info = typing_info_for(text).await;
        let line_index = LineIndex::new(text);

        // One past the last character of the `f` use in `f(x) = x`, as if the cursor were
        // placed immediately after it rather than on or before it.
        let use_offset = text.find("f(x) = x").unwrap() + 1;
        let position = line_index.position(text, use_offset);
        let range = single(definition_ranges(text, &line_index, &typing_info, position));

        let declaration_offset = text.find("map f").unwrap() + "map ".len();
        let expected = line_index.position(text, declaration_offset);
        assert_eq!(range.start, expected);
    }

    #[tokio::test]
    async fn jumps_from_a_struct_constructor_use_to_its_name_in_the_struct_declaration() {
        // A struct-desugared constructor used to have no declaration site at all (`declaration:
        // None`, per this module's own doc comment) — `merc_syntax::ConstructorDecl` now carries a
        // real span for the constructor's own name, so this resolves like any other constructor.
        let text = "sort D = struct c1(a: Bool) | c2;\nmap f: D -> Bool;\neqn f(c1(true)) = true;\ninit delta;";
        let typing_info = typing_info_for(text).await;
        let line_index = LineIndex::new(text);

        let use_offset = text.find("c1(true)").unwrap();
        let position = line_index.position(text, use_offset);
        let range = single(definition_ranges(text, &line_index, &typing_info, position));

        let declaration_offset = text.find("struct c1").unwrap() + "struct ".len();
        let expected = line_index.position(text, declaration_offset);
        assert_eq!(range.start, expected);
    }

    #[tokio::test]
    async fn jumps_from_a_struct_recogniser_use_to_its_own_name() {
        let text = "sort D = struct c1(a: Bool)?is_c1 | c2;\neqn true = is_c1(c1(true));\ninit delta;";
        let typing_info = typing_info_for(text).await;
        let line_index = LineIndex::new(text);

        let use_offset = text.find("is_c1(c1(true))").unwrap();
        let position = line_index.position(text, use_offset);
        let range = single(definition_ranges(text, &line_index, &typing_info, position));

        let declaration_offset = text.find("?is_c1").unwrap() + "?".len();
        let expected = line_index.position(text, declaration_offset);
        assert_eq!(range.start, expected);
    }

    #[tokio::test]
    async fn jumps_from_a_mapping_signature_sort_to_its_declaration() {
        let text = "sort D;\ncons c: D;\nmap f: D -> D;\ninit delta;";
        let typing_info = typing_info_for(text).await;
        let line_index = LineIndex::new(text);

        // The *second* `D` in `map f: D -> D;` (the range sort), not the domain one.
        let use_offset = text.rfind("D;").unwrap();
        let position = line_index.position(text, use_offset);
        let range = single(definition_ranges(text, &line_index, &typing_info, position));

        let declaration_offset = text.find("sort D").unwrap() + "sort ".len();
        let expected = line_index.position(text, declaration_offset);
        assert_eq!(range.start, expected);
    }

    #[tokio::test]
    async fn jumps_from_a_nested_sort_reference_to_its_declaration() {
        let text = "sort D;\ncons c: D;\nmap f: List(D) -> D;\ninit delta;";
        let typing_info = typing_info_for(text).await;
        let line_index = LineIndex::new(text);

        let use_offset = text.find("List(D)").unwrap() + "List(".len();
        let position = line_index.position(text, use_offset);
        let range = single(definition_ranges(text, &line_index, &typing_info, position));

        let declaration_offset = text.find("sort D").unwrap() + "sort ".len();
        let expected = line_index.position(text, declaration_offset);
        assert_eq!(range.start, expected);
    }

    #[tokio::test]
    async fn jumps_from_a_sort_alias_reference_to_the_aliased_declaration() {
        let text = "sort D;\nsort E = D;\ninit delta;";
        let typing_info = typing_info_for(text).await;
        let line_index = LineIndex::new(text);

        let use_offset = text.find("E = D").unwrap() + "E = ".len();
        let position = line_index.position(text, use_offset);
        let range = single(definition_ranges(text, &line_index, &typing_info, position));

        let declaration_offset = text.find("sort D").unwrap() + "sort ".len();
        let expected = line_index.position(text, declaration_offset);
        assert_eq!(range.start, expected);
    }

    #[tokio::test]
    async fn no_definition_for_a_built_in_sort_reference() {
        let text = "map f: Bool;\ninit delta;";
        let typing_info = typing_info_for(text).await;
        let line_index = LineIndex::new(text);

        let use_offset = text.find("Bool").unwrap();
        let position = line_index.position(text, use_offset);
        assert!(definition_ranges(text, &line_index, &typing_info, position).is_empty());
    }

    #[tokio::test]
    async fn jumps_from_a_hide_action_name_to_its_declaration() {
        let text = "act a: Nat;\nproc P(n: Nat) = a(n);\ninit hide({a}, P(1));";
        let typing_info = typing_info_for(text).await;
        let line_index = LineIndex::new(text);

        let use_offset = text.find("hide({a}").unwrap() + "hide({".len();
        let position = line_index.position(text, use_offset);
        let range = single(definition_ranges(text, &line_index, &typing_info, position));

        let declaration_offset = text.find("act a").unwrap() + "act ".len();
        let expected = line_index.position(text, declaration_offset);
        assert_eq!(range.start, expected);
    }

    #[tokio::test]
    async fn jumps_from_an_allow_multi_action_name_to_its_declaration() {
        let text = "act a: Nat;\nact b: Nat;\nproc P(n: Nat) = a(n)|b(n);\ninit allow({a|b}, P(1));";
        let typing_info = typing_info_for(text).await;
        let line_index = LineIndex::new(text);

        let use_offset = text.find("a|b}").unwrap() + "a|".len();
        let position = line_index.position(text, use_offset);
        let range = single(definition_ranges(text, &line_index, &typing_info, position));

        let declaration_offset = text.find("act b").unwrap() + "act ".len();
        let expected = line_index.position(text, declaration_offset);
        assert_eq!(range.start, expected);
    }

    #[tokio::test]
    async fn jumps_from_a_comm_action_name_to_its_declaration() {
        let text = "act a, b, c: Nat;\nproc P(n: Nat) = a(n)|b(n);\ninit comm({a|b -> c}, P(1));";
        let typing_info = typing_info_for(text).await;
        let line_index = LineIndex::new(text);

        let use_offset = text.find("-> c}").unwrap() + "-> ".len();
        let position = line_index.position(text, use_offset);
        let range = single(definition_ranges(text, &line_index, &typing_info, position));

        let declaration_offset = text.find("act a").unwrap() + "act a, b, ".len();
        let expected = line_index.position(text, declaration_offset);
        assert_eq!(range.start, expected);
    }

    #[tokio::test]
    async fn jumps_from_a_rename_action_name_to_its_declaration() {
        let text = "act a, b: Nat;\nproc P(n: Nat) = a(n);\ninit rename({a -> b}, P(1));";
        let typing_info = typing_info_for(text).await;
        let line_index = LineIndex::new(text);

        let use_offset = text.find("-> b}").unwrap() + "-> ".len();
        let position = line_index.position(text, use_offset);
        let range = single(definition_ranges(text, &line_index, &typing_info, position));

        let declaration_offset = text.find("act a, b").unwrap() + "act a, ".len();
        let expected = line_index.position(text, declaration_offset);
        assert_eq!(range.start, expected);
    }
}
