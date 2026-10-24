//! The sound server, through libpulse: this computer's outputs ("sinks") and the apps
//! playing on them ("sink inputs"). PulseAudio and PipeWire (through pipewire-pulse) both
//! speak it.
//!
//! libpulse's objects aren't `Send`, so they live on one thread of ours, next to libpulse's
//! own threaded main loop. That thread takes requests from a channel and does each one
//! start to finish, blocking (it has nothing else to do); the answers go back to tokio
//! through oneshot channels, and libpulse's change notifications through an mpsc channel
//! (its callbacks run on libpulse's thread, and only ever touch channels). The rest of the
//! daemon holds a [`Pulse`]. Dropping it ends the thread and the connection.
//!
//! libpulse wants its lock held whenever its objects are used from our thread; callbacks
//! run with it held by libpulse itself. Here the lock is only ever taken around a call
//! that starts something, never while waiting for the answer (the callbacks need it).

use std::collections::HashMap;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use libpulse_binding::callbacks::ListResult;
use libpulse_binding::context::introspect::{SinkInfo, SinkInputInfo};
use libpulse_binding::context::subscribe::InterestMaskSet;
use libpulse_binding::context::{Context, FlagSet, State};
use libpulse_binding::error::PAErr;
use libpulse_binding::mainloop::threaded::Mainloop;
use libpulse_binding::operation::Operation;
use libpulse_binding::volume::{ChannelVolumes, Volume};
use linksy_core::protocol::{DEFAULT_SINK, MAX_AUDIO_SINKS, MAX_AUDIO_STREAMS, MAX_AUDIO_TEXT};
use linksy_core::{AudioCommand, AudioSink, AudioState, AudioStream};
use tokio::sync::{mpsc as tokio_mpsc, oneshot};

/// How long the sound server gets to answer before it counts as gone.
const WAIT: Duration = Duration::from_secs(5);

enum Request {
    Snapshot(oneshot::Sender<Result<AudioState, String>>),
    Apply(AudioCommand, oneshot::Sender<Result<(), String>>),
}

/// What the audio thread listens to.
enum Msg {
    Request(Request),
    /// The connection's state changed (it may have failed).
    State,
    /// The [`Pulse`] was dropped.
    Quit,
}

/// A connection to the sound server, alive as long as this is.
pub struct Pulse {
    inbox: mpsc::Sender<Msg>,
    changes: tokio_mpsc::Receiver<()>,
}

impl Drop for Pulse {
    fn drop(&mut self) {
        let _ = self.inbox.send(Msg::Quit);
    }
}

impl Pulse {
    /// Connect. Never starts a sound server of its own; fails when none is running.
    pub async fn start() -> Result<Self, String> {
        let (inbox, queued) = mpsc::channel();
        let (changed, changes) = tokio_mpsc::channel(8);
        let (ready_tx, ready) = oneshot::channel();
        let notify = inbox.clone();
        std::thread::Builder::new()
            .name("linksy-audio".into())
            .spawn(move || thread_main(ready_tx, queued, notify, changed))
            .map_err(|e| e.to_string())?;
        match ready.await {
            Ok(Ok(())) => Ok(Self { inbox, changes }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err("the audio thread stopped".into()),
        }
    }

    /// Resolves each time something changed (an output, an app, a volume…), and with `None`
    /// once the connection is gone.
    pub async fn changed(&mut self) -> Option<()> {
        self.changes.recv().await
    }

    /// Forget the changes that came in meanwhile (the snapshot taken next has them all).
    pub fn settle(&mut self) {
        while self.changes.try_recv().is_ok() {}
    }

    /// Outputs and apps as they are now. `seq` is left for the caller to set.
    pub async fn snapshot(&self) -> Result<AudioState, String> {
        let (tx, rx) = oneshot::channel();
        self.inbox.send(Msg::Request(Request::Snapshot(tx))).map_err(|_| gone())?;
        rx.await.map_err(|_| gone())?
    }

    pub async fn apply(&self, command: AudioCommand) -> Result<(), String> {
        let (tx, rx) = oneshot::channel();
        self.inbox.send(Msg::Request(Request::Apply(command, tx))).map_err(|_| gone())?;
        rx.await.map_err(|_| gone())?
    }
}

/// libpulse's own message for an error (its `to_string` is an `Option`).
fn pa_error(e: PAErr) -> String {
    e.to_string().unwrap_or_else(|| "libpulse failed".into())
}

fn gone() -> String {
    "the sound server is gone".into()
}

/// The audio thread: connect, say so, then answer requests until the [`Pulse`] is dropped
/// or the sound server goes away. Dropping `changed` at the end is how the caller notices.
fn thread_main(ready: oneshot::Sender<Result<(), String>>, inbox: mpsc::Receiver<Msg>, notify: mpsc::Sender<Msg>, changed: tokio_mpsc::Sender<()>) {
    let mut conn = match Conn::open(&inbox, notify) {
        Ok(conn) => conn,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    conn.ml.lock();
    conn.ctx.set_subscribe_callback(Some(Box::new(move |_, _, _| {
        // Full, and one is already waiting: the snapshot taken next covers this change too.
        let _ = changed.try_send(());
    })));
    conn.ctx.subscribe(InterestMaskSet::SINK | InterestMaskSet::SINK_INPUT | InterestMaskSet::SERVER, |_| {});
    conn.ml.unlock();
    let _ = ready.send(Ok(()));
    while let Ok(msg) = inbox.recv() {
        match msg {
            Msg::Request(Request::Snapshot(reply)) => {
                let _ = reply.send(conn.snapshot());
            }
            Msg::Request(Request::Apply(command, reply)) => {
                let _ = reply.send(conn.apply(command));
            }
            Msg::State => {
                if conn.dead() {
                    break;
                }
            }
            Msg::Quit => break,
        }
    }
    conn.close();
}

/// One step of a list the server sends.
enum Part<T> {
    Item(T),
    End,
    Failed,
}

/// Run a libpulse list call (`conn.intro.method(args…)`), turn each item with `convert`
/// (dropping the ones it returns `None` for) and wait for the whole list. A macro, not a
/// function: libpulse's callbacks borrow their item for a lifetime of their own, which a
/// closure can only be given where it is passed straight to the call.
macro_rules! collect {
    ($conn:ident, $method:ident ( $($arg:expr),* ) => $convert:ident) => {{
        let (tx, rx) = mpsc::channel();
        $conn.ml.lock();
        let intro = $conn.ctx.introspect();
        let _op = intro.$method($($arg,)* move |result| {
            let part = match result {
                ListResult::Item(item) => match $convert(item) {
                    Some(v) => Part::Item(v),
                    None => return,
                },
                ListResult::End => Part::End,
                ListResult::Error => Part::Failed,
            };
            let _ = tx.send(part);
        });
        $conn.ml.unlock();
        gather(rx)
    }};
}

fn gather<T>(rx: mpsc::Receiver<Part<T>>) -> Result<Vec<T>, String> {
    let mut items = Vec::new();
    loop {
        match rx.recv_timeout(WAIT) {
            Ok(Part::Item(v)) => items.push(v),
            Ok(Part::End) => return Ok(items),
            Ok(Part::Failed) => return Err("the sound server refused to list them".into()),
            Err(_) => return Err("the sound server didn't answer".into()),
        }
    }
}

/// The main loop and the context on it. `ctx` is declared first so that it is dropped first.
struct Conn {
    ctx: Context,
    ml: Mainloop,
}

impl Conn {
    fn open(inbox: &mpsc::Receiver<Msg>, notify: mpsc::Sender<Msg>) -> Result<Self, String> {
        let mut ml = Mainloop::new().ok_or("libpulse could not make a main loop")?;
        let mut ctx = Context::new(&ml, "Linksy").ok_or("libpulse could not make a context")?;
        ctx.set_state_callback(Some(Box::new(move || {
            let _ = notify.send(Msg::State);
        })));
        // NOAUTOSPAWN: a sound server nobody started isn't ours to start.
        ctx.connect(None, FlagSet::NOAUTOSPAWN, None).map_err(pa_error)?;
        ml.lock();
        let started = ml.start();
        ml.unlock();
        started.map_err(pa_error)?;
        let mut conn = Self { ctx, ml };
        let deadline = Instant::now() + WAIT;
        loop {
            conn.ml.lock();
            let state = conn.ctx.get_state();
            conn.ml.unlock();
            match state {
                State::Ready => return Ok(conn),
                State::Failed | State::Terminated => {
                    conn.close();
                    return Err("no sound server to talk to".into());
                }
                _ => {}
            }
            // Sleep until the state changes (any message will do: none is a request yet).
            if inbox.recv_timeout(deadline.saturating_duration_since(Instant::now())).is_err() {
                conn.close();
                return Err("the sound server didn't answer".into());
            }
        }
    }

    fn dead(&mut self) -> bool {
        self.ml.lock();
        let state = self.ctx.get_state();
        self.ml.unlock();
        matches!(state, State::Failed | State::Terminated)
    }

    fn close(mut self) {
        self.ml.lock();
        self.ctx.disconnect();
        self.ml.unlock();
        self.ml.stop();
    }

    // ---- reading ----

    fn default_sink(&mut self) -> Result<Option<String>, String> {
        let (tx, rx) = mpsc::channel();
        self.ml.lock();
        let intro = self.ctx.introspect();
        let _op = intro.get_server_info(move |info| {
            let _ = tx.send(info.default_sink_name.as_deref().map(text));
        });
        self.ml.unlock();
        rx.recv_timeout(WAIT).map_err(|_| "the sound server didn't answer".to_string())
    }

    fn snapshot(&mut self) -> Result<AudioState, String> {
        let default = self.default_sink()?;
        let sinks = collect!(self, get_sink_info_list() => sink_of)?;
        let streams = collect!(self, get_sink_input_info_list() => stream_of)?;
        Ok(assemble(default, sinks, streams))
    }

    // ---- changing ----

    /// Start a change and wait for the server to say whether it worked.
    fn change(&mut self, start: impl FnOnce(&mut Context, Box<dyn FnMut(bool) + 'static>) -> Operation<dyn FnMut(bool)>) -> Result<(), String> {
        let (tx, rx) = mpsc::channel();
        self.ml.lock();
        let _op = start(
            &mut self.ctx,
            Box::new(move |ok| {
                let _ = tx.send(ok);
            }),
        );
        self.ml.unlock();
        match rx.recv_timeout(WAIT) {
            Ok(true) => Ok(()),
            Ok(false) => Err("the sound server refused".into()),
            Err(_) => Err("the sound server didn't answer".into()),
        }
    }

    /// An output's name for the server: [`DEFAULT_SINK`] becomes the default output's.
    fn sink_name(&mut self, sink: &str) -> Result<String, String> {
        if sink != DEFAULT_SINK {
            return Ok(sink.to_string());
        }
        self.default_sink()?.ok_or_else(|| "there is no default output".to_string())
    }

    fn sink_volumes(&mut self, name: &str) -> Result<ChannelVolumes, String> {
        let found = collect!(self, get_sink_info_by_name(name) => sink_volumes_of)?;
        found.into_iter().next().ok_or_else(|| format!("no output called {name}"))
    }

    fn stream_volumes(&mut self, index: u32) -> Result<ChannelVolumes, String> {
        let found = collect!(self, get_sink_input_info(index) => stream_volumes_of)?;
        found.into_iter().next().ok_or_else(|| format!("no app is playing as {index}"))
    }

    fn apply(&mut self, command: AudioCommand) -> Result<(), String> {
        // Everything arrives within the protocol's limits (the node checked), but libpulse
        // panics on a NUL byte, and the daemon aborts on a panic: never leave that to chance.
        if !command.is_valid() {
            return Err("bad audio command".into());
        }
        match command {
            AudioCommand::SetSinkVolume { sink, volume } => {
                let name = self.sink_name(&sink)?;
                let mut volumes = self.sink_volumes(&name)?;
                // Keeps the balance between channels: the loudest becomes `volume`.
                volumes.scale(volume_of(volume)).ok_or("that output has no channels")?;
                self.change(|ctx, cb| ctx.introspect().set_sink_volume_by_name(&name, &volumes, Some(cb)))
            }
            AudioCommand::MuteSink { sink, muted } => {
                let name = self.sink_name(&sink)?;
                self.change(|ctx, cb| ctx.introspect().set_sink_mute_by_name(&name, muted, Some(cb)))
            }
            AudioCommand::SetDefaultSink { sink } => {
                let name = self.sink_name(&sink)?;
                self.change(|ctx, cb| ctx.set_default_sink(&name, cb))
            }
            AudioCommand::SetStreamVolume { stream, volume } => {
                let index = stream_index(&stream)?;
                let mut volumes = self.stream_volumes(index)?;
                volumes.scale(volume_of(volume)).ok_or("that app has no channels")?;
                self.change(|ctx, cb| ctx.introspect().set_sink_input_volume(index, &volumes, Some(cb)))
            }
            AudioCommand::MuteStream { stream, muted } => {
                let index = stream_index(&stream)?;
                self.change(|ctx, cb| ctx.introspect().set_sink_input_mute(index, muted, Some(cb)))
            }
            AudioCommand::MoveStream { stream, sink } => {
                let index = stream_index(&stream)?;
                let name = self.sink_name(&sink)?;
                self.change(|ctx, cb| ctx.introspect().move_sink_input_by_name(index, &name, Some(cb)))
            }
        }
    }
}

// ---- translating ----

/// Percent of full scale; 100 is the server's "normal" volume (0 dB).
fn percent_of(v: Volume) -> u16 {
    let normal = u64::from(Volume::NORMAL.0);
    ((u64::from(v.0) * 100 + normal / 2) / normal).min(u64::from(u16::MAX)) as u16
}

fn volume_of(percent: u8) -> Volume {
    let normal = u64::from(Volume::NORMAL.0);
    Volume(((u64::from(percent) * normal + 50) / 100) as u32)
}

/// Fit the protocol's limits: no NUL bytes, at most [`MAX_AUDIO_TEXT`] bytes.
fn text(s: &str) -> String {
    let s: String = s.chars().filter(|c| *c != '\0').collect();
    if s.len() <= MAX_AUDIO_TEXT {
        return s;
    }
    let mut end = MAX_AUDIO_TEXT;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

fn sink_of(i: &SinkInfo<'_>) -> Option<(u32, AudioSink)> {
    let id = text(i.name.as_deref()?);
    let name = i.description.as_deref().map(text).filter(|d| !d.is_empty()).unwrap_or_else(|| id.clone());
    let form_factor = i.proplist.get_str("device.form_factor").map(|f| text(&f)).filter(|f| !f.is_empty());
    Some((i.index, AudioSink { id, name, volume: percent_of(i.volume.max()), muted: i.mute, form_factor }))
}

/// The output's index, for [`assemble`] to name the output an app plays on.
fn stream_of(i: &SinkInputInfo<'_>) -> Option<(u32, AudioStream)> {
    // A notification's beep isn't an app playing, and a stream with no volume has nothing
    // to set: neither belongs in a mixer.
    if !i.has_volume || i.proplist.get_str("media.role").as_deref() == Some("event") {
        return None;
    }
    let app = i
        .proplist
        .get_str("application.name")
        .or_else(|| i.name.as_deref().map(str::to_string))
        .map(|a| text(&a))
        .filter(|a| !a.is_empty())
        .unwrap_or_else(|| "Unknown".into());
    let title = i.proplist.get_str("media.name").map(|t| text(&t)).filter(|t| *t != app).unwrap_or_default();
    let stream = AudioStream { id: i.index.to_string(), app, title, sink: String::new(), volume: percent_of(i.volume.max()), muted: i.mute, paused: i.corked };
    Some((i.sink, stream))
}

fn sink_volumes_of(i: &SinkInfo<'_>) -> Option<ChannelVolumes> {
    Some(i.volume)
}

fn stream_volumes_of(i: &SinkInputInfo<'_>) -> Option<ChannelVolumes> {
    Some(i.volume)
}

fn stream_index(stream: &str) -> Result<u32, String> {
    stream.parse().map_err(|_| format!("no app is playing as {stream}"))
}

/// Put what the server said together: each app on the output's id (not its index, which
/// means nothing to other devices), within the protocol's limits.
fn assemble(default: Option<String>, sinks: Vec<(u32, AudioSink)>, streams: Vec<(u32, AudioStream)>) -> AudioState {
    let names: HashMap<u32, String> = sinks.iter().map(|(index, s)| (*index, s.id.clone())).collect();
    let sinks: Vec<AudioSink> = sinks.into_iter().map(|(_, s)| s).take(MAX_AUDIO_SINKS).collect();
    let streams = streams
        .into_iter()
        .filter_map(|(index, mut s)| {
            s.sink = names.get(&index)?.clone();
            sinks.iter().any(|k| k.id == s.sink).then_some(s)
        })
        .take(MAX_AUDIO_STREAMS)
        .collect();
    let default_sink = default.map(|d| text(&d)).filter(|d| sinks.iter().any(|s| s.id == *d));
    AudioState { seq: 0, default_sink, sinks, streams }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_and_volume_round_trip() {
        for p in 0..=100u8 {
            assert_eq!(percent_of(volume_of(p)), u16::from(p), "{p}%");
        }
        assert_eq!(volume_of(100), Volume::NORMAL);
        assert_eq!(volume_of(0), Volume::MUTED);
        // A boosted output reports more than 100; nothing overflows.
        assert_eq!(percent_of(Volume(Volume::NORMAL.0 + Volume::NORMAL.0 / 2)), 150);
        assert_eq!(percent_of(Volume(u32::MAX)), u16::MAX);
    }

    #[test]
    fn text_fits_the_protocol() {
        assert_eq!(text("Spea\0kers"), "Speakers");
        let long = "é".repeat(MAX_AUDIO_TEXT); // two bytes each: cut on a character, not mid-way
        let cut = text(&long);
        assert!(cut.len() <= MAX_AUDIO_TEXT && cut.chars().all(|c| c == 'é'));
    }

    fn sink(id: &str) -> AudioSink {
        AudioSink { id: id.into(), name: id.to_uppercase(), volume: 50, muted: false, form_factor: None }
    }

    fn stream(id: &str, app: &str) -> AudioStream {
        AudioStream { id: id.into(), app: app.into(), title: String::new(), sink: String::new(), volume: 100, muted: false, paused: false }
    }

    #[test]
    fn apps_are_named_by_their_output_and_stay_within_limits() {
        let state = assemble(
            Some("hdmi".into()),
            vec![(0, sink("speakers")), (3, sink("hdmi"))],
            // The second app plays on an output that is gone from the list.
            vec![(3, stream("12", "Firefox")), (9, stream("13", "Ghost")), (0, stream("14", "mpv"))],
        );
        assert_eq!(state.default_sink.as_deref(), Some("hdmi"));
        let apps: Vec<_> = state.streams.iter().map(|s| (s.app.as_str(), s.sink.as_str())).collect();
        assert_eq!(apps, [("Firefox", "hdmi"), ("mpv", "speakers")]);
        assert!(state.is_valid());

        // A default the list doesn't have is no default.
        assert_eq!(assemble(Some("unplugged".into()), vec![(0, sink("speakers"))], vec![]).default_sink, None);

        let many = (0..MAX_AUDIO_STREAMS as u32 + 20).map(|i| (0, stream(&i.to_string(), "app"))).collect();
        let sinks = (0..MAX_AUDIO_SINKS as u32 + 5).map(|i| (i, sink(&format!("sink{i}")))).collect();
        let state = assemble(None, sinks, many);
        assert_eq!((state.sinks.len(), state.streams.len()), (MAX_AUDIO_SINKS, MAX_AUDIO_STREAMS));
        assert!(state.is_valid());
    }

    /// Against a real sound server. Run it by hand with two null outputs and an app playing
    /// (see `scripts/test-audio.sh`):
    /// `cargo test -p linksyd talks_to_a_real_sound_server -- --ignored`.
    #[tokio::test]
    #[ignore = "needs a running PulseAudio or PipeWire, with two outputs and `pacat` playing"]
    async fn talks_to_a_real_sound_server() {
        let mut pulse = Pulse::start().await.expect("a sound server to talk to");
        let state = pulse.snapshot().await.unwrap();
        assert!(state.is_valid() && state.sinks.len() >= 2, "{state:?}");
        let first = state.default_sink.clone().expect("a default output");
        let other = state.sinks.iter().find(|s| s.id != first).unwrap().id.clone();
        let volume_of_first = |s: &AudioState| s.sinks.iter().find(|k| k.id == first).unwrap().volume;

        // An output's volume and mute, by the default's stand-in id and by its own id.
        pulse.settle();
        pulse.apply(AudioCommand::SetSinkVolume { sink: DEFAULT_SINK.into(), volume: 37 }).await.unwrap();
        assert_eq!(volume_of_first(&pulse.snapshot().await.unwrap()), 37);
        tokio::time::timeout(Duration::from_secs(2), pulse.changed()).await.expect("a change notification").expect("still connected");
        pulse.apply(AudioCommand::MuteSink { sink: first.clone(), muted: true }).await.unwrap();
        let after = pulse.snapshot().await.unwrap();
        assert!(after.sinks.iter().find(|k| k.id == first).unwrap().muted);
        pulse.apply(AudioCommand::MuteSink { sink: first.clone(), muted: false }).await.unwrap();
        pulse.apply(AudioCommand::SetSinkVolume { sink: first.clone(), volume: 100 }).await.unwrap();
        assert_eq!(volume_of_first(&pulse.snapshot().await.unwrap()), 100);

        // The default output changes, and back.
        pulse.apply(AudioCommand::SetDefaultSink { sink: other.clone() }).await.unwrap();
        assert_eq!(pulse.snapshot().await.unwrap().default_sink.as_deref(), Some(other.as_str()));
        pulse.apply(AudioCommand::SetDefaultSink { sink: first.clone() }).await.unwrap();

        // The app that is playing: its volume, its mute, and where it plays.
        let state = pulse.snapshot().await.unwrap();
        let app = state.streams.iter().find(|s| s.app == "pacat").expect("`pacat` playing").clone();
        pulse.apply(AudioCommand::SetStreamVolume { stream: app.id.clone(), volume: 25 }).await.unwrap();
        pulse.apply(AudioCommand::MuteStream { stream: app.id.clone(), muted: true }).await.unwrap();
        let now = |s: &AudioState| s.streams.iter().find(|k| k.id == app.id).unwrap().clone();
        let got = now(&pulse.snapshot().await.unwrap());
        assert_eq!((got.volume, got.muted), (25, true));
        pulse.apply(AudioCommand::MoveStream { stream: app.id.clone(), sink: other.clone() }).await.unwrap();
        assert_eq!(now(&pulse.snapshot().await.unwrap()).sink, other);
        pulse.apply(AudioCommand::MoveStream { stream: app.id.clone(), sink: DEFAULT_SINK.into() }).await.unwrap();
        assert_eq!(now(&pulse.snapshot().await.unwrap()).sink, first);

        // What can't be done is an error, never a panic or a hang.
        assert!(pulse.apply(AudioCommand::MuteSink { sink: "no-such-output".into(), muted: true }).await.is_err());
        assert!(pulse.apply(AudioCommand::SetStreamVolume { stream: "999999".into(), volume: 10 }).await.is_err());
        assert!(pulse.apply(AudioCommand::SetStreamVolume { stream: "not-a-number".into(), volume: 10 }).await.is_err());
        assert!(pulse.apply(AudioCommand::SetSinkVolume { sink: "a\0b".into(), volume: 10 }).await.is_err());
        assert!(pulse.snapshot().await.is_ok(), "still alive after the bad ones");
    }
}
