//! Builds `textDocument/semanticTokens/full` output from a parsed [`UntypedProcessSpecification`]
//! ([`semantic_tokens`]), [`UntypedPbes`] ([`pbes_semantic_tokens`]), or [`UntypedPres`]
//! ([`pres_semantic_tokens`]).
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
//! [`TokenKind::Parameter`] is the one exception to "flat table": a `proc`
//! declaration's (or PBES equation's) own parameters *are* scoped to that one
//! declaration's body, via the `current_params` threaded through
//! [`walk_process_expr`]/[`walk_pbes_expr`]/[`walk_data_expr`] — a name that is
//! merely some *other* declaration's parameter falls back to
//! [`TokenKind::Variable`] instead, rather than lighting up everywhere that
//! name happens to appear.
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
use merc_syntax::PbesExpr;
use merc_syntax::PbesExprKind;
use merc_syntax::PresExpr;
use merc_syntax::PresExprKind;
use merc_syntax::ProcessExpr;
use merc_syntax::ProcessExprKind;
use merc_syntax::PropVarInst;
use merc_syntax::SortExpression;
use merc_syntax::SortExpressionKind;
use merc_syntax::Span;
use merc_syntax::Traverse;
use merc_syntax::UntypedDataSpecification;
use merc_syntax::UntypedPbes;
use merc_syntax::UntypedPres;
use merc_syntax::UntypedProcessSpecification;

use crate::convert::LineIndex;
use crate::convert::is_identifier_byte;

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
    /// A `proc` declaration's own parameter (or a PBES equation's), or any reference to one —
    /// the assignment-form `x` in `P(x = e)`, an ordinary positional use like `P(x)` or a
    /// condition's `x == 0`, all alike (see [`SymbolTable::classify_data_id`]) — distinct from
    /// [`TokenKind::Variable`], which covers every *bound* variable (`sum`/`dist`/`forall`/
    /// `exists`/`lambda`/comprehension) and `var`/`glob` declaration instead.
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
/// Marks a constructor or accessor function implicitly declared by a `sort D = struct
/// c1(a: S)?is_c1 | c2;` alternative — `c1`/`c2` themselves ([`TokenKind::EnumMember`]) and their
/// accessor functions `a`/`is_c1` ([`TokenKind::Method`]) — as distinct from the same kinds
/// declared by a top-level `cons`/`map` block, via a custom [`SemanticTokenModifier`] (there's no
/// standard LSP modifier for this). See [`walk_sort_expression`]'s `SortExpressionKind::Struct`
/// arm.
const MODIFIER_STRUCT_VARIANT: u32 = 1 << 2;

/// The legend advertised by `capabilities::server_capabilities`; must list types/modifiers in the
/// exact order [`TokenKind`]/[`MODIFIER_DECLARATION`]/[`MODIFIER_DEFAULT_LIBRARY`]/
/// [`MODIFIER_STRUCT_VARIANT`] assume.
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
        token_modifiers: vec![
            SemanticTokenModifier::DECLARATION,
            SemanticTokenModifier::DEFAULT_LIBRARY,
            SemanticTokenModifier::new("structVariant"),
        ],
    }
}

/// Builds the full, delta-encoded semantic token list for `spec`.
pub fn semantic_tokens(text: &str, line_index: &LineIndex, spec: &UntypedProcessSpecification) -> Vec<SemanticToken> {
    let symbols = SymbolTable::collect_process(spec);
    let mut builder = Builder::new(text, line_index);

    tag_data_specification(&spec.data_specification, &symbols, &mut builder);

    for decl in &spec.global_variables {
        builder.push(&decl.span, TokenKind::Variable, true);
        walk_sort_expression(&decl.sort, &mut builder);
    }

    for decl in &spec.action_declarations {
        // `decl.span` covers the whole declaration (`a: Nat # Bool`, shared with every sibling in
        // a grouped `act a, b: Nat;` too) — `decl.identifier.span` is just the name, which is all
        // a declaration-modifier token should cover (its argument sorts get their own `Type`
        // tokens right below, which `decl.span` would otherwise overlap).
        builder.push(&decl.identifier.span, TokenKind::Event, true);
        for arg in &decl.args {
            walk_sort_expression(arg, &mut builder);
        }
    }

    for decl in &spec.process_declarations {
        // As above: `decl.span` covers the whole `P(n: Nat) = ...;` declaration, not just `P` —
        // `decl.identifier.span` is the precise name, leaving the params/body below to tag
        // themselves without `decl`'s own token swallowing them.
        builder.push(&decl.identifier.span, TokenKind::Method, true);
        let params: HashSet<&str> = decl.params.iter().map(|param| param.identifier.as_str()).collect();
        for param in &decl.params {
            builder.push(&param.span, TokenKind::Parameter, true);
            walk_sort_expression(&param.sort, &mut builder);
        }
        walk_process_expr(&decl.body, &symbols, &params, &mut builder);
    }

    if let Some(init) = &spec.init {
        // `init` is not a process declaration's body, so no parameter is in scope here — a bare
        // `x` in `init P(x = e)`'s expression would (rightly) fall back to `Variable`.
        walk_process_expr(init, &symbols, &HashSet::new(), &mut builder);
    }

    tag_keywords(text, &mut builder);

    builder.finish()
}

/// As [`semantic_tokens`], for a parsed PBES. Shares the data-specification pass and every
/// `DataExpr`/`SortExpression` walker with the process-specification side; only the
/// process-algebra-shaped parts (`act`/`proc`/`init`) differ, replaced here by a PBES's
/// propositional-variable equations, quantifier binders, and `PropVarInst`s.
pub fn pbes_semantic_tokens(text: &str, line_index: &LineIndex, spec: &UntypedPbes) -> Vec<SemanticToken> {
    let symbols = SymbolTable::collect_pbes(spec);
    let mut builder = Builder::new(text, line_index);

    tag_data_specification(&spec.data_specification, &symbols, &mut builder);

    for decl in &spec.global_variables {
        builder.push(&decl.span, TokenKind::Variable, true);
        walk_sort_expression(&decl.sort, &mut builder);
    }

    for eqn in &spec.equations {
        // A propositional-variable equation is PBES's one callable-name concept — no separate
        // action/process distinction to make (unlike `ProcessExprKind::Action`, see the module
        // docs above), so this and every `PropVarInst` below are unconditionally `Method`.
        builder.push(&eqn.variable.identifier.span, TokenKind::Method, true);
        let params: HashSet<&str> = eqn.variable.parameters.iter().map(|param| param.identifier.as_str()).collect();
        for param in &eqn.variable.parameters {
            builder.push(&param.span, TokenKind::Parameter, true);
            walk_sort_expression(&param.sort, &mut builder);
        }
        walk_pbes_expr(&eqn.formula, &symbols, &params, &mut builder);
    }

    // `init` is not an equation's own formula, so no parameter is in scope here.
    walk_prop_var_inst(&spec.init, &symbols, &HashSet::new(), &mut builder);

    tag_keywords(text, &mut builder);

    builder.finish()
}

/// As [`pbes_semantic_tokens`], for a parsed PRES — [`UntypedPres`] has the identical shape one
/// level down (see [`crate::completion::pres_completions`]'s doc comment), so this differs only in
/// walking [`PresExpr`] instead of [`PbesExpr`] for each equation's formula.
pub fn pres_semantic_tokens(text: &str, line_index: &LineIndex, spec: &UntypedPres) -> Vec<SemanticToken> {
    let symbols = SymbolTable::collect_pres(spec);
    let mut builder = Builder::new(text, line_index);

    tag_data_specification(&spec.data_specification, &symbols, &mut builder);

    for decl in &spec.global_variables {
        builder.push(&decl.span, TokenKind::Variable, true);
        walk_sort_expression(&decl.sort, &mut builder);
    }

    for eqn in &spec.equations {
        // As in `pbes_semantic_tokens`: a propositional-variable equation is PRES's one
        // callable-name concept too, so this and every `PropVarInst` below are unconditionally
        // `Method`.
        builder.push(&eqn.variable.identifier.span, TokenKind::Method, true);
        let params: HashSet<&str> = eqn.variable.parameters.iter().map(|param| param.identifier.as_str()).collect();
        for param in &eqn.variable.parameters {
            builder.push(&param.span, TokenKind::Parameter, true);
            walk_sort_expression(&param.sort, &mut builder);
        }
        walk_pres_expr(&eqn.formula, &symbols, &params, &mut builder);
    }

    // `init` is not an equation's own formula, so no parameter is in scope here.
    walk_prop_var_inst(&spec.init, &symbols, &HashSet::new(), &mut builder);

    tag_keywords(text, &mut builder);

    builder.finish()
}

/// The `sort`/`cons`/`map`/`eqn` part of tagging, shared by [`semantic_tokens`] and
/// [`pbes_semantic_tokens`] — both a process specification and a PBES have the identical
/// `UntypedDataSpecification` subtree.
fn tag_data_specification(data: &UntypedDataSpecification, symbols: &SymbolTable, builder: &mut Builder) {
    for decl in &data.sort_declarations {
        builder.push(&decl.span, TokenKind::Type, true);
        if let Some(expr) = &decl.expr {
            walk_sort_expression(expr, builder);
        }
    }

    for decl in &data.constructor_declarations {
        builder.push(&decl.span, TokenKind::EnumMember, true);
        walk_sort_expression(&decl.sort, builder);
    }

    for decl in &data.map_declarations {
        // Deliberately no `builder.push` for the mapping's own name, at declaration or at any
        // use site below (see `SymbolTable::classify_data_id`): mappings are left uncolored, so
        // they read as plain text rather than competing for a color with constructors/processes.
        walk_sort_expression(&decl.sort, builder);
    }

    // No `proc`/PBES-equation is in scope for a top-level data equation, so no parameter name is
    // ever in scope here — see `classify_data_id`'s `current_params`.
    let no_params = HashSet::new();
    for eqn_spec in &data.equation_declarations {
        for decl in &eqn_spec.variables {
            builder.push(&decl.span, TokenKind::Variable, true);
            walk_sort_expression(&decl.sort, builder);
        }
        for eqn in &eqn_spec.equations {
            if let Some(condition) = &eqn.condition {
                walk_data_expr(condition, symbols, &no_params, builder);
            }
            walk_data_expr(&eqn.lhs, symbols, &no_params, builder);
            walk_data_expr(&eqn.rhs, symbols, &no_params, builder);
        }
    }
}

/// Which declared identifiers name what — the disambiguation a TextMate grammar cannot do, since
/// the grammar gives the same shape to several different declaration kinds. Used to classify a
/// bare [`DataExprKind::Id`] (function, constructor, parameter, or variable) and a
/// [`ProcessExprKind::Action`] whose name might actually belong to a process, not an action (see
/// the module docs above).
struct SymbolTable<'a> {
    maps: HashSet<&'a str>,
    constructors: HashSet<&'a str>,
    /// Every accessor function (a named argument projection or a `?`-recogniser) a `sort D =
    /// struct c1(a: S)?is_c1 | c2;` alternative declares — kept separate from `maps` so a *use*
    /// of `a`/`is_c1` elsewhere still resolves to [`TokenKind::Method`] (see
    /// [`Self::classify_data_id`]), unlike a plain `map`-declared function, which is deliberately
    /// left uncolored. See [`walk_sort_expression`]'s `SortExpressionKind::Struct` arm for why the
    /// declaration site itself needs the distinct coloring in the first place.
    struct_accessors: HashSet<&'a str>,
    processes: HashSet<&'a str>,
}

impl<'a> SymbolTable<'a> {
    fn collect_process(spec: &'a UntypedProcessSpecification) -> Self {
        SymbolTable {
            processes: spec.process_declarations.iter().map(|decl| decl.identifier.as_str()).collect(),
            ..Self::collect_data(&spec.data_specification)
        }
    }

    /// As [`Self::collect_process`], for a PBES: `processes` stays empty, since
    /// [`SymbolTable::classify_action`] (the one thing that reads it) has nothing to disambiguate
    /// for a PBES's `PropVarInst`s (see [`pbes_semantic_tokens`]).
    fn collect_pbes(spec: &'a UntypedPbes) -> Self {
        Self::collect_data(&spec.data_specification)
    }

    /// As [`Self::collect_pbes`], for a PRES — same reasoning, `processes` stays empty.
    fn collect_pres(spec: &'a UntypedPres) -> Self {
        Self::collect_data(&spec.data_specification)
    }

    /// As [`Self::collect_process`]/[`Self::collect_pbes`]/[`Self::collect_pres`], for the
    /// data-specification-only namespace all three share — `processes` stays empty, filled in by
    /// [`Self::collect_process`].
    ///
    /// Also harvests every `sort D = struct c1(a: S)?is_c1 | c2;` alternative's own constructor
    /// name (`c1`/`c2`, into `constructors`) and accessor functions (`a`/`is_c1`, into
    /// `struct_accessors`) — `merc_typecheck` desugars a struct into real `cons`/`map`
    /// declarations (see `merc_syntax::ConstructorDecl`'s doc comment), but that desugaring runs
    /// on the *checked* specification, not the raw [`UntypedDataSpecification`] this table is
    /// built from, so a *use* of `c1`/`a`/`is_c1` elsewhere in the document would otherwise fall
    /// through [`Self::classify_data_id`]'s free/bound-variable fallback instead of resolving to
    /// the same kind its declaration gets (see [`walk_sort_expression`]'s `Struct` arm).
    fn collect_data(data: &'a UntypedDataSpecification) -> Self {
        let mut constructors: HashSet<&str> = data.constructor_declarations.iter().map(|decl| decl.identifier.as_str()).collect();
        let maps: HashSet<&str> = data.map_declarations.iter().map(|decl| decl.identifier.as_str()).collect();
        let mut struct_accessors: HashSet<&str> = HashSet::new();

        for decl in &data.sort_declarations {
            let Some(expr) = &decl.expr else { continue };
            let SortExpressionKind::Struct { inner } = &expr.node else { continue };
            for constructor in inner {
                constructors.insert(constructor.name.node.as_str());
                for (name, _) in &constructor.args {
                    if let Some(name) = name {
                        struct_accessors.insert(name.node.as_str());
                    }
                }
                if let Some(projection) = &constructor.projection {
                    struct_accessors.insert(projection.node.as_str());
                }
            }
        }

        SymbolTable {
            maps,
            constructors,
            struct_accessors,
            processes: HashSet::new(),
        }
    }

    /// Classifies a [`DataExprKind::Id`] occurrence, or `None` if it names a plain mapping —
    /// mappings are deliberately left uncolored (see [`semantic_tokens`]'s map-declaration loop),
    /// so a use site has to stay uncolored too rather than fall back to some other kind. A struct
    /// accessor (`struct_accessors`) is a mapping too, but colored like a [`TokenKind::Method`]
    /// instead of left uncolored — [`MODIFIER_STRUCT_VARIANT`] itself is only ever set at the
    /// declaration site inside the `struct` expression (see [`walk_sort_expression`]); a use
    /// elsewhere carries no modifier, same as any other [`TokenKind::Method`] reference.
    ///
    /// `current_params` is the *current* `proc`/PBES-equation declaration's own parameter names
    /// only — unlike every other set on `self`, parameters are genuinely scoped (see the module
    /// docs' "deliberately unscoped" note, which is about everything *except* this): a name that
    /// merely happens to be some *other* declaration's parameter must not read as
    /// [`TokenKind::Parameter`] here, or every process/equation would highlight every other one's
    /// parameter names too. Callers thread the right set in via [`walk_data_expr`].
    fn classify_data_id(&self, name: &str, current_params: &HashSet<&str>) -> Option<TokenKind> {
        if self.constructors.contains(name) {
            Some(TokenKind::EnumMember)
        } else if self.struct_accessors.contains(name) {
            Some(TokenKind::Method)
        } else if self.maps.contains(name) {
            None
        } else if current_params.contains(name) {
            Some(TokenKind::Parameter)
        } else {
            // Not declared as a map, constructor, or (in-scope) parameter: a bound or free
            // variable. This is also the fallback for a name that isn't declared at all —
            // flagging that is a diagnostics concern (type checking), not this pass's job.
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
        self.push_with_modifiers(span, kind, if is_declaration { MODIFIER_DECLARATION } else { 0 });
    }

    /// As [`Builder::push`], for a caller that needs to combine more than just
    /// [`MODIFIER_DECLARATION`] — a struct-declared constructor/accessor also carries
    /// [`MODIFIER_STRUCT_VARIANT`] (see [`walk_sort_expression`]'s `SortExpressionKind::Struct`
    /// arm).
    fn push_with_modifiers(&mut self, span: &Span, kind: TokenKind, modifiers: u32) {
        self.raw.push((span.clone(), kind, modifiers));
    }

    /// Tags `span` as [`TokenKind::Type`] with [`MODIFIER_DEFAULT_LIBRARY`] — a reference to one
    /// of mCRL2's own system sorts, never a declaration. See [`walk_sort_expression`].
    fn push_builtin_type(&mut self, span: &Span) {
        self.raw.push((span.clone(), TokenKind::Type, MODIFIER_DEFAULT_LIBRARY));
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
            SortExpressionKind::Struct { inner } => {
                // A `sort D = struct c1(a: S)?is_c1 | c2;` alternative implicitly declares a
                // constructor (`c1`) plus, per named argument or `?`-recogniser, an accessor
                // function (`a`, `is_c1`) — real declarations `merc_typecheck` desugars into the
                // same `cons`/`map` signature a top-level block would (see
                // `merc_syntax::ConstructorDecl`'s doc comment), so they're tagged the same base
                // kinds a `cons`/`map` declaration gets ([`TokenKind::EnumMember`]/
                // [`TokenKind::Method`]) — plus [`MODIFIER_STRUCT_VARIANT`], so a theme can still
                // tell a struct-declared constructor/accessor apart from an explicit block's.
                // Each argument's own sort (`S` above) is a child `SortExpression` node the
                // recursion below reaches on its own, same as `Complex`'s subsort above.
                for constructor in inner {
                    builder.push_with_modifiers(&constructor.name.span, TokenKind::EnumMember, MODIFIER_DECLARATION | MODIFIER_STRUCT_VARIANT);
                    for (name, _) in &constructor.args {
                        if let Some(name) = name {
                            builder.push_with_modifiers(&name.span, TokenKind::Method, MODIFIER_DECLARATION | MODIFIER_STRUCT_VARIANT);
                        }
                    }
                    if let Some(projection) = &constructor.projection {
                        builder.push_with_modifiers(&projection.span, TokenKind::Method, MODIFIER_DECLARATION | MODIFIER_STRUCT_VARIANT);
                    }
                }
            }
            _ => {}
        }
        ControlFlow::Continue(())
    });
}

/// Walks every identifier in `expr`'s subtree: bare references (classified via `symbols`, scoped
/// to `current_params` — see [`SymbolTable::classify_data_id`]) and any binder
/// (`lambda`/`forall`/`exists`/set-or-bag comprehension) it introduces along the way, plus that
/// binder's sort.
fn walk_data_expr(expr: &DataExpr, symbols: &SymbolTable, current_params: &HashSet<&str>, builder: &mut Builder) {
    expr.visit::<(), _>(|node| {
        match &node.node {
            DataExprKind::Id(name) => {
                if let Some(kind) = symbols.classify_data_id(name, current_params) {
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
///
/// `current_params` is the enclosing `proc` declaration's own parameter names (see
/// [`SymbolTable::classify_data_id`]) — the same set for every node in `expr`, since a process
/// body cannot itself declare a nested `proc`.
fn walk_process_expr(expr: &ProcessExpr, symbols: &SymbolTable, current_params: &HashSet<&str>, builder: &mut Builder) {
    expr.visit::<(), _>(|node| {
        match &node.node {
            ProcessExprKind::Id(name, assignments) => {
                builder.push(&name.span, TokenKind::Method, false);
                for assignment in assignments {
                    // The parameter name in `x = e` — a *use* of the *target* process `name`'s
                    // own parameter (by construction of the assignment syntax), not necessarily
                    // one of the enclosing process's — always `Parameter` regardless of
                    // `current_params`, same as before this function took that scope.
                    builder.push(&assignment.span, TokenKind::Parameter, false);
                    walk_data_expr(&assignment.expr, symbols, current_params, builder);
                }
            }
            ProcessExprKind::Action(name, arguments) => {
                builder.push(&name.span, symbols.classify_action(name), false);
                for argument in arguments {
                    walk_data_expr(argument, symbols, current_params, builder);
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
                walk_data_expr(expr, symbols, current_params, builder);
            }
            ProcessExprKind::Condition { condition, .. } => {
                walk_data_expr(condition, symbols, current_params, builder);
            }
            ProcessExprKind::At { operand, .. } => {
                walk_data_expr(operand, symbols, current_params, builder);
            }
            _ => {}
        }
        ControlFlow::Continue(())
    });
}

/// Walks every identifier in `expr`'s subtree: `PropVarInst`s (propositional-variable references,
/// always [`TokenKind::Method`] — see [`pbes_semantic_tokens`]'s module note), `val(...)`-wrapped
/// data expressions, and any `forall`/`exists` binder, descending into the data expressions each
/// carries — none of which `Traverse` crosses into on its own, same reasoning as
/// [`walk_process_expr`].
///
/// `current_params` is the enclosing PBES equation's own parameter names (see
/// [`SymbolTable::classify_data_id`]) — the same set for every node in `expr`, since a formula
/// cannot itself declare a nested equation.
fn walk_pbes_expr(expr: &PbesExpr, symbols: &SymbolTable, current_params: &HashSet<&str>, builder: &mut Builder) {
    expr.visit::<(), _>(|node| {
        match &node.node {
            PbesExprKind::PropVarInst(inst) => walk_prop_var_inst(inst, symbols, current_params, builder),
            PbesExprKind::DataValExpr(data_expr) => walk_data_expr(data_expr, symbols, current_params, builder),
            PbesExprKind::Quantifier { variables, .. } => {
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

/// As [`walk_pbes_expr`], for a [`PresExpr`] tree — a `val(...)`-wrapped data expression and any
/// `PropVarInst` are tagged the same way; a `sum`/`inf`/`sup` [`PresExprKind::Bound`] binder plays
/// the same role a PBES `Quantifier` does; and each side of a scalar multiplication
/// (`PresExprKind::RightConstantMultiply`/`LeftConstantMultiply`) carries its own `constant` —
/// a [`DataExpr`], not a nested [`PresExpr`], so `Traverse` doesn't reach it on its own, same
/// reasoning as every other data-expression field this module walks explicitly. `Equal`'s and
/// `Condition`'s own tag fields (`Eq`/`Condition`) carry no identifiers; their `body`/`lhs`/
/// `then`/`else_` children are plain [`PresExpr`] nodes `Traverse` already recurses into.
fn walk_pres_expr(expr: &PresExpr, symbols: &SymbolTable, current_params: &HashSet<&str>, builder: &mut Builder) {
    expr.visit::<(), _>(|node| {
        match &node.node {
            PresExprKind::PropVarInst(inst) => walk_prop_var_inst(inst, symbols, current_params, builder),
            PresExprKind::DataValExpr(data_expr) => walk_data_expr(data_expr, symbols, current_params, builder),
            PresExprKind::RightConstantMultiply { constant, .. } | PresExprKind::LeftConstantMultiply { constant, .. } => {
                walk_data_expr(constant, symbols, current_params, builder);
            }
            PresExprKind::Bound { variables, .. } => {
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

/// Tags a propositional-variable instantiation's own name, then walks each of its arguments —
/// shared by [`walk_pbes_expr`]/[`walk_pres_expr`] (a `PropVarInst` occurring inside a formula)
/// and [`pbes_semantic_tokens`]/[`pres_semantic_tokens`] (a PBES/PRES's `init`, which passes an
/// empty `current_params`: no equation is in scope there). `identifier` now carries its own span,
/// precisely the name (an upstream `merc_syntax` addition mirroring `ActionName`), so this tags it
/// directly rather than text-searching `inst`'s whole `name(args)` span for it.
fn walk_prop_var_inst(inst: &PropVarInst, symbols: &SymbolTable, current_params: &HashSet<&str>, builder: &mut Builder) {
    builder.push(&inst.node.identifier.span, TokenKind::Method, false);
    for argument in &inst.node.arguments {
        walk_data_expr(argument, symbols, current_params, builder);
    }
}

/// Every word-like mCRL2 keyword relevant to a process/data specification, for [`tag_keywords`]
/// (and, `pub(crate)`, for [`crate::completion`]'s keyword completion items). Built-in sort names
/// (`Bool`, `List`, …) are deliberately not here: [`walk_sort_expression`] already tags those,
/// more precisely (node by node, off the AST, not a blind text scan) — `completion.rs` has its
/// own small list for the same names, for the same reason `SYSTEM_SORTS` gives there.
///
/// `true`/`false`/`delta`/`tau` are genuinely reserved — the grammar rejects them as the prefix
/// of a longer identifier (`DataExprTrue = { "true" ~ !Id }` and siblings; see `merc_syntax`'s
/// own `keywords_are_not_prefix_of_identifiers` test) — so a word-boundary match of one of these
/// can never actually be a user identifier. The rest (`sort`, `map`, `proc`, …) only ever appear
/// as unambiguous block-introducing prefixes in the grammar and have no such guard, so in
/// principle nothing stops a spec from declaring, say, a map literally named `sort`; in practice
/// this essentially never happens, and accepting that rather than leaving every structural
/// keyword uncolored is the better trade.
pub(crate) const KEYWORDS: &[&str] = &[
    "sort", "cons", "map", "glob", "act", "proc", "init", "var", "eqn", "struct", "whr", "end",
    "forall", "exists", "lambda", "sum", "dist", "val", "true", "false", "delta", "tau",
    "hide", "block", "allow", "comm", "rename", "pbes", "pres", "mu", "nu",
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
    use crate::parse::SpecKind;
    use crate::parse::Specification;
    use crate::parse::parse;

    async fn tokens_for(text: &str) -> Vec<SemanticToken> {
        let outcome = parse(SpecKind::Process, text.to_string()).await;
        let line_index = LineIndex::new(text);
        match outcome {
            ParseOutcome::Ok(Specification::Process(spec)) => semantic_tokens(text, &line_index, &spec),
            _ => panic!("fixture failed to parse"),
        }
    }

    async fn pbes_tokens_for(text: &str) -> Vec<SemanticToken> {
        let outcome = parse(SpecKind::Pbes, text.to_string()).await;
        let line_index = LineIndex::new(text);
        match outcome {
            ParseOutcome::Ok(Specification::Pbes(spec)) => pbes_semantic_tokens(text, &line_index, &spec),
            _ => panic!("fixture failed to parse"),
        }
    }

    async fn pres_tokens_for(text: &str) -> Vec<SemanticToken> {
        let outcome = parse(SpecKind::Pres, text.to_string()).await;
        let line_index = LineIndex::new(text);
        match outcome {
            ParseOutcome::Ok(Specification::Pres(spec)) => pres_semantic_tokens(text, &line_index, &spec),
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
    async fn every_branch_of_a_guarded_choice_chain_is_tagged_not_just_the_last() {
        // Regression test: `x -> (...)`'s condition slot is a `DataExpr` in the grammar, and
        // `pest`'s greedy `ProcExprPrefix*` repetition used to let it swallow the parenthesized
        // "then" branch (and beyond) as nested data-expression structure — `.` read as list
        // indexing, `+` as addition — until it opportunistically found a later `->` to complete a
        // bogus second `Condition`. Only the chain's last branch ended up parsed (and therefore
        // tagged) as real process-algebra structure; every earlier branch's `e`/`P` calls and `x`
        // condition read as generic data-expression tokens instead of `Event`/`Method`/`Parameter`.
        // `crate::parse::parse` now runs `disambiguate_process_specification` on every parsed process
        // specification before handing it to any consumer, which reconstructs the intended
        // structure from the declared action/process names alone — this checks every branch, not
        // just the last one, gets the right token kind.
        let text = "act e: Bool;\nproc P(x: Bool) =\n  x -> (e(x).P(x)) +\n  x -> (e(x).P(x));\ninit P(true);";
        let tokens = tokens_for(text).await;
        let positions = absolute(&tokens);
        let line_index = LineIndex::new(text);

        let kind_at = |offset: usize| -> Option<u32> {
            let pos = line_index.position(text, offset);
            positions
                .iter()
                .find(|&&(l, c, ..)| l == pos.line && c == pos.character)
                .map(|&(.., ty, _)| ty)
        };

        // Both branches' conditions, action calls, and recursive process calls — not only the
        // last branch's.
        let mut x_offsets = text.match_indices('x').map(|(i, _)| i);
        x_offsets.next(); // skip the parameter declaration `P(x: Bool)`
        let condition_offsets: Vec<usize> = [x_offsets.next().unwrap(), x_offsets.nth(2).unwrap()].to_vec();
        for offset in condition_offsets {
            assert_eq!(kind_at(offset), Some(TokenKind::Parameter as u32), "condition `x` at byte {offset} should be tagged Parameter");
        }

        let event_offsets: Vec<usize> = text.match_indices("e(x)").map(|(i, _)| i).collect();
        assert_eq!(event_offsets.len(), 2, "fixture should contain exactly two `e(x)` calls");
        for offset in event_offsets {
            assert_eq!(kind_at(offset), Some(TokenKind::Event as u32), "`e` at byte {offset} should be tagged Event");
        }

        let method_offsets: Vec<usize> = text.match_indices("P(x)").map(|(i, _)| i).collect();
        assert_eq!(method_offsets.len(), 2, "fixture should contain exactly two `P(x)` calls");
        for offset in method_offsets {
            assert_eq!(kind_at(offset), Some(TokenKind::Method as u32), "`P` at byte {offset} should be tagged Method");
        }
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
    async fn ordinary_parameter_use_is_tagged_parameter_not_variable() {
        // `x` in the recursive call `P(x)` is a *positional* use of `P`'s own parameter — parsed
        // as an ordinary `ProcessExprKind::Action` argument (see the module docs' `ProcExprId`
        // quirk: positional process instantiation isn't `ProcessExprKind::Id` at all), not the
        // assignment form the test above covers, so it went through `classify_data_id`'s bound/
        // free-variable fallback before `SymbolTable::parameters` existed.
        let text = "act a: Bool;\nproc P(x: Bool) = a(x).P(x);\ninit P(true);";
        let tokens = tokens_for(text).await;
        let positions = absolute(&tokens);
        let line_index = LineIndex::new(text);

        let recursive_use = text.rfind("P(x)").unwrap() + "P(".len();
        let use_pos = line_index.position(text, recursive_use);

        assert!(positions.iter().any(|&(l, c, len, ty, modifiers)| l == use_pos.line
            && c == use_pos.character
            && len == 1
            && ty == TokenKind::Parameter as u32
            && modifiers == 0));
    }

    #[tokio::test]
    async fn parameter_of_one_process_is_not_tagged_parameter_in_an_unrelated_process() {
        // Regression test: `x` is `P`'s own parameter, but also happens to be the name of an
        // unrelated `glob`al variable that `Q` (which declares no parameter of its own) refers to
        // — `x` inside `Q`'s body must not light up as `Parameter` merely because *some* process
        // elsewhere in the document happens to declare a parameter with that name.
        let text = "glob x: Bool;\nact a: Bool;\nproc P(x: Bool) = a(x);\nproc Q = a(x);\ninit P(true) || Q;";
        let tokens = tokens_for(text).await;
        let positions = absolute(&tokens);
        let line_index = LineIndex::new(text);

        let p_use = text.find("a(x)").unwrap() + "a(".len();
        let q_use = text.rfind("a(x)").unwrap() + "a(".len();
        let p_use_pos = line_index.position(text, p_use);
        let q_use_pos = line_index.position(text, q_use);

        // Inside `P`, `x` is `P`'s own parameter.
        assert!(positions.iter().any(|&(l, c, len, ty, modifiers)| l == p_use_pos.line
            && c == p_use_pos.character
            && len == 1
            && ty == TokenKind::Parameter as u32
            && modifiers == 0));
        // Inside `Q`, the same name `x` is not a parameter of `Q` — it's the global variable — so
        // it must be tagged `Variable`, not `Parameter`.
        assert!(positions.iter().any(|&(l, c, len, ty, modifiers)| l == q_use_pos.line
            && c == q_use_pos.character
            && len == 1
            && ty == TokenKind::Variable as u32
            && modifiers == 0));
    }

    #[tokio::test]
    async fn pbes_propvarinst_parameter_use_is_tagged_parameter_not_variable() {
        // As `ordinary_parameter_use_is_tagged_parameter_not_variable`, for a PBES: `n` inside
        // `val(n)` is a use of the equation's own parameter, not a free/bound variable.
        let text = "pbes mu X(n: Bool) = val(n) || X(n);\ninit X(true);";
        let tokens = pbes_tokens_for(text).await;
        let positions = absolute(&tokens);
        let line_index = LineIndex::new(text);

        let val_use = text.find("val(n)").unwrap() + "val(".len();
        let use_pos = line_index.position(text, val_use);

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

    #[tokio::test]
    async fn struct_constructor_and_accessors_are_tagged_distinctly_from_a_plain_cons_map_block() {
        let text = "sort D = struct c1(a: Bool)?is_c1 | c2; map f: D -> Bool; eqn f(c1(true)) = true;";
        let tokens = tokens_for(text).await;
        let positions = absolute(&tokens);
        let line_index = LineIndex::new(text);

        // `c1`'s own name inside the struct declaration: an `EnumMember`, same base kind a
        // top-level `cons` gets, but with `MODIFIER_STRUCT_VARIANT` layered on top of
        // `MODIFIER_DECLARATION` so a theme can still tell the two apart.
        let c1_decl_pos = line_index.position(text, text.find("c1(a").unwrap());
        assert!(positions.iter().any(|&(l, c, len, ty, modifiers)| l == c1_decl_pos.line
            && c == c1_decl_pos.character
            && len == 2
            && ty == TokenKind::EnumMember as u32
            && modifiers == (MODIFIER_DECLARATION | MODIFIER_STRUCT_VARIANT)));

        // `a`, the named projection: a `Method`, not left uncolored the way a plain `map` is.
        let a_decl_pos = line_index.position(text, text.find("a: Bool").unwrap());
        assert!(positions.iter().any(|&(l, c, len, ty, modifiers)| l == a_decl_pos.line
            && c == a_decl_pos.character
            && len == 1
            && ty == TokenKind::Method as u32
            && modifiers == (MODIFIER_DECLARATION | MODIFIER_STRUCT_VARIANT)));

        // `is_c1`, the recogniser: same treatment as the projection above.
        let is_c1_decl_pos = line_index.position(text, text.find("is_c1").unwrap());
        assert!(positions.iter().any(|&(l, c, len, ty, modifiers)| l == is_c1_decl_pos.line
            && c == is_c1_decl_pos.character
            && len == 5
            && ty == TokenKind::Method as u32
            && modifiers == (MODIFIER_DECLARATION | MODIFIER_STRUCT_VARIANT)));

        // `c2` has no arguments and no recogniser: still an `EnumMember` declaration.
        let c2_decl_pos = line_index.position(text, text.find("c2;").unwrap());
        assert!(positions.iter().any(|&(l, c, len, ty, modifiers)| l == c2_decl_pos.line
            && c == c2_decl_pos.character
            && len == 2
            && ty == TokenKind::EnumMember as u32
            && modifiers == (MODIFIER_DECLARATION | MODIFIER_STRUCT_VARIANT)));

        // A *use* of the constructor, `c1(true)` in the equation, still reads as a plain
        // `EnumMember` — no `MODIFIER_STRUCT_VARIANT` outside the struct declaration itself.
        let c1_use_pos = line_index.position(text, text.rfind("c1(true)").unwrap());
        assert!(positions.iter().any(|&(l, c, len, ty, modifiers)| l == c1_use_pos.line
            && c == c1_use_pos.character
            && len == 2
            && ty == TokenKind::EnumMember as u32
            && modifiers == 0));
    }

    #[tokio::test]
    async fn sort_alias_declaration_token_does_not_overlap_its_own_definition() {
        // Regression test: `SortDecl`'s alias form (`sort L = List(Nat);`) used to keep the whole
        // `L = List(Nat);` as its span upstream, which not only mis-highlighted `sort_symbol`'s
        // outline entry but produced two overlapping semantic tokens here — the (wrongly wide)
        // declaration token and the `List`/`Nat` tokens `walk_sort_expression` pushes for the
        // definition nested inside it.
        let text = "sort L = List(Nat);";
        let tokens = tokens_for(text).await;
        let positions = absolute(&tokens);
        for window in positions.windows(2) {
            let [(l1, c1, len1, ..), (l2, c2, ..)] = window else { unreachable!() };
            if l1 == l2 {
                assert!(c1 + len1 <= *c2, "tokens on the same line must not overlap: {window:?}");
            }
        }
    }

    #[tokio::test]
    async fn pbes_equation_and_parameter_and_propvarinst_are_tagged() {
        let text = "pbes mu X(n: Bool) = val(n) || X(n);\ninit X(true);";
        let tokens = pbes_tokens_for(text).await;
        let positions = absolute(&tokens);
        let line_index = LineIndex::new(text);

        let decl_pos = line_index.position(text, text.find('X').unwrap());
        let param_decl_pos = line_index.position(text, text.find("n: Bool").unwrap());
        let recursive_use_pos = line_index.position(text, text.rfind("X(n)").unwrap());
        let init_use_pos = line_index.position(text, text.find("init X").unwrap() + "init ".len());

        assert!(positions.iter().any(|&(l, c, len, ty, modifiers)| l == decl_pos.line
            && c == decl_pos.character
            && len == 1
            && ty == TokenKind::Method as u32
            && modifiers == MODIFIER_DECLARATION));
        assert!(positions.iter().any(|&(l, c, _, ty, modifiers)| l == param_decl_pos.line
            && c == param_decl_pos.character
            && ty == TokenKind::Parameter as u32
            && modifiers == MODIFIER_DECLARATION));
        assert!(positions.iter().any(|&(l, c, len, ty, modifiers)| l == recursive_use_pos.line
            && c == recursive_use_pos.character
            && len == 1
            && ty == TokenKind::Method as u32
            && modifiers == 0));
        assert!(positions.iter().any(|&(l, c, len, ty, modifiers)| l == init_use_pos.line
            && c == init_use_pos.character
            && len == 1
            && ty == TokenKind::Method as u32
            && modifiers == 0));
    }

    #[tokio::test]
    async fn pbes_quantifier_binder_is_tagged_as_a_variable_declaration() {
        let text = "pbes mu X = forall n: Bool . val(n);\ninit X;";
        let tokens = pbes_tokens_for(text).await;
        let positions = absolute(&tokens);
        let line_index = LineIndex::new(text);

        let binder_pos = line_index.position(text, text.find("n: Bool").unwrap());
        assert!(positions.iter().any(|&(l, c, _, ty, modifiers)| l == binder_pos.line
            && c == binder_pos.character
            && ty == TokenKind::Variable as u32
            && modifiers == MODIFIER_DECLARATION));
    }

    #[tokio::test]
    async fn pbes_tokens_are_sorted_and_non_overlapping() {
        let text = "sort D;\ncons c: D;\nmap f: D -> D;\nvar x: D;\neqn f(x) = f(c);\npbes mu X(n: Bool) = val(n) || X(n);\ninit X(true);";
        let tokens = pbes_tokens_for(text).await;
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
    async fn pres_propvarinst_parameter_use_is_tagged_parameter_not_variable() {
        // As `pbes_propvarinst_parameter_use_is_tagged_parameter_not_variable`, for a PRES: `n`
        // inside `val(n)` is a use of the equation's own parameter, and the recursive `X(n)`'s
        // argument likewise, across a PRES-specific `+` (`PresExprKind::Binary`).
        let text = "pres mu X(n: Nat) = val(n) + X(n); init X(0);";
        let tokens = pres_tokens_for(text).await;
        let positions = absolute(&tokens);
        let line_index = LineIndex::new(text);

        let val_use = text.find("val(n)").unwrap() + "val(".len();
        let recursive_use = text.rfind("X(n)").unwrap() + "X(".len();
        let val_pos = line_index.position(text, val_use);
        let recursive_pos = line_index.position(text, recursive_use);

        for pos in [val_pos, recursive_pos] {
            assert!(positions.iter().any(|&(l, c, len, ty, modifiers)| l == pos.line
                && c == pos.character
                && len == 1
                && ty == TokenKind::Parameter as u32
                && modifiers == 0));
        }
    }

    #[tokio::test]
    async fn pres_equation_and_parameter_and_propvarinst_are_tagged() {
        let text = "pres mu X(n: Nat) = val(n) + X(n); init X(0);";
        let tokens = pres_tokens_for(text).await;
        let positions = absolute(&tokens);
        let line_index = LineIndex::new(text);

        let decl_pos = line_index.position(text, text.find('X').unwrap());
        let param_decl_pos = line_index.position(text, text.find("n: Nat").unwrap());
        let recursive_use_pos = line_index.position(text, text.rfind("X(n)").unwrap());
        let init_use_pos = line_index.position(text, text.find("init X").unwrap() + "init ".len());

        assert!(positions.iter().any(|&(l, c, len, ty, modifiers)| l == decl_pos.line
            && c == decl_pos.character
            && len == 1
            && ty == TokenKind::Method as u32
            && modifiers == MODIFIER_DECLARATION));
        assert!(positions.iter().any(|&(l, c, _, ty, modifiers)| l == param_decl_pos.line
            && c == param_decl_pos.character
            && ty == TokenKind::Parameter as u32
            && modifiers == MODIFIER_DECLARATION));
        assert!(positions.iter().any(|&(l, c, len, ty, modifiers)| l == recursive_use_pos.line
            && c == recursive_use_pos.character
            && len == 1
            && ty == TokenKind::Method as u32
            && modifiers == 0));
        assert!(positions.iter().any(|&(l, c, len, ty, modifiers)| l == init_use_pos.line
            && c == init_use_pos.character
            && len == 1
            && ty == TokenKind::Method as u32
            && modifiers == 0));
    }

    #[tokio::test]
    async fn pres_bound_binder_is_tagged_as_a_variable_declaration() {
        // `sum`/`inf`/`sup` are PRES's own binder forms (`PresExprKind::Bound`), distinct from a
        // PBES `Quantifier` node but playing the identical role here.
        let text = "pres mu X = sum n: Nat . val(n); init X;";
        let tokens = pres_tokens_for(text).await;
        let positions = absolute(&tokens);
        let line_index = LineIndex::new(text);

        let binder_pos = line_index.position(text, text.find("n: Nat").unwrap());
        assert!(positions.iter().any(|&(l, c, _, ty, modifiers)| l == binder_pos.line
            && c == binder_pos.character
            && ty == TokenKind::Variable as u32
            && modifiers == MODIFIER_DECLARATION));
    }

    #[tokio::test]
    async fn pres_constant_multiply_operand_is_walked_as_a_data_expression() {
        // `PresExprKind::RightConstantMultiply`/`LeftConstantMultiply`'s own `constant` field is a
        // `DataExpr`, not a nested `PresExpr` — `Traverse` doesn't reach it on its own, so
        // `walk_pres_expr` has to walk it explicitly (see its doc comment).
        let text = "pres mu X(n: Nat) = val(n) * X(n); init X(0);";
        let tokens = pres_tokens_for(text).await;
        let positions = absolute(&tokens);
        let line_index = LineIndex::new(text);

        let constant_use = text.rfind("val(n) * X(n)").unwrap() + "val(".len();
        let constant_pos = line_index.position(text, constant_use);
        assert!(positions.iter().any(|&(l, c, len, ty, modifiers)| l == constant_pos.line
            && c == constant_pos.character
            && len == 1
            && ty == TokenKind::Parameter as u32
            && modifiers == 0));
    }

    #[tokio::test]
    async fn parameter_of_one_pres_equation_is_not_tagged_parameter_in_an_unrelated_equation() {
        // As `parameter_of_one_process_is_not_tagged_parameter_in_an_unrelated_process`, for a
        // PRES: `n` is `X`'s own parameter, but also the name of an unrelated `glob`al variable
        // that `Y` (which declares no parameter of its own) refers to.
        let text = "glob n: Bool;\npres mu X(n: Bool) = val(n);\nnu Y = val(n);\ninit X(true);";
        let tokens = pres_tokens_for(text).await;
        let positions = absolute(&tokens);
        let line_index = LineIndex::new(text);

        let x_use = text.find("val(n)").unwrap() + "val(".len();
        let y_use = text.rfind("val(n)").unwrap() + "val(".len();
        let x_use_pos = line_index.position(text, x_use);
        let y_use_pos = line_index.position(text, y_use);

        // Inside `X`, `n` is `X`'s own parameter.
        assert!(positions.iter().any(|&(l, c, len, ty, modifiers)| l == x_use_pos.line
            && c == x_use_pos.character
            && len == 1
            && ty == TokenKind::Parameter as u32
            && modifiers == 0));
        // Inside `Y`, the same name `n` is not a parameter of `Y` — it's the global variable —
        // so it must be tagged `Variable`, not `Parameter`.
        assert!(positions.iter().any(|&(l, c, len, ty, modifiers)| l == y_use_pos.line
            && c == y_use_pos.character
            && len == 1
            && ty == TokenKind::Variable as u32
            && modifiers == 0));
    }

    #[tokio::test]
    async fn pres_tokens_are_sorted_and_non_overlapping() {
        let text = "sort D;\ncons c: D;\nmap f: D -> D;\nvar x: D;\neqn f(x) = f(c);\npres mu X(n: Nat) = val(n) + X(n); init X(0);";
        let tokens = pres_tokens_for(text).await;
        let positions = absolute(&tokens);

        for window in positions.windows(2) {
            let [(l1, c1, len1, ..), (l2, c2, ..)] = window else { unreachable!() };
            assert!((*l1, *c1) < (*l2, *c2), "tokens must be strictly ordered by position");
            if l1 == l2 {
                assert!(c1 + len1 <= *c2, "tokens on the same line must not overlap");
            }
        }
    }
}
