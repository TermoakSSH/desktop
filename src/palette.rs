//! Matching and ranking of the command palette (Cmd/Ctrl+K): what is shown
//! for what is typed, and in which order. No GPUI here; the palette itself
//! is `app/palette.rs`.
//!
//! Each word typed must match, fuzzily (its letters in order, not
//! necessarily together), the title of an entry, its detail or one of its
//! keywords. The score is the one of `fzy`: consecutive letters and letters
//! at the start of words count more, gaps count less; matching the title
//! counts more than matching the detail. Entries used recently get a bonus,
//! and with nothing typed they come first, then the rest grouped by kind.

use std::cmp::Ordering;

/// Kind of entry (also the order of the groups with nothing typed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    /// An open tab: switch to it.
    Tab,
    /// A saved host: connect.
    Host,
    /// A server session: attach.
    Session,
    /// A snippet: run it in the current terminal.
    Snippet,
    /// An action of the app.
    Command,
}

/// What the palette knows about an entry to find it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Stable identity, to remember it among the recent ones
    /// (`host:<id>`, `cmd:<name>`...).
    pub key: String,
    pub kind: Kind,
    pub title: String,
    /// Second line (address, command, menu...).
    pub detail: String,
    /// More words it is found by (tags, names in English...).
    pub keywords: Vec<String>,
}

/// An entry to show: its index in the list given and the characters of
/// its title that matched (to highlight them).
#[derive(Debug, Clone, PartialEq)]
pub struct Ranked {
    pub index: usize,
    pub score: f64,
    /// Character positions (not bytes) in the title.
    pub hits: Vec<usize>,
}

/// Recent entries remembered.
pub const MAX_RECENT: usize = 20;

/// Puts `key` first among the recent entries.
pub fn remember(recent: &mut Vec<String>, key: &str) {
    recent.retain(|k| k != key);
    recent.insert(0, key.to_string());
    recent.truncate(MAX_RECENT);
}

// Scores of fzy (https://github.com/jhawthorn/fzy, MIT).
const SCORE_MIN: f64 = f64::NEG_INFINITY;
const SCORE_MAX: f64 = 1000.0;
const GAP_LEADING: f64 = -0.005;
const GAP_TRAILING: f64 = -0.005;
const GAP_INNER: f64 = -0.01;
const MATCH_CONSECUTIVE: f64 = 1.0;
const MATCH_SLASH: f64 = 0.9;
const MATCH_WORD: f64 = 0.8;
const MATCH_CAPITAL: f64 = 0.7;
const MATCH_DOT: f64 = 0.6;
/// Longest text scored (longer details are cut).
const MAX_TEXT: usize = 256;
/// Matching the detail or a keyword instead of the title.
const DETAIL_PENALTY: f64 = 0.75;
/// Bonus of the most recent entry (less for older ones).
const RECENT_BONUS: f64 = 1.5;

/// Bonus of matching `c` right after `prev`: at the start of a word counts
/// more.
fn bonus(prev: char, c: char) -> f64 {
    match prev {
        '/' | '\\' => MATCH_SLASH,
        '-' | '_' | ' ' | '@' | ':' | '(' | '[' | ',' => MATCH_WORD,
        '.' => MATCH_DOT,
        p if p.is_lowercase() && c.is_uppercase() => MATCH_CAPITAL,
        _ => 0.0,
    }
}

fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// Fuzzy match of `needle` in `haystack`, ignoring case: the score (higher
/// is better) and the positions (in characters) of the letters matched.
pub fn fuzzy(needle: &str, haystack: &str) -> Option<(f64, Vec<usize>)> {
    let n: Vec<char> = needle.chars().map(fold).collect();
    let h_orig: Vec<char> = haystack.chars().take(MAX_TEXT).collect();
    let h: Vec<char> = h_orig.iter().map(|c| fold(*c)).collect();
    let (nl, hl) = (n.len(), h.len());
    if nl == 0 || nl > hl {
        return None;
    }
    // Every letter, in order?
    let mut it = h.iter();
    if !n.iter().all(|c| it.any(|x| x == c)) {
        return None;
    }
    if nl == hl {
        // The same text.
        return Some((SCORE_MAX, (0..hl).collect()));
    }
    let bonuses: Vec<f64> = (0..hl)
        .map(|j| {
            let prev = if j == 0 { '/' } else { h_orig[j - 1] };
            bonus(prev, h_orig[j])
        })
        .collect();
    // D: best score with needle[i] matched at haystack[j]; M: best score of
    // needle[..=i] in haystack[..=j].
    let mut d = vec![vec![SCORE_MIN; hl]; nl];
    let mut m = vec![vec![SCORE_MIN; hl]; nl];
    for i in 0..nl {
        let mut prev = SCORE_MIN;
        let gap = if i == nl - 1 { GAP_TRAILING } else { GAP_INNER };
        for j in 0..hl {
            if n[i] == h[j] {
                let score = if i == 0 {
                    j as f64 * GAP_LEADING + bonuses[j]
                } else if j > 0 {
                    (m[i - 1][j - 1] + bonuses[j]).max(d[i - 1][j - 1] + MATCH_CONSECUTIVE)
                } else {
                    SCORE_MIN
                };
                d[i][j] = score;
                prev = score.max(prev + gap);
            } else {
                prev += gap;
            }
            m[i][j] = prev;
        }
    }
    let score = m[nl - 1][hl - 1];
    // Back from the end: where each letter matched.
    let mut positions = vec![0; nl];
    let mut required = false;
    let mut j = hl;
    for i in (0..nl).rev() {
        while j > 0 {
            j -= 1;
            if d[i][j] != SCORE_MIN && (required || d[i][j] == m[i][j]) {
                required = i > 0 && j > 0 && m[i][j] == d[i - 1][j - 1] + MATCH_CONSECUTIVE;
                positions[i] = j;
                break;
            }
        }
    }
    Some((score, positions))
}

/// The entries to show for `query`, best first. `recent` are keys, the
/// most recent first.
pub fn rank(query: &str, entries: &[Entry], recent: &[String]) -> Vec<Ranked> {
    let recency = |e: &Entry| recent.iter().position(|k| *k == e.key);
    let words: Vec<&str> = query.split_whitespace().collect();
    if words.is_empty() {
        // Recent ones first (most recent first), then by kind, as given.
        let mut out: Vec<(Option<usize>, Kind, usize)> = entries
            .iter()
            .enumerate()
            .map(|(i, e)| (recency(e), e.kind, i))
            .collect();
        out.sort_by(|a, b| match (a.0, b.0) {
            (Some(x), Some(y)) => x.cmp(&y),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => (a.1, a.2).cmp(&(b.1, b.2)),
        });
        return out
            .into_iter()
            .map(|(_, _, index)| Ranked {
                index,
                score: 0.0,
                hits: Vec::new(),
            })
            .collect();
    }
    let mut out: Vec<(Ranked, Kind)> = Vec::new();
    'entries: for (index, e) in entries.iter().enumerate() {
        let mut total = 0.0;
        let mut hits: Vec<usize> = Vec::new();
        for word in &words {
            let title = fuzzy(word, &e.title);
            let other = std::iter::once(e.detail.as_str())
                .chain(e.keywords.iter().map(String::as_str))
                .filter_map(|text| fuzzy(word, text).map(|(s, _)| s - DETAIL_PENALTY))
                .fold(SCORE_MIN, f64::max);
            match title {
                Some((s, pos)) if s >= other => {
                    total += s;
                    hits.extend(pos);
                }
                _ if other > SCORE_MIN => total += other,
                _ => continue 'entries,
            }
        }
        if let Some(r) = recency(e) {
            total += RECENT_BONUS * (1.0 - r as f64 / MAX_RECENT as f64);
        }
        hits.sort_unstable();
        hits.dedup();
        out.push((
            Ranked {
                index,
                score: total,
                hits,
            },
            e.kind,
        ));
    }
    out.sort_by(|(a, ka), (b, kb)| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(Ordering::Equal)
            .then(ka.cmp(kb))
            .then(a.index.cmp(&b.index))
    });
    out.into_iter().map(|(r, _)| r).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: Kind, key: &str, title: &str, detail: &str) -> Entry {
        Entry {
            key: key.into(),
            kind,
            title: title.into(),
            detail: detail.into(),
            keywords: Vec::new(),
        }
    }

    fn titles(query: &str, entries: &[Entry], recent: &[String]) -> Vec<String> {
        rank(query, entries, recent)
            .into_iter()
            .map(|r| entries[r.index].title.clone())
            .collect()
    }

    #[test]
    fn fuzzy_letters_in_order() {
        assert!(fuzzy("wbs", "web-server").is_some());
        assert!(fuzzy("swb", "web-server").is_none());
        assert!(fuzzy("", "web").is_none());
        assert!(fuzzy("webserverx", "web-server").is_none());
        // Case is ignored, also outside ASCII.
        assert!(fuzzy("ÑAN", "ñandú").is_some());
        // The same text is the best possible.
        assert_eq!(fuzzy("Web", "web").unwrap().0, SCORE_MAX);
        // Positions of the letters, preferring starts of words and runs.
        assert_eq!(fuzzy("ws", "web-server").unwrap().1, vec![0, 4]);
        assert_eq!(fuzzy("prod", "my prod db").unwrap().1, vec![3, 4, 5, 6]);
        assert_eq!(fuzzy("nt", "New Tab").unwrap().1, vec![0, 4]);
    }

    #[test]
    fn better_matches_score_higher() {
        let s = |n: &str, h: &str| fuzzy(n, h).unwrap().0;
        // Together beats scattered.
        assert!(s("db", "db-primary") > s("db", "d-backup"));
        // Start of a word beats the middle of one.
        assert!(s("set", "settings") > s("set", "reset"));
        assert!(s("ws", "web-server") > s("ws", "towers"));
        // Shorter text with the same match: fewer gaps.
        assert!(s("web", "web") > s("web", "web-1"));
        assert!(s("web", "web-1") > s("web", "web-1-production-long-name"));
    }

    #[test]
    fn every_word_must_match_somewhere() {
        let entries = vec![
            entry(Kind::Host, "h1", "prod-db", "10.0.0.5"),
            entry(Kind::Host, "h2", "prod-web", "10.0.0.6"),
            entry(Kind::Host, "h3", "staging-db", "10.1.0.5"),
        ];
        // "prod-web" has a "d" and a "b" too, but further apart.
        assert_eq!(
            titles("prod db", &entries, &[]),
            vec!["prod-db", "prod-web"]
        );
        assert_eq!(
            titles("db", &entries, &[]),
            vec!["prod-db", "staging-db", "prod-web"]
        );
        // The detail counts too (less than the title).
        assert_eq!(titles("10.1", &entries, &[]), vec!["staging-db"]);
        assert!(titles("nothing", &entries, &[]).is_empty());
        // Keywords.
        let mut tagged = entries.clone();
        tagged[1].keywords = vec!["nginx".into()];
        assert_eq!(titles("nginx", &tagged, &[]), vec!["prod-web"]);
    }

    #[test]
    fn title_beats_detail() {
        let entries = vec![
            entry(Kind::Command, "c1", "Open SFTP", "Terminal"),
            entry(Kind::Host, "h1", "backup", "sftp.example.com"),
        ];
        assert_eq!(titles("sftp", &entries, &[]), vec!["Open SFTP", "backup"]);
        // Hits are only for the title.
        let r = rank("sftp", &entries, &[]);
        assert_eq!(r[0].hits, vec![5, 6, 7, 8]);
        assert!(r[1].hits.is_empty());
    }

    #[test]
    fn recent_first_with_nothing_typed() {
        let entries = vec![
            entry(Kind::Command, "cmd:a", "Settings", ""),
            entry(Kind::Host, "host:1", "web", ""),
            entry(Kind::Tab, "tab:1", "web · htop", ""),
            entry(Kind::Snippet, "snippet:1", "Disk usage", ""),
            entry(Kind::Host, "host:2", "db", ""),
        ];
        let recent = vec!["snippet:1".to_string(), "host:2".to_string()];
        assert_eq!(
            titles("", &entries, &recent),
            vec!["Disk usage", "db", "web · htop", "web", "Settings"]
        );
        assert_eq!(
            titles("   ", &entries, &[]),
            vec!["web · htop", "web", "db", "Disk usage", "Settings"]
        );
    }

    #[test]
    fn recent_ones_win_ties() {
        let entries = vec![
            entry(Kind::Host, "host:1", "web-1", ""),
            entry(Kind::Host, "host:2", "web-2", ""),
        ];
        assert_eq!(titles("web", &entries, &[]), vec!["web-1", "web-2"]);
        let recent = vec!["host:2".to_string()];
        assert_eq!(titles("web", &entries, &recent), vec!["web-2", "web-1"]);
        // But a clearly better match still wins.
        let entries = vec![
            entry(Kind::Host, "host:1", "db", ""),
            entry(Kind::Host, "host:2", "dashboard-b", ""),
        ];
        let recent = vec!["host:2".to_string()];
        assert_eq!(titles("db", &entries, &recent)[0], "db");
    }

    #[test]
    fn remembering() {
        let mut recent = vec!["a".to_string(), "b".to_string()];
        remember(&mut recent, "b");
        assert_eq!(recent, vec!["b", "a"]);
        remember(&mut recent, "c");
        assert_eq!(recent, vec!["c", "b", "a"]);
        for i in 0..50 {
            remember(&mut recent, &i.to_string());
        }
        assert_eq!(recent.len(), MAX_RECENT);
        assert_eq!(recent[0], "49");
    }
}
