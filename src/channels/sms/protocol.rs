use serde::Deserialize;

use crate::protocol::content::{ChatContent, Operation, OutboundContent};
use crate::protocol::entities::ConnectorKind;
use crate::protocol::message::MessageKind;
use crate::router::InboundEvent;

use crate::channels::OutboundDelivery;

pub const CHANNEL_TYPE: &str = ConnectorKind::Sms.as_str();

/// One received SMS as sim-server reports it. `seq` is the persistent monotonic
/// poll cursor; `index` is the SIM slot (reused — never cursor on it).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SmsMessage {
    #[serde(rename = "Index", default)]
    pub index: String,
    #[serde(rename = "Seq", default)]
    pub seq: Option<i64>,
    #[serde(rename = "Smstat", default)]
    pub status: String,
    #[serde(rename = "Phone", default)]
    pub phone: String,
    #[serde(rename = "Content", default)]
    pub content: String,
    #[serde(rename = "Date", default)]
    pub date: String,
}

/// Maps a received SMS onto the router's inbound shape: the peer phone number
/// is the conversation. Blank content (carrier artifacts) and blank senders are
/// skipped — there is nothing to route.
pub fn to_inbound_event(message: &SmsMessage) -> Option<InboundEvent> {
    let phone = message.phone.trim();
    if phone.is_empty() || message.content.trim().is_empty() {
        return None;
    }
    let chat = ChatContent {
        sender: phone.to_owned(),
        sender_id: None,
        text: message.content.clone(),
        attachments: Vec::new(),
        is_from_me: false,
        quoted: None,
    };
    Some(InboundEvent {
        channel_type: CHANNEL_TYPE.to_owned(),
        platform_id: phone.to_owned(),
        thread_id: None,
        kind: MessageKind::Chat,
        content: serde_json::to_string(&chat).ok()?,
        is_mention: false,
        is_group: false,
    })
}

/// The next poll position after seeing `messages`: never moves backwards, never
/// moves past a message without a `Seq` (those are routed but cannot be cursored).
#[must_use]
pub fn advance_cursor(cursor: i64, messages: &[SmsMessage]) -> i64 {
    messages
        .iter()
        .filter_map(|message| message.seq)
        .fold(cursor, i64::max)
}

/// Flattens an outbound message to the single plain-text string SMS can carry.
/// Interactive operations degrade: a question rides as its text (the answer
/// comes back as a plain inbound), an approval points at the web admin (an SMS
/// sender is spoofable — not an authorization channel), attachments become a
/// count note (no MMS).
#[must_use]
pub fn render_sms(delivery: &OutboundDelivery) -> String {
    let text = render_operation(&delivery.content);
    let note = attachment_note(delivery.files.len());
    let combined = match (text.is_empty(), note) {
        (_, None) => text,
        (true, Some(note)) => note,
        (false, Some(note)) => format!("{text}\n{note}"),
    };
    to_gsm7_safe(&combined)
}

/// Folds outbound text to a 7-bit-ASCII subset SMS can always carry. The SIM
/// backend forces UCS2 for any non-ASCII byte, and this firmware rejects the
/// `AT+CSMP=...,8` that text-mode UCS2 needs (CMS ERROR 305) — so a single
/// accent or emoji would make the whole reply undeliverable. Accents
/// transliterate (café → cafe), common typographic punctuation maps to ASCII,
/// and anything else (emoji, CJK) is dropped. Plain ASCII passes untouched.
#[must_use]
pub fn to_gsm7_safe(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        if ch.is_ascii() {
            out.push(ch);
        } else if let Some(replacement) = transliterate(ch) {
            out.push_str(replacement);
        }
        // else: drop (emoji, symbols with no ASCII equivalent)
    }
    out
}

fn transliterate(ch: char) -> Option<&'static str> {
    let s = match ch {
        'à' | 'á' | 'â' | 'ä' | 'ã' | 'å' => "a",
        'À' | 'Á' | 'Â' | 'Ä' | 'Ã' | 'Å' => "A",
        'ç' => "c",
        'Ç' => "C",
        'è' | 'é' | 'ê' | 'ë' => "e",
        'È' | 'É' | 'Ê' | 'Ë' => "E",
        'ì' | 'í' | 'î' | 'ï' => "i",
        'Ì' | 'Í' | 'Î' | 'Ï' => "I",
        'ñ' => "n",
        'Ñ' => "N",
        'ò' | 'ó' | 'ô' | 'ö' | 'õ' => "o",
        'Ò' | 'Ó' | 'Ô' | 'Ö' | 'Õ' => "O",
        'ù' | 'ú' | 'û' | 'ü' => "u",
        'Ù' | 'Ú' | 'Û' | 'Ü' => "U",
        'ý' | 'ÿ' => "y",
        'œ' => "oe",
        'Œ' => "OE",
        'æ' => "ae",
        'Æ' => "AE",
        'ß' => "ss",
        '\u{2019}' | '\u{2018}' | '\u{201A}' | '`' => "'",
        '\u{201C}' | '\u{201D}' | '\u{201E}' | '«' | '»' => "\"",
        '\u{2013}' | '\u{2014}' | '\u{2011}' | '\u{2012}' => "-",
        '…' => "...",
        '•' | '·' => "-",
        '\u{00A0}' | '\u{202F}' | '\u{2009}' => " ",
        '€' => "EUR",
        '£' => "GBP",
        '°' => " deg",
        '×' => "x",
        '✓' | '✔' => "[ok]",
        '→' => "->",
        _ => return None,
    };
    Some(s)
}

fn render_operation(content: &OutboundContent) -> String {
    let text = content.text.clone().unwrap_or_default();
    match &content.operation {
        None => text,
        Some(Operation::AskQuestion {
            question, options, ..
        }) => {
            if text.is_empty() {
                format!("{question} ({})", options.join(" / "))
            } else {
                text
            }
        }
        Some(Operation::Approval { summary, .. }) => {
            format!("[approval needed — use the web admin] {summary}")
        }
        Some(Operation::Edit { text: edited, .. }) => edited.clone(),
        Some(Operation::Reaction { emoji, .. }) => emoji.clone(),
    }
}

fn attachment_note(count: usize) -> Option<String> {
    match count {
        0 => None,
        1 => Some("[1 attachment not deliverable over SMS]".to_owned()),
        n => Some(format!("[{n} attachments not deliverable over SMS]")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::channels::OutboundFile;

    fn sms(seq: Option<i64>, phone: &str, content: &str) -> SmsMessage {
        SmsMessage {
            index: "7".to_owned(),
            seq,
            status: "0".to_owned(),
            phone: phone.to_owned(),
            content: content.to_owned(),
            date: "2026-06-12 17:25:25".to_owned(),
        }
    }

    #[test]
    fn payload_parses_with_sim_server_field_names() {
        let json = r#"{"Index":"47","Seq":48,"Smstat":"0","Phone":"+33612345678",
                       "Content":"hello","Date":"2026-06-12 17:25:25"}"#;
        let message: SmsMessage = serde_json::from_str(json).expect("parse");
        assert_eq!(message.seq, Some(48));
        assert_eq!(message.phone, "+33612345678");
        assert_eq!(message.content, "hello");
    }

    #[test]
    fn payload_tolerates_missing_fields() {
        let message: SmsMessage = serde_json::from_str("{}").expect("parse");
        assert_eq!(message.seq, None);
        assert_eq!(message.phone, "");
    }

    #[test]
    fn to_inbound_event_maps_the_phone_to_the_conversation() {
        let event = to_inbound_event(&sms(Some(48), "+33612345678", "hello")).expect("routable");
        assert_eq!(event.channel_type, "sms");
        assert_eq!(event.platform_id, "+33612345678");
        assert_eq!(event.thread_id, None);
        assert_eq!(event.kind, MessageKind::Chat);
        assert!(!event.is_group);
        assert!(!event.is_mention);
        let chat: ChatContent = serde_json::from_str(&event.content).expect("chat json");
        assert_eq!(chat.sender, "+33612345678");
        assert_eq!(chat.text, "hello");
        assert!(!chat.is_from_me);
    }

    #[test]
    fn blank_content_or_sender_is_skipped() {
        assert_eq!(to_inbound_event(&sms(Some(1), "+336", "   ")), None);
        assert_eq!(to_inbound_event(&sms(Some(1), "  ", "hello")), None);
    }

    #[test]
    fn read_and_unread_both_route() {
        for status in ["0", "1"] {
            let mut message = sms(Some(1), "+336", "hello");
            message.status = status.to_owned();
            assert!(
                to_inbound_event(&message).is_some(),
                "Smstat {status} must route — the cursor is the dedup, not read state"
            );
        }
    }

    #[test]
    fn advance_cursor_is_monotonic_and_ignores_missing_seq() {
        let cases: &[(i64, Vec<Option<i64>>, i64)] = &[
            (0, vec![], 0),
            (48, vec![Some(49), Some(50)], 50),
            (48, vec![None], 48),
            (48, vec![Some(50), None, Some(49)], 50),
            (48, vec![Some(3)], 48), // never backwards
        ];
        for (start, seqs, expected) in cases {
            let messages: Vec<SmsMessage> = seqs.iter().map(|seq| sms(*seq, "+336", "x")).collect();
            assert_eq!(
                advance_cursor(*start, &messages),
                *expected,
                "start={start} seqs={seqs:?}"
            );
        }
    }

    fn delivery(content: OutboundContent, file_count: usize) -> OutboundDelivery {
        OutboundDelivery {
            kind: "chat".to_owned(),
            content,
            files: (0..file_count)
                .map(|n| OutboundFile {
                    name: format!("file-{n}.png"),
                    path: std::path::PathBuf::from(format!("/tmp/file-{n}.png")),
                })
                .collect(),
        }
    }

    #[test]
    fn gsm7_safe_transliterates_accents_and_drops_emoji() {
        let cases: &[(&str, &str)] = &[
            ("plain ascii", "plain ascii"),
            ("café déjà vu", "cafe deja vu"),
            ("Ça coûte 5€", "Ca coute 5EUR"),
            (
                "\u{201C}quote\u{201D} \u{2014} dash\u{2026}",
                "\"quote\" - dash...",
            ),
            ("done \u{2705} ok \u{2713}", "done  ok [ok]"),
            ("emoji \u{1F600}\u{1F44D} gone", "emoji  gone"),
            ("Œuvre n°1", "OEuvre n deg1"),
        ];
        for (input, expected) in cases {
            let got = to_gsm7_safe(input);
            assert!(got.is_ascii(), "output must be pure ASCII: {got:?}");
            assert_eq!(&got, expected, "input={input:?}");
        }
    }

    #[test]
    fn render_sms_output_is_always_ascii() {
        let d = OutboundDelivery {
            kind: "chat".to_owned(),
            content: OutboundContent::from_text("Réponse: déployé ✅ à 22°C"),
            files: Vec::new(),
        };
        let out = render_sms(&d);
        assert!(out.is_ascii(), "render_sms must yield ASCII: {out:?}");
        assert_eq!(out, "Reponse: deploye  a 22 degC");
    }

    #[test]
    fn render_sms_covers_every_outbound_shape() {
        let question_with_text = OutboundContent {
            text: Some("Deploy now? (ship / wait)".to_owned()),
            operation: Some(Operation::AskQuestion {
                question_id: "q1".to_owned(),
                title: "Deploy".to_owned(),
                question: "Deploy now?".to_owned(),
                options: vec!["ship".to_owned(), "wait".to_owned()],
            }),
            ..OutboundContent::from_text("")
        };
        let bare_question = OutboundContent {
            text: None,
            ..question_with_text.clone()
        };
        let approval = OutboundContent {
            operation: Some(Operation::Approval {
                approval_id: "a1".to_owned(),
                command: "groups-create".to_owned(),
                args: serde_json::Map::new(),
                summary: "create agent Coder".to_owned(),
            }),
            ..OutboundContent::from_text("")
        };
        let cases: &[(&str, OutboundContent, usize, &str)] = &[
            ("chat", OutboundContent::from_text("hello"), 0, "hello"),
            (
                "question keeps its text rendering",
                question_with_text,
                0,
                "Deploy now? (ship / wait)",
            ),
            (
                "bare question is composed",
                bare_question,
                0,
                "Deploy now? (ship / wait)",
            ),
            (
                "approval points at the web admin",
                approval,
                0,
                "[approval needed - use the web admin] create agent Coder",
            ),
            (
                "files become a note",
                OutboundContent::from_text("chart attached"),
                1,
                "chart attached\n[1 attachment not deliverable over SMS]",
            ),
            (
                "files-only message is just the note",
                OutboundContent::from_text(""),
                2,
                "[2 attachments not deliverable over SMS]",
            ),
        ];
        for (name, content, files, expected) in cases {
            assert_eq!(
                &render_sms(&delivery(content.clone(), *files)),
                expected,
                "{name}"
            );
        }
    }
}
