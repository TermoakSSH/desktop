//! Command autocompletion in the terminal: the suggestions for the line being
//! typed. Tracking that line ([`LineTracker`]) lives in
//! `termoak_client::line`, shared with the mobile apps.

use termoak_client::complete::Suggestion;

pub use termoak_client::line::{LineTracker, screen_matches};

/// Suggestions received for a line.
#[derive(Debug, Clone)]
pub struct Suggestions {
    /// Line they were computed for.
    pub line: String,
    pub items: Vec<Suggestion>,
    /// Selected in the list (Alt+↑/↓).
    pub selected: usize,
}

impl Suggestions {
    pub fn current(&self) -> Option<&Suggestion> {
        self.items.get(self.selected)
    }

    /// The suggestions that still match `line` (which extends the line they
    /// were computed for), with the missing part recomputed. They are shown
    /// while the new ones arrive, so the ghost text does not flicker.
    pub fn narrow(&self, line: &str) -> Option<Suggestions> {
        if !line.starts_with(self.line.as_str()) {
            return None;
        }
        let selected = self.current().map(|s| s.text.clone());
        let items: Vec<Suggestion> = self
            .items
            .iter()
            .filter(|s| s.text.len() > line.len() && s.text.starts_with(line))
            .map(|s| Suggestion {
                insert: s.text[line.len()..].to_string(),
                ..s.clone()
            })
            .collect();
        if items.is_empty() {
            return None;
        }
        let selected = selected
            .and_then(|t| items.iter().position(|s| s.text == t))
            .unwrap_or(0);
        Some(Suggestions {
            line: line.to_string(),
            items,
            selected,
        })
    }

    /// Moves the selection (`delta` = ±1), wrapping around.
    pub fn step(&mut self, delta: i32) {
        let n = self.items.len() as i32;
        if n > 0 {
            self.selected = (self.selected as i32 + delta).rem_euclid(n) as usize;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn suggestions(line: &str, texts: &[&str]) -> Suggestions {
        Suggestions {
            line: line.into(),
            items: texts
                .iter()
                .map(|t| Suggestion {
                    text: t.to_string(),
                    insert: t[line.len()..].to_string(),
                    description: String::new(),
                    source: termoak_client::complete::SuggestionSource::Command,
                })
                .collect(),
            selected: 0,
        }
    }

    #[test]
    fn narrowing_keeps_what_still_fits() {
        let mut s = suggestions("g", &["git status", "git pull", "grep -rn "]);
        s.selected = 1;
        let n = s.narrow("git").unwrap();
        assert_eq!(n.items.len(), 2);
        assert_eq!(n.current().unwrap().text, "git pull");
        assert_eq!(n.current().unwrap().insert, " pull");
        // If none matches any more, or the line was erased, nothing is left.
        assert!(s.narrow("gz").is_none());
        assert!(s.narrow("").is_none());
        assert!(s.narrow("git status").is_none());
    }

    #[test]
    fn selection_wraps_around() {
        let mut s = suggestions("g", &["git status", "git pull", "grep -rn "]);
        s.step(-1);
        assert_eq!(s.current().unwrap().text, "grep -rn ");
        s.step(1);
        s.step(1);
        assert_eq!(s.current().unwrap().text, "git pull");
    }
}
