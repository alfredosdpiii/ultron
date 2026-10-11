//! Fuzzy matching (port of packages/tui/src/fuzzy.ts). Lower scores are better matches.

/// Result of matching a query against a text. Scores are the TS `number` values (fractions from `i * 0.1`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FuzzyMatch {
    pub matches: bool,
    pub score: f64,
}

const NO_MATCH: FuzzyMatch = FuzzyMatch {
    matches: false,
    score: 0.0,
};

// Indices are UTF-16 code units, as in TS, because they feed the score.
fn match_query(query: &[u16], text: &[u16]) -> FuzzyMatch {
    if query.is_empty() {
        return FuzzyMatch {
            matches: true,
            score: 0.0,
        };
    }
    if query.len() > text.len() {
        return NO_MATCH;
    }
    let mut score = 0.0;
    let mut last: i64 = -1;
    let mut consecutive = 0i64;
    for &q in query {
        let from = (last + 1) as usize;
        let Some(off) = text[from.min(text.len())..].iter().position(|&c| c == q) else {
            return NO_MATCH;
        };
        let i = (from + off) as i64;
        let boundary = i == 0 || {
            let p = text[(i - 1) as usize];
            char::from_u32(u32::from(p)).is_some_and(|c| c.is_whitespace() || "-_./:".contains(c))
        };
        if last == i - 1 {
            consecutive += 1;
            score -= (consecutive * 5) as f64;
        } else {
            consecutive = 0;
            if last >= 0 {
                score += ((i - last - 1) * 2) as f64;
            }
        }
        if boundary {
            score -= 10.0;
        }
        score += i as f64 * 0.1;
        last = i;
    }
    if query == text {
        score -= 100.0;
    }
    FuzzyMatch { matches: true, score }
}

fn swap_alnum(q: &str) -> Option<String> {
    let split = q.find(|c: char| c.is_ascii_digit() != q.starts_with(|d: char| d.is_ascii_digit()))?;
    let (a, b) = q.split_at(split);
    let letters = |s: &str| !s.is_empty() && s.bytes().all(|c| c.is_ascii_lowercase());
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit());
    ((letters(a) && digits(b)) || (digits(a) && letters(b))).then(|| format!("{b}{a}"))
}

/// `fuzzyMatch`: characters of `query` must appear in order in `text` (case-insensitive); `abc12` also tries `12abc`.
pub fn fuzzy_match(query: &str, text: &str) -> FuzzyMatch {
    let q = query.to_lowercase();
    let t: Vec<u16> = text.to_lowercase().encode_utf16().collect();
    let primary = match_query(&q.encode_utf16().collect::<Vec<_>>(), &t);
    if primary.matches {
        return primary;
    }
    match swap_alnum(&q) {
        Some(s) => {
            let m = match_query(&s.encode_utf16().collect::<Vec<_>>(), &t);
            if m.matches {
                FuzzyMatch {
                    matches: true,
                    score: m.score + 5.0,
                }
            } else {
                primary
            }
        }
        None => primary,
    }
}

/// `fuzzyFilter`: keep items matching every whitespace- or `/`-separated token, best total score first (stable).
pub fn fuzzy_filter<T: Clone>(items: &[T], query: &str, get_text: impl Fn(&T) -> String) -> Vec<T> {
    let tokens: Vec<&str> = query
        .split(|c: char| c.is_whitespace() || c == '/')
        .filter(|t| !t.is_empty())
        .collect();
    if tokens.is_empty() {
        return items.to_vec();
    }
    let mut scored: Vec<(f64, &T)> = items
        .iter()
        .filter_map(|item| {
            let text = get_text(item);
            let mut total = 0.0;
            for t in &tokens {
                let m = fuzzy_match(t, &text);
                if !m.matches {
                    return None;
                }
                total += m.score;
            }
            Some((total, item))
        })
        .collect();
    scored.sort_by(|a, b| a.0.total_cmp(&b.0));
    scored.into_iter().map(|(_, item)| item.clone()).collect()
}
