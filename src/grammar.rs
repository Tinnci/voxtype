//! Conservative local text-cleanup suggestions for recent dictation text.
//!
//! This module deliberately does not claim semantic grammar understanding. It
//! returns byte-addressed, Unicode-safe edits so a UI can review each change
//! before applying it. Nothing here mutates an input context automatically.

use serde::Serialize;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum EditSafety {
    Safe,
    Review,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum EditConfidence {
    High,
    Medium,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CleanupEdit {
    pub start_byte: usize,
    pub end_byte: usize,
    pub original: String,
    pub replacement: String,
    pub rule_id: &'static str,
    pub message: &'static str,
    pub safety: EditSafety,
    pub confidence: EditConfidence,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct GrammarReport {
    pub schema: u8,
    pub clean: bool,
    pub original_fingerprint: String,
    pub original_bytes: usize,
    pub original_characters: usize,
    pub suggested: String,
    pub safe_edit_count: usize,
    pub review_edit_count: usize,
    pub edits: Vec<CleanupEdit>,
}

impl GrammarReport {
    #[must_use]
    pub const fn is_clean(&self) -> bool {
        self.clean
    }

    #[must_use]
    pub fn render(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| {
            r#"{"schema":1,"clean":false,"error":"cleanup.serialization"}"#.to_owned()
        })
    }
}

#[derive(Clone, Copy)]
struct Candidate {
    start: usize,
    end: usize,
    replacement: &'static str,
    rule_id: &'static str,
    message: &'static str,
    safety: EditSafety,
    confidence: EditConfidence,
    priority: u8,
}

/// Produces conservative, reviewable cleanup suggestions.
#[must_use]
pub fn check(text: &str) -> GrammarReport {
    let mut candidates = Vec::new();
    outer_horizontal_whitespace(text, &mut candidates);
    repeated_words(text, &mut candidates);
    punctuation_spacing(text, &mut candidates);
    repeated_punctuation(text, &mut candidates);
    repeated_horizontal_whitespace(text, &mut candidates);
    sentence_case(text, &mut candidates);

    candidates.sort_by_key(|candidate| (candidate.priority, candidate.start, candidate.end));
    let mut selected: Vec<Candidate> = Vec::new();
    for candidate in candidates {
        if candidate.start >= candidate.end
            || !text.is_char_boundary(candidate.start)
            || !text.is_char_boundary(candidate.end)
            || selected
                .iter()
                .any(|edit| ranges_overlap(candidate.start, candidate.end, edit.start, edit.end))
        {
            continue;
        }
        selected.push(candidate);
    }
    selected.sort_by_key(|candidate| candidate.start);

    let edits = selected
        .into_iter()
        .map(|candidate| CleanupEdit {
            start_byte: candidate.start,
            end_byte: candidate.end,
            original: text[candidate.start..candidate.end].to_owned(),
            replacement: candidate.replacement.to_owned(),
            rule_id: candidate.rule_id,
            message: candidate.message,
            safety: candidate.safety,
            confidence: candidate.confidence,
        })
        .collect::<Vec<_>>();
    let suggested = apply_edits(text, &edits).unwrap_or_else(|| text.to_owned());
    let safe_edit_count = edits
        .iter()
        .filter(|edit| edit.safety == EditSafety::Safe)
        .count();
    let review_edit_count = edits.len().saturating_sub(safe_edit_count);
    GrammarReport {
        schema: 1,
        clean: edits.is_empty(),
        original_fingerprint: fingerprint(text),
        original_bytes: text.len(),
        original_characters: text.chars().count(),
        suggested,
        safe_edit_count,
        review_edit_count,
        edits,
    }
}

/// Applies a set of non-overlapping edits only when every original span still
/// matches. This protects future review UIs from applying a stale report.
#[must_use]
pub fn apply_edits(text: &str, edits: &[CleanupEdit]) -> Option<String> {
    let mut previous_end = 0;
    for edit in edits {
        if edit.start_byte < previous_end
            || edit.start_byte > edit.end_byte
            || edit.end_byte > text.len()
            || !text.is_char_boundary(edit.start_byte)
            || !text.is_char_boundary(edit.end_byte)
            || text.get(edit.start_byte..edit.end_byte)? != edit.original
        {
            return None;
        }
        previous_end = edit.end_byte;
    }
    let mut output = String::with_capacity(text.len());
    let mut cursor = 0;
    for edit in edits {
        output.push_str(&text[cursor..edit.start_byte]);
        output.push_str(&edit.replacement);
        cursor = edit.end_byte;
    }
    output.push_str(&text[cursor..]);
    Some(output)
}

/// Applies a complete report only when the entire source text still matches
/// the report fingerprint and length, not merely the edited spans.
#[must_use]
pub fn apply_report(text: &str, report: &GrammarReport) -> Option<String> {
    if text.len() != report.original_bytes
        || text.chars().count() != report.original_characters
        || fingerprint(text) != report.original_fingerprint
    {
        return None;
    }
    apply_edits(text, &report.edits)
}

fn outer_horizontal_whitespace(text: &str, candidates: &mut Vec<Candidate>) {
    let leading_end = text
        .char_indices()
        .take_while(|(_, character)| is_horizontal_whitespace(*character))
        .map(|(index, character)| index + character.len_utf8())
        .last()
        .unwrap_or(0);
    if leading_end > 0 {
        push(
            candidates,
            0,
            leading_end,
            "",
            "cleanup.outer-whitespace",
            "Remove leading horizontal whitespace",
            EditSafety::Safe,
            EditConfidence::High,
            0,
        );
    }
    let trailing_start = text
        .char_indices()
        .rev()
        .take_while(|(_, character)| is_horizontal_whitespace(*character))
        .map(|(index, _)| index)
        .last()
        .unwrap_or(text.len());
    if trailing_start < text.len() && trailing_start >= leading_end {
        push(
            candidates,
            trailing_start,
            text.len(),
            "",
            "cleanup.outer-whitespace",
            "Remove trailing horizontal whitespace",
            EditSafety::Safe,
            EditConfidence::High,
            0,
        );
    }
}

fn repeated_words(text: &str, candidates: &mut Vec<Candidate>) {
    let tokens = token_spans(text);
    for pair in tokens.windows(2) {
        let previous = pair[0];
        let current = pair[1];
        let Some(previous_word) = normalized_word(text, previous) else {
            continue;
        };
        let Some(current_word) = normalized_word(text, current) else {
            continue;
        };
        if previous_word.value.eq_ignore_ascii_case(current_word.value)
            && !text[previous.end..current.start].contains('\n')
            && current_word.start == current.start
        {
            push(
                candidates,
                previous.end,
                current_word.end,
                "",
                "cleanup.repeated-word",
                "Remove an adjacent repeated word",
                EditSafety::Review,
                EditConfidence::Medium,
                1,
            );
        }
    }
}

fn punctuation_spacing(text: &str, candidates: &mut Vec<Candidate>) {
    for (index, character) in text.char_indices() {
        if !is_sentence_punctuation(character) {
            continue;
        }
        let start = text[..index]
            .char_indices()
            .rev()
            .take_while(|(_, previous)| is_horizontal_whitespace(*previous))
            .map(|(position, _)| position)
            .last()
            .unwrap_or(index);
        if start < index {
            push(
                candidates,
                start,
                index,
                "",
                "cleanup.punctuation-spacing",
                "Remove horizontal whitespace before punctuation",
                EditSafety::Safe,
                EditConfidence::High,
                2,
            );
        }
    }
}

fn repeated_punctuation(text: &str, candidates: &mut Vec<Candidate>) {
    let mut run: Option<(usize, char, usize)> = None;
    for (index, character) in text
        .char_indices()
        .chain(std::iter::once((text.len(), '\0')))
    {
        match run {
            Some((start, previous, count)) if character == previous => {
                run = Some((start, previous, count + 1));
            }
            Some((start, previous, count)) => {
                if count > 1 && is_collapsible_punctuation(previous) {
                    push(
                        candidates,
                        start,
                        index,
                        punctuation_replacement(previous),
                        "cleanup.repeated-punctuation",
                        "Collapse repeated punctuation",
                        EditSafety::Review,
                        EditConfidence::Medium,
                        3,
                    );
                }
                run = Some((index, character, 1));
            }
            None => run = Some((index, character, 1)),
        }
    }
}

fn repeated_horizontal_whitespace(text: &str, candidates: &mut Vec<Candidate>) {
    let mut run_start = None;
    for (index, character) in text
        .char_indices()
        .chain(std::iter::once((text.len(), '\0')))
    {
        if is_horizontal_whitespace(character) {
            run_start.get_or_insert(index);
            continue;
        }
        let Some(start) = run_start.take() else {
            continue;
        };
        if text[start..index].chars().count() > 1
            && start > 0
            && index < text.len()
            && !text[..start].ends_with('\n')
        {
            push(
                candidates,
                start,
                index,
                " ",
                "cleanup.repeated-whitespace",
                "Collapse repeated horizontal whitespace",
                EditSafety::Review,
                EditConfidence::High,
                4,
            );
        }
    }
}

fn sentence_case(text: &str, candidates: &mut Vec<Candidate>) {
    let mut sentence_start = true;
    let protected = protected_token_spans(text);
    for (index, character) in text.char_indices() {
        if protected
            .iter()
            .any(|span| index >= span.start && index < span.end)
        {
            sentence_start = false;
            continue;
        }
        if sentence_start {
            if character.is_ascii_lowercase() {
                let replacement = uppercase_ascii(character);
                push(
                    candidates,
                    index,
                    index + character.len_utf8(),
                    replacement,
                    "cleanup.sentence-case",
                    "Capitalize an ASCII sentence beginning",
                    EditSafety::Review,
                    EditConfidence::Medium,
                    5,
                );
                sentence_start = false;
            } else if !character.is_whitespace() && !is_opening_quote(character) {
                sentence_start = false;
            }
        } else if is_sentence_terminator(character) {
            sentence_start = true;
        }
    }
}

fn protected_token_spans(text: &str) -> Vec<Span> {
    token_spans(text)
        .into_iter()
        .filter(|span| {
            let token = &text[span.start..span.end];
            token.contains("://") || token.contains('@') || token.starts_with("www.")
        })
        .collect()
}

#[derive(Clone, Copy)]
struct Span {
    start: usize,
    end: usize,
}

#[derive(Clone, Copy)]
struct Word<'a> {
    start: usize,
    end: usize,
    value: &'a str,
}

fn token_spans(text: &str) -> Vec<Span> {
    let mut spans = Vec::new();
    let mut start = None;
    for (index, character) in text
        .char_indices()
        .chain(std::iter::once((text.len(), ' ')))
    {
        if character.is_whitespace() {
            if let Some(start) = start.take() {
                spans.push(Span { start, end: index });
            }
        } else {
            start.get_or_insert(index);
        }
    }
    spans
}

fn normalized_word(text: &str, span: Span) -> Option<Word<'_>> {
    let token = &text[span.start..span.end];
    let value = token.trim_matches(is_sentence_punctuation);
    if value.is_empty() || !value.chars().any(char::is_alphanumeric) {
        return None;
    }
    let relative_start = token.find(value)?;
    Some(Word {
        start: span.start + relative_start,
        end: span.start + relative_start + value.len(),
        value,
    })
}

#[allow(clippy::too_many_arguments)]
fn push(
    candidates: &mut Vec<Candidate>,
    start: usize,
    end: usize,
    replacement: &'static str,
    rule_id: &'static str,
    message: &'static str,
    safety: EditSafety,
    confidence: EditConfidence,
    priority: u8,
) {
    candidates.push(Candidate {
        start,
        end,
        replacement,
        rule_id,
        message,
        safety,
        confidence,
        priority,
    });
}

fn ranges_overlap(a_start: usize, a_end: usize, b_start: usize, b_end: usize) -> bool {
    a_start < b_end && b_start < a_end
}

fn fingerprint(text: &str) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("fnv1a64:{hash:016x}")
}

const fn is_horizontal_whitespace(character: char) -> bool {
    matches!(character, ' ' | '\t' | '\u{00a0}' | '\u{3000}')
}

const fn is_sentence_punctuation(character: char) -> bool {
    matches!(
        character,
        ',' | '.' | ';' | ':' | '!' | '?' | '，' | '。' | '；' | '：' | '！' | '？'
    )
}

const fn is_collapsible_punctuation(character: char) -> bool {
    matches!(
        character,
        ',' | ';' | ':' | '!' | '?' | '，' | '；' | '：' | '！' | '？'
    )
}

const fn punctuation_replacement(character: char) -> &'static str {
    match character {
        ',' => ",",
        ';' => ";",
        ':' => ":",
        '!' => "!",
        '?' => "?",
        '，' => "，",
        '；' => "；",
        '：' => "：",
        '！' => "！",
        '？' => "？",
        _ => "",
    }
}

const fn uppercase_ascii(character: char) -> &'static str {
    match character {
        'a' => "A",
        'b' => "B",
        'c' => "C",
        'd' => "D",
        'e' => "E",
        'f' => "F",
        'g' => "G",
        'h' => "H",
        'i' => "I",
        'j' => "J",
        'k' => "K",
        'l' => "L",
        'm' => "M",
        'n' => "N",
        'o' => "O",
        'p' => "P",
        'q' => "Q",
        'r' => "R",
        's' => "S",
        't' => "T",
        'u' => "U",
        'v' => "V",
        'w' => "W",
        'x' => "X",
        'y' => "Y",
        'z' => "Z",
        _ => "",
    }
}

const fn is_opening_quote(character: char) -> bool {
    matches!(character, '"' | '\'' | '“' | '‘' | '(' | '[' | '{')
}

const fn is_sentence_terminator(character: char) -> bool {
    matches!(character, '.' | '!' | '?' | '。' | '！' | '？')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn produces_non_overlapping_reviewable_edits() {
        let source = "  hello   hello , world!!  ";
        let report = check(source);
        assert_eq!(report.suggested, "Hello, world!");
        assert_eq!(
            apply_report(source, &report),
            Some(report.suggested.clone())
        );
        assert!(report.safe_edit_count > 0);
        assert!(report.review_edit_count > 0);
        assert!(
            report
                .edits
                .windows(2)
                .all(|pair| pair[0].end_byte <= pair[1].start_byte)
        );
    }

    #[test]
    fn preserves_newlines_ellipsis_urls_and_clean_chinese() {
        for source in [
            "这是正常的中文。下一句也正常。",
            "第一行\n第二行",
            "请访问 https://example.com/a?q=1...",
            "C++ 与 Rust 都可以。",
        ] {
            let report = check(source);
            assert_eq!(report.suggested, source);
        }
    }

    #[test]
    fn repeated_word_is_review_only() {
        let report = check("I had had enough.");
        let edit = report
            .edits
            .iter()
            .find(|edit| edit.rule_id == "cleanup.repeated-word")
            .expect("repeated-word edit");
        assert_eq!(edit.safety, EditSafety::Review);
        assert_eq!(report.suggested, "I had enough.");
    }

    #[test]
    fn stale_or_non_boundary_edits_are_rejected() {
        let report = check("hello  world");
        assert!(apply_edits("hello changed", &report.edits).is_none());
        assert!(apply_report("jello  world", &report).is_none());
        let mut invalid = report.edits;
        invalid[0].start_byte = 1;
        assert!(apply_edits("你好  world", &invalid).is_none());
    }

    #[test]
    fn json_round_trips_arbitrary_unicode_content() {
        let report = check("  你好 👋  世界！  ");
        let value: serde_json::Value =
            serde_json::from_str(&report.render()).expect("cleanup report JSON");
        assert_eq!(value["schema"], 1);
        assert_eq!(value["original_fingerprint"], report.original_fingerprint);
        assert!(value["edits"].is_array());
    }
}
