//! Fuzzy matching for the command palette: the query's characters must appear
//! in order in the candidate; matches at word starts and in runs score higher.

/// Score of `candidate` for `query`, or `None` if it does not match. An empty
/// query matches everything with score 0.
pub fn score(query: &str, candidate: &str) -> Option<i64> {
    let query: Vec<char> = query
        .chars()
        .filter(|c| !c.is_whitespace())
        .flat_map(char::to_lowercase)
        .collect();
    if query.is_empty() {
        return Some(0);
    }
    let chars: Vec<char> = candidate.chars().collect();
    let lower: Vec<char> = chars.iter().flat_map(|c| c.to_lowercase()).collect();
    if lower.len() != chars.len() {
        // Characters whose lowercase form is longer: fall back to a plain
        // substring check so indices stay aligned.
        let haystack: String = lower.iter().collect();
        let needle: String = query.iter().collect();
        return haystack.contains(&needle).then_some(1);
    }

    let mut total = 0;
    let mut position = 0;
    let mut previous: Option<usize> = None;
    for wanted in &query {
        let found = (position..lower.len()).find(|&index| lower[index] == *wanted)?;
        let word_start =
            found == 0 || matches!(chars[found - 1], ' ' | '.' | '_' | '-' | ':' | '/' | '(');
        let camel_start =
            found > 0 && chars[found].is_uppercase() && chars[found - 1].is_lowercase();
        total += 1;
        if word_start || camel_start {
            total += 8;
        }
        match previous {
            Some(last) if last + 1 == found => total += 5,
            Some(last) => total -= (found - last - 1).min(10) as i64,
            None => total -= found.min(10) as i64 / 2,
        }
        previous = Some(found);
        position = found + 1;
    }
    // Shorter candidates win ties.
    Some(total * 100 - chars.len() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_characters_in_order() {
        assert!(score("ctab", "Close Tab").is_some());
        assert!(score("tabc", "Close Tab").is_none());
        assert!(score("xyz", "Close Tab").is_none());
        assert_eq!(score("", "anything"), Some(0));
        assert_eq!(score("  ", "anything"), Some(0));
    }

    #[test]
    fn is_case_insensitive() {
        assert!(score("CLOSE", "close tab").is_some());
        assert!(score("orders", "public.Orders").is_some());
    }

    #[test]
    fn prefers_word_starts_and_runs() {
        let word_starts = score("ct", "Close Tab").unwrap();
        let scattered = score("ct", "Select").unwrap();
        assert!(word_starts > scattered);

        let run = score("tab", "Close Tab").unwrap();
        let spread = score("tab", "Toggle Database").unwrap();
        assert!(run > spread);
    }

    #[test]
    fn prefers_shorter_candidates_on_ties() {
        assert!(score("orders", "orders").unwrap() > score("orders", "orders_archive").unwrap());
    }
}
