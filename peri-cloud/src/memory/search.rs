//! Unicode bigrams make Chinese recall explicit; ASCII entities keep full words.
use std::collections::BTreeSet;

pub(super) fn terms(text: &str) -> Vec<String> {
    let lower = text.to_lowercase();
    let mut words = BTreeSet::new();
    let mut ascii = String::new();
    let mut previous = None;
    for ch in lower.chars().chain(std::iter::once(' ')) {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            ascii.push(ch);
            previous = None;
        } else {
            if !ascii.is_empty() {
                words.insert(std::mem::take(&mut ascii));
            }
            if ch.is_alphanumeric() {
                words.insert(ch.to_string());
                if let Some(prev) = previous {
                    words.insert(format!("{prev}{ch}"));
                }
                previous = Some(ch);
            } else {
                previous = None;
            }
        }
    }
    words.into_iter().collect()
}

pub(super) fn expression(text: &str) -> String {
    terms(text)
        .into_iter()
        .take(64)
        .map(|term| format!("\"{term}\""))
        .collect::<Vec<_>>()
        .join(" OR ")
}
