//! Positional voice in multiplayer through VoySpeak, the way SaltyChat does it for FiveM,
//! beside the built-in voice chat (`voice_chat`): the game's link to VoyOMSI's plugin.
//!
//! VoySpeak (a TeamSpeak-style voice client, <https://voyspeak.com>) runs plugins
//! with a 3D voice API (`ctx.voice.setListenerPose` / `setClientPose`). VoyOMSI's plugin
//! (`tools/voyspeak-plugin`) listens on `127.0.0.1:38088`; the game connects to it and
//! sends one JSON object a line:
//!
//! - `hello`: first, the key of `~/.voyomsi/voice-plugin.key` (made by the game, read by the
//!   plugin): anything else on that port - a web page posting to it, another program - is
//!   not the game and is hung up on. The plugin answers `welcome`, or `refused` and hangs up
//!   (another game is already linked to it).
//! - `initiate`: the voice server the session uses (its unique id, the in-game channel and its
//!   password, the voice range) and the nickname to take there. The plugin moves the user
//!   into that channel, renames them and switches 3D voice on - only on the server whose
//!   unique id the session names, never on another one the user happens to be on.
//! - `self`: where the listener (the camera) is and which way it looks, ten times a second;
//!   also whether they are on the bus radio and holding its key (`onRadio`, `keyed`).
//! - `players`: every other player who can be heard: nickname, where their head is, their
//!   range, a volume when the sound is muffled (one of the two sits in a bus), and whether
//!   they are keying the map-wide bus radio (`radio`).
//! - `reset`: the session is over: 3D voice off, the user's nickname and channel as before.
//!
//! The plugin answers with `state` (in the channel, or what is wrong), `talk` (who speaks now)
//! and `mute`. The session's voice server is the host's to name (`omsi_lan::voyspeak`).

use glam::DVec3;
use omsi_lan::voyspeak::{nickname, VoySpeakServer};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError};
use std::time::{Duration, Instant};

/// The port of the plugin (SaltyChat's WebSocket port, for those who know it).
pub(crate) const DEFAULT_PORT: u16 = 38088;
/// Inside a bus with the other one outside it (or in another bus): heard through the
/// bodywork at this share of the open-air loudness.
const MUFFLED: f32 = 0.35;
const SEND_EVERY: f32 = 0.1;
const ASK_EVERY: f32 = 4.0;
const ASK_TRIES: u32 = 8;
/// "Start VoySpeak ..." is on the HUD this long (s) while the plugin is not there.
const HINT_SECS: f32 = 10.0;
/// Lines waiting for the plugin: beyond this many (a plugin that stopped reading) the
/// newest are dropped rather than piled up.
const QUEUE: usize = 64;
/// A plugin that takes this long to read a line has hung: the link is made again.
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);
/// The plugin answers `hello` within this long, or it is not one that knows the key.
const HELLO_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(not(test))]
const RECONNECT: Duration = Duration::from_secs(3);
#[cfg(test)]
const RECONNECT: Duration = Duration::from_millis(20);

/// `~/.voyomsi/voice-plugin.key`: the key the game says `hello` with and the plugin checks it
/// against (no web page or other program on this machine can read it).
pub(crate) fn key_path() -> Option<PathBuf> {
    Some(omsi_lan::data_dir()?.join("voice-plugin.key"))
}

/// The key in `path`, made the first time (32 random bytes as hex, readable by this user only).
fn plugin_key(path: &Path) -> Option<String> {
    if let Ok(k) = std::fs::read_to_string(path) {
        let k = k.trim();
        if k.len() >= 32 && k.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Some(k.to_string());
        }
    }
    let key = random_hex(32);
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    match o.open(path).and_then(|mut f| f.write_all(key.as_bytes())) {
        Ok(()) => Some(key),
        Err(e) => {
            log::warn!("VoySpeak: cannot write the plugin's key {}: {e}", path.display());
            None
        }
    }
}

/// `bytes` random bytes as hex: std's `RandomState` keys come from the system's random source.
fn random_hex(bytes: usize) -> String {
    use std::hash::{BuildHasher, Hasher};
    let mut out = String::with_capacity(bytes * 2);
    let mut n = 0u64;
    while out.len() < bytes * 2 {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(n);
        h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0));
        out.push_str(&format!("{:016x}", h.finish()));
        n += 1;
    }
    out.truncate(bytes * 2);
    out
}

/// What the plugin said.
#[derive(Debug, Clone, PartialEq)]
enum Event {
    /// The plugin welcomed us (again): it needs the session told afresh.
    Linked,
    Unlinked,
    /// The plugin would not have us (another game is linked to it, it did not answer).
    Refused(String),
    Line(Value),
}

/// The link to the plugin: a thread keeps a TCP connection to it, made again whenever it
/// breaks (the plugin starts after the game, VoySpeak is restarted).
struct Link {
    tx: SyncSender<String>,
    rx: Receiver<Event>,
}

impl Link {
    fn start(addr: SocketAddr, key: Option<PathBuf>) -> Option<Link> {
        let (tx, out_rx) = mpsc::sync_channel::<String>(QUEUE);
        let (ev_tx, rx) = mpsc::channel::<Event>();
        std::thread::Builder::new().name("voyspeak".into()).spawn(move || link_thread(addr, key, out_rx, ev_tx)).ok()?;
        Some(Link { tx, rx })
    }
}

/// Drop what the game sends for `wait` (nobody to send it to). False when the game is gone.
fn idle(out: &Receiver<String>, wait: Duration) -> bool {
    let until = Instant::now() + wait;
    while Instant::now() < until {
        match out.try_recv() {
            Ok(_) => {}
            Err(TryRecvError::Disconnected) => return false,
            Err(TryRecvError::Empty) => std::thread::sleep(Duration::from_millis(20)),
        }
    }
    true
}

fn link_thread(addr: SocketAddr, key_file: Option<PathBuf>, out: Receiver<String>, events: Sender<Event>) {
    let mut told_missing = false;
    let mut told_refused = String::new();
    loop {
        // (read each time: the file may have been made or replaced meanwhile)
        let Some(key) = key_file.as_deref().and_then(plugin_key) else {
            if !told_missing {
                log::warn!("VoySpeak: no key for the plugin (no home directory?)");
                told_missing = true;
            }
            if !idle(&out, RECONNECT) {
                return;
            }
            continue;
        };
        let stream = match TcpStream::connect_timeout(&addr, Duration::from_millis(500)) {
            Ok(s) => s,
            Err(e) => {
                if !told_missing {
                    log::info!("VoySpeak: no plugin at {addr} ({e}); trying again every few seconds");
                    told_missing = true;
                }
                // (lines for a plugin that is not there are dropped; the game stops the
                // thread by dropping its sender)
                if !idle(&out, RECONNECT) {
                    return;
                }
                continue;
            }
        };
        told_missing = false;
        let _ = stream.set_nodelay(true);
        let _ = stream.set_read_timeout(Some(Duration::from_millis(20)));
        // (a plugin that stopped reading would block the thread for good)
        let _ = stream.set_write_timeout(Some(WRITE_TIMEOUT));
        let mut writer = match stream.try_clone() {
            Ok(w) => w,
            Err(_) => continue,
        };
        let hello = json!({ "type": "hello", "game": "VoyOMSI", "protocol": 2, "key": key }).to_string() + "\n";
        if writer.write_all(hello.as_bytes()).is_err() {
            if !idle(&out, RECONNECT) {
                return;
            }
            continue;
        }
        let since = Instant::now();
        let mut welcomed = false;
        let mut refused: Option<String> = None;
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        let alive = 'link: loop {
            // what the game sends (dropped until the plugin has welcomed us)
            loop {
                match out.try_recv() {
                    Ok(mut l) if welcomed => {
                        l.push('\n');
                        if writer.write_all(l.as_bytes()).is_err() {
                            break 'link true;
                        }
                    }
                    Ok(_) => {}
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => break 'link false,
                }
            }
            if !welcomed && since.elapsed() > HELLO_TIMEOUT {
                refused = Some("the VoyOMSI plugin in VoySpeak does not answer (an older version? install it again)".into());
                break 'link true;
            }
            // what the plugin says
            match reader.read_line(&mut line) {
                Ok(0) => break 'link true,
                Ok(_) => {
                    if let Ok(v) = serde_json::from_str::<Value>(line.trim()) {
                        match v.get("type").and_then(|t| t.as_str()) {
                            Some("welcome") if !welcomed => {
                                welcomed = true;
                                told_refused.clear();
                                log::info!("VoySpeak: linked to the plugin at {addr}");
                                if events.send(Event::Linked).is_err() {
                                    break 'link false;
                                }
                            }
                            Some("refused") => {
                                refused = Some(v.get("error").and_then(|e| e.as_str()).unwrap_or("the plugin would not link").to_string());
                            }
                            _ if welcomed => {
                                if events.send(Event::Line(v)).is_err() {
                                    break 'link false;
                                }
                            }
                            _ => {}
                        }
                    }
                    line.clear();
                }
                Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut) => {}
                Err(_) => break 'link true,
            }
        };
        if welcomed {
            let _ = events.send(Event::Unlinked);
        }
        if !alive {
            return;
        }
        match refused {
            Some(r) => {
                if r != told_refused {
                    log::warn!("VoySpeak: {r}");
                    told_refused = r.clone();
                }
                if events.send(Event::Refused(r)).is_err() {
                    return;
                }
            }
            None if welcomed => log::info!("VoySpeak: the plugin went away"),
            None => {}
        }
        if !idle(&out, RECONNECT) {
            return;
        }
    }
}

/// What the game knows of the VoySpeak voice.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Status {
    /// The plugin is there.
    pub linked: bool,
    /// The plugin is on the session's voice server, in its channel.
    pub in_channel: bool,
    /// What went wrong, as the plugin says (wrong server, no such channel, ...).
    pub problem: Option<String>,
    pub mic_muted: bool,
}

/// A player as the voice places them, each frame.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Speaker {
    pub id: u32,
    pub name: String,
    /// Where their head is.
    pub at: DVec3,
    /// Which bus they are in (its player's id), None out in the open.
    pub inside: Option<u32>,
    /// They drive a bus: on the company radio (can hear and key it).
    pub on_radio: bool,
    /// They are holding the radio key (map-wide transmit).
    pub radio_keyed: bool,
}

/// Where we hear from: the camera, and which bus it is in.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Listener {
    pub at: DVec3,
    /// Degrees, 0 = north, clockwise (the camera's yaw).
    pub yaw: f32,
    pub inside: Option<u32>,
    /// We drive a bus: on the company radio.
    pub on_radio: bool,
    /// We are holding the radio key.
    pub radio_keyed: bool,
}

/// The game's side of the VoySpeak voice during a session.
pub(crate) struct VoySpeak {
    link: Option<Link>,
    addr: SocketAddr,
    /// The file of the key the plugin knows the game by (`key_path`).
    key: Option<PathBuf>,
    /// Why the plugin would not have us, until it does.
    refused: Option<String>,
    /// How long the session has gone without the plugin (the HUD's hint is for the first
    /// seconds of it only).
    unlinked_t: f32,
    /// The host's voice server: None until it answered, Some(None) for none.
    server: Option<Option<VoySpeakServer>>,
    asked: u32,
    ask_t: f32,
    send_t: f32,
    /// What `initiate` last told the plugin (sent again when it changes or the link is new).
    initiated: Option<(VoySpeakServer, String)>,
    /// The positions go to the plugin relative to this point: a map's coordinates run into
    /// the millions of metres, more than a 32-bit float of an audio engine keeps to the
    /// centimetre.
    origin: Option<DVec3>,
    /// Who is speaking now, by nickname.
    pub talking: hashbrown::HashSet<String>,
    pub status: Status,
}

impl VoySpeak {
    /// The voice of a session (`port` 0: the default).
    pub(crate) fn new(port: u16) -> VoySpeak {
        let port = if port == 0 { DEFAULT_PORT } else { port };
        VoySpeak {
            link: None,
            addr: SocketAddr::from(([127, 0, 0, 1], port)),
            key: key_path(),
            refused: None,
            unlinked_t: 0.0,
            server: None,
            asked: 0,
            ask_t: 0.0,
            send_t: 0.0,
            initiated: None,
            origin: None,
            talking: Default::default(),
            status: Status::default(),
        }
    }

    /// The host's voice server is known (we host, or its answer came).
    pub(crate) fn set_server(&mut self, server: Option<VoySpeakServer>) {
        if self.server.as_ref() != Some(&server) {
            match &server {
                Some(s) => log::info!("VoySpeak: the session talks on voice server '{}' channel '{}' (range {} m)", s.server_uid, s.channel, s.range),
                None => log::info!("VoySpeak: the session names no voice server"),
            }
        }
        self.server = Some(server);
    }

    /// The host's answer is in (or we host): the voice server, or that there is none.
    pub(crate) fn known(&self) -> bool {
        self.server.is_some()
    }

    /// The voice server, once known.
    pub(crate) fn server(&self) -> Option<&VoySpeakServer> {
        self.server.as_ref().and_then(|s| s.as_ref())
    }

    /// Ask the host for its voice server now and then until it answers (a host without
    /// VoySpeak never does: given up after a while). True when it is time to ask.
    pub(crate) fn should_ask(&mut self, dt: f32) -> bool {
        if self.server.is_some() || self.asked >= ASK_TRIES {
            return false;
        }
        self.ask_t -= dt;
        if self.ask_t > 0.0 {
            return false;
        }
        self.ask_t = ASK_EVERY;
        self.asked += 1;
        true
    }

    /// Queue a line for the plugin; false when it could not be (the plugin is not reading).
    fn send(&self, v: Value) -> bool {
        self.link.as_ref().is_some_and(|l| l.tx.try_send(v.to_string()).is_ok())
    }

    /// Once a frame: take in what the plugin said, tell it the session, where we are and
    /// where the others are.
    pub(crate) fn tick(&mut self, dt: f32, me: (&str, u32), listener: Option<Listener>, others: &[Speaker]) {
        let Some(server) = self.server().cloned() else { return };
        if self.link.is_none() {
            self.link = Link::start(self.addr, self.key.clone());
        }
        let mut relink = false;
        let events: Vec<Event> = self.link.as_ref().map(|l| l.rx.try_iter().collect()).unwrap_or_default();
        for e in events {
            match e {
                Event::Linked => relink = true,
                Event::Unlinked => {
                    relink = false;
                    self.status = Status::default();
                    self.talking.clear();
                }
                Event::Refused(r) => self.refused = Some(r),
                Event::Line(v) => self.on_line(&v),
            }
        }
        if relink {
            self.status.linked = true;
            self.refused = None;
            self.initiated = None;
        }
        if !self.status.linked {
            self.unlinked_t += dt;
            return;
        }
        let nick = nickname(me.0, me.1);
        if self.initiated.as_ref() != Some(&(server.clone(), nick.clone())) {
            let sent = self.send(json!({
                "type": "initiate",
                "game": "VoyOMSI",
                "protocol": 1,
                "serverUid": server.server_uid,
                "channel": server.channel,
                "password": server.password,
                "nickname": nick,
                "range": server.range,
            }));
            // (not taken in: told again next frame)
            if !sent {
                return;
            }
            self.initiated = Some((server.clone(), nick));
        }
        self.send_t -= dt;
        if self.send_t > 0.0 {
            return;
        }
        self.send_t = SEND_EVERY;
        let Some(ear) = listener else { return };
        let origin = *self.origin.get_or_insert_with(|| (ear.at / 1000.0).round() * 1000.0);
        let rel = |p: DVec3| {
            let r = p - origin;
            // (to the centimetre: the messages stay short)
            [(r.x * 100.0).round() / 100.0, (r.y * 100.0).round() / 100.0, (r.z * 100.0).round() / 100.0]
        };
        let [x, y, z] = rel(ear.at);
        let on_radio = server.radio && ear.on_radio;
        let keyed = on_radio && ear.radio_keyed;
        self.send(json!({ "type": "self", "x": x, "y": y, "z": z, "yaw": saltychat_yaw(ear.yaw), "onRadio": on_radio, "keyed": keyed }));
        let players: Vec<Value> = others
            .iter()
            .map(|o| {
                let [x, y, z] = rel(o.at);
                let radio_tx = on_radio && o.on_radio && o.radio_keyed;
                json!({
                    "nickname": nickname(&o.name, o.id),
                    "x": x, "y": y, "z": z,
                    "range": server.range,
                    "volume": heard_volume(&ear, o, server.range),
                    "radio": radio_tx,
                })
            })
            .collect();
        self.send(json!({ "type": "players", "players": players, "onRadio": on_radio }));
    }

    fn on_line(&mut self, v: &Value) {
        let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
        let b = |k: &str| v.get(k).and_then(|x| x.as_bool()).unwrap_or(false);
        match v.get("type").and_then(|t| t.as_str()).unwrap_or("") {
            "state" => {
                let was = self.status.clone();
                self.status.in_channel = b("inChannel");
                self.status.problem = Some(s("error")).filter(|e| !e.is_empty());
                if was.in_channel != self.status.in_channel || was.problem != self.status.problem {
                    match &self.status.problem {
                        Some(p) => log::warn!("VoySpeak: {p}"),
                        None if self.status.in_channel => log::info!("VoySpeak: in the session's voice channel"),
                        None => log::info!("VoySpeak: not in the session's voice channel"),
                    }
                }
            }
            "talk" => {
                let n = s("nickname");
                if b("talking") {
                    self.talking.insert(n);
                } else {
                    self.talking.remove(&n);
                }
            }
            "mute" => self.status.mic_muted = b("microphoneMuted"),
            _ => {}
        }
    }

    /// The HUD's line, when there is something to say.
    pub(crate) fn hud_line(&self) -> Option<String> {
        self.server()?;
        let tr = |s: &str| omsi_ui::tr(s).into_owned();
        if !self.status.linked {
            if let Some(r) = &self.refused {
                return Some(format!("VoySpeak: {r}"));
            }
            // (for the first seconds: a player who does not want to talk is not told so all
            // session long)
            return (self.unlinked_t < HINT_SECS).then(|| tr("Voice: start VoySpeak with the VoyOMSI plugin to talk"));
        }
        if let Some(p) = &self.status.problem {
            return Some(format!("VoySpeak: {p}"));
        }
        if !self.status.in_channel {
            return Some(tr("Voice: joining the VoySpeak channel ..."));
        }
        self.status.mic_muted.then(|| tr("Voice: your VoySpeak microphone is muted"))
    }

    /// The players talking in VoySpeak now (their nicknames), for the talker list.
    pub(crate) fn talkers(&self) -> impl Iterator<Item = &str> {
        self.talking.iter().map(String::as_str)
    }
}

impl Drop for VoySpeak {
    fn drop(&mut self) {
        // (the plugin switches 3D voice off; the thread ends when the sender goes)
        let _ = self.send(json!({ "type": "reset" }));
    }
}

/// The camera's yaw as SaltyChat's `Rotation` (and VoySpeak's `yawDeg`) has it: GTA's
/// heading, 0 = north and counter-clockwise, -180..180.
fn saltychat_yaw(yaw: f32) -> f32 {
    let r = (-yaw + 180.0).rem_euclid(360.0) - 180.0;
    (r * 10.0).round() / 10.0
}

/// A player heard through a bus's bodywork: the volume (0..1) the plugin is to use instead
/// of its own fall-off with distance; None in the open (or both in the same bus).
fn heard_volume(me: &Listener, o: &Speaker, range: f32) -> Option<f32> {
    if me.inside == o.inside {
        return None;
    }
    let d = (o.at - me.at).length() as f32;
    let open = (1.0 - d / range.max(1.0)).clamp(0.0, 1.0);
    Some((open * MUFFLED * 100.0).round() / 100.0)
}

/// The other players as the voice places them (`voice_chat::place_of`: their head on foot, the
/// driver's place in their bus, the bus they ride in); a driver is on the company radio.
pub(crate) fn speakers(lan: &omsi_net::LanSession, game: &omsi_lan::game::LanGame, my_bus: Option<DVec3>) -> Vec<Speaker> {
    lan.peers()
        .filter(|p| p.has_pose && p.pose.id != lan.my_id)
        .filter_map(|p| {
            let place = crate::voice_chat::place_of(lan, game, my_bus, p.pose.id)?;
            let on_radio = p.pose.walker.is_none() && p.pose.has_vehicle();
            Some(Speaker { id: p.pose.id, name: p.pose.name.clone(), at: place.at, inside: place.inside, on_radio, radio_keyed: p.pose.radio_keyed && on_radio })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::net::TcpListener;

    fn server() -> VoySpeakServer {
        VoySpeakServer { server_uid: "AbC=".into(), channel: "5".into(), password: String::new(), range: 20.0, radio: true }
    }

    #[test]
    fn yaw_and_muffling() {
        // (north stays north; east, clockwise from it, is -90 counter-clockwise)
        assert_eq!(saltychat_yaw(0.0), 0.0);
        assert_eq!(saltychat_yaw(90.0), -90.0);
        let me = Listener { at: DVec3::ZERO, yaw: 0.0, inside: None, on_radio: false, radio_keyed: false };
        let near = Speaker { id: 2, name: "a".into(), at: DVec3::new(5.0, 0.0, 0.0), inside: None, on_radio: false, radio_keyed: false };
        assert_eq!(heard_volume(&me, &near, 20.0), None, "both in the open: the plugin's own fall-off");
        let in_bus = Speaker { inside: Some(2), ..near };
        assert_eq!(heard_volume(&me, &in_bus, 20.0), Some(0.26), "through the bodywork");
    }

    /// The game says hello with the key, is welcomed, tells the plugin the session and the
    /// players; the plugin's lines come back as the status.
    #[test]
    fn talks_to_a_plugin() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let dir = std::env::temp_dir().join(format!("voyomsi-voyspeak-test-{}", std::process::id()));
        let key_file = dir.join("voice-plugin.key");
        let plugin = std::thread::spawn(move || {
            let (s, _) = listener.accept().unwrap();
            s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut w = s.try_clone().unwrap();
            let mut r = BufReader::new(s);
            let mut lines = Vec::new();
            let mut l = String::new();
            r.read_line(&mut l).unwrap();
            lines.push(l.clone());
            w.write_all(b"{\"type\":\"welcome\"}\n{\"type\":\"state\",\"inChannel\":true}\n").unwrap();
            while lines.len() < 4 {
                l.clear();
                if r.read_line(&mut l).unwrap_or(0) == 0 {
                    break;
                }
                lines.push(l.clone());
            }
            let mut rest = String::new();
            let _ = r.get_mut().read_to_string(&mut rest);
            lines
        });
        let mut g = VoySpeak::new(port);
        g.key = Some(key_file.clone());
        g.set_server(Some(server()));
        let ear = Listener { at: DVec3::new(1000.0, 2000.0, 3.0), yaw: 90.0, inside: None, on_radio: true, radio_keyed: false };
        let others = [Speaker { id: 2, name: "Bob".into(), at: DVec3::new(1005.0, 2000.0, 3.0), inside: None, on_radio: true, radio_keyed: true }];
        let t0 = Instant::now();
        while !(g.status.linked && g.status.in_channel) && t0.elapsed() < Duration::from_secs(5) {
            g.tick(0.2, ("Ann", 1), Some(ear), &others);
            std::thread::sleep(Duration::from_millis(20));
        }
        for _ in 0..5 {
            g.tick(0.2, ("Ann", 1), Some(ear), &others);
            std::thread::sleep(Duration::from_millis(20));
        }
        drop(g);
        let lines = plugin.join().unwrap();
        let key = std::fs::read_to_string(&key_file).unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(lines[0].contains("\"hello\"") && lines[0].contains(key.trim()), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("\"initiate\"") && l.contains("Ann #1")), "{lines:?}");
        assert!(lines.iter().any(|l| l.contains("\"players\"") && l.contains("Bob #2") && l.contains("\"radio\":true")), "{lines:?}");
    }
}
