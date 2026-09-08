//! `textDocument/definition`: resolves the identifier at a position to its declaration site(s).

use std::path::Path;

use lsp_types::Location;
use lsp_types::Position;
use lsp_types::Range;
use lsp_types::Url;
use merc_syntax::SourceMap;
use merc_syntax::Span;
use merc_syntax::scan_imports;
use merc_typecheck::ResolvedName;
use merc_typecheck::TypingInfo;

use crate::convert;
use crate::convert::LineIndex;

/// The declaration location(s) for the identifier at `position` — empty if it doesn't resolve to
/// a declaration site at all, one for almost every [`ResolvedName`] variant, or more than one only
/// for a [`ResolvedName::ActionSet`] naming several `act` declarations at once (see the module
/// docs above). `sources`/`line_indexes` are `document.sources`/`document.line_indexes` — see
/// [`convert::location`], which resolves each declaration span through them; a location a span
/// resolves to but that [`convert::location`] can't build a `Location` for (shouldn't arise in
/// practice — see its own doc comment) is silently dropped rather than shown wrong.
pub fn definition_locations(
    text: &str,
    line_index: &LineIndex,
    sources: &SourceMap,
    line_indexes: &[LineIndex],
    typing_info: &TypingInfo,
    position: Position,
) -> Vec<Location> {
    let Some(offset) = line_index.offset(text, position) else {
        return Vec::new();
    };
    let Some(node) = typing_info.at_offset(offset) else {
        return Vec::new();
    };
    let declarations: Vec<Span> = match &node.name {
        Some(ResolvedName::Constructor { declaration, .. })
        | Some(ResolvedName::Mapping { declaration, .. })
        | Some(ResolvedName::Action { declaration, .. })
        | Some(ResolvedName::Process { declaration, .. })
        | Some(ResolvedName::PropositionalVariable { declaration, .. })
        | Some(ResolvedName::StateVariable { declaration, .. })
        | Some(ResolvedName::Sort { declaration, .. })
        | Some(ResolvedName::SystemDefined { declaration, .. })
        | Some(ResolvedName::Variable { declaration, .. }) => declaration.iter().cloned().collect(),
        Some(ResolvedName::ActionSet { declarations, .. }) => declarations.clone(),
        _ => Vec::new(),
    };
    declarations.iter().filter_map(|declaration| convert::location(sources, line_indexes, declaration)).collect()
}

/// If `position` sits on an `%import "relative/path"` directive's own quoted path, resolves
/// straight to that file's start, with no [`TypingInfo`] involved at all: `%import` is a purely
/// syntactic, file-level relationship, so this works even when the document currently fails to
/// type check. 
/// 
/// `None` when `position` isn't on a directive's path, or `doc_path` is `None` — an
/// untitled/unsaved buffer has no directory a relative import path could resolve against, the same
/// condition under which `parse.rs` doesn't resolve `%import` at all.
pub fn import_directive_target(text: &str, line_index: &LineIndex, doc_path: Option<&Path>, position: Position) -> Option<Location> {
    let doc_path = doc_path?;
    let offset = line_index.offset(text, position)?;
    let directive = scan_imports(text)
        .into_iter()
        .find(|directive| (directive.node.path_span.start..=directive.node.path_span.end).contains(&offset))?;

    let directory = doc_path.parent().unwrap_or_else(|| Path::new("."));
    let target = directory.join(&directive.node.path);
    let target = target.canonicalize().unwrap_or(target);
    let uri = Url::from_file_path(&target).ok()?;
    Some(Location {
        uri,
        range: Range::default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::ParseOutcome;
    use crate::parse::SpecKind;
    use crate::parse::Specification;
    use crate::parse::parse_ignoring_sources as parse;
    use crate::typecheck::TypecheckOutcome;
    use crate::typecheck::typecheck_ignoring_sources as typecheck;

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

    /// Test convenience: [`definition_locations`] against a single-file `SourceMap` built from
    /// `text` alone.
    fn definition_ranges(text: &str, line_index: &LineIndex, typing_info: &TypingInfo, position: Position) -> Vec<Range> {
        let mut sources = SourceMap::new();
        // An absolute-looking (if fake) path: `convert::location` builds a `file://` URI via
        // `Url::from_file_path`, which requires one.
        sources.add_text("/test.mcrl2", text.to_string());
        let line_indexes = vec![line_index.clone()];
        definition_locations(text, line_index, &sources, &line_indexes, typing_info, position)
            .into_iter()
            .map(|location| location.range)
            .collect()
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

    /// As [`typing_info_for`], but keeping the real `Document` instead of
    /// discarding everything but the `TypingInfo` — needed by any fixture whose
    /// declaration might resolve outside the current document.
    async fn document_for(text: &str, path: Option<std::path::PathBuf>) -> crate::document::Document {
        let (outcome, sources) = crate::parse::parse(SpecKind::Process, text.to_string(), path).await;
        let (checked, sources) = match &outcome {
            ParseOutcome::Ok(Specification::Process(spec)) => {
                let (result, sources) = crate::typecheck::typecheck((**spec).clone(), sources).await;
                (Some(crate::document::CheckedOutcome::Process(result)), sources)
            }
            ParseOutcome::Ok(_) => panic!("fixture parsed as something other than a process specification"),
            ParseOutcome::ParseError(error) => panic!("fixture failed to parse: {error}"),
            ParseOutcome::Internal(message) => panic!("internal error parsing fixture: {message}"),
        };
        crate::document::Document::new(text.to_string(), 0, outcome, checked, sources)
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
    async fn jumps_from_a_built_in_sort_reference_to_its_bundled_template() {
        let text = "map f: Bool;\ninit delta;";
        let mut document = document_for(text, None).await;
        let typing_info = document.typing_info().expect("fixture should type check");

        let use_offset = text.find("Bool").unwrap();
        let position = document.line_index.position(text, use_offset);
        let locations = definition_locations(
            &document.text,
            &document.line_index,
            &document.sources,
            &document.line_indexes,
            &typing_info,
            position,
        );
        let [location] = locations.as_slice() else {
            panic!("expected exactly one definition, got {locations:?}");
        };
        assert_eq!(location.uri.scheme(), convert::VIRTUAL_DOCUMENT_SCHEME);
        let decoded = convert::decode_virtual_uri(&location.uri).expect("should decode back to the registered name");
        assert!(
            decoded.contains("bool.mcrl2"),
            "expected Bool to resolve into its own bundled template, got: {decoded}"
        );
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

    #[tokio::test]
    async fn jumps_from_a_propositional_variable_use_to_its_equation() {
        let text = "pbes mu X(n: Bool) = val(n);\ninit X(true);";
        let spec = merc_syntax::UntypedPbes::parse(text).unwrap_or_else(|error| panic!("fixture failed to parse: {error}"));
        let typing_info = match crate::typecheck::typecheck_pbes(spec).await {
            crate::typecheck::PbesTypecheckOutcome::Ok(mut checked) => checked.typing_info(),
            crate::typecheck::PbesTypecheckOutcome::Error(error) => panic!("fixture failed to typecheck: {error}"),
            crate::typecheck::PbesTypecheckOutcome::Internal(message) => panic!("internal error typechecking fixture: {message}"),
        };
        let line_index = LineIndex::new(text);

        let use_offset = text.find("X(true)").unwrap();
        let position = line_index.position(text, use_offset);
        let range = single(definition_ranges(text, &line_index, &typing_info, position));

        let declaration_offset = text.find("mu X").unwrap() + "mu ".len();
        let expected = line_index.position(text, declaration_offset);
        assert_eq!(range.start, expected);
    }

    #[tokio::test]
    async fn jumps_from_a_state_variable_use_to_its_fixpoint_declaration() {
        let text = "act a: Nat;\nform nu X(n: Nat = 0) . [a(n)]X(n);";
        let spec = merc_syntax::UntypedStateFrmSpec::parse(text).unwrap_or_else(|error| panic!("fixture failed to parse: {error}"));
        let typing_info = match crate::typecheck::typecheck_modal_ignoring_sources(spec).await {
            crate::typecheck::ModalTypecheckOutcome::Ok(mut checked) => checked.typing_info(),
            crate::typecheck::ModalTypecheckOutcome::Error(error) => panic!("fixture failed to typecheck: {error}"),
            crate::typecheck::ModalTypecheckOutcome::Internal(message) => panic!("internal error typechecking fixture: {message}"),
        };
        let line_index = LineIndex::new(text);

        let use_offset = text.rfind("X(n)").unwrap();
        let position = line_index.position(text, use_offset);
        let range = single(definition_ranges(text, &line_index, &typing_info, position));

        let declaration_offset = text.find("nu X").unwrap() + "nu ".len();
        let expected = line_index.position(text, declaration_offset);
        assert_eq!(range.start, expected);
    }

    /// Writes `files` (relative-path -> contents) into a fresh temp directory and returns it —
    /// mirrors `merc_syntax::imports`'s and `parse.rs`'s own test helpers of the same name.
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
    async fn jumps_from_a_cross_file_action_use_to_its_declaration_in_the_imported_file() {
        let dir = temp_project(&[
            ("main.mcrl2", "%import \"common.mcrl2\"\ninit a;\n"),
            ("common.mcrl2", "act a;\n"),
        ]);
        let main_path = dir.path().join("main.mcrl2");
        let text = std::fs::read_to_string(&main_path).unwrap();
        let mut document = document_for(&text, Some(main_path)).await;
        let typing_info = document.typing_info().expect("fixture should type check");

        let use_offset = text.find("init a").unwrap() + "init ".len();
        let position = document.line_index.position(&text, use_offset);
        let locations = definition_locations(
            &document.text,
            &document.line_index,
            &document.sources,
            &document.line_indexes,
            &typing_info,
            position,
        );
        let [location] = locations.as_slice() else {
            panic!("expected exactly one definition, got {locations:?}");
        };
        assert_eq!(location.uri.scheme(), "file");
        assert!(
            location.uri.as_str().ends_with("common.mcrl2"),
            "expected the action's declaration to resolve into common.mcrl2, got: {}",
            location.uri
        );
        assert_eq!(location.range.start, Position { line: 0, character: 4 });
    }

    #[tokio::test]
    async fn jumps_from_an_import_directives_path_to_the_imported_file() {
        let dir = temp_project(&[
            ("main.mcrl2", "%import \"common.mcrl2\"\ninit a;\n"),
            ("common.mcrl2", "act a;\n"),
        ]);
        let main_path = dir.path().join("main.mcrl2");
        let text = std::fs::read_to_string(&main_path).unwrap();
        let line_index = LineIndex::new(&text);

        let path_offset = text.find("common.mcrl2").unwrap();
        let position = line_index.position(&text, path_offset);
        let location =
            import_directive_target(&text, &line_index, Some(main_path.as_path()), position).expect("should resolve the import path");

        assert_eq!(location.uri.scheme(), "file");
        assert!(
            location.uri.as_str().ends_with("common.mcrl2"),
            "expected the import path to resolve to common.mcrl2, got: {}",
            location.uri
        );
    }

    #[tokio::test]
    async fn no_import_target_when_the_cursor_is_outside_the_directives_path() {
        let dir = temp_project(&[("main.mcrl2", "%import \"common.mcrl2\"\ninit delta;\n"), ("common.mcrl2", "")]);
        let main_path = dir.path().join("main.mcrl2");
        let text = std::fs::read_to_string(&main_path).unwrap();
        let line_index = LineIndex::new(&text);

        // On `%import` itself, not the quoted path.
        let position = line_index.position(&text, 0);
        assert!(import_directive_target(&text, &line_index, Some(main_path.as_path()), position).is_none());
    }

    #[tokio::test]
    async fn no_import_target_for_an_untitled_buffer() {
        let text = "%import \"common.mcrl2\"\ninit delta;\n";
        let line_index = LineIndex::new(text);
        let path_offset = text.find("common.mcrl2").unwrap();
        let position = line_index.position(text, path_offset);
        assert!(import_directive_target(text, &line_index, None, position).is_none());
    }
}
