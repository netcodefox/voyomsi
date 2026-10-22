//! Positional voice through VoySpeak (a TeamSpeak-style voice client,
//! <https://voyspeak.com>) and VoyOMSI's plugin for it (`tools/voyspeak-plugin`),
//! beside the built-in voice chat (`omsi_net::voice_net`): the session's side of it.
//!
//! The voice server is the host's to name: `voice_server_uid`, `voice_channel` and
//! `voice_channel_password` in a dedicated server's `server.cfg`, or `~/.voyomsi/voice.cfg`
//! for a game that hosts (the same keys; `voice_range` and `voice_radio` as well). A joining
//! game asks for it with the command `voyspeak?` and the host answers `voyspeak …` (`VoySpeakServer::command`).
//! The game's link to the plugin is the app's `voyspeak`.
//!
//! Players are told apart in the voice channel by their nicknames: `<name> #<id>` with the
//! session's player id, which every game of the session derives the same way.

use std::collections::HashMap;
use std::sync::OnceLock;

/// How far a player is heard (m) when the host does not say.
pub const DEFAULT_RANGE: f32 = 20.0;
/// TeamSpeak's limit on a nickname's length (characters).
const MAX_NICK: usize = 30;

/// The VoySpeak server a session uses, as its host names it.
#[derive(Debug, Clone, PartialEq)]
pub struct VoySpeakServer {
    /// The voice server's unique id (the one in its info panel). Never empty: the plugin
    /// moves nobody about on a voice server the session does not name.
    pub server_uid: String,
    /// The in-game channel: its id or its name.
    pub channel: String,
    pub password: String,
    /// How far a player is heard (m).
    pub range: f32,
    /// Map-wide bus radio (drivers hold the radio key); off when the host says so.
    pub radio: bool,
}

impl VoySpeakServer {
    /// From `key = value` pairs (keys lower case): `voice_server_uid`, `voice_channel`,
    /// `voice_channel_password`, `voice_range`, `voice_radio`. None without a channel,
    /// without the voice server's unique id, or when the answer to `voyspeak?` would not fit a
    /// command.
    pub fn from_kv(get: impl Fn(&str) -> Option<String>) -> Option<VoySpeakServer> {
        let channel = get("voice_channel").map(|c| c.trim().to_string()).filter(|c| !c.is_empty())?;
        let flag = |k: &str, d: bool| get(k).map(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes" | "on")).unwrap_or(d);
        let s = VoySpeakServer {
            server_uid: get("voice_server_uid").map(|v| v.trim().to_string()).unwrap_or_default(),
            channel,
            password: get("voice_channel_password").map(|v| v.trim().to_string()).unwrap_or_default(),
            range: get("voice_range").and_then(|v| v.trim().parse::<f32>().ok()).filter(|r| r.is_finite()).map(|r| r.clamp(2.0, 200.0)).unwrap_or(DEFAULT_RANGE),
            radio: flag("voice_radio", true),
        };
        if s.server_uid.is_empty() {
            log::warn!("VoySpeak: voice_channel is set but voice_server_uid is not: no VoySpeak voice (the plugin moves players only on the voice server the session names)");
            return None;
        }
        // (a command is cut at MAX_CHAT characters: a cut password or channel would be a wrong one)
        let len = VoySpeakServer::command(Some(&s)).chars().count();
        if len > omsi_net::MAX_CHAT {
            log::warn!("VoySpeak: voice_server_uid, voice_channel and voice_channel_password are too long to tell the players ({len} of {} characters encoded): no VoySpeak voice", omsi_net::MAX_CHAT);
            return None;
        }
        Some(s)
    }

    /// `~/.voyomsi/voice.cfg` of a game that hosts (the same keys as `server.cfg`).
    pub fn host_file() -> Option<VoySpeakServer> {
        let text = std::fs::read_to_string(crate::data_dir()?.join("voice.cfg")).ok()?;
        let kv = parse_kv(&text);
        VoySpeakServer::from_kv(|k| kv.get(k).cloned())
    }

    /// The host's answer to `voyspeak?`.
    pub fn command(server: Option<&VoySpeakServer>) -> String {
        match server {
            Some(s) => format!("voyspeak {} {} {} {} {}", s.range, enc(&s.server_uid), enc(&s.channel), enc(&s.password), s.radio as u8),
            None => "voyspeak -".into(),
        }
    }

    /// The host's answer as a joining game reads it: Some(None) for a session without one.
    pub fn parse_command(text: &str) -> Option<Option<VoySpeakServer>> {
        let rest = text.strip_prefix("voyspeak ")?.trim();
        if rest == "-" {
            return Some(None);
        }
        let mut f = rest.split(' ');
        let range = f.next()?.parse::<f32>().ok().filter(|r| r.is_finite())?.clamp(2.0, 200.0);
        let (uid, channel, password) = (dec(f.next()?), dec(f.next()?), dec(f.next().unwrap_or("")));
        let radio = f.next() != Some("0");
        if channel.is_empty() || uid.is_empty() {
            return Some(None);
        }
        Some(Some(VoySpeakServer { server_uid: uid, channel, password, range, radio }))
    }
}

/// A dedicated server's VoySpeak server (`server.cfg`), set once at its start.
static SERVER: OnceLock<Option<VoySpeakServer>> = OnceLock::new();

/// The dedicated server's own (`server.cfg`): `hosted` then answers with it.
pub fn set_server(server: Option<VoySpeakServer>) {
    let _ = SERVER.set(server);
}

/// The voice server this game names when it hosts: a dedicated server's `server.cfg`, else
/// `~/.voyomsi/voice.cfg`.
pub fn hosted() -> Option<VoySpeakServer> {
    match SERVER.get() {
        Some(v) => v.clone(),
        None => VoySpeakServer::host_file(),
    }
}

pub(crate) fn parse_kv(text: &str) -> HashMap<String, String> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect()
}

/// A command's field: no spaces, no '|' (the LAN messages' separator), '-' for empty.
fn enc(s: &str) -> String {
    if s.is_empty() {
        return "-".into();
    }
    let mut out = String::new();
    for c in s.chars() {
        match c {
            '%' | ' ' | '|' | '-' => out.push_str(&format!("%{:02X}", c as u32)),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

fn dec(s: &str) -> String {
    if s == "-" {
        return String::new();
    }
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Some(v) = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// A player's nickname in the voice channel: their name (letters, digits and a few signs)
/// and their id in the session, within TeamSpeak's 30 characters.
pub fn nickname(name: &str, id: u32) -> String {
    let tail = format!(" #{id}");
    let clean: String = name.chars().map(|c| if c.is_alphanumeric() || matches!(c, '-' | '_' | '.' | '\'') { c } else { ' ' }).collect();
    let clean = clean.split_whitespace().collect::<Vec<_>>().join(" ");
    let clean = if clean.is_empty() { "Driver".to_string() } else { clean };
    let room = MAX_NICK.saturating_sub(tail.chars().count());
    let short: String = clean.chars().take(room).collect();
    format!("{}{tail}", short.trim_end())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hosts_answer_round_trips() {
        let s = VoySpeakServer { server_uid: "abc+/=Def".into(), channel: "OMSI - In game".into(), password: "p|w 100%".into(), range: 25.0, radio: false };
        let c = VoySpeakServer::command(Some(&s));
        assert!(!c.contains('|') && c.len() < omsi_net::MAX_CHAT);
        assert_eq!(VoySpeakServer::parse_command(&c), Some(Some(s)));
        // (no voice server's unique id: no voice)
        let open = VoySpeakServer { server_uid: String::new(), channel: "12".into(), password: String::new(), range: 20.0, radio: true };
        assert_eq!(VoySpeakServer::parse_command(&VoySpeakServer::command(Some(&open))), Some(None));
        assert_eq!(VoySpeakServer::parse_command(&VoySpeakServer::command(None)), Some(None));
        // (the built-in voice chat's answer is not one)
        assert_eq!(VoySpeakServer::parse_command("voice 1 1 75"), None);
    }

    #[test]
    fn server_cfg_keys() {
        let kv = parse_kv("# x\nvoice_server_uid = AbC=\nvoice_channel = 5\nvoice_range = 1000\nvoice_radio = 0\n");
        let s = VoySpeakServer::from_kv(|k| kv.get(k).cloned()).unwrap();
        assert_eq!((s.server_uid.as_str(), s.channel.as_str(), s.range, s.radio), ("AbC=", "5", 200.0, false));
        assert!(VoySpeakServer::from_kv(|_| None).is_none());
        // no unique id: the plugin would move players on whatever server they are on
        let kv = parse_kv("voice_channel = 5\n");
        assert!(VoySpeakServer::from_kv(|k| kv.get(k).cloned()).is_none());
        // too long to tell the players whole
        let kv = parse_kv(&format!("voice_server_uid = AbC=\nvoice_channel = 5\nvoice_channel_password = {}\n", "p-".repeat(40)));
        assert!(VoySpeakServer::from_kv(|k| kv.get(k).cloned()).is_none());
    }

    #[test]
    fn nicknames_fit_teamspeak() {
        assert_eq!(nickname("Max Mustermann", 3), "Max Mustermann #3");
        assert_eq!(nickname("  <>|  ", 12), "Driver #12");
        let long = nickname(&"x".repeat(80), 123456);
        assert!(long.chars().count() <= MAX_NICK && long.ends_with(" #123456"), "{long}");
    }
}
