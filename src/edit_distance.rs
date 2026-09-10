//! A "closest declared name" lookup, built on [`strsim::levenshtein`] — the "did you mean '...'?"
//! suggestions [`crate::diagnostics`] appends to an undeclared-name error, using the candidate
//! lists [`crate::names`] extracts from the document's raw parse.

/// Finds the candidate closest to `name` by Levenshtein distance, provided it is close enough to
/// be worth suggesting as a typo fix — within a third of `name`'s own length, rounded up and never
/// less than 1 (so a one- or two-character name still tolerates a single-character slip). Ties
/// keep whichever candidate `candidates` yields first. A candidate identical to `name` is never
/// suggested — that would not be a typo fix at all, and can happen when `name` is itself a
/// legitimate declaration excluded from `candidates` by the caller only in some cases.
pub fn closest<'a>(name: &str, candidates: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let max_distance = name.chars().count().div_ceil(3).max(1);

    candidates
        .into_iter()
        .filter(|&candidate| candidate != name)
        .map(|candidate| (candidate, strsim::levenshtein(name, candidate)))
        .filter(|&(_, distance)| distance <= max_distance)
        .min_by_key(|&(_, distance)| distance)
        .map(|(candidate, _)| candidate)
}

/// Renders a `" — did you mean 'X'?"` suffix for `name` against `candidates` (see [`closest`]), or
/// an empty string when nothing is close enough to suggest.
pub fn suggestion<'a>(name: &str, candidates: impl IntoIterator<Item = &'a str>) -> String {
    match closest(name, candidates) {
        Some(candidate) => format!(" — did you mean '{candidate}'?"),
        None => String::new(),
    }
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

    #[test]
    fn suggestion_formats_a_found_candidate() {
        assert_eq!(suggestion("Bol", ["Bool"]), " — did you mean 'Bool'?");
    }

    #[test]
    fn suggestion_is_empty_when_nothing_is_close() {
        assert_eq!(suggestion("xyz", ["Bool"]), "");
    }
}
