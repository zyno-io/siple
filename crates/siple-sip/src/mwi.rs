//! Message waiting indication: `application/simple-message-summary` (RFC 3842)
//! parser and the per-account status it feeds.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::Serialize;

/// New / old counts of one message class (`voice`, `fax`, ...), urgent ones included separately.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct MessageCounts {
    /// New messages (urgent ones are counted in `urgent_new` *and* here, per RFC 3842).
    pub new: u32,
    /// Old (read) messages.
    pub old: u32,
    /// Urgent new messages.
    pub urgent_new: u32,
    /// Urgent old messages.
    pub urgent_old: u32,
}

/// Message-waiting state of one account.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MwiStatus {
    /// Account id.
    pub account: String,
    /// `Messages-Waiting: yes` (or, when the header is missing, any new message).
    pub waiting: bool,
    /// `Message-Account` URI, if the notifier sent one.
    pub account_uri: Option<String>,
    /// `Voice-Message` counts.
    pub voice: MessageCounts,
    /// Other classes (`fax`, `pager`, `multimedia`, `text`, `none`) by lower-case name.
    pub other: BTreeMap<String, MessageCounts>,
    /// When the last NOTIFY was applied.
    pub updated_at: DateTime<Utc>,
}

impl MwiStatus {
    /// Same content, ignoring the timestamp.
    pub fn same_content(&self, o: &MwiStatus) -> bool {
        self.account == o.account
            && self.waiting == o.waiting
            && self.account_uri == o.account_uri
            && self.voice == o.voice
            && self.other == o.other
    }

    /// Compact form for [`crate::AccountStatus`].
    pub fn summary(&self) -> MwiSummary {
        MwiSummary {
            waiting: self.waiting,
            new: self.voice.new,
            old: self.voice.old,
        }
    }
}

/// Convenience view of the voice mailbox in the account status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct MwiSummary {
    /// Messages waiting.
    pub waiting: bool,
    /// New voice messages.
    pub new: u32,
    /// Old voice messages.
    pub old: u32,
}

/// The fields of a parsed body (no account / timestamp).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ParsedSummary {
    /// `Messages-Waiting`, if present.
    pub waiting: Option<bool>,
    /// `Message-Account`.
    pub account_uri: Option<String>,
    /// `Voice-Message`.
    pub voice: MessageCounts,
    /// Other classes.
    pub other: BTreeMap<String, MessageCounts>,
}

/// Parses a `simple-message-summary` body. Tolerant of case, whitespace, LF/CRLF, unknown lines
/// and malformed counts (those are skipped).
pub fn parse_summary(body: &str) -> ParsedSummary {
    let mut out = ParsedSummary::default();
    for line in body.lines() {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim();
        match name.as_str() {
            "messages-waiting" => {
                out.waiting = match value.to_ascii_lowercase().as_str() {
                    "yes" => Some(true),
                    "no" => Some(false),
                    _ => out.waiting,
                }
            }
            "message-account" => {
                if !value.is_empty() {
                    out.account_uri = Some(value.to_string());
                }
            }
            n => {
                let Some(class) = n.strip_suffix("-message") else {
                    continue;
                };
                let Some(counts) = parse_counts(value) else {
                    continue;
                };
                if class == "voice" {
                    out.voice = counts;
                } else if !class.is_empty() {
                    out.other.insert(class.to_string(), counts);
                }
            }
        }
    }
    out
}

/// `new/old` optionally followed by `(urgent_new/urgent_old)`.
fn parse_counts(v: &str) -> Option<MessageCounts> {
    let (main, urgent) = match v.split_once('(') {
        Some((m, u)) => (m, Some(u.trim().trim_end_matches(')'))),
        None => (v, None),
    };
    let (new, old) = pair(main)?;
    let (urgent_new, urgent_old) = urgent.and_then(pair).unwrap_or((0, 0));
    Some(MessageCounts {
        new,
        old,
        urgent_new,
        urgent_old,
    })
}

fn pair(s: &str) -> Option<(u32, u32)> {
    let (a, b) = s.split_once('/')?;
    Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
}

/// Builds the status for `account` from a body.
pub fn status_from_body(account: &str, body: &str, now: DateTime<Utc>) -> MwiStatus {
    let p = parse_summary(body);
    let any_new = p.voice.new > 0 || p.other.values().any(|c| c.new > 0);
    MwiStatus {
        account: account.to_string(),
        waiting: p.waiting.unwrap_or(any_new),
        account_uri: p.account_uri,
        voice: p.voice,
        other: p.other,
        updated_at: now,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn freeswitch_body() {
        let b = "Messages-Waiting: yes\r\nMessage-Account: sip:1000@pbx.example.test\r\nVoice-Message: 1/0\r\n";
        let s = status_from_body("a", b, Utc::now());
        assert!(s.waiting);
        assert_eq!(s.account_uri.as_deref(), Some("sip:1000@pbx.example.test"));
        assert_eq!(
            s.voice,
            MessageCounts {
                new: 1,
                old: 0,
                urgent_new: 0,
                urgent_old: 0
            }
        );
        assert_eq!(
            s.summary(),
            MwiSummary {
                waiting: true,
                new: 1,
                old: 0
            }
        );
    }

    #[test]
    fn no_messages() {
        let s = status_from_body(
            "a",
            "Messages-Waiting: no\r\nVoice-Message: 0/0 (0/0)\r\n",
            Utc::now(),
        );
        assert!(!s.waiting);
        assert_eq!(s.voice, MessageCounts::default());
    }

    #[test]
    fn urgent_and_other_classes_tolerant() {
        let b = "messages-waiting:   YES  \nMESSAGE-ACCOUNT:sip:x@y\nVoice-Message : 2 / 8 ( 1 / 3 )\nFax-Message: 1/0\nPager-Message: 0/4 (0/1)\nGarbage line\nText-Message: x/y\n";
        let p = parse_summary(b);
        assert_eq!(p.waiting, Some(true));
        assert_eq!(p.account_uri.as_deref(), Some("sip:x@y"));
        assert_eq!(
            p.voice,
            MessageCounts {
                new: 2,
                old: 8,
                urgent_new: 1,
                urgent_old: 3
            }
        );
        assert_eq!(p.other["fax"].new, 1);
        assert_eq!(p.other["pager"].urgent_old, 1);
        assert!(!p.other.contains_key("text"), "malformed counts skipped");
    }

    #[test]
    fn waiting_inferred_without_header() {
        let s = status_from_body("a", "Voice-Message: 3/1\n", Utc::now());
        assert!(s.waiting);
        let s = status_from_body("a", "Voice-Message: 0/1\n", Utc::now());
        assert!(!s.waiting);
        let s = status_from_body("a", "", Utc::now());
        assert!(!s.waiting);
    }

    #[test]
    fn same_content_ignores_time() {
        let a = status_from_body("a", "Messages-Waiting: no\n", Utc::now());
        let mut b = a.clone();
        b.updated_at += chrono::Duration::seconds(5);
        assert!(a.same_content(&b));
        b.waiting = true;
        assert!(!a.same_content(&b));
    }
}
