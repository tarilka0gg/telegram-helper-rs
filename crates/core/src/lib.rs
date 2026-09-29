pub mod config;
pub mod crypto;
pub mod db;

use std::sync::atomic::AtomicBool;

/// Process-wide flags shared between the Telegram side and the web UI.
#[derive(Default)]
pub struct Status {
    pub userbot_connected: AtomicBool,
}
