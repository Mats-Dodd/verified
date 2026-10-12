//! Partial JSON: the value a prefix of a JSON document is heading towards.
//!
//! A port of `fixJson` and `parsePartialJson` from `ai@7.0.127`
//! (`packages/ai/src/util/fix-json.ts`, `parse-partial-json.ts`, Copyright
//! Vercel, Inc., Apache License 2.0): a single linear scan that cuts the text
//! back to its last complete token and closes every open string, object and
//! array. The fold uses it for `tool-input-delta`, so a tool part shows its
//! arguments while they stream.
//!
//! Like the original, invalid JSON is not repaired; the result is handed to a
//! real JSON parser, which rejects it.

use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Root,
    Finish,
    InsideString,
    InsideStringEscape,
    InsideStringUnicodeEscape,
    InsideLiteral,
    InsideNumber,
    InsideObjectStart,
    InsideObjectKey,
    InsideObjectAfterKey,
    InsideObjectBeforeValue,
    InsideObjectAfterValue,
    InsideObjectAfterComma,
    InsideArrayStart,
    InsideArrayAfterValue,
    InsideArrayAfterComma,
}

/// The scanner's state. Byte offsets replace the original's UTF-16 indices;
/// `valid_end` is one past the last character known to be valid.
struct Scanner<'text> {
    input: &'text str,
    stack: Vec<State>,
    valid_end: usize,
    literal_start: usize,
    unicode_escape_digits: u8,
}

const LITERALS: [&str; 3] = ["true", "false", "null"];

impl Scanner<'_> {
    fn top(&self) -> Option<State> {
        self.stack.last().copied()
    }

    fn swap(&mut self, state: State) {
        self.stack.pop();
        self.stack.push(state);
    }

    fn value_start(&mut self, character: char, end: usize, at: usize, swap_state: State) {
        let inner = match character {
            '"' => {
                self.valid_end = end;
                State::InsideString
            }
            'f' | 't' | 'n' => {
                self.valid_end = end;
                self.literal_start = at;
                State::InsideLiteral
            }
            '-' => State::InsideNumber,
            '0'..='9' => {
                self.valid_end = end;
                State::InsideNumber
            }
            '{' => {
                self.valid_end = end;
                State::InsideObjectStart
            }
            '[' => {
                self.valid_end = end;
                State::InsideArrayStart
            }
            _ => return,
        };
        self.swap(swap_state);
        self.stack.push(inner);
    }

    fn after_object_value(&mut self, character: char, end: usize) {
        match character {
            ',' => self.swap(State::InsideObjectAfterComma),
            '}' => {
                self.valid_end = end;
                self.stack.pop();
            }
            _ => {}
        }
    }

    fn after_array_value(&mut self, character: char, end: usize) {
        match character {
            ',' => self.swap(State::InsideArrayAfterComma),
            ']' => {
                self.valid_end = end;
                self.stack.pop();
            }
            _ => {}
        }
    }

    fn partial_literal(&self, end: usize) -> &str {
        self.input.get(self.literal_start..end).unwrap_or_default()
    }

    #[expect(
        clippy::too_many_lines,
        reason = "one arm per scanner state, like the original's switch"
    )]
    fn step(&mut self, at: usize, character: char) {
        let end = at.saturating_add(character.len_utf8());
        let Some(state) = self.top() else {
            return;
        };
        match state {
            State::Root => self.value_start(character, end, at, State::Finish),
            State::Finish => {}
            State::InsideObjectStart => match character {
                '"' => self.swap(State::InsideObjectKey),
                '}' => {
                    self.valid_end = end;
                    self.stack.pop();
                }
                _ => {}
            },
            State::InsideObjectAfterComma => {
                if character == '"' {
                    self.swap(State::InsideObjectKey);
                }
            }
            State::InsideObjectKey => {
                if character == '"' {
                    self.swap(State::InsideObjectAfterKey);
                }
            }
            State::InsideObjectAfterKey => {
                if character == ':' {
                    self.swap(State::InsideObjectBeforeValue);
                }
            }
            State::InsideObjectBeforeValue => {
                self.value_start(character, end, at, State::InsideObjectAfterValue);
            }
            State::InsideObjectAfterValue => self.after_object_value(character, end),
            State::InsideString => match character {
                '"' => {
                    self.stack.pop();
                    self.valid_end = end;
                }
                '\\' => self.stack.push(State::InsideStringEscape),
                _ => self.valid_end = end,
            },
            State::InsideArrayStart => {
                if character == ']' {
                    self.valid_end = end;
                    self.stack.pop();
                } else {
                    self.valid_end = end;
                    self.value_start(character, end, at, State::InsideArrayAfterValue);
                }
            }
            State::InsideArrayAfterValue => match character {
                ',' => self.swap(State::InsideArrayAfterComma),
                ']' => {
                    self.valid_end = end;
                    self.stack.pop();
                }
                _ => self.valid_end = end,
            },
            State::InsideArrayAfterComma => {
                self.value_start(character, end, at, State::InsideArrayAfterValue);
            }
            State::InsideStringEscape => {
                self.stack.pop();
                if character == 'u' {
                    self.unicode_escape_digits = 0;
                    self.stack.push(State::InsideStringUnicodeEscape);
                } else {
                    self.valid_end = end;
                }
            }
            State::InsideStringUnicodeEscape => {
                if character.is_ascii_hexdigit() {
                    self.unicode_escape_digits = self.unicode_escape_digits.saturating_add(1);
                    if self.unicode_escape_digits == 4 {
                        self.stack.pop();
                        self.valid_end = end;
                    }
                }
            }
            State::InsideNumber => match character {
                '0'..='9' => self.valid_end = end,
                'e' | 'E' | '-' | '.' => {}
                ',' => {
                    self.stack.pop();
                    if self.top() == Some(State::InsideArrayAfterValue) {
                        self.after_array_value(character, end);
                    }
                    if self.top() == Some(State::InsideObjectAfterValue) {
                        self.after_object_value(character, end);
                    }
                }
                '}' => {
                    self.stack.pop();
                    if self.top() == Some(State::InsideObjectAfterValue) {
                        self.after_object_value(character, end);
                    }
                }
                ']' => {
                    self.stack.pop();
                    if self.top() == Some(State::InsideArrayAfterValue) {
                        self.after_array_value(character, end);
                    }
                }
                _ => {
                    self.stack.pop();
                }
            },
            State::InsideLiteral => {
                let partial = self.partial_literal(end);
                if LITERALS.iter().any(|literal| literal.starts_with(partial)) {
                    self.valid_end = end;
                } else {
                    self.stack.pop();
                    if self.top() == Some(State::InsideObjectAfterValue) {
                        self.after_object_value(character, end);
                    } else if self.top() == Some(State::InsideArrayAfterValue) {
                        self.after_array_value(character, end);
                    }
                }
            }
        }
    }

    fn finish(self) -> String {
        let mut result = self
            .input
            .get(..self.valid_end)
            .unwrap_or_default()
            .to_owned();
        for state in self.stack.iter().rev() {
            match state {
                State::InsideString => result.push('"'),
                State::InsideObjectKey
                | State::InsideObjectAfterKey
                | State::InsideObjectAfterComma
                | State::InsideObjectStart
                | State::InsideObjectBeforeValue
                | State::InsideObjectAfterValue => result.push('}'),
                State::InsideArrayStart
                | State::InsideArrayAfterComma
                | State::InsideArrayAfterValue => result.push(']'),
                State::InsideLiteral => {
                    let partial = self.input.get(self.literal_start..).unwrap_or_default();
                    if let Some(rest) = LITERALS
                        .iter()
                        .find_map(|literal| literal.strip_prefix(partial))
                    {
                        result.push_str(rest);
                    }
                }
                State::Root
                | State::Finish
                | State::InsideStringEscape
                | State::InsideStringUnicodeEscape
                | State::InsideNumber => {}
            }
        }
        result
    }
}

/// Closes a JSON prefix: `{"a": [1, "b` becomes `{"a": [1, "b"]}`.
#[must_use]
pub fn fix_json(input: &str) -> String {
    let mut scanner = Scanner {
        input,
        stack: vec![State::Root],
        valid_end: 0,
        literal_start: 0,
        unicode_escape_digits: 0,
    };
    for (at, character) in input.char_indices() {
        scanner.step(at, character);
    }
    scanner.finish()
}

/// The value a JSON prefix is heading towards: the text itself if it parses,
/// else its [`fix_json`] repair, else nothing.
#[must_use]
pub fn parse_partial_json(text: &str) -> Option<Value> {
    serde_json::from_str(text)
        .or_else(|_whole| serde_json::from_str(&fix_json(text)))
        .ok()
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use serde_json::json;

    use super::*;

    /// Every prefix of a few documents, repaired by the AI SDK's own
    /// `fixJson` (see `tests/fixtures/fix-json.ts`).
    #[test]
    fn closes_prefixes_exactly_like_the_ai_sdk() {
        let cases: Vec<(String, String)> =
            serde_json::from_str(include_str!("../tests/fixtures/fix-json.json"))
                .expect("the fixJson oracle");
        assert!(cases.len() > 100, "the oracle has cases");
        for (input, fixed) in cases {
            assert_eq!(fix_json(&input), fixed, "{input:?}");
        }
    }

    #[test]
    fn parses_partial_tool_input() {
        assert_eq!(
            parse_partial_json("{\"path\":\"src/li"),
            Some(json!({"path":"src/li"}))
        );
        assert_eq!(parse_partial_json(""), None);
        assert_eq!(parse_partial_json("{\"a\":\"b\"}"), Some(json!({"a":"b"})));
    }

    proptest! {
        #[test]
        fn repairing_never_panics_and_keeps_complete_json(text in "\\PC{0,64}") {
            let fixed = fix_json(&text);
            if serde_json::from_str::<Value>(&text).is_ok() && text.trim() == text {
                prop_assert_eq!(fixed, text);
            }
        }
    }
}
