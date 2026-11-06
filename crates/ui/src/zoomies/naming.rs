//! A zoomie's default name, derived locally from its prompt (no model call):
//! the first few meaningful words, editable later in Settings.

/// Longest derived name, in characters.
const MAX_CHARS: usize = 32;
const MAX_WORDS: usize = 4;

/// Openers that carry no meaning in a name.
const OPENERS: &[&[&str]] = &[
    &["hey"],
    &["hi"],
    &["ok"],
    &["okay"],
    &["please"],
    &["pls"],
    &["can", "you"],
    &["could", "you"],
    &["would", "you"],
    &["i", "want", "you", "to"],
    &["i'd", "like", "you", "to"],
    &["i", "need", "you", "to"],
    &["go", "and"],
    &["just"],
    &["kindly"],
];

/// Words a name should not end on.
const TRAILING: &[&str] = &[
    "a", "an", "the", "and", "or", "to", "of", "for", "in", "on", "at", "with", "from", "by",
    "into", "my", "our", "your", "its", "is", "are", "that", "this", "then", "&", "about", "as",
];

/// "Summarize yesterday's commits and post to Slack" → "Summarize yesterday's
/// commits". Empty or symbol-only prompts name the zoomie "Zoomie".
pub fn derive_name(prompt: &str) -> String {
    let first_line = prompt
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("");
    let first_line = strip_links(first_line);
    // One sentence is plenty.
    let sentence = sentences(&first_line)
        .into_iter()
        .find(|part| part.chars().any(char::is_alphanumeric))
        .unwrap_or_default();
    let mut words: Vec<String> = sentence
        .split_whitespace()
        .filter(|word| !word.contains("://") && !word.starts_with("www."))
        .map(clean_word)
        .filter(|word| word.chars().any(char::is_alphanumeric))
        .collect();
    strip_openers(&mut words);
    let mut name: Vec<String> = Vec::new();
    let mut len = 0;
    for word in words {
        let extra = word.chars().count() + usize::from(!name.is_empty());
        if name.len() == MAX_WORDS || (len + extra > MAX_CHARS && !name.is_empty()) {
            break;
        }
        len += extra;
        name.push(word);
    }
    while name.len() > 1
        && name
            .last()
            .is_some_and(|word| TRAILING.contains(&word.to_lowercase().as_str()))
    {
        name.pop();
    }
    let mut name = name.join(" ");
    if name.chars().count() > MAX_CHARS {
        name = name.chars().take(MAX_CHARS - 1).collect::<String>();
        name.push('…');
    }
    let mut chars = name.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => "Zoomie".into(),
    }
}

/// Sentences end at `.!?;:` followed by a space or the end, so `main.rs`
/// and `10:30` stay whole.
fn sentences(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        let ends = matches!(c, '.' | '!' | '?' | ';' | ':')
            && chars.peek().is_none_or(|next| next.is_whitespace());
        if ends {
            out.push(std::mem::take(&mut current).trim().to_string());
        } else {
            current.push(c);
        }
    }
    out.push(current.trim().to_string());
    out
}

/// `[text](url)` → `text`.
fn strip_links(text: &str) -> String {
    let mut out = String::new();
    let mut rest = text;
    while let Some(open) = rest.find('[') {
        let Some(mid) = rest[open..].find("](").map(|at| open + at) else {
            break;
        };
        let Some(close) = rest[mid..].find(')').map(|at| mid + at) else {
            break;
        };
        out.push_str(&rest[..open]);
        out.push_str(&rest[open + 1..mid]);
        rest = &rest[close + 1..];
    }
    out.push_str(rest);
    out
}

/// Markdown and quoting around a word: `**fix**`, "`cargo`", (src), [link](…).
fn clean_word(word: &str) -> String {
    let word = word.trim_matches(|c: char| {
        matches!(
            c,
            '*' | '_'
                | '`'
                | '"'
                | '“'
                | '”'
                | '‘'
                | '’'
                | '('
                | ')'
                | '['
                | ']'
                | '<'
                | '>'
                | '#'
                | '~'
                | ','
                | '-'
                | '—'
                | '–'
        )
    });
    // `@src/main.rs` mentions read as their file name.
    let word = word.strip_prefix('@').unwrap_or(word);
    word.to_string()
}

fn strip_openers(words: &mut Vec<String>) {
    loop {
        let lower: Vec<String> = words.iter().map(|w| w.to_lowercase()).collect();
        let Some(opener) = OPENERS.iter().find(|opener| {
            opener.len() < lower.len()
                && opener
                    .iter()
                    .zip(&lower)
                    .all(|(want, have)| *want == have.as_str())
        }) else {
            return;
        };
        words.drain(..opener.len());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn takes_the_first_few_meaningful_words() {
        for (prompt, name) in [
            (
                "Summarize yesterday's commits and post to Slack",
                "Summarize yesterday's commits",
            ),
            ("check the build", "Check the build"),
            ("Run the test suite and fix failures", "Run the test suite"),
            (
                "Please triage new GitHub issues every morning",
                "Triage new GitHub issues",
            ),
            ("Can you update the changelog?", "Update the changelog"),
            ("hey, look at the logs", "Look at the logs"),
            (
                "I want you to review open pull requests",
                "Review open pull requests",
            ),
            ("bump deps. then open a PR", "Bump deps"),
            ("**Fix** the `flaky` test", "Fix the flaky test"),
            ("Read [the docs](https://x.dev) for news", "Read the docs"),
            ("Look at @src/main.rs", "Look at src/main.rs"),
            (
                "\n\n  write a haiku about rust\nsecond line",
                "Write a haiku",
            ),
            ("Deploy to https://example.com now", "Deploy to now"),
        ] {
            assert_eq!(derive_name(prompt), name, "{prompt:?}");
        }
    }

    #[test]
    fn never_ends_on_a_connective() {
        assert_eq!(derive_name("Clean up and"), "Clean up");
        assert_eq!(derive_name("Write tests for the"), "Write tests");
        assert_eq!(derive_name("the"), "The");
    }

    #[test]
    fn stays_short() {
        let name = derive_name("Internationalization configuration verification orchestration");
        assert!(name.chars().count() <= MAX_CHARS, "{name}");
        let one = derive_name("Supercalifragilisticexpialidociousandthensomemorewordsglued");
        assert_eq!(one.chars().count(), MAX_CHARS);
        assert!(one.ends_with('…'));
    }

    #[test]
    fn empty_prompts_get_a_default() {
        assert_eq!(derive_name(""), "Zoomie");
        assert_eq!(derive_name("   \n "), "Zoomie");
        assert_eq!(derive_name("!!! ??? ..."), "Zoomie");
        assert_eq!(derive_name("please"), "Please");
    }

    #[test]
    fn keeps_non_ascii() {
        assert_eq!(derive_name("überprüfe die Logs"), "Überprüfe die Logs");
    }
}
