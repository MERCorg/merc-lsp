//! Builds `textDocument/documentSymbol` output from a parsed [`UntypedProcessSpecification`],
//! [`UntypedPbes`], or [`UntypedPres`].
//!
//! `merc_syntax::Traverse` doesn't apply here — it only covers expression-level node types and
//! is deliberately not used to walk the specification's top-level declarations (sorts, maps,
//! equations, actions, processes, …); those are walked by hand below, one flat loop per field.
//!
//! `ActDecl`/`ProcDecl`/`EqnDecl`/`EqnSpec` are handled inline in [`document_symbols`] via type
//! inference rather than through named helper functions the way `SortDecl`/`IdDecl<Id>` are
//! below — purely a style choice at this point (all four are `merc_syntax` crate-root
//! re-exports too), not forced by anything.
//!
//! [`pbes_symbols`]/[`pres_symbols`] share the data-specification part of the outline with
//! [`document_symbols`] via [`data_specification_symbols`], then each add their own equations
//! (`mu`/`nu`-tagged, named boolean/real formulas) and `init`, the latter now located via
//! `PropVarInst::span` (upstream `merc_syntax` change) rather than a text search — see
//! [`init_symbol`].

use lsp_types::DocumentSymbol;
use lsp_types::Range;
use lsp_types::SymbolKind;
use merc_syntax::IdDecl;
use merc_syntax::ProcessExpr;
use merc_syntax::PropVarInst;
use merc_syntax::Span;
use merc_syntax::SortDecl;
use merc_syntax::UntypedDataSpecification;
use merc_syntax::UntypedPbes;
use merc_syntax::UntypedPres;
use merc_syntax::UntypedProcessSpecification;

use crate::convert::LineIndex;

/// Builds the full, hierarchical outline for `spec`, ordered by source position.
///
/// Source order has to be reconstructed explicitly: the grammar allows the specification's
/// top-level blocks (`sort`, `map`, `eqn`, `act`, `proc`, …) to appear in any order and to
/// repeat, but the AST groups everything by kind.
pub fn document_symbols(text: &str, line_index: &LineIndex, spec: &UntypedProcessSpecification) -> Vec<DocumentSymbol> {
    let mut symbols = data_specification_symbols(text, line_index, &spec.data_specification);

    for decl in &spec.global_variables {
        symbols.push(id_decl_symbol(text, line_index, decl, SymbolKind::VARIABLE));
    }
    for decl in &spec.action_declarations {
        let detail = if decl.args.is_empty() {
            None
        } else {
            Some(decl.args.iter().map(ToString::to_string).collect::<Vec<_>>().join(" # "))
        };
        symbols.push(symbol_at(decl.identifier.node.clone(), detail, SymbolKind::EVENT, text, line_index, &decl.identifier.span, None));
    }
    for decl in &spec.process_declarations {
        let children: Vec<DocumentSymbol> = decl
            .params
            .iter()
            .map(|param| id_decl_symbol(text, line_index, param, SymbolKind::VARIABLE))
            .collect();
        let detail = if decl.params.is_empty() {
            None
        } else {
            Some(decl.params.iter().map(ToString::to_string).collect::<Vec<_>>().join(", "))
        };
        symbols.push(symbol_at(decl.identifier.node.clone(), detail, SymbolKind::FUNCTION, text, line_index, &decl.identifier.span, Some(children)));
    }
    if let Some(init) = &spec.init {
        symbols.push(init_symbol(text, line_index, init));
    }

    symbols.sort_by_key(|symbol| (symbol.range.start.line, symbol.range.start.character));
    symbols
}

/// Builds the outline for a parsed PBES: the shared data-specification part, its global
/// variables, then one entry per named boolean equation (`mu`/`nu X(params) = formula;`, with
/// each parameter as a child), and `init`.
pub fn pbes_symbols(text: &str, line_index: &LineIndex, spec: &UntypedPbes) -> Vec<DocumentSymbol> {
    let mut symbols = data_specification_symbols(text, line_index, &spec.data_specification);

    for decl in &spec.global_variables {
        symbols.push(id_decl_symbol(text, line_index, decl, SymbolKind::VARIABLE));
    }
    for eqn in &spec.equations {
        let children: Vec<DocumentSymbol> = eqn
            .variable
            .parameters
            .iter()
            .map(|param| id_decl_symbol(text, line_index, param, SymbolKind::VARIABLE))
            .collect();
        let detail = Some(format!("{} {}", eqn.operator, eqn.formula));
        symbols.push(symbol_at(
            eqn.variable.identifier.node.clone(),
            detail,
            SymbolKind::FUNCTION,
            text,
            line_index,
            &eqn.variable.identifier.span,
            Some(children),
        ));
    }
    symbols.push(pbes_init_symbol(text, line_index, &spec.init));

    symbols.sort_by_key(|symbol| (symbol.range.start.line, symbol.range.start.character));
    symbols
}

/// Builds the outline for a parsed PRES: same shape as [`pbes_symbols`], for a real (rather than
/// boolean) equation system. Each equation's formula has no upstream `Display` impl yet (unlike
/// [`merc_syntax::PbesExpr`]), so its detail only shows the fixed-point operator, not the
/// right-hand side.
pub fn pres_symbols(text: &str, line_index: &LineIndex, spec: &UntypedPres) -> Vec<DocumentSymbol> {
    let mut symbols = data_specification_symbols(text, line_index, &spec.data_specification);

    for decl in &spec.global_variables {
        symbols.push(id_decl_symbol(text, line_index, decl, SymbolKind::VARIABLE));
    }
    for eqn in &spec.equations {
        let children: Vec<DocumentSymbol> = eqn
            .variable
            .parameters
            .iter()
            .map(|param| id_decl_symbol(text, line_index, param, SymbolKind::VARIABLE))
            .collect();
        symbols.push(symbol_at(
            eqn.variable.identifier.node.clone(),
            Some(eqn.operator.to_string()),
            SymbolKind::FUNCTION,
            text,
            line_index,
            &eqn.variable.identifier.span,
            Some(children),
        ));
    }
    symbols.push(pbes_init_symbol(text, line_index, &spec.init));

    symbols.sort_by_key(|symbol| (symbol.range.start.line, symbol.range.start.character));
    symbols
}

/// The `sort`/`cons`/`map`/`eqn` part of the outline, shared by [`document_symbols`],
/// [`pbes_symbols`], and [`pres_symbols`].
fn data_specification_symbols(text: &str, line_index: &LineIndex, data: &UntypedDataSpecification) -> Vec<DocumentSymbol> {
    let mut symbols = Vec::new();

    for decl in &data.sort_declarations {
        symbols.push(sort_symbol(text, line_index, decl));
    }

    for decl in &data.constructor_declarations {
        symbols.push(id_decl_symbol(text, line_index, decl, SymbolKind::CONSTRUCTOR));
    }

    for decl in &data.map_declarations {
        symbols.push(id_decl_symbol(text, line_index, decl, SymbolKind::FUNCTION));
    }

    for eqn_spec in &data.equation_declarations {
        // `EqnSpec.span` exists but can absorb trailing whitespace past its own `;` (see its doc
        // comment upstream), which would make an empty-looking gap in the outline read as part of
        // this block's range — synthesizing the min-start/max-end over its children's spans
        // instead stays exactly as tight as what's actually being shown as children below. The
        // (grammar-legal) empty block is skipped, since there is then nothing to point the range
        // at either way.
        let spans = eqn_spec
            .variables
            .iter()
            .map(|decl| &decl.span)
            .chain(eqn_spec.equations.iter().map(|eqn| &eqn.span));
        let span = spans.fold(None::<Span>, |acc, span| match acc {
            Some(acc) => Some(Span {
                start: acc.start.min(span.start),
                end: acc.end.max(span.end),
            }),
            None => Some(span.clone()),
        });
        let Some(span) = span else { continue };
        let range = line_index.range(text, &span);

        let mut children: Vec<DocumentSymbol> = eqn_spec
            .variables
            .iter()
            .map(|decl| id_decl_symbol(text, line_index, decl, SymbolKind::VARIABLE))
            .collect();
        children.extend(eqn_spec.equations.iter().map(|eqn| {
            let range = line_index.range(text, &eqn.span);
            build_symbol(eqn.lhs.to_string(), Some(eqn.to_string()), SymbolKind::FIELD, range, range, None)
        }));

        symbols.push(build_symbol("eqn".to_string(), None, SymbolKind::NAMESPACE, range, range, Some(children)));
    }

    symbols
}

fn sort_symbol(text: &str, line_index: &LineIndex, decl: &SortDecl) -> DocumentSymbol {
    symbol_at(decl.identifier.clone(), decl.expr.as_ref().map(|expr| expr.to_string()), SymbolKind::STRUCT, text, line_index, &decl.span, None)
}

fn id_decl_symbol<Id>(text: &str, line_index: &LineIndex, decl: &IdDecl<Id>, kind: SymbolKind) -> DocumentSymbol {
    symbol_at(decl.identifier.clone(), Some(decl.sort.to_string()), kind, text, line_index, &decl.span, None)
}

fn init_symbol(text: &str, line_index: &LineIndex, init: &ProcessExpr) -> DocumentSymbol {
    let range = line_index.range(text, &init.span);
    build_symbol("init".to_string(), Some(init.to_string()), SymbolKind::OBJECT, range, range, None)
}

/// A PBES/PRES `init X(..);` symbol, located via `PropVarInst::span` (an upstream `merc_syntax`
/// addition — it used to carry no `Span` at all, unlike every other node this module builds a
/// symbol for, and had to be recovered with a text search over the whole document instead).
fn pbes_init_symbol(text: &str, line_index: &LineIndex, init: &PropVarInst) -> DocumentSymbol {
    let range = line_index.range(text, &init.span);
    build_symbol("init".to_string(), Some(init.to_string()), SymbolKind::OBJECT, range, range, None)
}

/// Builds a symbol whose `range` and `selection_range` are both exactly `span` — for a
/// declaration kind (`SortDecl`, `IdDecl`) whose own span `merc_syntax` now gives precisely the
/// identifier itself (previously the whole group in `sort A, B, C;` and its `cons`/`map`/`var`/
/// `glob` siblings all shared one span, byte-identical for every sibling), or, for `ActDecl`/
/// `ProcDecl`/`PropVarDecl` — whose own span still covers the whole declaration (`act a, b: Nat;`,
/// `proc P(n: Nat) = ...;`) — the identifier's own precise span (`decl.identifier.span`), now that
/// `identifier` on those three carries a [`Span`] of its own rather than being a plain `String`.
fn symbol_at(
    name: String,
    detail: Option<String>,
    kind: SymbolKind,
    text: &str,
    line_index: &LineIndex,
    span: &Span,
    children: Option<Vec<DocumentSymbol>>,
) -> DocumentSymbol {
    let range = line_index.range(text, span);
    build_symbol(name, detail, kind, range, range, children)
}

fn build_symbol(
    name: String,
    detail: Option<String>,
    kind: SymbolKind,
    range: Range,
    selection_range: Range,
    children: Option<Vec<DocumentSymbol>>,
) -> DocumentSymbol {
    #[allow(deprecated)]
    DocumentSymbol {
        name,
        detail,
        kind,
        tags: None,
        deprecated: None,
        range,
        selection_range,
        children,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::ParseOutcome;
    use crate::parse::SpecKind;
    use crate::parse::Specification;
    use crate::parse::parse;

    async fn symbols_for(text: &str) -> Vec<DocumentSymbol> {
        let outcome = parse(SpecKind::Process, text.to_string()).await;
        let line_index = LineIndex::new(text);
        match outcome {
            ParseOutcome::Ok(Specification::Process(spec)) => document_symbols(text, &line_index, &spec),
            _ => panic!("fixture failed to parse"),
        }
    }

    async fn pbes_symbols_for(text: &str) -> Vec<DocumentSymbol> {
        let outcome = parse(SpecKind::Pbes, text.to_string()).await;
        let line_index = LineIndex::new(text);
        match outcome {
            ParseOutcome::Ok(Specification::Pbes(spec)) => pbes_symbols(text, &line_index, &spec),
            _ => panic!("fixture failed to parse"),
        }
    }

    async fn pres_symbols_for(text: &str) -> Vec<DocumentSymbol> {
        let outcome = parse(SpecKind::Pres, text.to_string()).await;
        let line_index = LineIndex::new(text);
        match outcome {
            ParseOutcome::Ok(Specification::Pres(spec)) => pres_symbols(text, &line_index, &spec),
            _ => panic!("fixture failed to parse"),
        }
    }

    #[tokio::test]
    async fn grouped_sort_declarations_get_distinct_selection_ranges() {
        let text = "sort A, B, C;\ninit delta;";
        let symbols = symbols_for(text).await;
        let sorts: Vec<_> = symbols.iter().filter(|s| s.kind == SymbolKind::STRUCT).collect();
        assert_eq!(sorts.len(), 3);

        // `Range`/`Position` don't derive `Hash`, so compare their fields as a tuple instead.
        let ranges: std::collections::HashSet<_> = sorts
            .iter()
            .map(|s| {
                let r = s.selection_range;
                (r.start.line, r.start.character, r.end.line, r.end.character)
            })
            .collect();
        assert_eq!(ranges.len(), 3, "each grouped declaration should select only its own identifier");
    }

    #[tokio::test]
    async fn eqn_block_range_covers_its_children() {
        let text = "sort D;\nvar x: D;\neqn x = x;\ninit delta;";
        let symbols = symbols_for(text).await;
        let eqn = symbols
            .iter()
            .find(|s| s.kind == SymbolKind::NAMESPACE)
            .expect("expected an eqn container symbol");
        let children = eqn.children.as_ref().expect("eqn container should have children");
        assert!(!children.is_empty());
        for child in children {
            assert!(eqn.range.start <= child.range.start);
            assert!(eqn.range.end >= child.range.end);
        }
    }

    #[tokio::test]
    async fn symbols_are_ordered_by_source_position() {
        let text = "act a;\nsort D;\ninit a;";
        let symbols = symbols_for(text).await;
        let starts: Vec<_> = symbols.iter().map(|s| s.range.start).collect();
        let mut sorted = starts.clone();
        sorted.sort();
        assert_eq!(starts, sorted);
    }

    #[tokio::test]
    async fn pbes_equation_and_init_are_located() {
        let text = "pbes mu X(n: Bool) = true;\ninit X(n);".to_string();
        let symbols = pbes_symbols_for(&text).await;

        let equation = symbols
            .iter()
            .find(|s| s.kind == SymbolKind::FUNCTION)
            .expect("expected the boolean equation as a symbol");
        assert_eq!(equation.name, "X");
        assert_eq!(equation.children.as_ref().map(Vec::len), Some(1), "the equation's parameter should be a child");

        let init = symbols.iter().find(|s| s.name == "init").expect("expected an init symbol");
        // `init` is located via `PropVarInst::span` (see `pbes_init_symbol`); confirm it points at
        // the propositional variable instantiation itself (`X(n)`, after the `init ` keyword on
        // line 1), not just somewhere past the start of the file.
        assert_eq!((init.range.start.line, init.range.start.character), (1, "init ".len() as u32));
    }

    #[tokio::test]
    async fn pres_equation_and_init_are_located() {
        let text = "pres mu X(n: Bool) = 0;\ninit X(n);".to_string();
        let symbols = pres_symbols_for(&text).await;

        let equation = symbols
            .iter()
            .find(|s| s.kind == SymbolKind::FUNCTION)
            .expect("expected the real equation as a symbol");
        assert_eq!(equation.name, "X");
        assert_eq!(equation.children.as_ref().map(Vec::len), Some(1), "the equation's parameter should be a child");

        let init = symbols.iter().find(|s| s.name == "init").expect("expected an init symbol");
        assert_eq!((init.range.start.line, init.range.start.character), (1, "init ".len() as u32));
    }

    #[tokio::test]
    async fn pbes_init_is_located_precisely_even_with_a_leading_data_spec() {
        // Regression test for the upstream grammar quirk where `PbesSpec`/`PresSpec`'s optional
        // leading `DataSpec` reused the same `SOI`/`EOI`-wrapped rule `DataSpec::parse` itself
        // uses, making a real data specification ahead of `pbes`/`pres` fail to parse whenever
        // anything followed it (i.e. always) — fixed upstream by factoring the declarations out
        // into `DataSpecBody`, embedded without its own `SOI`/`EOI`.
        let text = "sort D;\ncons d: D;\npbes mu X(n: Bool) = true;\ninit X(n);".to_string();
        let symbols = pbes_symbols_for(&text).await;

        let sort = symbols.iter().find(|s| s.kind == SymbolKind::STRUCT).expect("expected the leading data spec's sort");
        assert_eq!(sort.name, "D");

        let init = symbols.iter().find(|s| s.name == "init").expect("expected an init symbol");
        assert_eq!(init.range.start.line, 3);
    }
}
