//! `textDocument/definition`: resolves the identifier at a position to its declaration site.
//!
//! Covers every [`ResolvedName`] variant that carries a declaration span: a user-declared
//! constructor or mapping, a variable (an equation's own `var`-block, a process/PBES parameter, a
//! `sum`/`dist`/quantifier binder), and an action/process reference itself. A built-in or a symbol
//! declared only on the system-defined specification has no declaration site `merc_typecheck`
//! exposes at all, so those resolve to `None` rather than a location — same as a struct-desugared
//! constructor/mapping, or a binder with no real span of its own (`declaration: None`; see
//! `ResolvedName`'s doc comment upstream). Shares [`crate::hover`]'s offset→[`TypedNode`] lookup
//! and its scoping caveat: a checked specification is only available once the whole process
//! specification type checks.

use lsp_types::Position;
use lsp_types::Range;
use merc_typecheck::ResolvedName;
use merc_typecheck::TypingInfo;

use crate::convert::LineIndex;
use crate::hover::typed_node_at;

/// The declaration range for the identifier at `position`, if it resolves to one.
pub fn definition_range(text: &str, line_index: &LineIndex, typing_info: &TypingInfo, position: Position) -> Option<Range> {
    let offset = line_index.offset(text, position)?;
    let node = typed_node_at(typing_info, offset)?;

    let declaration = match &node.name {
        Some(ResolvedName::Constructor { declaration, .. })
        | Some(ResolvedName::Mapping { declaration, .. })
        | Some(ResolvedName::Variable { declaration, .. })
        | Some(ResolvedName::Action { declaration, .. })
        | Some(ResolvedName::Process { declaration, .. }) => declaration.as_ref(),
        _ => None,
    }?;

    Some(line_index.range(text, declaration))
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
        let range = definition_range(text, &line_index, &typing_info, position).expect("expected a definition");

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
        let range = definition_range(text, &line_index, &typing_info, position).expect("expected a definition");

        let declaration_offset = text.find("n: Nat)").unwrap();
        let expected = line_index.position(text, declaration_offset);
        assert_eq!(range.start, expected);
    }
}
