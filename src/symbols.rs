//! Builds `textDocument/documentSymbol` output from a parsed [`UntypedProcessSpecification`],
//! [`UntypedPbes`], [`UntypedPres`], or [`UntypedStateFrmSpec`].

use lsp_types::DocumentSymbol;
use lsp_types::Range;
use lsp_types::SymbolKind;
use merc_syntax::IdDecl;
use merc_syntax::ProcessExpr;
use merc_syntax::PropVarInst;
use merc_syntax::SortDecl;
use merc_syntax::SourceMap;
use merc_syntax::Span;
use merc_syntax::StateFrm;
use merc_syntax::StateFrmKind;
use merc_syntax::StateVarAssignment;
use merc_syntax::UntypedDataSpecification;
use merc_syntax::UntypedPbes;
use merc_syntax::UntypedPres;
use merc_syntax::UntypedProcessSpecification;
use merc_syntax::UntypedStateFrmSpec;

use crate::convert;
use crate::convert::LineIndex;
use crate::diagnostics;

/// Builds the full, hierarchical outline for `spec`, ordered by source position.
///
/// Source order has to be reconstructed explicitly: the grammar allows the specification's
/// top-level blocks (`sort`, `map`, `eqn`, `act`, `proc`, …) to appear in any order and to
/// repeat, but the AST groups everything by kind.
pub fn document_symbols(text: &str, line_index: &LineIndex, sources: &SourceMap, spec: &UntypedProcessSpecification) -> Vec<DocumentSymbol> {
    let mut groups = ImportGroups::default();
    let mut symbols = data_specification_symbols(text, line_index, sources, &spec.data_specification, &mut groups);

    for decl in &spec.global_variables {
        place(&mut symbols, &mut groups, text, line_index, sources, &decl.identifier.span, |target| {
            id_decl_symbol(decl, SymbolKind::VARIABLE, target)
        });
    }
    for decl in &spec.action_declarations {
        place(&mut symbols, &mut groups, text, line_index, sources, &decl.identifier.span, |target| {
            let detail = if decl.args.is_empty() {
                None
            } else {
                Some(decl.args.iter().map(ToString::to_string).collect::<Vec<_>>().join(" # "))
            };
            symbol_at(decl.identifier.node.clone(), detail, SymbolKind::EVENT, target, &decl.identifier.span, None)
        });
    }
    for decl in &spec.process_declarations {
        place(&mut symbols, &mut groups, text, line_index, sources, &decl.identifier.span, |target| {
            let children: Vec<DocumentSymbol> = decl.params.iter().map(|param| id_decl_symbol(param, SymbolKind::VARIABLE, target)).collect();
            let detail = if decl.params.is_empty() {
                None
            } else {
                Some(decl.params.iter().map(ToString::to_string).collect::<Vec<_>>().join(", "))
            };
            symbol_at(decl.identifier.node.clone(), detail, SymbolKind::FUNCTION, target, &decl.identifier.span, Some(children))
        });
    }
    // The importing file's own `init` always wins over anything an import might declare (see
    // `merc_syntax::imports`' own doc comment), so this is always local — no `place` needed.
    if let Some(init) = &spec.init {
        symbols.push(init_symbol(text, line_index, init));
    }

    symbols.extend(groups.finish());
    symbols.sort_by_key(|symbol| (symbol.range.start.line, symbol.range.start.character));
    symbols
}

/// Builds the outline for a parsed PBES: the shared data-specification part, its global
/// variables, then one entry per named boolean equation (`mu`/`nu X(params) = formula;`, with
/// each parameter as a child), and `init`.
///
/// PBES/PRES specifications have no `%import` support of their own (see `merc_syntax::imports`),
/// so `groups` below never actually accumulates anything for one — [`place`] is still used
/// throughout for consistency with [`document_symbols`]/[`modal_symbols`] rather than because
/// anything here can really be foreign.
pub fn pbes_symbols(text: &str, line_index: &LineIndex, sources: &SourceMap, spec: &UntypedPbes) -> Vec<DocumentSymbol> {
    let mut groups = ImportGroups::default();
    let mut symbols = data_specification_symbols(text, line_index, sources, &spec.data_specification, &mut groups);

    for decl in &spec.global_variables {
        place(&mut symbols, &mut groups, text, line_index, sources, &decl.identifier.span, |target| {
            id_decl_symbol(decl, SymbolKind::VARIABLE, target)
        });
    }
    for eqn in &spec.equations {
        place(&mut symbols, &mut groups, text, line_index, sources, &eqn.variable.identifier.span, |target| {
            let children: Vec<DocumentSymbol> = eqn.variable.parameters.iter().map(|param| id_decl_symbol(param, SymbolKind::VARIABLE, target)).collect();
            let detail = Some(format!("{} {}", eqn.operator, eqn.formula));
            symbol_at(eqn.variable.identifier.node.clone(), detail, SymbolKind::FUNCTION, target, &eqn.variable.identifier.span, Some(children))
        });
    }
    symbols.push(pbes_init_symbol(text, line_index, &spec.init));

    symbols.extend(groups.finish());
    symbols.sort_by_key(|symbol| (symbol.range.start.line, symbol.range.start.character));
    symbols
}

/// Builds the outline for a parsed PRES: same shape as [`pbes_symbols`], for a real (rather than
/// boolean) equation system. Each equation's formula has no upstream `Display` impl yet (unlike
/// [`merc_syntax::PbesExpr`]), so its detail only shows the fixed-point operator, not the
/// right-hand side.
pub fn pres_symbols(text: &str, line_index: &LineIndex, sources: &SourceMap, spec: &UntypedPres) -> Vec<DocumentSymbol> {
    let mut groups = ImportGroups::default();
    let mut symbols = data_specification_symbols(text, line_index, sources, &spec.data_specification, &mut groups);

    for decl in &spec.global_variables {
        place(&mut symbols, &mut groups, text, line_index, sources, &decl.identifier.span, |target| {
            id_decl_symbol(decl, SymbolKind::VARIABLE, target)
        });
    }
    for eqn in &spec.equations {
        place(&mut symbols, &mut groups, text, line_index, sources, &eqn.variable.identifier.span, |target| {
            let children: Vec<DocumentSymbol> = eqn.variable.parameters.iter().map(|param| id_decl_symbol(param, SymbolKind::VARIABLE, target)).collect();
            symbol_at(eqn.variable.identifier.node.clone(), Some(eqn.operator.to_string()), SymbolKind::FUNCTION, target, &eqn.variable.identifier.span, Some(children))
        });
    }
    symbols.push(pbes_init_symbol(text, line_index, &spec.init));

    symbols.extend(groups.finish());
    symbols.sort_by_key(|symbol| (symbol.range.start.line, symbol.range.start.character));
    symbols
}

/// Builds the outline for a parsed modal (mu-calculus) formula: the shared data-specification
/// part, the formula's own `act` declarations, then every `mu`/`nu` fixpoint variable declared
/// anywhere in the formula (see [`collect_fixed_points`]) — nested under whichever enclosing
/// fixpoint declares it, the same structure the formula itself has. A formula with no fixpoint at
/// all (`[a]true`, say) simply has no entries past the `act` declarations — there is no flat
/// top-level list the way a PBES/PRES's equations are to fall back to.
///
/// The formula itself (`spec.formula`) is always this document's own — only `data_specification`
/// and `action_declarations` can carry anything `%import`ed in (see
/// `merc_syntax::UntypedStateFrmSpec::parse_with_imports`) — so [`collect_fixed_points`] needs no
/// [`ImportGroups`] awareness at all, unlike the two loops ahead of it.
pub fn modal_symbols(text: &str, line_index: &LineIndex, sources: &SourceMap, spec: &UntypedStateFrmSpec) -> Vec<DocumentSymbol> {
    let mut groups = ImportGroups::default();
    let mut symbols = data_specification_symbols(text, line_index, sources, &spec.data_specification, &mut groups);

    for decl in &spec.action_declarations {
        place(&mut symbols, &mut groups, text, line_index, sources, &decl.identifier.span, |target| {
            let detail = if decl.args.is_empty() {
                None
            } else {
                Some(decl.args.iter().map(ToString::to_string).collect::<Vec<_>>().join(" # "))
            };
            symbol_at(decl.identifier.node.clone(), detail, SymbolKind::EVENT, target, &decl.identifier.span, None)
        });
    }

    collect_fixed_points(&spec.formula, text, line_index, &mut symbols);

    symbols.extend(groups.finish());
    symbols.sort_by_key(|symbol| (symbol.range.start.line, symbol.range.start.character));
    symbols
}

/// Recursively finds every `mu`/`nu X(...) = ...` declared anywhere within `formula`, appending
/// one [`DocumentSymbol`] per declaration to `out` — a sibling fixpoint (`mu X = ... && mu Y =
/// ...`) becomes a sibling entry, while one nested inside another's own body (`mu X = nu Y = ...`)
/// becomes a child of it, found by recursing into `body` as that fixpoint's own `out` list instead
/// of the caller's. Mirrors [`data_specification_symbols`]'s `eqn` block in spirit — a container
/// grouping declarations by nesting — but has to walk the formula tree by hand to find them, since
/// (unlike a PBES/PRES's `equations`) they aren't listed anywhere flat.
///
/// Always local (see [`modal_symbols`]'s doc comment) — no [`ImportGroups`] involved.
fn collect_fixed_points(formula: &StateFrm, text: &str, line_index: &LineIndex, out: &mut Vec<DocumentSymbol>) {
    match &formula.node {
        StateFrmKind::FixedPoint { operator, variable, body } => {
            let mut children: Vec<DocumentSymbol> = variable.arguments.iter().map(|argument| state_var_assignment_symbol(text, line_index, argument)).collect();
            collect_fixed_points(body, text, line_index, &mut children);
            let detail = format!("{operator} {variable}");
            let target = SpanTarget::Local { text, line_index };
            out.push(symbol_at(variable.identifier.clone(), Some(detail), SymbolKind::FUNCTION, target, &variable.span, Some(children)));
        }
        StateFrmKind::Unary { expr, .. } | StateFrmKind::Modality { expr, .. } => collect_fixed_points(expr, text, line_index, out),
        StateFrmKind::Binary { lhs, rhs, .. } => {
            collect_fixed_points(lhs, text, line_index, out);
            collect_fixed_points(rhs, text, line_index, out);
        }
        StateFrmKind::Quantifier { body, .. } | StateFrmKind::Bound { body, .. } => collect_fixed_points(body, text, line_index, out),
        StateFrmKind::DataValExprLeftMult(_, expr) | StateFrmKind::DataValExprRightMult(expr, _) => collect_fixed_points(expr, text, line_index, out),
        StateFrmKind::True
        | StateFrmKind::False
        | StateFrmKind::Delay(_)
        | StateFrmKind::Yaled(_)
        | StateFrmKind::Id(_, _)
        | StateFrmKind::Resolved(_, _, _)
        | StateFrmKind::DataValExpr(_) => {}
    }
}

/// A fixpoint variable's own parameter (`n: Nat = 0` in `mu X(n: Nat = 0) = ...`), shown with its
/// declared sort and initial value together as `detail` — unlike [`id_decl_symbol`]'s plain sort,
/// since the initial value is as much a part of this declaration as the sort is.
fn state_var_assignment_symbol(text: &str, line_index: &LineIndex, argument: &StateVarAssignment) -> DocumentSymbol {
    symbol_at(
        argument.identifier.node.clone(),
        Some(format!("{} = {}", argument.sort, argument.expr)),
        SymbolKind::VARIABLE,
        SpanTarget::Local { text, line_index },
        &argument.identifier.span,
        None,
    )
}

/// The `sort`/`cons`/`map`/`eqn` part of the outline, shared by [`document_symbols`],
/// [`pbes_symbols`], [`pres_symbols`], and [`modal_symbols`] — every declaration here goes through
/// [`place`], so one `%import`ed from another file lands in `groups` instead of the returned `Vec`.
fn data_specification_symbols(text: &str, line_index: &LineIndex, sources: &SourceMap, data: &UntypedDataSpecification, groups: &mut ImportGroups) -> Vec<DocumentSymbol> {
    let mut symbols = Vec::new();

    for decl in &data.sort_declarations {
        place(&mut symbols, groups, text, line_index, sources, &decl.span, |target| sort_symbol(decl, target));
    }

    for decl in &data.constructor_declarations {
        place(&mut symbols, groups, text, line_index, sources, &decl.identifier.span, |target| id_decl_symbol(decl, SymbolKind::CONSTRUCTOR, target));
    }

    for decl in &data.map_declarations {
        place(&mut symbols, groups, text, line_index, sources, &decl.identifier.span, |target| id_decl_symbol(decl, SymbolKind::FUNCTION, target));
    }

    for eqn_spec in &data.equation_declarations {
        // `EqnSpec.span` exists but can absorb trailing whitespace past its own `;` (see its doc
        // comment upstream), which would make an empty-looking gap in the outline read as part of
        // this block's range — synthesizing the min-start/max-end over its children's spans
        // instead stays exactly as tight as what's actually being shown as children below. The
        // (grammar-legal) empty block is skipped, since there is then nothing to point the range
        // at either way. Every span here comes from the same file (one `var .. eqn ..` block is
        // parsed as a unit), so using any one of them to decide local-vs-`%import`ed is safe.
        let spans = eqn_spec
            .variables
            .iter()
            .map(|decl| &decl.identifier.span)
            .chain(eqn_spec.equations.iter().map(|eqn| &eqn.span));
        let span = spans.fold(None::<Span>, |acc, span| match acc {
            Some(acc) => Some(Span {
                start: acc.start.min(span.start),
                end: acc.end.max(span.end),
            }),
            None => Some(span.clone()),
        });
        let Some(span) = span else { continue };

        place(&mut symbols, groups, text, line_index, sources, &span, |target| {
            let mut children: Vec<DocumentSymbol> = eqn_spec.variables.iter().map(|decl| id_decl_symbol(decl, SymbolKind::VARIABLE, target)).collect();
            children.extend(eqn_spec.equations.iter().map(|eqn| symbol_at(eqn.lhs.to_string(), Some(eqn.to_string()), SymbolKind::FIELD, target, &eqn.span, None)));
            symbol_at("eqn".to_string(), None, SymbolKind::NAMESPACE, target, &span, Some(children))
        });
    }

    symbols
}

fn sort_symbol(decl: &SortDecl, target: SpanTarget) -> DocumentSymbol {
    symbol_at(decl.identifier.clone(), decl.expr.as_ref().map(|expr| expr.to_string()), SymbolKind::STRUCT, target, &decl.span, None)
}

fn id_decl_symbol<Id>(decl: &IdDecl<Id>, kind: SymbolKind, target: SpanTarget) -> DocumentSymbol {
    symbol_at(decl.identifier.node.clone(), Some(decl.sort.to_string()), kind, target, &decl.identifier.span, None)
}

/// Always local: the importing file's own `init` always wins over anything an import might
/// declare (see `merc_syntax::imports`' own doc comment), so `init` is never `%import`ed.
fn init_symbol(text: &str, line_index: &LineIndex, init: &ProcessExpr) -> DocumentSymbol {
    let range = line_index.range(text, &init.span);
    build_symbol("init".to_string(), Some(init.to_string()), SymbolKind::OBJECT, range, range, None)
}

/// A PBES/PRES `init X(..);` symbol, located via `PropVarInst::span` (an upstream `merc_syntax`
/// addition — it used to carry no `Span` at all, unlike every other node this module builds a
/// symbol for, and had to be recovered with a text search over the whole document instead).
/// Always local — PBES/PRES have no `%import` support at all (see [`pbes_symbols`]'s doc comment).
fn pbes_init_symbol(text: &str, line_index: &LineIndex, init: &PropVarInst) -> DocumentSymbol {
    let range = line_index.range(text, &init.span);
    build_symbol("init".to_string(), Some(init.to_string()), SymbolKind::OBJECT, range, range, None)
}

/// Where a symbol's `range`/`selectionRange` should come from: either resolved normally against
/// this document's own `text`/`line_index`, or — for a declaration `%import`ed from elsewhere,
/// which `DocumentSymbol` has no way to point outside the requested document for at all — a fixed
/// anchor [`Range`] shared by every symbol pulled in through the same `%import` line: that line
/// itself, in *this* document. See [`ImportGroups`] and [`place`] for how a declaration ends up
/// with one or the other.
#[derive(Clone, Copy)]
enum SpanTarget<'a> {
    Local { text: &'a str, line_index: &'a LineIndex },
    Imported(Range),
}

impl SpanTarget<'_> {
    fn range(&self, span: &Span) -> Range {
        match self {
            SpanTarget::Local { text, line_index } => line_index.range(text, span),
            SpanTarget::Imported(range) => *range,
        }
    }
}

/// Builds a symbol whose `range`/`selectionRange` both come from `target` — either `span` itself,
/// resolved locally, or (see [`SpanTarget::Imported`]) a fixed anchor that ignores `span`
/// entirely. `span` is the identifier's own precise span: for a declaration kind (`SortDecl`,
/// `IdDecl`) that's `merc_syntax`'s own per-declaration span; for `ActDecl`/`ProcDecl`/
/// `PropVarDecl` — whose own span still covers the whole declaration (`act a, b: Nat;`,
/// `proc P(n: Nat) = ...;`) — it's `decl.identifier.span` instead.
fn symbol_at(name: String, detail: Option<String>, kind: SymbolKind, target: SpanTarget, span: &Span, children: Option<Vec<DocumentSymbol>>) -> DocumentSymbol {
    let range = target.range(span);
    build_symbol(name, detail, kind, range, range, children)
}

fn build_symbol(name: String, detail: Option<String>, kind: SymbolKind, range: Range, selection_range: Range, children: Option<Vec<DocumentSymbol>>) -> DocumentSymbol {
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

/// Decides, for one declaration's representative `span`, whether it belongs to `text` itself (in
/// which case `build` runs against [`SpanTarget::Local`] and the result is appended straight to
/// `symbols`) or to something `text` `%import`s (in which case `build` instead runs against
/// [`SpanTarget::Imported`].
fn place(symbols: &mut Vec<DocumentSymbol>, groups: &mut ImportGroups, text: &str, line_index: &LineIndex, sources: &SourceMap, span: &Span, build: impl FnOnce(SpanTarget) -> DocumentSymbol) {
    let target = if convert::is_local_span(sources, span) {
        SpanTarget::Local { text, line_index }
    } else {
        match groups.anchor_for(text, line_index, sources, span) {
            Some(range) => SpanTarget::Imported(range),
            None => return,
        }
    };

    let symbol = build(target);
    match target {
        SpanTarget::Local { .. } => symbols.push(symbol),
        SpanTarget::Imported(range) => groups.push(range, symbol),
    }
}

/// Accumulates every symbol `text`'s own `%import`s (transitively) contribute, grouped by
/// directive — grouping (rather than showing each one at its real, out-of-document location) is
/// the only representable option here, since `DocumentSymbol` has no way to point outside the
/// requested document at all (see [`SpanTarget`]'s own doc comment).
#[derive(Default)]
struct ImportGroups {
    /// One entry per `%import` directive that has contributed at least one symbol so far, in the
    /// order its first symbol was found: the directive's own range within `text` (used as both
    /// the group's own `range`/`selectionRange` and every one of its children's — see
    /// [`SpanTarget::Imported`]), the import path to name the group after, and its children so
    /// far.
    entries: Vec<(Range, String, Vec<DocumentSymbol>)>,
}

impl ImportGroups {
    /// The anchor [`Range`] a declaration whose `span` lands outside `text` should be built
    /// against — the range, within `text` itself, of whichever of `text`'s own `%import`
    /// directives (transitively) pulls `span`'s file in — creating a fresh (childless, for now)
    /// group the first time a particular directive is seen. `None` if `span`'s file isn't
    /// reachable through any of `text`'s own `%import`s at all.
    fn anchor_for(&mut self, text: &str, line_index: &LineIndex, sources: &SourceMap, span: &Span) -> Option<Range> {
        let directory = diagnostics::root_import_directory(sources)?;
        let directive = diagnostics::owning_import_directive(text, sources, &directory, sources.lookup(span.start))?;
        let range = line_index.range(text, &directive.span);

        if !self.entries.iter().any(|(existing, ..)| *existing == range) {
            self.entries.push((range, directive.node.path, Vec::new()));
        }
        Some(range)
    }

    /// Appends `symbol` to whichever group is anchored at `range` — always one a prior
    /// [`Self::anchor_for`] call already created.
    fn push(&mut self, range: Range, symbol: DocumentSymbol) {
        if let Some((_, _, children)) = self.entries.iter_mut().find(|(existing, ..)| *existing == range) {
            children.push(symbol);
        }
    }

    /// Turns every accumulated group into one `SymbolKind::MODULE` [`DocumentSymbol`] — named
    /// after the import path, its own `range`/`selectionRange` the `%import` line itself — in the
    /// order each directive's first symbol was found.
    fn finish(self) -> Vec<DocumentSymbol> {
        self.entries.into_iter().map(|(range, path, children)| build_symbol(path, None, SymbolKind::MODULE, range, range, Some(children))).collect()
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
        let (outcome, sources) = parse(SpecKind::Process, text.to_string(), None).await;
        let line_index = LineIndex::new(text);
        match outcome {
            ParseOutcome::Ok(Specification::Process(spec)) => document_symbols(text, &line_index, &sources, &spec),
            _ => panic!("fixture failed to parse"),
        }
    }

    async fn pbes_symbols_for(text: &str) -> Vec<DocumentSymbol> {
        let (outcome, sources) = parse(SpecKind::Pbes, text.to_string(), None).await;
        let line_index = LineIndex::new(text);
        match outcome {
            ParseOutcome::Ok(Specification::Pbes(spec)) => pbes_symbols(text, &line_index, &sources, &spec),
            _ => panic!("fixture failed to parse"),
        }
    }

    async fn pres_symbols_for(text: &str) -> Vec<DocumentSymbol> {
        let (outcome, sources) = parse(SpecKind::Pres, text.to_string(), None).await;
        let line_index = LineIndex::new(text);
        match outcome {
            ParseOutcome::Ok(Specification::Pres(spec)) => pres_symbols(text, &line_index, &sources, &spec),
            _ => panic!("fixture failed to parse"),
        }
    }

    async fn modal_symbols_for(text: &str) -> Vec<DocumentSymbol> {
        let (outcome, sources) = parse(SpecKind::Modal, text.to_string(), None).await;
        let line_index = LineIndex::new(text);
        match outcome {
            ParseOutcome::Ok(Specification::Modal(spec)) => modal_symbols(text, &line_index, &sources, &spec),
            _ => panic!("fixture failed to parse"),
        }
    }

    /// Writes `files` (relative-path -> contents) into a fresh temp directory and returns it —
    /// mirrors `merc_syntax::imports`'s and `goto_definition.rs`'s own test helpers of the same
    /// name.
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
        let text = "pres mu X(n: Bool) = true;\ninit X(n);".to_string();
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

    #[tokio::test]
    async fn modal_fixed_point_and_action_are_located() {
        let text = "act a: Nat;\nform nu X(n: Nat = 0) . [a(n)]X(n);".to_string();
        let symbols = modal_symbols_for(&text).await;

        let action = symbols.iter().find(|s| s.kind == SymbolKind::EVENT).expect("expected the act declaration as a symbol");
        assert_eq!(action.name, "a");

        let fixed_point = symbols.iter().find(|s| s.kind == SymbolKind::FUNCTION).expect("expected the fixpoint variable as a symbol");
        assert_eq!(fixed_point.name, "X");
        assert_eq!(fixed_point.children.as_ref().map(Vec::len), Some(1), "the fixpoint's own parameter should be a child");
    }

    #[tokio::test]
    async fn nested_fixed_point_becomes_a_child_of_the_enclosing_one() {
        let text = "form mu X . (nu Y . X) && true;".to_string();
        let symbols = modal_symbols_for(&text).await;

        let outer = symbols.iter().find(|s| s.name == "X").expect("expected the outer fixpoint as a top-level symbol");
        let inner = outer.children.as_ref().and_then(|children| children.iter().find(|child| child.name == "Y"));
        assert!(inner.is_some(), "expected 'Y' to be nested under 'X', got children: {:?}", outer.children);
    }

    #[tokio::test]
    async fn imported_declarations_are_grouped_under_their_own_import_line() {
        let dir = temp_project(&[
            ("main.mcrl2", "%import \"common.mcrl2\"\nact b: Nat;\ninit a(c) . b(c);\n"),
            ("common.mcrl2", "sort D;\ncons c: D;\nact a: D;\n"),
        ]);
        let main_path = dir.path().join("main.mcrl2");
        let text = std::fs::read_to_string(&main_path).unwrap();
        let (outcome, sources) = parse(SpecKind::Process, text.clone(), Some(main_path)).await;
        let ParseOutcome::Ok(Specification::Process(spec)) = outcome else {
            panic!("fixture failed to parse");
        };
        let line_index = LineIndex::new(&text);
        let symbols = document_symbols(&text, &line_index, &sources, &spec);

        let group = symbols.iter().find(|s| s.kind == SymbolKind::MODULE).expect("expected an import group");
        assert_eq!(group.name, "common.mcrl2");
        // Anchored on the `%import` line itself (line 0) — the only place a `DocumentSymbol`
        // pulled in from another file can legitimately point within *this* document.
        assert_eq!(group.range.start.line, 0);
        assert_eq!(group.selection_range, group.range);

        let children = group.children.as_ref().expect("import group should have children");
        let names: std::collections::HashSet<_> = children.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, std::collections::HashSet::from(["D", "c", "a"]));
        for child in children {
            assert_eq!(child.range, group.range, "every symbol %import\"ed through the same directive shares its anchor range");
        }

        // `b` and `init` are this document's own — not swept into the import group.
        assert!(symbols.iter().any(|s| s.name == "b" && s.kind == SymbolKind::EVENT));
        assert!(symbols.iter().any(|s| s.name == "init"));
        assert!(!children.iter().any(|s| s.name == "b" || s.name == "init"));
    }

    #[tokio::test]
    async fn diamond_imported_declarations_land_in_a_single_group() {
        let dir = temp_project(&[
            ("main.mcrl2", "%import \"a.mcrl2\"\n%import \"b.mcrl2\"\ninit delta;\n"),
            ("a.mcrl2", "%import \"common.mcrl2\"\n"),
            ("b.mcrl2", "%import \"common.mcrl2\"\n"),
            ("common.mcrl2", "sort D;\n"),
        ]);
        let main_path = dir.path().join("main.mcrl2");
        let text = std::fs::read_to_string(&main_path).unwrap();
        let (outcome, sources) = parse(SpecKind::Process, text.clone(), Some(main_path)).await;
        let ParseOutcome::Ok(Specification::Process(spec)) = outcome else {
            panic!("fixture failed to parse");
        };
        let line_index = LineIndex::new(&text);
        let symbols = document_symbols(&text, &line_index, &sources, &spec);

        let groups: Vec<_> = symbols.iter().filter(|s| s.kind == SymbolKind::MODULE).collect();
        assert_eq!(groups.len(), 1, "a diamond import should still only produce one group, got: {symbols:?}");
    }
}
