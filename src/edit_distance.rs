//! Levenshtein edit distance and a "closest declared name" lookup built on it — the "did you
//! mean '...'?" suggestions [`crate::diagnostics`] appends to an undeclared-name error, using the
//! candidate lists [`crate::names`] extracts from the document's raw parse.

/// The Levenshtein distance between `a` and `b`: the minimum number of single-character
/// insertions, deletions, or substitutions to turn one into the other.
///
/// Computed over `char`s, not bytes, so a multi-byte UTF-8 character counts as one edit like
/// everywhere else in mCRL2 identifiers-are-ASCII-in-practice territory this server lives in (see
/// `convert.rs`'s UTF-16 handling for the general policy). Classic two-row dynamic programming,
/// `O(a.len() * b.len())` time and `O(b.len())` space — more than fast enough for the short
/// identifiers this is run against.
pub fn distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();

    let mut previous_row: Vec<usize> = (0..=b.len()).collect();
    let mut current_row = vec![0; b.len() + 1];

    for (i, &a_char) in a.iter().enumerate() {
        current_row[0] = i + 1;
        for (j, &b_char) in b.iter().enumerate() {
            let substitution_cost = usize::from(a_char != b_char);
            current_row[j + 1] = (previous_row[j + 1] + 1) // deletion
                .min(current_row[j] + 1) // insertion
                .min(previous_row[j] + substitution_cost); // substitution
        }
        std::mem::swap(&mut previous_row, &mut current_row);
    }

    previous_row[b.len()]
}

/// Finds the candidate closest to `name` by [`distance`], provided it is close enough to be worth
/// suggesting as a typo fix — within a third of `name`'s own length, rounded up and never less
/// than 1 (so a one- or two-character name still tolerates a single-character slip). Ties keep
/// whichever candidate `candidates` yields first. A candidate identical to `name` is never
/// suggested — that would not be a typo fix at all, and can happen when `name` is itself a
/// legitimate declaration excluded from `candidates` by the caller only in some cases.
pub fn closest<'a>(name: &str, candidates: impl IntoIterator<Item = &'a str>) -> Option<&'a str> {
    let max_distance = name.chars().count().div_ceil(3).max(1);

    candidates
        .into_iter()
        .filter(|&candidate| candidate != name)
        .map(|candidate| (candidate, distance(name, candidate)))
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
    fn identical_strings_have_zero_distance() {
        assert_eq!(distance("hello", "hello"), 0);
    }

    #[test]
    fn single_substitution() {
        assert_eq!(distance("cat", "cot"), 1);
    }

    #[test]
    fn single_insertion_or_deletion() {
        assert_eq!(distance("cat", "cats"), 1);
        assert_eq!(distance("cats", "cat"), 1);
    }

    #[test]
    fn empty_strings() {
        assert_eq!(distance("", ""), 0);
        assert_eq!(distance("abc", ""), 3);
        assert_eq!(distance("", "abc"), 3);
    }

    #[test]
    fn multibyte_characters_count_as_one_edit() {
        // "café" -> "cafe" is a single substitution ('é' for 'e'), not a multi-byte-wide one.
        assert_eq!(distance("café", "cafe"), 1);
    }

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
    fn suggestion_formats_a_found_candidate() {
        assert_eq!(suggestion("Bol", ["Bool"]), " — did you mean 'Bool'?");
    }

    #[test]
    fn suggestion_is_empty_when_nothing_is_close() {
        assert_eq!(suggestion("xyz", ["Bool"]), "");
    }
}
