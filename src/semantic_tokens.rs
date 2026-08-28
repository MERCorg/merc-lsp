//! Builds `textDocument/semanticTokens/full` output from a parsed
//! [`UntypedProcessSpecification`].
//!
//! This is required for mCRL2 since the grammar is ambiguous, for example a(f)
//! could be a function application, an action instantiation, or a process
//! instantiation depending on the context.
//! 
//! A system sort (`Bool`, `Nat`, …, and the parameterized
//! `List`/`Set`/`Bag`/`FSet`/`FBag`) is tagged [`TokenKind::Type`] like any
//! other sort reference, but with [`MODIFIER_DEFAULT_LIBRARY`] set, so a theme
//! can still tell it apart from a user's own `sort` declaration.
//!
//! Deliberately unscoped: identifiers are classified against one flat table of
//! declared names (see [`SymbolTable`]), not full lexical scoping. A bound
//! variable therefore reads as [`TokenKind::Variable`] both at its binder and
//! at every use — same as a free/global variable — since telling them apart
//! needs real name resolution, which nothing upstream exposes yet for
//! process/action bodies (see `PLAN.md`). Only the `declaration` modifier marks
//! a binder site specifically. This still recovers everything a TextMate
//! grammar structurally cannot.
//!
//! One grammar quirk the symbol table also has to paper over: a process
//! reference with no arguments and no parentheses (the overwhelmingly common
//! case — `proc P = a . P;`'s recursive `P`) is *not* parsed as
//! [`ProcessExprKind::Id`]. `ProcExprId` in the grammar requires parentheses
//! (even empty ones, `P()`); a bare `P` instead falls through to the same
//! `Action` rule a real action instantiation uses, landing as
//! [`ProcessExprKind::Action`]. So that variant is looked up against *both* the
//! declared action names and the declared process names before deciding
//! [`TokenKind::Event`] vs [`TokenKind::Method`] — this is the one place a
//! `ProcessExprKind::Action` might actually name a process, not an action.

use std::collections::HashSet;
use std::ops::ControlFlow;

use lsp_types::SemanticToken;
use lsp_types::SemanticTokenModifier;
use lsp_types::SemanticTokenType;
use lsp_types::SemanticTokensLegend;
use merc_syntax::DataExpr;
use merc_syntax::DataExprKind;
use merc_syntax::ProcessExpr;
use merc_syntax::ProcessExprKind;
use merc_syntax::SortExpression;
use merc_syntax::SortExpressionKind;
use merc_syntax::Span;
use merc_syntax::Traverse;
use merc_syntax::UntypedProcessSpecification;

use crate::convert::LineIndex;
use crate::convert::is_identifier_byte;
use crate::symbols::find_identifier;

/// A token's semantic type, as an index into the legend returned by [`legend`] — the two must be
/// kept in lock-step, since only this numeric index (not a name) is sent over the wire.
#[derive(Clone, Copy)]
enum TokenKind {
    Type = 0,
    Variable = 1,
    /// A process reference, whether a `proc` declaration itself or an instantiation of one — see
    /// [`SymbolTable::classify_action`]. Deliberately kept a distinct token type from
    /// [`TokenKind::Event`], so a theme colors a process instantiation differently from an action
    /// instantiation even though both can parse as the same [`ProcessExprKind::Action`] shape
    /// (see the module docs above).
    Method = 2,
    /// An action declaration or instantiation — see [`SymbolTable::classify_action`]. Kept a
    /// separate token type from [`TokenKind::Method`] specifically so actions and processes don't
    /// end up the same color.
    Event = 3,
    EnumMember = 4,
    /// A `proc` declaration's own parameter, or a reference to one (the `x` in `P(x = e)`) —
    /// distinct from [`TokenKind::Variable`], which covers every *bound* variable (`sum`/`dist`/
    /// `forall`/`exists`/`lambda`/comprehension) and `var`/`glob` declaration instead.
    Parameter = 5,
    /// A reserved mCRL2 word (`sort`, `proc`, `sum`, `true`, …) — see [`tag_keywords`].
    Keyword = 6,
}

const MODIFIER_DECLARATION: u32 = 1 << 0;
/// Marks a system-defined sort (`Bool`, `Nat`, …, and the parameterized `List`/`Set`/`Bag`/
/// `FSet`/`FBag`) — mCRL2's own reserved sort names, as distinct from a user's own `sort`
/// declaration, via the standard [`SemanticTokenModifier::DEFAULT_LIBRARY`]. See
/// [`walk_sort_expression`].
const MODIFIER_DEFAULT_LIBRARY: u32 = 1 << 1;

/// The legend advertised by `capabilities::server_capabilities`; must list types/modifiers in the
/// exact order [`TokenKind`]/[`MODIFIER_DECLARATION`]/[`MODIFIER_DEFAULT_LIBRARY`] assume.
pub fn legend() -> SemanticTokensLegend {
    SemanticTokensLegend {
        token_types: vec![
            SemanticTokenType::TYPE,
            SemanticTokenType::VARIABLE,
            SemanticTokenType::METHOD,
            SemanticTokenType::EVENT,
            SemanticTokenType::ENUM_MEMBER,
            SemanticTokenType::PARAMETER,
            SemanticTokenType::KEYWORD,
        ],
        token_modifiers: vec![SemanticTokenModifier::DECLARATION, SemanticTokenModifier::DEFAULT_LIBRARY],
    }
}

/// Builds the full, delta-encoded semantic token list for `spec`.
pub fn semantic_tokens(text: &str, line_index: &LineIndex, spec: &UntypedProcessSpecification) -> Vec<SemanticToken> {
    let symbols = SymbolTable::collect(spec);
    let mut builder = Builder::new(text, line_index);

    for decl in &spec.data_specification.sort_declarations {
        builder.push(&decl.span, TokenKind::Type, true);
        if let Some(expr) = &decl.expr {
            walk_sort_expression(expr, &mut builder);
        }
    }

    for decl in &spec.data_specification.constructor_declarations {
        builder.push(&decl.span, TokenKind::EnumMember, true);
        walk_sort_expression(&decl.sort, &mut builder);
    }

    for decl in &spec.data_specification.map_declarations {
        // Deliberately no `builder.push` for the mapping's own name, at declaration or at any
        // use site below (see `SymbolTable::classify_data_id`): mappings are left uncolored, so
        // they read as plain text rather than competing for a color with constructors/processes.
        walk_sort_expression(&decl.sort, &mut builder);
    }

    for eqn_spec in &spec.data_specification.equation_declarations {
        for decl in &eqn_spec.variables {
            builder.push(&decl.span, TokenKind::Variable, true);
            walk_sort_expression(&decl.sort, &mut builder);
        }
        for eqn in &eqn_spec.equations {
            if let Some(condition) = &eqn.condition {
                walk_data_expr(condition, &symbols, &mut builder);
            }
            walk_data_expr(&eqn.lhs, &symbols, &mut builder);
            walk_data_expr(&eqn.rhs, &symbols, &mut builder);
        }
    }

    for decl in &spec.global_variables {
        builder.push(&decl.span, TokenKind::Variable, true);
        walk_sort_expression(&decl.sort, &mut builder);
    }

    for decl in &spec.action_declarations {
        builder.push(&decl.span, TokenKind::Event, true);
        for arg in &decl.args {
            walk_sort_expression(arg, &mut builder);
        }
    }

    for decl in &spec.process_declarations {
        builder.push(&decl.span, TokenKind::Method, true);
        for param in &decl.params {
            builder.push(&param.span, TokenKind::Parameter, true);
            walk_sort_expression(&param.sort, &mut builder);
        }
        walk_process_expr(&decl.body, &symbols, &mut builder);
    }

    if let Some(init) = &spec.init {
        walk_process_expr(init, &symbols, &mut builder);
    }

    tag_keywords(text, &mut builder);

    builder.finish()
}

/// Which declared identifiers name what — the disambiguation a TextMate grammar cannot do, since
/// the grammar gives the same shape to several different declaration kinds. Used to classify a
/// bare [`DataExprKind::Id`] (function, constructor, or variable) and a
/// [`ProcessExprKind::Action`] whose name might actually belong to a process, not an action (see
/// the module docs above).
struct SymbolTable<'a> {
    maps: HashSet<&'a str>,
    constructors: HashSet<&'a str>,
    processes: HashSet<&'a str>,
}

impl<'a> SymbolTable<'a> {
    fn collect(spec: &'a UntypedProcessSpecification) -> Self {
        SymbolTable {
            maps: spec
                .data_specification
                .map_declarations
                .iter()
                .map(|decl| decl.identifier.as_str())
                .collect(),
            constructors: spec
                .data_specification
                .constructor_declarations
                .iter()
                .map(|decl| decl.identifier.as_str())
                .collect(),
            processes: spec.process_declarations.iter().map(|decl| decl.identifier.as_str()).collect(),
        }
    }

    /// Classifies a [`DataExprKind::Id`] occurrence, or `None` if it names a mapping — mappings
    /// are deliberately left uncolored (see [`semantic_tokens`]'s map-declaration loop), so a use
    /// site has to stay uncolored too rather than fall back to some other kind.
    fn classify_data_id(&self, name: &str) -> Option<TokenKind> {
        if self.constructors.contains(name) {
            Some(TokenKind::EnumMember)
        } else if self.maps.contains(name) {
            None
        } else {
            // Not declared as a map or constructor: a bound or free variable. This is also the
            // fallback for a name that isn't declared at all — flagging that is a diagnostics
            // concern (type checking), not this pass's job.
            Some(TokenKind::Variable)
        }
    }

    /// Classifies a [`ProcessExprKind::Action`] occurrence, which — per the module docs — is also
    /// how a parenthesis-less process reference parses.
    fn classify_action(&self, name: &str) -> TokenKind {
        if self.processes.contains(name) {
            TokenKind::Method
        } else {
            // Either a real action, or a name nothing declares — same reasoning as
            // `classify_data_id`'s fallback.
            TokenKind::Event
        }
    }
}

/// Accumulates `(span, kind, modifiers)` triples in the order they're discovered and turns them
/// into the sorted, delta-encoded `Vec<SemanticToken>` the protocol requires, in [`Builder::finish`].
struct Builder<'a> {
    text: &'a str,
    line_index: &'a LineIndex,
    raw: Vec<(Span, TokenKind, u32)>,
}

impl<'a> Builder<'a> {
    fn new(text: &'a str, line_index: &'a LineIndex) -> Self {
        Builder { text, line_index, raw: Vec::new() }
    }

    /// Tags `span` directly — used for expression-level AST nodes, whose span is already exactly
    /// the identifier (verified against `merc_syntax`'s `precedence.rs`/`consume.rs`: `DataExpr`
    /// leaves and `SortExpression` references are spanned from their own grammar rule, not a
    /// surrounding one).
    fn push(&mut self, span: &Span, kind: TokenKind, is_declaration: bool) {
        let modifiers = if is_declaration { MODIFIER_DECLARATION } else { 0 };
        self.raw.push((span.clone(), kind, modifiers));
    }

    /// Tags `span` as [`TokenKind::Type`] with [`MODIFIER_DEFAULT_LIBRARY`] — a reference to one
    /// of mCRL2's own system sorts, never a declaration. See [`walk_sort_expression`].
    fn push_builtin_type(&mut self, span: &Span) {
        self.raw.push((span.clone(), TokenKind::Type, MODIFIER_DEFAULT_LIBRARY));
    }

    /// Tags just `identifier` within `span`, for the one shape that still covers more than the
    /// name itself: a process/action instantiation's (`ProcExprId`/`Action` are spanned over the
    /// whole `name(args...)`, not just `name`). Every declaration kind's own span is precisely
    /// the identifier already (see `symbols.rs`'s `symbol_at`) — [`Builder::push`] tags those
    /// directly.
    fn push_identifier(&mut self, span: &Span, identifier: &str, kind: TokenKind, is_declaration: bool) {
        let span = find_identifier(self.text, span, identifier).unwrap_or_else(|| span.clone());
        self.push(&span, kind, is_declaration);
    }

    fn finish(mut self) -> Vec<SemanticToken> {
        // Byte offset is a valid sort key even though the protocol wants line/character order:
        // offsets increase monotonically with (line, character) for any span within one document.
        self.raw.sort_by_key(|(span, ..)| span.start);

        let mut tokens = Vec::with_capacity(self.raw.len());
        let mut prev_line = 0u32;
        let mut prev_start = 0u32;

        for (span, kind, modifiers) in self.raw {
            let range = self.line_index.range(self.text, &span);
            // Every span tagged above is a single identifier, which can't contain a newline, so
            // start/end always land on the same line and a plain character delta is exact.
            let length = range.end.character.saturating_sub(range.start.character);
            if length == 0 {
                // A synthetic (zero-width or default) span has nothing to underline.
                continue;
            }

            let delta_line = range.start.line - prev_line;
            let delta_start = if delta_line == 0 {
                range.start.character - prev_start
            } else {
                range.start.character
            };

            tokens.push(SemanticToken {
                delta_line,
                delta_start,
                length,
                token_type: kind as u32,
                token_modifiers_bitset: modifiers,
            });

            prev_line = range.start.line;
            prev_start = range.start.character;
        }

        tokens
    }
}

/// Walks every sort name in `expr`'s subtree, distinguishing a user-defined sort
/// ([`SortExpressionKind::Reference`]/[`SortExpressionKind::Resolved`]) from one of mCRL2's own
/// system sorts ([`SortExpressionKind::Simple`] — `Bool`/`Pos`/`Nat`/`Int`/`Real` — and
/// [`SortExpressionKind::Complex`] — the parameterized `List`/`Set`/`Bag`/`FSet`/`FBag`) via
/// [`MODIFIER_DEFAULT_LIBRARY`]. A TextMate grammar cannot make either distinction context-free:
/// `D` and `Bool` are both just identifiers to it, and it has no notion of "declared by the user"
/// at all.
///
/// `Complex`'s own span covers the whole `List(Nat)`, not just the keyword (unlike every other
/// span this module tags directly — see [`Builder::push`]'s doc comment) — its subsort is a
/// *child* node the recursion below reaches on its own, so tagging the whole span here would
/// double-tag it with an overlapping token. `ComplexSort`'s `Display` is exactly its source
/// keyword (`List`, `Set`, …), so its own span is cheap to compute the same way a few `merc_syntax`
/// declaration spans are: it starts exactly where the whole node does.
fn walk_sort_expression(expr: &SortExpression, builder: &mut Builder) {
    expr.visit::<(), _>(|node| {
        match &node.node {
            SortExpressionKind::Reference(_) | SortExpressionKind::Resolved(_, _) => {
                builder.push(&node.span, TokenKind::Type, false);
            }
            SortExpressionKind::Simple(_) => {
                builder.push_builtin_type(&node.span);
            }
            SortExpressionKind::Complex(complex_sort, _) => {
                let keyword = complex_sort.to_string();
                let span = Span { start: node.span.start, end: node.span.start + keyword.len() };
                builder.push_builtin_type(&span);
            }
            _ => {}
        }
        ControlFlow::Continue(())
    });
}

/// Walks every identifier in `expr`'s subtree: bare references (classified via `symbols`) and any
/// binder (`lambda`/`forall`/`exists`/set-or-bag comprehension) it introduces along the way, plus
/// that binder's sort.
fn walk_data_expr(expr: &DataExpr, symbols: &SymbolTable, builder: &mut Builder) {
    expr.visit::<(), _>(|node| {
        match &node.node {
            DataExprKind::Id(name) => {
                if let Some(kind) = symbols.classify_data_id(name) {
                    builder.push(&node.span, kind, false);
                }
            }
            DataExprKind::SetBagComp { variable, .. } => {
                builder.push(&variable.span, TokenKind::Variable, true);
                walk_sort_expression(&variable.sort, builder);
            }
            DataExprKind::Lambda { variables, .. } | DataExprKind::Quantifier { variables, .. } => {
                for variable in variables {
                    builder.push(&variable.span, TokenKind::Variable, true);
                    walk_sort_expression(&variable.sort, builder);
                }
            }
            _ => {}
        }
        ControlFlow::Continue(())
    });
}

/// Walks every identifier in `expr`'s subtree: process instantiations, action instantiations, and
/// any `sum`/`dist` binder, descending into the data expressions each carries — arguments,
/// assignments, distributions, and conditions — none of which `Traverse` crosses into on its own,
/// since they're a different node type ([`DataExpr`], not [`ProcessExpr`]).
fn walk_process_expr(expr: &ProcessExpr, symbols: &SymbolTable, builder: &mut Builder) {
    expr.visit::<(), _>(|node| {
        match &node.node {
            ProcessExprKind::Id(name, assignments) => {
                builder.push_identifier(&node.span, name, TokenKind::Method, false);
                for assignment in assignments {
                    // The parameter name in `x = e` — a *use* of an existing process parameter,
                    // not a new binding, hence no `MODIFIER_DECLARATION`.
                    builder.push(&assignment.span, TokenKind::Parameter, false);
                    walk_data_expr(&assignment.expr, symbols, builder);
                }
            }
            ProcessExprKind::Action(name, arguments) => {
                builder.push_identifier(&node.span, name, symbols.classify_action(name), false);
                for argument in arguments {
                    walk_data_expr(argument, symbols, builder);
                }
            }
            ProcessExprKind::Sum { variables, .. } => {
                for variable in variables {
                    builder.push(&variable.span, TokenKind::Variable, true);
                    walk_sort_expression(&variable.sort, builder);
                }
            }
            ProcessExprKind::Dist { variables, expr, .. } => {
                for variable in variables {
                    builder.push(&variable.span, TokenKind::Variable, true);
                    walk_sort_expression(&variable.sort, builder);
                }
                walk_data_expr(expr, symbols, builder);
            }
            ProcessExprKind::Condition { condition, .. } => {
                walk_data_expr(condition, symbols, builder);
            }
            ProcessExprKind::At { operand, .. } => {
                walk_data_expr(operand, symbols, builder);
            }
            _ => {}
        }
        ControlFlow::Continue(())
    });
}

/// Every word-like mCRL2 keyword relevant to a process/data specification, for [`tag_keywords`].
/// Built-in sort names (`Bool`, `List`, …) are deliberately not here: [`walk_sort_expression`]
/// already tags those, more precisely (node by node, off the AST, not a blind text scan).
///
/// `true`/`false`/`delta`/`tau` are genuinely reserved — the grammar rejects them as the prefix
/// of a longer identifier (`DataExprTrue = { "true" ~ !Id }` and siblings; see `merc_syntax`'s
/// own `keywords_are_not_prefix_of_identifiers` test) — so a word-boundary match of one of these
/// can never actually be a user identifier. The rest (`sort`, `map`, `proc`, …) only ever appear
/// as unambiguous block-introducing prefixes in the grammar and have no such guard, so in
/// principle nothing stops a spec from declaring, say, a map literally named `sort`; in practice
/// this essentially never happens, and accepting that rather than leaving every structural
/// keyword uncolored is the better trade.
const KEYWORDS: &[&str] = &[
    "sort", "cons", "map", "glob", "act", "proc", "init", "var", "eqn", "struct", "whr",
    "forall", "exists", "lambda", "sum", "dist", "val", "true", "false", "delta", "tau",
    "hide", "block", "allow", "comm", "rename",
];

/// Tags every occurrence of a reserved mCRL2 keyword (see [`KEYWORDS`]) as [`TokenKind::Keyword`].
///
/// Unlike every other pass in this module, this is a blind scan over the raw source text, not
/// the AST — deliberately: a general-purpose LSP server can't assume its client has (or even
/// *can* have) a TextMate grammar to fall back on for something as basic as keyword coloring —
/// that format is a VS Code-family convention, not a universal one, and plenty of LSP clients
/// (Neovim, Helix, Emacs' `eglot`, …) have no such fallback at all. So the server colors keywords
/// itself, the same way it colors everything else. Word-boundary matching (via
/// [`is_identifier_byte`]) keeps this from matching a keyword-shaped substring of a longer
/// identifier (`sortable` does not contain the keyword `sort`), and `%`-comments — mCRL2's only
/// comment syntax, running to end of line, with no escape mechanism that could hide a literal `%`
/// inside anything else — are skipped explicitly, since nothing else marks their extent for a
/// text-only pass to lean on.
fn tag_keywords(text: &str, builder: &mut Builder) {
    let bytes = text.as_bytes();
    let mut in_comment = false;
    let mut i = 0;
    while i < bytes.len() {
        let byte = bytes[i];
        if in_comment {
            in_comment = byte != b'\n';
            i += 1;
            continue;
        }
        if byte == b'%' {
            in_comment = true;
            i += 1;
            continue;
        }
        if !is_identifier_byte(byte) {
            i += 1;
            continue;
        }

        let word_start = i;
        while i < bytes.len() && is_identifier_byte(bytes[i]) {
            i += 1;
        }
        let word = &text[word_start..i];
        if KEYWORDS.contains(&word) {
            builder.push(&Span { start: word_start, end: i }, TokenKind::Keyword, false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::ParseOutcome;
    use crate::parse::parse;

    async fn tokens_for(text: &str) -> Vec<SemanticToken> {
        let outcome = parse(text.to_string()).await;
        let line_index = LineIndex::new(text);
        match outcome {
            ParseOutcome::Ok(spec) => semantic_tokens(text, &line_index, &spec),
            _ => panic!("fixture failed to parse"),
        }
    }

    /// Reconstructs absolute (line, character, length, type, modifiers) tuples from the
    /// delta-encoded token stream, so assertions can be written against plain positions.
    fn absolute(tokens: &[SemanticToken]) -> Vec<(u32, u32, u32, u32, u32)> {
        let mut line = 0u32;
        let mut character = 0u32;
        let mut result = Vec::new();
        for token in tokens {
            line += token.delta_line;
            character = if token.delta_line == 0 { character + token.delta_start } else { token.delta_start };
            result.push((line, character, token.length, token.token_type, token.token_modifiers_bitset));
        }
        result
    }

    #[tokio::test]
    async fn distinguishes_action_process_and_map_calls_with_identical_shape() {
        // `a(true)`, `P(true)`, and `f(true)` are syntactically identical apart from which block
        // declared the name — exactly what a TextMate grammar cannot tell apart.
        let text = "map f: Bool -> Bool;\nact a: Bool;\nproc P(b: Bool) = a(f(b));\ninit P(true);";
        let tokens = tokens_for(text).await;
        let positions = absolute(&tokens);

        let event_start = text.find("a(f(b))").unwrap();
        let mapping_start = text.find("f(b)").unwrap();
        let method_start = text.rfind("P(true)").unwrap();

        let line_index = LineIndex::new(text);
        let event_pos = line_index.position(text, event_start);
        let mapping_pos = line_index.position(text, mapping_start);
        let method_pos = line_index.position(text, method_start);

        // `a` (an action) and `P` (a process) get distinct, differently-colored token kinds even
        // though both parse as the same `ProcessExprKind::Action` shape.
        assert!(
            positions
                .iter()
                .any(|&(l, c, len, ty, _)| l == event_pos.line && c == event_pos.character && len == 1 && ty == TokenKind::Event as u32)
        );
        assert!(positions.iter().any(|&(l, c, _, ty, _)| l == method_pos.line
            && c == method_pos.character
            && ty == TokenKind::Method as u32));
        // `f` (a mapping) is deliberately left uncolored: no token starts at its use site.
        assert!(!positions.iter().any(|&(l, c, ..)| l == mapping_pos.line && c == mapping_pos.character));
    }

    #[tokio::test]
    async fn classifies_bare_identifiers_by_declaring_block() {
        // `c` (a constructor) and `x` (a variable) are indistinguishable from source text alone.
        let text = "sort D;\ncons c: D;\nmap f: D -> Bool;\nvar x: D;\neqn f(x) = f(c);";
        let tokens = tokens_for(text).await;
        let positions = absolute(&tokens);
        let line_index = LineIndex::new(text);

        let c_use = text.rfind('c').unwrap();
        let x_use = text.match_indices('x').nth(1).unwrap().0; // second `x`: the use in `f(x)`.
        let c_pos = line_index.position(text, c_use);
        let x_pos = line_index.position(text, x_use);

        assert!(positions.iter().any(|&(l, c, _, ty, modifiers)| l == c_pos.line
            && c == c_pos.character
            && ty == TokenKind::EnumMember as u32
            && modifiers == 0));
        assert!(positions.iter().any(|&(l, c, _, ty, modifiers)| l == x_pos.line
            && c == x_pos.character
            && ty == TokenKind::Variable as u32
            && modifiers == 0));
    }

    #[tokio::test]
    async fn tokens_are_sorted_and_non_overlapping() {
        let text = "sort D;\ncons c: D;\nmap f: D -> D;\nvar x: D;\neqn f(x) = f(c);\nact a;\nproc P = a . P;\ninit P;";
        let tokens = tokens_for(text).await;
        let positions = absolute(&tokens);

        for window in positions.windows(2) {
            let [(l1, c1, len1, ..), (l2, c2, ..)] = window else { unreachable!() };
            assert!((*l1, *c1) < (*l2, *c2), "tokens must be strictly ordered by position");
            if l1 == l2 {
                assert!(c1 + len1 <= *c2, "tokens on the same line must not overlap");
            }
        }
    }

    #[tokio::test]
    async fn quantifier_binder_is_tagged_as_a_variable_declaration() {
        // `forall`'s body extends as far right as it can, so the quantifier has to sit on one
        // side of the `eqn` and not straddle its `=` — `allP` (a nullary map) is the other side.
        let text = "sort D;\nmap p: D -> Bool;\nmap allP: Bool;\neqn allP = forall x: D . p(x);";
        let tokens = tokens_for(text).await;
        let positions = absolute(&tokens);
        let line_index = LineIndex::new(text);

        let binder = text.find("x:").unwrap();
        let binder_pos = line_index.position(text, binder);

        assert!(positions.iter().any(|&(l, c, _, ty, modifiers)| l == binder_pos.line
            && c == binder_pos.character
            && ty == TokenKind::Variable as u32
            && modifiers == MODIFIER_DECLARATION));
    }

    #[tokio::test]
    async fn assignment_form_instantiation_tags_the_parameter_name() {
        // `x` in `P(x = 1)` is a *use* of `P`'s own parameter, not a new binding — this is new
        // ground `Assignment` couldn't cover before it carried a span of its own.
        let text = "proc P(x: Bool) = delta;\ninit P(x = true);";
        let tokens = tokens_for(text).await;
        let positions = absolute(&tokens);
        let line_index = LineIndex::new(text);

        let use_site = text.rfind("x = true").unwrap();
        let use_pos = line_index.position(text, use_site);

        assert!(positions.iter().any(|&(l, c, len, ty, modifiers)| l == use_pos.line
            && c == use_pos.character
            && len == 1
            && ty == TokenKind::Parameter as u32
            && modifiers == 0));
    }

    #[tokio::test]
    async fn system_sorts_are_tagged_default_library_user_sorts_are_not() {
        let text = "sort D;\nmap f: D -> Bool;\nmap g: List(Nat) -> D;";
        let tokens = tokens_for(text).await;
        let positions = absolute(&tokens);
        let line_index = LineIndex::new(text);

        let user_sort_pos = line_index.position(text, text.rfind("D -> Bool").unwrap());
        let bool_pos = line_index.position(text, text.rfind("Bool").unwrap());
        let list_pos = line_index.position(text, text.find("List(Nat)").unwrap());
        let nat_pos = line_index.position(text, text.find("Nat").unwrap());

        assert!(positions.iter().any(|&(l, c, _, ty, modifiers)| l == user_sort_pos.line
            && c == user_sort_pos.character
            && ty == TokenKind::Type as u32
            && modifiers == 0), "a user sort reference must not be marked default-library");
        assert!(positions.iter().any(|&(l, c, _, ty, modifiers)| l == bool_pos.line
            && c == bool_pos.character
            && ty == TokenKind::Type as u32
            && modifiers == MODIFIER_DEFAULT_LIBRARY), "Bool must be marked default-library");
        assert!(positions.iter().any(|&(l, c, len, ty, modifiers)| l == list_pos.line
            && c == list_pos.character
            && len == 4 // "List", not "List(Nat)" — must not swallow the subsort.
            && ty == TokenKind::Type as u32
            && modifiers == MODIFIER_DEFAULT_LIBRARY));
        assert!(positions.iter().any(|&(l, c, _, ty, modifiers)| l == nat_pos.line
            && c == nat_pos.character
            && ty == TokenKind::Type as u32
            && modifiers == MODIFIER_DEFAULT_LIBRARY), "List's own subsort Nat is tagged separately");
    }

    #[tokio::test]
    async fn process_parameter_declaration_is_tagged_parameter_not_variable() {
        let text = "proc P(x: Bool) = delta;\ninit P(true);";
        let tokens = tokens_for(text).await;
        let positions = absolute(&tokens);
        let line_index = LineIndex::new(text);

        let param_pos = line_index.position(text, text.find('x').unwrap());
        assert!(positions.iter().any(|&(l, c, _, ty, modifiers)| l == param_pos.line
            && c == param_pos.character
            && ty == TokenKind::Parameter as u32
            && modifiers == MODIFIER_DECLARATION));
    }
}
