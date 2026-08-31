//! `textDocument/definition`: resolves the identifier at a position to its declaration site.
//!
//! Only covers what [`ResolvedName`] itself carries a declaration span for: a user-declared
//! constructor or mapping (`ResolvedName::Constructor`/`Mapping`). An equation variable, a
//! built-in, or a symbol declared only on the system-defined specification has no declaration
//! site `merc_typecheck` exposes at all, so those resolve to `None` rather than a location — same
//! as a struct-desugared constructor/mapping with no real span of its own (`declaration: None`;
//! see `ResolvedName`'s doc comment upstream). Shares [`crate::hover`]'s offset→[`TypedNode`]
//! lookup and its scoping caveats (typing info only covers `eqn`-block expressions; a checked
//! specification is only available once the whole process specification type checks).

use lsp_types::Position;
use lsp_types::Range;
use merc_typecheck::DataSpecification;
use merc_typecheck::ResolvedName;

use crate::convert::LineIndex;
use crate::hover::typed_node_at;

/// The declaration range for the identifier at `position`, if it resolves to one.
pub fn definition_range(text: &str, line_index: &LineIndex, data_specification: &DataSpecification, position: Position) -> Option<Range> {
    let offset = line_index.offset(text, position)?;
    let typing_info = data_specification.typing_info();
    let node = typed_node_at(&typing_info, offset)?;

    let declaration = match &node.name {
        Some(ResolvedName::Constructor { declaration, .. }) => declaration.as_ref(),
        Some(ResolvedName::Mapping { declaration, .. }) => declaration.as_ref(),
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

    async fn checked_for(text: &str) -> DataSpecification {
        let spec = match parse(SpecKind::Process, text.to_string()).await {
            ParseOutcome::Ok(Specification::Process(spec)) => *spec,
            _ => panic!("fixture failed to parse"),
        };
        match typecheck(spec).await {
            TypecheckOutcome::Ok(checked) => checked.into_data_specification(),
            TypecheckOutcome::Error(error) => panic!("fixture failed to typecheck: {error}"),
            TypecheckOutcome::Internal(message) => panic!("internal error typechecking fixture: {message}"),
        }
    }

    #[tokio::test]
    async fn jumps_from_a_mapping_use_to_its_declaration() {
        let text = "sort D;\ncons c: D;\nmap f: D -> D;\nvar x: D;\neqn f(x) = x;\ninit delta;";
        let data_specification = checked_for(text).await;
        let line_index = LineIndex::new(text);

        let use_offset = text.find("f(x) = x").unwrap();
        let position = line_index.position(text, use_offset);
        let range = definition_range(text, &line_index, &data_specification, position).expect("expected a definition");

        let declaration_offset = text.find("map f").unwrap() + "map ".len();
        let expected = line_index.position(text, declaration_offset);
        assert_eq!(range.start, expected);
    }

    #[tokio::test]
    async fn no_definition_for_an_equation_variable() {
        let text = "sort D;\nvar x: D;\neqn x = x;\ninit delta;";
        let data_specification = checked_for(text).await;
        let line_index = LineIndex::new(text);

        // `x` resolves to `ResolvedName::Variable`, which carries no declaration span.
        let use_offset = text.rfind('x').unwrap();
        let position = line_index.position(text, use_offset);
        assert!(definition_range(text, &line_index, &data_specification, position).is_none());
    }
}
