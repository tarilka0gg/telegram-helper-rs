pub mod config;
pub mod crypto;
pub mod db;
pub mod intent;
pub mod llm;

/// System prompt of the free-text intent router (ported verbatim from the Python original).
pub const AGENT_PROMPT: &str = include_str!("agent_prompt.txt");

use std::sync::atomic::AtomicBool;

/// Process-wide flags shared between the Telegram side and the web UI.
#[derive(Default)]
pub struct Status {
    pub userbot_connected: AtomicBool,
}
