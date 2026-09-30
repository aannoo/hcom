//! "Did you mean" suggestions for mistyped names, commands, and keys.

/// Optimal string alignment distance (Levenshtein plus adjacent transposition),
/// so `tnua` → `tuna` counts as one edit.
pub fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut d = vec![vec![0usize; b.len() + 1]; a.len() + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            d[i][j] = (d[i - 1][j] + 1)
                .min(d[i][j - 1] + 1)
                .min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                d[i][j] = d[i][j].min(d[i - 2][j - 2] + 1);
            }
        }
    }
    d[a.len()][b.len()]
}

/// Candidates close to `input`, nearest first, at most `max` of them.
///
/// Tolerance scales with length: short words (agent names are 4 letters)
/// allow one edit, longer ones two. A candidate that starts with the input
/// (`sta` → `status`) also matches, since users abbreviate.
pub fn suggest<'a, I>(input: &str, candidates: I, max: usize) -> Vec<String>
where
    I: IntoIterator<Item = &'a str>,
{
    let input = input.to_lowercase();
    if input.is_empty() {
        return Vec::new();
    }
    let tolerance = if input.chars().count() <= 4 { 1 } else { 2 };
    let mut scored: Vec<(usize, String)> = Vec::new();
    for cand in candidates {
        let lower = cand.to_lowercase();
        if lower == input || scored.iter().any(|(_, c)| c == cand) {
            continue;
        }
        let dist = edit_distance(&input, &lower);
        if dist <= tolerance {
            scored.push((dist, cand.to_string()));
        } else if input.len() >= 2 && lower.starts_with(&input) {
            scored.push((tolerance + 1, cand.to_string()));
        }
    }
    scored.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    scored.into_iter().take(max).map(|(_, c)| c).collect()
}

/// `"\nDid you mean: a, b?"`, or empty when nothing is close.
pub fn did_you_mean<'a, I>(input: &str, candidates: I) -> String
where
    I: IntoIterator<Item = &'a str>,
{
    let hits = suggest(input, candidates, 3);
    if hits.is_empty() {
        String::new()
    } else {
        format!("\nDid you mean: {}?", hits.join(", "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distance_counts_transposition_as_one_edit() {
        assert_eq!(edit_distance("tnua", "tuna"), 1);
        assert_eq!(edit_distance("kil", "kill"), 1);
        assert_eq!(edit_distance("abc", "abc"), 0);
        assert_eq!(edit_distance("", "ab"), 2);
    }

    #[test]
    fn suggest_scales_tolerance_and_orders_by_distance() {
        let names = ["tuna", "tune", "nazu", "logo"];
        assert_eq!(suggest("tnua", names, 3), vec!["tuna"]);
        assert_eq!(suggest("tuxa", names, 3), vec!["tuna"]);
        assert!(suggest("zzzz", names, 3).is_empty());
        let cmds = ["list", "listen", "status", "send"];
        assert_eq!(suggest("lst", cmds, 3), vec!["list"]);
        assert_eq!(suggest("stat", cmds, 3), vec!["status"]);
    }

    #[test]
    fn suggest_skips_exact_and_duplicates() {
        assert!(suggest("tuna", ["tuna", "tuna"], 3).is_empty());
        assert_eq!(suggest("tunx", ["tuna", "tuna"], 3), vec!["tuna"]);
    }

    #[test]
    fn did_you_mean_formats_or_is_empty() {
        assert_eq!(
            did_you_mean("claud", ["claude", "codex"]),
            "\nDid you mean: claude?"
        );
        assert_eq!(did_you_mean("xyz", ["claude"]), "");
    }
}
