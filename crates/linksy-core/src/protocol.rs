//! Wire protocol. Every connection starts with a handshake on the first bidirectional
//! stream. After that:
//! - each file transfer gets its own bidirectional stream (offer, answer, raw file bytes),
//! - each media session (screen mirroring…) gets its own bidirectional stream too
//!   (request, answer, then media bytes one way and control messages the other),
//! - small one-shot messages (notifications, pings) each get a unidirectional stream.
//!
//! Control messages are JSON, framed with a big-endian u32 length.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::error::{Result, protocol};
use crate::identity::DeviceInfo;

pub const PROTOCOL_VERSION: u32 = 1;
const MAX_FRAME: usize = 1 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Intent {
    /// Reconnect between devices that already trust each other.
    Connect,
    /// Start pairing; both users must confirm the code.
    Pair,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Handshake {
    /// `stay_connected`: the sender keeps this link open, so the receiver shouldn't close
    /// it for being idle.
    Hello {
        version: u32,
        device: DeviceInfo,
        intent: Intent,
        #[serde(default)]
        stay_connected: bool,
        /// Token from a pairing invite (QR code) the dialer scanned; pairing then needs no
        /// code comparison.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        invite: Option<String>,
    },
    Welcome {
        device: DeviceInfo,
        #[serde(default)]
        stay_connected: bool,
    },
    Reject { reason: RejectReason },
    PairDecision { accept: bool },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectReason {
    NotPaired,
    IncompatibleVersion,
    Busy,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    /// Relative path with `/` separators. Folders are sent as their files.
    pub path: String,
    pub size: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TransferMsg {
    /// `import`: photos and videos for the receiver's picture library ("photo
    /// auto-import"), saved without asking when the receiver takes imports.
    Offer {
        id: Uuid,
        files: Vec<FileEntry>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        import: bool,
        /// One file (≤ 10 MiB) for the receiver's clipboard (an image, or a file to paste),
        /// taken without asking.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        clipboard: bool,
    },
    /// `offsets[i]` is how many bytes of file `i` the receiver already has.
    Accept { offsets: Vec<u64> },
    Decline,
    /// Sent after each file's bytes.
    FileDone { blake3: String },
    /// Receiver's final verdict.
    Complete,
    Failed { reason: String },
}

/// What a media session carries.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    /// The receiver of the request shares its screen with the requester.
    Screen,
    /// Like `Screen`, and the requester may also use its pointer and keyboard.
    Control,
    /// Sidecar: the *requester* shares a new virtual monitor, and the receiver shows it
    /// full screen and sends pointer and keyboard input back, as a second display.
    Display,
    /// The receiver shares its camera; the requester uses it as a webcam.
    Camera,
    /// The requester (a phone) drives the receiver's pointer and keyboard without seeing
    /// its screen: a touchpad, keyboard or presentation remote. No media; only input.
    Input,
    /// The receiver shares its microphone; the requester uses it as a microphone. Raw
    /// 16-bit 48 kHz mono PCM, little-endian (docs/protocol.md §4.8).
    Microphone,
    /// A shell on the receiver (a computer), shown on the requester (a phone): raw
    /// terminal output one way, `term_*` control messages the other (docs/protocol.md §4.15).
    Terminal,
}

impl SessionKind {
    /// Whether the device that asked for the session is the one sending media.
    pub fn requester_shares(self) -> bool {
        matches!(self, Self::Display)
    }

    /// Whether the requester turns what it receives into a device for its apps (a webcam,
    /// a microphone) rather than showing it.
    pub fn is_device(self) -> bool {
        matches!(self, Self::Camera | Self::Microphone)
    }

    /// Whether the viewing side sends input back.
    pub fn has_input(self) -> bool {
        matches!(self, Self::Control | Self::Display | Self::Input)
    }
}

/// Input sent by the viewer of a `control` session, framed like every other message
/// (u32 length + JSON). Positions are fractions of the shared screen (0.0–1.0), so they
/// don't depend on either device's resolution. Keys are X11 keysyms.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ControlMsg {
    Pointer { x: f64, y: f64 },
    Button { button: MouseButton, pressed: bool },
    /// `discrete`: wheel clicks (dx/dy are steps); otherwise smooth scrolling in pixels.
    Scroll { dx: f64, dy: f64, discrete: bool },
    Key { keysym: u32, pressed: bool },
    /// Move the pointer by this many pixels (a touchpad); for `input` sessions.
    PointerBy { dx: f64, dy: f64 },
    /// Type this text, as if on a keyboard.
    Text { text: String },
    /// Terminal sessions: what was typed, as the terminal's bytes (xterm sequences).
    TermInput { data: String },
    TermResize { cols: u16, rows: u16 },
    /// End the shell, not just this view of it.
    TermClose,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseButton {
    Left,
    Middle,
    Right,
    Back,
    Forward,
}

impl MouseButton {
    /// Linux evdev code (BTN_LEFT…), what input APIs take.
    pub fn evdev(self) -> i32 {
        match self {
            Self::Left => 0x110,
            Self::Right => 0x111,
            Self::Middle => 0x112,
            Self::Back => 0x113,
            Self::Forward => 0x114,
        }
    }
}

/// Encode a control message as a frame.
pub fn control_frame(msg: &ControlMsg) -> Vec<u8> {
    let body = serde_json::to_vec(msg).unwrap_or_default();
    let mut out = (body.len() as u32).to_be_bytes().to_vec();
    out.extend(body);
    out
}

/// Read control frames from a byte stream (blocking), calling `f` for each until the
/// stream ends. Unknown or malformed messages are skipped.
pub fn read_control_frames(mut r: impl std::io::Read, mut f: impl FnMut(ControlMsg)) {
    let mut len = [0u8; 4];
    while r.read_exact(&mut len).is_ok() {
        let n = u32::from_be_bytes(len) as usize;
        if n > MAX_FRAME {
            return;
        }
        let mut body = vec![0u8; n];
        if r.read_exact(&mut body).is_err() {
            return;
        }
        if let Ok(msg) = serde_json::from_slice(&body) {
            f(msg);
        }
    }
}

/// Opening a media session. After `session_accept`, the sharing side writes the media
/// (for `screen`: H.264 Annex-B byte stream) and the requesting side may write framed
/// control messages back.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionMsg {
    SessionRequest {
        id: Uuid,
        kind: SessionKind,
        /// For a `terminal`: the code, the size, and which one to reattach.
        #[serde(flatten, default, skip_serializing_if = "Option::is_none")]
        terminal: Option<TerminalOpen>,
    },
    /// For a `display`, the accepting device may give its screen's size (landscape), so
    /// the second display is made in its shape.
    SessionAccept {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        width: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        height: Option<u32>,
        /// For a `terminal`: its id, to reattach later.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        terminal: Option<String>,
    },
    /// `reason: "unsupported"` when the device can't do this kind of session at all (as
    /// opposed to its user saying no). Absent otherwise.
    SessionDecline {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        /// A wrong terminal code: how many tries are left before it locks.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tries_left: Option<u32>,
    },
}

/// What a phone sends to open a terminal (docs/protocol.md §4.15).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalOpen {
    /// The terminal code as typed; only the computer can check it.
    pub code: String,
    pub cols: u16,
    pub rows: u16,
    /// A running terminal to reattach, by id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resume: Option<String>,
}

/// A running terminal, as `terminals` lists it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalInfo {
    pub id: String,
    /// The shell's window title (OSC 0/2), or the shell's name.
    pub title: String,
    pub started_ms: u64,
}

/// A phone plugged into a computer by USB (§4.11).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluggedPhone {
    /// Its USB serial, or vendor:product.
    pub key: String,
    /// The USB descriptor strings.
    pub manufacturer: String,
    pub product: String,
    /// `plugged`, `switching`, `accessory` or `failed`.
    pub state: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    Notify { title: String, body: String },
    /// Text for the receiver's clipboard.
    Clipboard { text: String },
    /// The sender's new name, so connected devices update without reconnecting.
    Renamed { name: String },
    /// The key the sender's private discovery tags are made from (§4.16), 64 hex chars.
    /// Sent to paired devices on every connection.
    DiscoveryKey { key: String },
    Ping,
    /// A web link for the receiver to open in its browser ("Open on PC"). `http`/`https`
    /// only; receivers drop anything else.
    OpenUrl { url: String },
    /// The sender's battery, sent when it changes noticeably and on connecting.
    Battery(Battery),
    /// Play a loud sound until `ring_stop` or the user notices ("Ring my phone").
    Ring,
    RingStop,
    /// The phones plugged into the sender (a computer) by USB, sent to connected phones when
    /// that changes and after connecting (§4.11): a phone offers its own Connect button.
    UsbPlugged { phones: Vec<PluggedPhone> },
    /// Use the cable for this plugged-in phone (its `key` from `usb_plugged`).
    UsbConnect { key: String },
    /// A notification on the sender (a phone) to show here; sent again with the same `key`
    /// when it changes.
    NotificationPosted(RemoteNotification),
    /// That notification is gone on the sender.
    NotificationRemoved { key: String },
    /// Answer a notification that `can_reply` with this text.
    NotificationReply { key: String, text: String },
    /// Press one of a notification's `actions`.
    NotificationAction { key: String, action: String },
    /// The user dismissed it on the receiving side; dismiss it at the source too.
    NotificationDismiss { key: String },
    /// Everything playing on the sender (all its media players), sent when it changes.
    MediaState(MediaState),
    /// Control one of the receiver's players.
    MediaCommand { player: String, command: MediaCommand },
    /// Start (or stop) sending `media_state` to the sender while connected.
    MediaWatch { on: bool },
    /// The sender's audio outputs and the apps playing on them, sent when it changes.
    AudioState(AudioState),
    /// Change the receiver's audio: an output's or an app's volume, mute, or where it plays.
    AudioCommand { command: AudioCommand },
    /// Start (or stop) sending `audio_state` to the sender while connected.
    AudioWatch { on: bool },
    /// The sender (a phone) has a call coming in, going on, or none any more.
    CallState {
        state: CallState,
        /// Who's calling, when the phone may say.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        contact: Option<String>,
    },
    /// "Still here": sent every `every_s` seconds to a device that asked with
    /// `heartbeat_watch` (lock when you walk away).
    Heartbeat,
    /// Ask the receiver for a `heartbeat` every `every_s` seconds (`on`), or to stop.
    HeartbeatWatch { on: bool, every_s: u32 },
    /// Ask the receiver to send `clipboard` whenever its clipboard changes (`on`), or to
    /// stop. Not every desktop allows watching it (GNOME doesn't).
    ClipboardWatch { on: bool },
    /// What the camera of a running `camera` session can do and how it's set (phone →
    /// computer), sent when the session starts and after every change; the keys are in
    /// docs/protocol.md §4.8. The computer changes them with `camera_control` frames on the
    /// session itself.
    CameraState { session: uuid::Uuid, state: serde_json::Value },
    /// The sender's own picture (its avatar), a JPEG or PNG ≤ 128 KiB in base64, sent after
    /// connecting; the receiver shows it for that device.
    Picture { image: String },
    /// "Where are you?" (computer → phone, docs/protocol.md §4.14): answered once with
    /// `location` or `location_refused`. Nothing is kept or tracked.
    Locate,
    /// Which terminals are running here for the asker (docs/protocol.md §4.15).
    TerminalList,
    Terminals { list: Vec<TerminalInfo> },
    /// Where the sender is now: degrees, accuracy in metres, when it was measured.
    Location {
        lat: f64,
        lon: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        accuracy_m: Option<f64>,
        at_ms: u64,
    },
    /// The sender won't or can't say where it is (turned off, no permission, no fix).
    LocationRefused { reason: String },
    /// Texting from the computer. Requests (computer → phone):
    SmsListThreads { limit: u32 },
    SmsListMessages {
        thread: String,
        /// Only messages older than this (paging back); newest first.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        before_ms: Option<u64>,
        limit: u32,
    },
    /// Send `text` to `to` (one address, or several for a group); `client_id` comes back
    /// in `sms_sent`.
    SmsSend { client_id: String, to: Vec<String>, text: String },
    /// Answers and news (phone → computer):
    SmsThreads { threads: Vec<SmsThread> },
    SmsMessages { thread: String, messages: Vec<SmsMessage>, more: bool },
    /// A message arrived or was sent on the phone (by Linksy or not).
    SmsMessage { thread: String, message: SmsMessage },
    SmsSent {
        client_id: String,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
}

/// Browsing a device's files ("phone in the Files sidebar"): the first message of a
/// bidirectional stream, one request per stream. Paths are absolute within what the
/// serving device shares ("/" is its root), with `/` separators.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum FsRequest {
    /// A folder's entries → `fs_entries`.
    List { path: String },
    /// One entry → `fs_entry`.
    Stat { path: String },
    /// Up to `length` bytes from `offset` → `fs_data` and then exactly that many raw bytes.
    Read { path: String, offset: u64, length: u64 },
    /// `length` raw bytes follow the request, to write at `offset` (`truncate`: the file
    /// ends after them; creates it) → `fs_ok`.
    Write { path: String, offset: u64, length: u64, truncate: bool },
    Mkdir { path: String },
    /// A file, or a folder with everything in it.
    Delete { path: String },
    Rename { path: String, to: String },
    /// What's shared, to label it ("Pixel 9 (Documents)") → `fs_info`. Optional: a device
    /// that doesn't know it answers `fs_error`.
    Info,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FsReply {
    FsEntries { entries: Vec<FsEntry> },
    FsEntry { entry: FsEntry },
    FsData { length: u64 },
    FsOk,
    /// The shared folder's name, for labels.
    FsInfo { name: String },
    FsError {
        message: String,
        #[serde(default)]
        not_found: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FsEntry {
    pub name: String,
    pub dir: bool,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub modified_ms: u64,
}

/// A conversation, newest first in lists.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SmsThread {
    pub id: String,
    /// Phone numbers, and what the phone's contacts call them (same order; may be empty).
    pub addresses: Vec<String>,
    #[serde(default)]
    pub names: Vec<String>,
    #[serde(default)]
    pub snippet: String,
    pub date_ms: u64,
    #[serde(default)]
    pub unread: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SmsMessage {
    pub id: String,
    pub from_me: bool,
    /// Who sent it (for messages from others, in groups).
    #[serde(default)]
    pub address: String,
    pub body: String,
    pub date_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CallState {
    Ringing,
    Active,
    Idle,
}

/// A device's media players, as a whole: a later `seq` replaces an earlier one (messages
/// can arrive out of order), and a player missing from it has gone. `seq` is the time it
/// was taken, in milliseconds since 1970, so it keeps growing across restarts.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaState {
    pub seq: u64,
    pub players: Vec<MediaPlayer>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaPlayer {
    /// Stable id on the sender, for commands.
    pub id: String,
    /// The app playing ("Spotify").
    pub app: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub artist: String,
    #[serde(default)]
    pub album: String,
    pub playing: bool,
    /// Where it is and how long it is, in milliseconds, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub length_ms: Option<u64>,
    #[serde(default)]
    pub can_next: bool,
    #[serde(default)]
    pub can_previous: bool,
    #[serde(default)]
    pub can_seek: bool,
    /// Cover art, a small JPEG or PNG (≤ 64 KiB) in standard base64.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub art: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum MediaCommand {
    Play,
    Pause,
    PlayPause,
    Next,
    Previous,
    /// Jump to this position.
    Seek { position_ms: u64 },
}

/// Longest id, name or title in an audio message, in bytes.
pub const MAX_AUDIO_TEXT: usize = 256;
/// Most outputs, and most playing apps, one `audio_state` may list.
pub const MAX_AUDIO_SINKS: usize = 64;
pub const MAX_AUDIO_STREAMS: usize = 256;

/// The id that stands for whichever output is the default, so a device that only wants a
/// master volume needn't list the outputs first.
pub const DEFAULT_SINK: &str = "@DEFAULT_SINK@";

/// A device's audio, as a whole: its outputs and the apps playing on them. A later `seq`
/// replaces an earlier one (messages can arrive out of order), and an output or app
/// missing from it has gone. `seq` is the time it was taken, in milliseconds since 1970.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioState {
    pub seq: u64,
    /// The output new sound goes to, when there is one (an [`AudioSink::id`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_sink: Option<String>,
    pub sinks: Vec<AudioSink>,
    /// The apps playing, each on one of the `sinks`.
    #[serde(default)]
    pub streams: Vec<AudioStream>,
}

/// One audio output: speakers, headphones, HDMI, a Bluetooth speaker…
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioSink {
    /// Stable on the sender for as long as the output exists, for commands.
    pub id: String,
    /// What to show the user.
    pub name: String,
    /// Percent: 100 is full scale (0 dB). A sender may report more when it boosts.
    pub volume: u16,
    pub muted: bool,
    /// What it is, when the sender knows: `speaker`, `headphone`, `headset`, `hdmi`, `tv`…
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub form_factor: Option<String>,
}

/// One app playing sound.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioStream {
    /// Stable while the app plays, for commands; a new one when it plays again.
    pub id: String,
    /// The app ("Firefox").
    pub app: String,
    /// What it plays, when it says ("Song – Band").
    #[serde(default)]
    pub title: String,
    /// The output it plays on (an [`AudioSink::id`]).
    pub sink: String,
    pub volume: u16,
    pub muted: bool,
    /// The app has the stream open but isn't playing now (a paused video). Left out, it
    /// plays.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub paused: bool,
}

/// Volumes in commands are percent, 0 to 100: nobody can make another device louder than
/// full scale.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum AudioCommand {
    /// An output's volume. `sink` is an [`AudioSink::id`], or [`DEFAULT_SINK`].
    SetSinkVolume { sink: String, volume: u8 },
    MuteSink { sink: String, muted: bool },
    /// Send new sound to this output from now on.
    SetDefaultSink { sink: String },
    /// An app's volume, `stream` being an [`AudioStream::id`].
    SetStreamVolume { stream: String, volume: u8 },
    MuteStream { stream: String, muted: bool },
    /// Play an app on another output (`sink` as above, [`DEFAULT_SINK`] included).
    MoveStream { stream: String, sink: String },
}

fn audio_text_ok(text: &str) -> bool {
    // A NUL ends a C string early; the sound server's library would trip over one.
    text.len() <= MAX_AUDIO_TEXT && !text.contains('\0')
}

impl AudioState {
    /// Within the limits the protocol sets (counts, lengths, no NUL bytes). Receivers drop
    /// a state that isn't; senders keep to them.
    pub fn is_valid(&self) -> bool {
        self.sinks.len() <= MAX_AUDIO_SINKS
            && self.streams.len() <= MAX_AUDIO_STREAMS
            && self.default_sink.as_deref().is_none_or(audio_text_ok)
            && self.sinks.iter().all(|s| audio_text_ok(&s.id) && audio_text_ok(&s.name) && s.form_factor.as_deref().is_none_or(audio_text_ok))
            && self.streams.iter().all(|s| audio_text_ok(&s.id) && audio_text_ok(&s.app) && audio_text_ok(&s.title) && audio_text_ok(&s.sink))
    }
}

impl AudioCommand {
    /// Volumes at most 100, ids within the limits and free of NUL bytes.
    pub fn is_valid(&self) -> bool {
        match self {
            Self::SetSinkVolume { sink, volume } => *volume <= 100 && audio_text_ok(sink),
            Self::SetStreamVolume { stream, volume } => *volume <= 100 && audio_text_ok(stream),
            Self::MuteSink { sink, .. } | Self::SetDefaultSink { sink } => audio_text_ok(sink),
            Self::MuteStream { stream, .. } => audio_text_ok(stream),
            Self::MoveStream { stream, sink } => audio_text_ok(stream) && audio_text_ok(sink),
        }
    }
}

/// A notification mirrored from another device (a phone's WhatsApp message…).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteNotification {
    /// Stable id on the sender (Android: the StatusBarNotification key).
    pub key: String,
    /// The app's id (Android package name), for muting.
    pub app: String,
    pub app_name: String,
    pub title: String,
    #[serde(default)]
    pub body: String,
    /// Whether `notification_reply` works on it.
    #[serde(default)]
    pub can_reply: bool,
    /// Buttons the notification has, besides replying.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub actions: Vec<NotificationButton>,
    /// The app's icon, a small PNG (≤ 64 KiB) in standard base64.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationButton {
    pub id: String,
    pub label: String,
}

/// A device's battery as it reports it: `level` in percent (0–100).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Battery {
    pub level: u8,
    pub charging: bool,
}

pub async fn write_msg<T: Serialize>(s: &mut quinn::SendStream, msg: &T) -> Result<()> {
    let body = serde_json::to_vec(msg)?;
    s.write_all(&(body.len() as u32).to_be_bytes()).await?;
    s.write_all(&body).await?;
    Ok(())
}

/// Read one frame without knowing its type yet (the first message on a new stream).
pub async fn read_value(s: &mut quinn::RecvStream) -> Result<serde_json::Value> {
    read_msg(s).await
}

pub async fn read_msg<T: for<'de> Deserialize<'de>>(s: &mut quinn::RecvStream) -> Result<T> {
    let mut len = [0u8; 4];
    s.read_exact(&mut len).await?;
    let len = u32::from_be_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(protocol(format!("frame of {len} bytes exceeds limit")));
    }
    let mut body = vec![0u8; len];
    s.read_exact(&mut body).await?;
    Ok(serde_json::from_slice(&body)?)
}

#[cfg(test)]
mod tests {

    #[test]
    fn terminal_requests_round_trip() {
        let id = Uuid::nil();
        let open = TerminalOpen { code: "482913".into(), cols: 80, rows: 24, resume: None };
        let msg = SessionMsg::SessionRequest { id, kind: SessionKind::Terminal, terminal: Some(open) };
        let json = serde_json::to_string(&msg).unwrap();
        assert!(json.contains(r#""code":"482913""#) && json.contains(r#""kind":"terminal""#), "{json}");
        assert_eq!(serde_json::from_str::<SessionMsg>(&json).unwrap(), msg);
        // As the node reads it: a JSON value first.
        let value: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(serde_json::from_value::<SessionMsg>(value).unwrap(), msg);
        // Other sessions carry no terminal part.
        let screen = SessionMsg::SessionRequest { id, kind: SessionKind::Screen, terminal: None };
        let json = serde_json::to_string(&screen).unwrap();
        assert_eq!(serde_json::from_str::<SessionMsg>(&json).unwrap(), screen);
        assert_eq!(serde_json::from_str::<SessionMsg>(r#"{"type":"session_request","id":"00000000-0000-0000-0000-000000000000","kind":"screen"}"#).unwrap(), screen);
    }

    use super::*;

    #[test]
    fn control_frames_round_trip_and_skip_junk() {
        let msgs = [
            ControlMsg::Pointer { x: 0.25, y: 0.75 },
            ControlMsg::Button { button: MouseButton::Right, pressed: true },
            ControlMsg::Scroll { dx: 0.0, dy: -2.0, discrete: true },
            ControlMsg::Key { keysym: 0xff0d, pressed: false },
        ];
        let mut bytes = Vec::new();
        for (i, m) in msgs.iter().enumerate() {
            bytes.extend(control_frame(m));
            if i == 1 {
                // A message from a newer version: skipped, the rest still read.
                let junk = br#"{"type":"teleport"}"#;
                bytes.extend((junk.len() as u32).to_be_bytes());
                bytes.extend(junk);
            }
        }
        let mut got = Vec::new();
        read_control_frames(&bytes[..], |m| got.push(m));
        assert_eq!(got, msgs);
        assert_eq!(serde_json::to_string(&msgs[1]).unwrap(), r#"{"type":"button","button":"right","pressed":true}"#);
    }

    fn headphones() -> AudioState {
        AudioState {
            seq: 1_760_945_000_123,
            default_sink: Some("alsa_output.pci-0000_00_1f.3.analog-stereo".into()),
            sinks: vec![AudioSink {
                id: "alsa_output.pci-0000_00_1f.3.analog-stereo".into(),
                name: "Built-in Audio Analog Stereo".into(),
                volume: 40,
                muted: false,
                form_factor: Some("headphone".into()),
            }],
            streams: vec![AudioStream {
                id: "57".into(),
                app: "Firefox".into(),
                title: "Song".into(),
                sink: "alsa_output.pci-0000_00_1f.3.analog-stereo".into(),
                volume: 100,
                muted: true,
                paused: true,
            }],
        }
    }

    #[test]
    fn audio_messages_have_the_documented_shape() {
        let state = serde_json::to_string(&Message::AudioState(headphones())).unwrap();
        assert!(state.starts_with(r#"{"type":"audio_state","seq":1760945000123,"default_sink":"alsa_output"#), "{state}");
        assert!(state.contains(r#""form_factor":"headphone""#) && state.contains(r#""streams":[{"id":"57","app":"Firefox""#) && state.contains(r#""paused":true"#), "{state}");
        assert_eq!(serde_json::from_str::<Message>(&state).unwrap(), Message::AudioState(headphones()));

        let cmd = Message::AudioCommand { command: AudioCommand::SetSinkVolume { sink: DEFAULT_SINK.into(), volume: 30 } };
        assert_eq!(serde_json::to_string(&cmd).unwrap(), r#"{"type":"audio_command","command":{"action":"set_sink_volume","sink":"@DEFAULT_SINK@","volume":30}}"#);
        let moved = r#"{"type":"audio_command","command":{"action":"move_stream","stream":"57","sink":"hdmi"}}"#;
        assert_eq!(
            serde_json::from_str::<Message>(moved).unwrap(),
            Message::AudioCommand { command: AudioCommand::MoveStream { stream: "57".into(), sink: "hdmi".into() } }
        );
        assert_eq!(serde_json::to_string(&Message::AudioWatch { on: true }).unwrap(), r#"{"type":"audio_watch","on":true}"#);
        // A phone may leave out what it hasn't got: no default output, no apps, no form factor.
        let bare = r#"{"type":"audio_state","seq":5,"sinks":[{"id":"music","name":"Media","volume":60,"muted":false}]}"#;
        let Message::AudioState(got) = serde_json::from_str(bare).unwrap() else { panic!("not an audio_state") };
        assert!(got.is_valid() && got.default_sink.is_none() && got.streams.is_empty() && got.sinks[0].form_factor.is_none());
    }

    #[test]
    fn audio_limits_are_enforced() {
        assert!(headphones().is_valid());
        let set = |volume| AudioCommand::SetStreamVolume { stream: "1".into(), volume };
        assert!(set(100).is_valid() && set(0).is_valid());
        assert!(!set(101).is_valid(), "nobody makes another device louder than full scale");
        let long = "x".repeat(MAX_AUDIO_TEXT + 1);
        assert!(!AudioCommand::MuteSink { sink: long.clone(), muted: true }.is_valid());
        assert!(!AudioCommand::SetDefaultSink { sink: "a\0b".into() }.is_valid(), "a NUL byte breaks C strings");
        assert!(!AudioCommand::MoveStream { stream: "1".into(), sink: "a\0".into() }.is_valid());

        let mut state = headphones();
        state.streams[0].title = long;
        assert!(!state.is_valid());
        let mut state = headphones();
        state.sinks[0].name = "Speakers\0".into();
        assert!(!state.is_valid());
        let mut state = headphones();
        state.streams = vec![state.streams[0].clone(); MAX_AUDIO_STREAMS + 1];
        assert!(!state.is_valid());
        let mut state = headphones();
        state.sinks = vec![state.sinks[0].clone(); MAX_AUDIO_SINKS + 1];
        assert!(!state.is_valid());
    }
}
