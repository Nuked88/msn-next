//! Wire-level domain model. Serialization and cryptography deliberately live outside this crate.

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

pub const PROTOCOL_VERSION: u16 = 1;
pub const MAX_TEXT_BYTES: usize = 16_384;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    pub protocol_version: u16,
    pub conversation_id: [u8; 32],
    pub sender_device_id: [u8; 16],
    pub message_number: u64,
    pub previous_message_number: u64,
    pub event: ChatEvent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChatEvent {
    Text(TextMessage),
    Nudge(Nudge),
    EmoticonOffer(EmoticonOffer),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmoticonOffer {
    pub metadata: Emoticon,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextMessage {
    pub text: String,
    pub emoticons: Vec<EmoticonSpan>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmoticonSpan {
    /// UTF-8 byte offsets into `TextMessage::text`.
    pub start: u32,
    pub end: u32,
    pub asset_id: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Emoticon {
    /// BLAKE3 digest of the original asset bytes, calculated by the attachments crate.
    pub asset_id: [u8; 32],
    pub mime: Mime,
    pub width: u16,
    pub height: u16,
    pub animated: bool,
    pub suggested_triggers: Vec<String>,
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Mime {
    Png,
    Jpeg,
    Gif,
    Webp,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Nudge {
    pub id: [u8; 16],
    pub intensity: u8,
    pub timestamp_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trigger {
    pub text: String,
    pub asset_id: [u8; 32],
    pub case_sensitive: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TriggerError {
    TextTooLong,
    Empty,
    TooLong,
    NonAscii,
    ContainsWhitespace,
    Duplicate(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextError {
    TooLong,
    InvalidSpan,
}

pub fn validate_text_message(message: &TextMessage) -> Result<(), TextError> {
    if message.text.len() > MAX_TEXT_BYTES {
        return Err(TextError::TooLong);
    }
    let mut previous_end = 0;
    for span in &message.emoticons {
        let start = span.start as usize;
        let end = span.end as usize;
        if start < previous_end
            || start >= end
            || end > message.text.len()
            || !message.text.is_char_boundary(start)
            || !message.text.is_char_boundary(end)
        {
            return Err(TextError::InvalidSpan);
        }
        previous_end = end;
    }
    Ok(())
}

/// Resolves local shortcuts into stable asset references before a message is encrypted.
pub fn resolve_emoticons(
    text: &str,
    triggers: &[Trigger],
) -> Result<Vec<EmoticonSpan>, TriggerError> {
    if text.len() > MAX_TEXT_BYTES {
        return Err(TriggerError::TextTooLong);
    }
    validate_triggers(triggers)?;
    let mut ordered: Vec<&Trigger> = triggers.iter().collect();
    ordered.sort_by_key(|trigger| std::cmp::Reverse(trigger.text.len()));

    let mut spans = Vec::new();
    let mut cursor = 0;
    let mut in_code = false;
    while cursor < text.len() {
        let rest = &text[cursor..];
        let current = rest.chars().next().expect("cursor is inside text");
        if current == '`' {
            in_code = !in_code;
            cursor += current.len_utf8();
            continue;
        }
        if current == '\\' {
            cursor += current.len_utf8();
            if cursor < text.len() {
                cursor += text[cursor..]
                    .chars()
                    .next()
                    .expect("character after escape")
                    .len_utf8();
            }
            continue;
        }
        if !in_code && !inside_url(text, cursor) {
            if let Some(trigger) = ordered
                .iter()
                .find(|trigger| matches_trigger(rest, trigger))
            {
                let end = cursor + trigger.text.len();
                spans.push(EmoticonSpan {
                    start: cursor as u32,
                    end: end as u32,
                    asset_id: trigger.asset_id,
                });
                cursor = end;
                continue;
            }
        }
        cursor += current.len_utf8();
    }
    Ok(spans)
}

pub fn validate_triggers(triggers: &[Trigger]) -> Result<(), TriggerError> {
    for (index, trigger) in triggers.iter().enumerate() {
        if trigger.text.is_empty() {
            return Err(TriggerError::Empty);
        }
        if trigger.text.len() > 32 {
            return Err(TriggerError::TooLong);
        }
        if !trigger.text.is_ascii() {
            return Err(TriggerError::NonAscii);
        }
        if trigger.text.chars().any(char::is_whitespace) {
            return Err(TriggerError::ContainsWhitespace);
        }
        if triggers[..index].iter().any(|other| {
            if trigger.case_sensitive || other.case_sensitive {
                other.text == trigger.text
            } else {
                other.text.eq_ignore_ascii_case(&trigger.text)
            }
        }) {
            return Err(TriggerError::Duplicate(trigger.text.clone()));
        }
    }
    Ok(())
}

fn matches_trigger(input: &str, trigger: &Trigger) -> bool {
    input.get(..trigger.text.len()).is_some_and(|candidate| {
        if trigger.case_sensitive {
            candidate == trigger.text
        } else {
            candidate.eq_ignore_ascii_case(&trigger.text)
        }
    })
}

fn inside_url(text: &str, cursor: usize) -> bool {
    let token_start = text[..cursor]
        .rfind(char::is_whitespace)
        .map_or(0, |index| index + 1);
    let token = &text[token_start..cursor];
    token.starts_with("http://") || token.starts_with("https://")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NudgeRateLimit {
    accepted_ms: VecDeque<u64>,
}

impl Default for NudgeRateLimit {
    fn default() -> Self {
        Self {
            accepted_ms: VecDeque::with_capacity(5),
        }
    }
}

impl NudgeRateLimit {
    /// At most one nudge every 5 seconds and five per rolling minute, per contact.
    pub fn try_acquire(&mut self, now_ms: u64) -> Result<(), u64> {
        while self
            .accepted_ms
            .front()
            .is_some_and(|time| now_ms.saturating_sub(*time) >= 60_000)
        {
            self.accepted_ms.pop_front();
        }
        if let Some(last) = self.accepted_ms.back() {
            let elapsed = now_ms.saturating_sub(*last);
            if elapsed < 5_000 {
                return Err(5_000 - elapsed);
            }
        }
        if self.accepted_ms.len() == 5 {
            return Err(
                60_000 - now_ms.saturating_sub(*self.accepted_ms.front().expect("five entries"))
            );
        }
        self.accepted_ms.push_back(now_ms);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trigger(text: &str, id: u8) -> Trigger {
        Trigger {
            text: text.into(),
            asset_id: [id; 32],
            case_sensitive: true,
        }
    }

    #[test]
    fn emoticons_resolve_expected_spans() {
        let text = "ciao :-) :) \\:) `:)` https://x.test/:)";
        let spans = resolve_emoticons(
            text,
            &[trigger(":)", 1), trigger(":-)", 2), trigger(":-)", 2)],
        );
        assert!(
            spans.is_err(),
            "duplicate triggers must be rejected explicitly"
        );

        let spans = resolve_emoticons(text, &[trigger(":)", 1), trigger(":-)", 2)]).unwrap();
        assert_eq!(spans.len(), 2);
        assert_eq!(&text[spans[0].start as usize..spans[0].end as usize], ":-)");
        assert_eq!(&text[spans[1].start as usize..spans[1].end as usize], ":)");
        assert_eq!(spans[0].asset_id, [2; 32]);
        assert_eq!(
            validate_text_message(&TextMessage {
                text: text.into(),
                emoticons: spans
            }),
            Ok(())
        );
    }

    #[test]
    fn rejects_invalid_emoticon_spans() {
        let message = TextMessage {
            text: "è".into(),
            emoticons: vec![EmoticonSpan {
                start: 1,
                end: 2,
                asset_id: [1; 32],
            }],
        };
        assert_eq!(validate_text_message(&message), Err(TextError::InvalidSpan));
    }

    #[test]
    fn nudges_are_rate_limited() {
        let mut limit = NudgeRateLimit::default();
        for second in [0, 5, 10, 15, 20] {
            assert_eq!(limit.try_acquire(second * 1_000), Ok(()));
        }
        assert_eq!(limit.try_acquire(25_000), Err(35_000));
        assert_eq!(limit.try_acquire(60_000), Ok(()));
        assert_eq!(limit.try_acquire(61_000), Err(4_000));
    }
}
