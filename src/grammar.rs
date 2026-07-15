//! Small local grammar and typography checks for recent dictation text.

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GrammarIssue {
    pub code: &'static str,
    pub message: &'static str,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GrammarReport {
    pub original: String,
    pub suggested: String,
    pub issues: Vec<GrammarIssue>,
}

impl GrammarReport {
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.issues.is_empty()
    }

    #[must_use]
    pub fn render(&self) -> String {
        if self.is_clean() {
            return format!("clean=true suggested={}", self.suggested);
        }
        let codes = self
            .issues
            .iter()
            .map(|issue| issue.code)
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "clean=false issues={} codes={} suggested={}",
            self.issues.len(),
            codes,
            self.suggested
        )
    }
}

#[must_use]
pub fn check(text: &str) -> GrammarReport {
    let mut issues = Vec::new();
    let mut suggested = text.trim().to_owned();
    if suggested != text {
        issues.push(issue(
            "grammar.outer_whitespace",
            "Remove leading or trailing whitespace",
        ));
    }

    let collapsed = collapse_whitespace(&suggested);
    if collapsed != suggested {
        issues.push(issue(
            "grammar.repeated_whitespace",
            "Collapse repeated whitespace",
        ));
        suggested = collapsed;
    }

    let punctuation = normalize_punctuation_spacing(&suggested);
    if punctuation != suggested {
        issues.push(issue(
            "grammar.punctuation_spacing",
            "Remove spaces before punctuation",
        ));
        suggested = punctuation;
    }

    let deduplicated = remove_repeated_words(&suggested);
    if deduplicated != suggested {
        issues.push(issue(
            "grammar.repeated_word",
            "Remove an adjacent repeated word",
        ));
        suggested = deduplicated;
    }

    let punctuation = collapse_repeated_punctuation(&suggested);
    if punctuation != suggested {
        issues.push(issue(
            "grammar.repeated_punctuation",
            "Collapse repeated punctuation",
        ));
        suggested = punctuation;
    }

    let capitalized = capitalize_sentences(&suggested);
    if capitalized != suggested {
        issues.push(issue(
            "grammar.sentence_case",
            "Capitalize sentence beginnings",
        ));
        suggested = capitalized;
    }

    GrammarReport {
        original: text.to_owned(),
        suggested,
        issues,
    }
}

const fn issue(code: &'static str, message: &'static str) -> GrammarIssue {
    GrammarIssue { code, message }
}

fn collapse_whitespace(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut whitespace = false;
    for character in text.chars() {
        if character.is_whitespace() {
            whitespace = true;
        } else {
            if whitespace && !output.is_empty() {
                output.push(' ');
            }
            whitespace = false;
            output.push(character);
        }
    }
    output
}

fn normalize_punctuation_spacing(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    for character in text.chars() {
        if is_punctuation(character) && output.ends_with(' ') {
            output.pop();
        }
        output.push(character);
    }
    output
}

fn collapse_repeated_punctuation(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut previous = None;
    for character in text.chars() {
        if Some(character) == previous && is_punctuation(character) {
            continue;
        }
        output.push(character);
        previous = Some(character);
    }
    output
}

fn remove_repeated_words(text: &str) -> String {
    let words = text.split(' ').collect::<Vec<_>>();
    let mut output: Vec<String> = Vec::with_capacity(words.len());
    for word in words {
        let normalized = word.trim_matches(is_punctuation);
        let repeated = output.last().is_some_and(|previous| {
            previous
                .trim_matches(is_punctuation)
                .eq_ignore_ascii_case(normalized)
                && !normalized.is_empty()
        });
        if repeated {
            let mut suffix = word
                .chars()
                .rev()
                .take_while(|character| is_punctuation(*character))
                .collect::<Vec<_>>();
            suffix.reverse();
            if let Some(previous) = output.last_mut() {
                previous.extend(suffix);
            }
        } else {
            output.push(word.to_owned());
        }
    }
    output.join(" ")
}

fn capitalize_sentences(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut sentence_start = true;
    for character in text.chars() {
        if sentence_start && character.is_ascii_alphabetic() {
            output.push(character.to_ascii_uppercase());
            sentence_start = false;
        } else {
            output.push(character);
            if !character.is_whitespace() {
                sentence_start = matches!(character, '.' | '!' | '?');
            }
        }
    }
    output
}

fn is_punctuation(character: char) -> bool {
    matches!(
        character,
        ',' | '.' | ';' | ':' | '!' | '?' | '，' | '。' | '；' | '：' | '！' | '？'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fixes_common_dictation_typography() {
        let report = check("  hello   hello , world!!  ");
        assert_eq!(report.suggested, "Hello, world!");
        assert!(!report.is_clean());
    }

    #[test]
    fn preserves_clean_chinese_text() {
        let report = check("这是正常的中文。下一句也正常。");
        assert!(report.is_clean());
        assert_eq!(report.suggested, report.original);
    }
}
