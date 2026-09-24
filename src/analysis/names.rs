//! Declared-name extraction from the raw parsed AST, broken down by category — the candidate
//! lists [`crate::analysis::edit_distance::closest`] searches to build a "did you mean '...'?" suggestion
//! for an undeclared-name diagnostic (see [`crate::features::diagnostics`]) and, filtered the same way, what
//! [`crate::features::completion`] offers once it knows what kind of name the cursor sits where a name is
//! expected (see [`crate::features::completion_context`]).

use merc_syntax::StateFrm;
use merc_syntax::StateFrmKind;
use merc_syntax::UntypedDataSpecification;
use merc_syntax::UntypedPbes;
use merc_syntax::UntypedPres;
use merc_syntax::UntypedProcessSpecification;
use merc_syntax::UntypedStateFrmSpec;

/// mCRL2's built-in sort names — the basic sorts plus the parameterized container sorts.
pub const SYSTEM_SORTS: &[&str] = &[
    "Bool", "Pos", "Nat", "Int", "Real", "List", "Set", "Bag", "FSet", "FBag",
];

/// Declared sort names (`sort D;` / `sort D = ...;`).
pub fn sort_names(data: &UntypedDataSpecification) -> impl Iterator<Item = &str> {
    data.sort_declarations
        .iter()
        .map(|decl| decl.identifier.as_str())
}

/// Names usable as a data *value*: constructors, maps, and equation-bound variables — everything
/// [`crate::features::completion`]'s `push_data_value_items` offers, minus the global variables, which a
/// process specification, a PBES, and a PRES each declare at their own top level rather than
/// inside the shared data specification (see [`process_data_value_names`]/[`pbes_data_value_names`]).
fn data_value_names(data: &UntypedDataSpecification) -> impl Iterator<Item = &str> {
    let constructors = data
        .constructor_declarations
        .iter()
        .map(|decl| decl.identifier.as_str());

    let maps = data
        .map_declarations
        .iter()
        .map(|decl| decl.identifier.as_str());

    let variables = data.equation_declarations.iter().flat_map(|eqn_spec| {
        eqn_spec
            .variables
            .iter()
            .map(|decl| decl.identifier.as_str())
    });

    constructors.chain(maps).chain(variables)
}

pub fn process_sort_names(spec: &UntypedProcessSpecification) -> impl Iterator<Item = &str> {
    sort_names(&spec.data_specification)
}

pub fn process_data_value_names(spec: &UntypedProcessSpecification) -> impl Iterator<Item = &str> {
    let globals = spec
        .global_variables
        .iter()
        .map(|decl| decl.identifier.as_str());
    data_value_names(&spec.data_specification).chain(globals)
}

pub fn process_action_names(spec: &UntypedProcessSpecification) -> impl Iterator<Item = &str> {
    spec.action_declarations
        .iter()
        .map(|decl| decl.identifier.as_str())
}

pub fn process_names(spec: &UntypedProcessSpecification) -> impl Iterator<Item = &str> {
    spec.process_declarations
        .iter()
        .map(|decl| decl.identifier.as_str())
}

/// As [`process_action_names`] chained with [`process_names`] — the candidate set for a call that
/// could name either (`ProcessError::UndeclaredActionOrProcess`).
pub fn process_action_or_process_names(
    spec: &UntypedProcessSpecification,
) -> impl Iterator<Item = &str> {
    process_action_names(spec).chain(process_names(spec))
}

/// The parameter names of the process declared as `process` (by identifier), or nothing if no
/// such process exists — the candidate set for `ProcessError::UnknownProcessParameter`, whose
/// `process` field names exactly one declaration to search.
pub fn process_parameter_names<'a>(
    spec: &'a UntypedProcessSpecification,
    process: &str,
) -> impl Iterator<Item = &'a str> {
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
    let globals = spec
        .global_variables
        .iter()
        .map(|decl| decl.identifier.as_str());
    data_value_names(&spec.data_specification).chain(globals)
}

pub fn pbes_propositional_variable_names(spec: &UntypedPbes) -> impl Iterator<Item = &str> {
    spec.equations
        .iter()
        .map(|eqn| eqn.variable.identifier.as_str())
}

/// As [`pbes_sort_names`], for a PRES.
pub fn pres_sort_names(spec: &UntypedPres) -> impl Iterator<Item = &str> {
    sort_names(&spec.data_specification)
}

/// As [`pbes_data_value_names`], for a PRES.
pub fn pres_data_value_names(spec: &UntypedPres) -> impl Iterator<Item = &str> {
    let globals = spec
        .global_variables
        .iter()
        .map(|decl| decl.identifier.as_str());
    data_value_names(&spec.data_specification).chain(globals)
}

/// As [`pbes_propositional_variable_names`], for a PRES.
pub fn pres_propositional_variable_names(spec: &UntypedPres) -> impl Iterator<Item = &str> {
    spec.equations
        .iter()
        .map(|eqn| eqn.variable.identifier.as_str())
}

/// As [`pbes_sort_names`], for a modal (mu-calculus) formula.
pub fn modal_sort_names(spec: &UntypedStateFrmSpec) -> impl Iterator<Item = &str> {
    sort_names(&spec.data_specification)
}

/// As [`pbes_data_value_names`], for a modal formula — no `glob`al variables to chain in, unlike a
/// process specification/PBES/PRES: [`UntypedStateFrmSpec`] declares none of its own (a state
/// formula's only own-declared names are its `act`ions and fixpoint variables).
pub fn modal_data_value_names(spec: &UntypedStateFrmSpec) -> impl Iterator<Item = &str> {
    data_value_names(&spec.data_specification)
}

/// The `act`-declared action names in scope for every modality in `spec`'s formula.
pub fn modal_action_names(spec: &UntypedStateFrmSpec) -> impl Iterator<Item = &str> {
    spec.action_declarations
        .iter()
        .map(|decl| decl.identifier.as_str())
}

/// Every fixpoint (`mu`/`nu`) variable name declared anywhere in `spec`'s formula.
pub fn modal_state_variable_names(spec: &UntypedStateFrmSpec) -> Vec<&str> {
    let mut names = Vec::new();
    collect_state_variable_names(&spec.formula, &mut names);
    names
}

fn collect_state_variable_names<'a>(formula: &'a StateFrm, names: &mut Vec<&'a str>) {
    match &formula.node {
        StateFrmKind::FixedPoint { variable, body, .. } => {
            names.push(variable.identifier.as_str());
            collect_state_variable_names(body, names);
        }
        StateFrmKind::Unary { expr, .. } | StateFrmKind::Modality { expr, .. } => {
            collect_state_variable_names(expr, names)
        }
        StateFrmKind::Binary { lhs, rhs, .. } => {
            collect_state_variable_names(lhs, names);
            collect_state_variable_names(rhs, names);
        }
        StateFrmKind::Quantifier { body, .. } | StateFrmKind::Bound { body, .. } => {
            collect_state_variable_names(body, names)
        }
        StateFrmKind::DataValExprLeftMult(_, expr)
        | StateFrmKind::DataValExprRightMult(expr, _) => collect_state_variable_names(expr, names),
        StateFrmKind::True
        | StateFrmKind::False
        | StateFrmKind::Delay(_)
        | StateFrmKind::Yaled(_)
        | StateFrmKind::Id(_, _)
        | StateFrmKind::Resolved(_, _, _)
        | StateFrmKind::DataValExpr(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::parse::ParseOutcome;
    use crate::analysis::parse::SpecKind;
    use crate::analysis::parse::Specification;
    use crate::analysis::parse::parse_ignoring_sources as parse;

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
        assert_eq!(
            process_action_or_process_names(&spec).collect::<Vec<_>>(),
            ["a", "P"]
        );
    }

    #[tokio::test]
    async fn process_parameter_names_finds_the_named_process_only() {
        let text = "act a: Bool;\nproc P(n: Bool) = a(n);\nproc Q = delta;\ninit P(true);";
        let spec = process_spec_for(text).await;

        assert_eq!(
            process_parameter_names(&spec, "P").collect::<Vec<_>>(),
            ["n"]
        );
        assert_eq!(
            process_parameter_names(&spec, "Q").collect::<Vec<_>>(),
            Vec::<&str>::new()
        );
        assert_eq!(
            process_parameter_names(&spec, "Nonexistent").collect::<Vec<_>>(),
            Vec::<&str>::new()
        );
    }
}
