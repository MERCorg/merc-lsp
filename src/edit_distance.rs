/// A "closest declared name" lookup, built on [`strsim::levenshtein`] — the fix suggestion behind
/// both the "did you mean '...'?" suffix [`crate::diagnostics`] appends to an undeclared-name
/// error and the "change to '...'" quick fix [`crate::code_action`] offers for it, using the
/// candidate lists [`crate::names`] extracts from the document's raw parse.
pub fn closest<'a, I>(name: &str, candidates: I) -> Option<&'a str> 
    where I: IntoIterator<Item = &'a str>
{
    let max_distance = name.chars().count().div_ceil(3).max(1);

    candidates
        .into_iter()
        .filter(|&candidate| candidate != name)
        .map(|candidate| (candidate, strsim::levenshtein(name, candidate)))
        .filter(|&(_, distance)| distance <= max_distance)
        .min_by_key(|&(_, distance)| distance)
        .map(|(candidate, _)| candidate)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn closest_picks_the_nearest_within_threshold() {
        let candidates = ["Bool", "Nat", "Int"];
        assert_eq!(closest("Bol", candidates), Some("Bool"));
        assert_eq!(closest("Nut", candidates), Some("Nat"));
    }

    #[test]
    fn closest_ignores_a_match_too_far_away() {
        let candidates = ["Bool", "Nat"];
        assert_eq!(closest("Completely different", candidates), None);
    }

    #[test]
    fn closest_never_suggests_the_name_itself() {
        let candidates = ["foo", "bar"];
        assert_eq!(closest("foo", candidates), None);
    }

    #[test]
    fn closest_counts_a_multibyte_character_as_one_edit() {
        // "café" -> "cafe" is a single substitution ('é' for 'e'), not a multi-byte-wide one —
        // within the one-edit threshold `closest`'s own length ("cafe".len() == 4) allows.
        let candidates = ["cafe"];
        assert_eq!(closest("café", candidates), Some("cafe"));
    }
}
