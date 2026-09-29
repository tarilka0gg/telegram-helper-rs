//! Typed form of the JSON action the LLM router returns for a free-text phrase.

use rusqlite::types::Value as Sql;
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "intent", rename_all = "snake_case")]
pub enum Intent {
    SendMessage {
        recipient: String,
        text: String,
    },
    SummarizeChat {
        contact: String,
    },
    TasksForChat {
        contact: String,
    },
    DraftReply {
        contact: String,
        #[serde(default)]
        instruction: Option<String>,
    },
    Catchup {
        contact: String,
    },
    Search {
        query: String,
    },
    NewsDigest {
        topic: String,
        #[serde(default)]
        hours: Option<i64>,
    },
    ListTodos,
    SetSetting {
        key: String,
        value: Value,
    },
    FindInChats {
        query: String,
        #[serde(default)]
        action: Option<String>,
    },
    AddNewsTopic {
        topic: String,
        #[serde(default)]
        hours: Option<i64>,
    },
    RemoveNewsTopic {
        topic: String,
    },
    AddReminder {
        text: String,
        #[serde(default)]
        when: Option<String>,
        #[serde(default)]
        peer_query: Option<String>,
    },
    RemoveReminder {
        query: String,
    },
    AddRemindersFromChat {
        contact: String,
    },
    Chat {
        reply: String,
    },
    Multi {
        actions: Vec<Intent>,
    },
    #[serde(other)]
    Unknown,
}

pub fn parse_intent(raw: &str) -> Intent {
    let mut s = raw.trim();
    if let Some(rest) = s.strip_prefix("```") {
        s = rest.trim_start();
        s = s.strip_prefix("json").unwrap_or(s).trim_start();
        s = s.strip_suffix("```").unwrap_or(s).trim_end();
    }
    serde_json::from_str(s).unwrap_or(Intent::Unknown)
}

/// Actions to execute: a `Multi` expands (recursively, at most 5 in total); anything else is itself.
pub fn flatten(intent: Intent) -> Vec<Intent> {
    fn inner(intent: Intent, room: usize) -> Vec<Intent> {
        match intent {
            Intent::Multi { actions } => {
                let mut out = Vec::new();
                for a in actions {
                    if out.len() >= room {
                        break;
                    }
                    out.extend(inner(a, room - out.len()));
                }
                out
            }
            other => vec![other],
        }
    }
    inner(intent, 5)
}

const SETTING_KEYS: &[&str] = &[
    "auto_reply_enabled",
    "auto_reply_mode",
    "auto_reply_text",
    "auto_reply_cooldown_min",
    "digest_enabled",
    "digest_time",
    "news_enabled",
    "news_digest_time",
    "news_window_hours",
    "reminders_enabled",
    "reminder_lead_hours",
    "reminder_overdue_enabled",
    "ignore_archived",
    "use_heavy_model",
    "llm_provider",
    "timezone",
];

/// Whitelist of settings the agent may change; returns the canonical column name.
pub fn setting_column(key: &str) -> Option<&'static str> {
    SETTING_KEYS.iter().copied().find(|k| *k == key)
}

pub fn json_to_sql(v: &Value) -> Option<Sql> {
    match v {
        Value::Bool(b) => Some(Sql::Integer(*b as i64)),
        Value::Number(n) => n.as_i64().map(Sql::Integer),
        Value::String(s) => Some(Sql::Text(s.clone())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_and_fenced() {
        assert_eq!(
            parse_intent(r#"{"intent":"send_message","recipient":"Оля","text":"привіт"}"#),
            Intent::SendMessage { recipient: "Оля".into(), text: "привіт".into() }
        );
        assert_eq!(parse_intent("```json\n{\"intent\":\"list_todos\"}\n```"), Intent::ListTodos);
        assert_eq!(parse_intent("hello"), Intent::Unknown);
        assert_eq!(parse_intent(r#"{"intent":"nonsense"}"#), Intent::Unknown);
        assert!(matches!(parse_intent(r#"{"intent":"add_reminder","text":"x"}"#), Intent::AddReminder { when: None, .. }));
    }

    #[test]
    fn multi_flatten_and_cap() {
        let one = r#"{"intent":"set_setting","key":"digest_enabled","value":true}"#;
        let raw = format!(r#"{{"intent":"multi","actions":[{one},{one}]}}"#);
        assert_eq!(flatten(parse_intent(&raw)).len(), 2);
        let many = format!(r#"{{"intent":"multi","actions":[{}]}}"#, [one; 9].join(","));
        assert_eq!(flatten(parse_intent(&many)).len(), 5);
    }

    #[test]
    fn setting_whitelist_and_coercion() {
        assert_eq!(setting_column("timezone"), Some("timezone"));
        assert_eq!(setting_column("transcription_mode"), None);
        assert_eq!(json_to_sql(&Value::Bool(true)), Some(Sql::Integer(1)));
        assert_eq!(json_to_sql(&Value::String("07:00".into())), Some(Sql::Text("07:00".into())));
        assert_eq!(json_to_sql(&serde_json::json!([1])), None);
    }

    #[test]
    fn parser_survives_garbage_and_type_confusion() {
        let cases = [
            "",
            "{}",
            "[]",
            "null",
            "true",
            "\"intent\"",
            "{\"intent\":null}",
            "{\"intent\":5}",
            "{\"intent\":\"send_message\"}",
            "{\"intent\":\"send_message\",\"recipient\":1,\"text\":[]}",
            "{\"intent\":\"multi\",\"actions\":\"no\"}",
            "{\"intent\":\"multi\",\"actions\":[null,1,{}]}",
            "```",
            "```json",
            "```json\n```",
            "{\"intent\":\"chat\",\"reply\":\"\\ud800\"}",
            &format!("{{\"intent\":\"chat\",\"reply\":\"{}\"}}", "я".repeat(100_000)),
            &"[".repeat(5000),
            &"{\"a\":".repeat(5000),
            "{\"intent\":\"set_setting\",\"key\":\"digest_time\",\"value\":{\"nested\":[1,2]}}",
            "\u{feff}{\"intent\":\"list_todos\"}",
        ];
        for c in cases {
            let _ = flatten(parse_intent(c)); // must not panic or overflow the stack
        }
        // nested multi bombs are capped
        let mut deep = r#"{"intent":"list_todos"}"#.to_string();
        for _ in 0..100 {
            deep = format!(r#"{{"intent":"multi","actions":[{deep},{deep}]}}"#).chars().take(200_000).collect();
        }
        let _ = flatten(parse_intent(&deep));
        // json_to_sql never accepts structures or floats
        assert_eq!(json_to_sql(&Value::Null), None);
        assert_eq!(json_to_sql(&serde_json::json!(1.5)), None);
        assert_eq!(json_to_sql(&serde_json::json!({"a": 1})), None);
    }
}
