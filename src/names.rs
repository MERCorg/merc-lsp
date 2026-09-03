//! Declared-name extraction from the raw parsed AST, broken down by category — the candidate
//! lists [`crate::edit_distance::closest`] searches to build a "did you mean '...'?" suggestion
//! for an undeclared-name diagnostic (see [`crate::diagnostics`]) and, filtered the same way, what
//! [`crate::completion`] offers once it knows what kind of name the cursor sits where a name is
//! expected (see [`crate::completion_context`]).
//!
//! Every function here mirrors, one level down, the identical `UntypedDataSpecification` subtree
//! shared by a process specification, a PBES, and a PRES (see `completion.rs`'s module docs for
//! why that sharing runs this deep) — so the `process_*`/`pbes_*` pairs below are the same
//! extraction against two different top-level containers, not independently written code.

use merc_syntax::UntypedDataSpecification;
use merc_syntax::UntypedPbes;
use merc_syntax::UntypedProcessSpecification;

/// mCRL2's built-in sort names — the basic sorts plus the parameterized container sorts. Not
/// discovered from the AST (unlike every other name here): a system sort is never *declared*, so
/// there is no declaration list to walk the way there is for a user's own `sort`s.
pub const SYSTEM_SORTS: &[&str] = &["Bool", "Pos", "Nat", "Int", "Real", "List", "Set", "Bag", "FSet", "FBag"];

/// Declared sort names (`sort D;` / `sort D = ...;`) — not the built-ins, see [`SYSTEM_SORTS`].
pub fn sort_names(data: &UntypedDataSpecification) -> impl Iterator<Item = &str> {
    data.sort_declarations.iter().map(|decl| decl.identifier.as_str())
}

/// Names usable as a data *value*: constructors, maps, and equation-bound variables — everything
/// [`crate::completion`]'s `push_data_value_items` offers, minus the global variables, which a
/// process specification, a PBES, and a PRES each declare at their own top level rather than
/// inside the shared data specification (see [`process_data_value_names`]/[`pbes_data_value_names`]).
fn data_value_names(data: &UntypedDataSpecification) -> impl Iterator<Item = &str> {
    let constructors = data.constructor_declarations.iter().map(|decl| decl.identifier.as_str());
    let maps = data.map_declarations.iter().map(|decl| decl.identifier.as_str());
    let variables = data
        .equation_declarations
        .iter()
        .flat_map(|eqn_spec| eqn_spec.variables.iter().map(|decl| decl.identifier.as_str()));
    constructors.chain(maps).chain(variables)
}

pub fn process_sort_names(spec: &UntypedProcessSpecification) -> impl Iterator<Item = &str> {
    sort_names(&spec.data_specification)
}

pub fn process_data_value_names(spec: &UntypedProcessSpecification) -> impl Iterator<Item = &str> {
    let globals = spec.global_variables.iter().map(|decl| decl.identifier.as_str());
    data_value_names(&spec.data_specification).chain(globals)
}

pub fn process_action_names(spec: &UntypedProcessSpecification) -> impl Iterator<Item = &str> {
    spec.action_declarations.iter().map(|decl| decl.identifier.as_str())
}

pub fn process_names(spec: &UntypedProcessSpecification) -> impl Iterator<Item = &str> {
    spec.process_declarations.iter().map(|decl| decl.identifier.as_str())
}

/// As [`process_action_names`] chained with [`process_names`] — the candidate set for a call that
/// could name either (`ProcessError::UndeclaredActionOrProcess`).
pub fn process_action_or_process_names(spec: &UntypedProcessSpecification) -> impl Iterator<Item = &str> {
    process_action_names(spec).chain(process_names(spec))
}

/// The parameter names of the process declared as `process` (by identifier), or nothing if no
/// such process exists — the candidate set for `ProcessError::UnknownProcessParameter`, whose
/// `process` field names exactly one declaration to search.
pub fn process_parameter_names<'a>(spec: &'a UntypedProcessSpecification, process: &str) -> impl Iterator<Item = &'a str> {
    spec.process_declarations
        .iter()
        .find(|decl| decl.identifier.as_str() == process)
        .into_iter()
        .flat_map(|decl| decl.params.iter().map(|param| param.identifier.as_str()))
}

pub fn pbes_sort_names(spec: &UntypedPbes) -> impl Iterator<Item = &str> {
    sort_names(&spec.data_specification)
}

pub fn pbes_data_value_names(spec: &UntypedPbes) -> impl Iterator<Item = &str> {
    let globals = spec.global_variables.iter().map(|decl| decl.identifier.as_str());
    data_value_names(&spec.data_specification).chain(globals)
}

pub fn pbes_propositional_variable_names(spec: &UntypedPbes) -> impl Iterator<Item = &str> {
    spec.equations.iter().map(|eqn| eqn.variable.identifier.as_str())
}

// No `pres_*` counterparts: PRES has no type checker upstream yet (see `crate::typecheck`'s
// module docs), so `diagnostics.rs` never has a PRES error to build a suggestion for — nothing
// would call them.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::ParseOutcome;
    use crate::parse::SpecKind;
    use crate::parse::Specification;
    use crate::parse::parse;

    async fn process_spec_for(text: &str) -> UntypedProcessSpecification {
        match parse(SpecKind::Process, text.to_string()).await {
            ParseOutcome::Ok(Specification::Process(spec)) => *spec,
            _ => panic!("fixture failed to parse"),
        }
    }

    #[tokio::test]
    async fn each_category_collects_the_right_declarations() {
        let text = "sort D;\ncons c: D;\nmap f: D -> D;\nvar x: D;\neqn f(x) = x;\nglob g: D;\nact a: D;\nproc P(n: D) = a(n);\ninit P(g);";
        let spec = process_spec_for(text).await;

        assert_eq!(process_sort_names(&spec).collect::<Vec<_>>(), ["D"]);
        let mut data_values: Vec<_> = process_data_value_names(&spec).collect();
        data_values.sort_unstable();
        assert_eq!(data_values, ["c", "f", "g", "x"]);
        assert_eq!(process_action_names(&spec).collect::<Vec<_>>(), ["a"]);
        assert_eq!(process_names(&spec).collect::<Vec<_>>(), ["P"]);
        assert_eq!(process_action_or_process_names(&spec).collect::<Vec<_>>(), ["a", "P"]);
    }

    #[tokio::test]
    async fn process_parameter_names_finds_the_named_process_only() {
        let text = "act a: Bool;\nproc P(n: Bool) = a(n);\nproc Q = delta;\ninit P(true);";
        let spec = process_spec_for(text).await;

        assert_eq!(process_parameter_names(&spec, "P").collect::<Vec<_>>(), ["n"]);
        assert_eq!(process_parameter_names(&spec, "Q").collect::<Vec<_>>(), Vec::<&str>::new());
        assert_eq!(process_parameter_names(&spec, "Nonexistent").collect::<Vec<_>>(), Vec::<&str>::new());
    }
}
