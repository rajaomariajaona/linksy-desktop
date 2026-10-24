//! Audio, both ways, over the sound server (libpulse: PulseAudio, or PipeWire through
//! pipewire-pulse):
//! - this computer's outputs and the apps playing on them go to devices that ask
//!   (`audio_watch`), again each time something changes;
//! - their commands (an output's or an app's volume or mute, the default output, where an
//!   app plays) come back to the sound server, from any paired device.
//!
//! Nothing is connected to the sound server while no device is watching and no command is
//! being carried out.

use std::collections::HashSet;
use std::time::Duration;

use linksy_core::{AudioCommand, AudioState, Event, Fingerprint, Node};
use tokio::sync::broadcast::error::RecvError;
use tokio::time::Instant;
use tracing::{debug, warn};

use crate::pulse::Pulse;

/// Several changes tend to come together (a volume being dragged, say): send once they settle.
const SETTLE: Duration = Duration::from_millis(150);
/// Watchers waiting for a sound server that went away: look again this often.
const RETRY: Duration = Duration::from_secs(10);

pub async fn run(node: Node) {
    let mut events = node.subscribe();
    let mut audio = Audio { node, watchers: HashSet::new(), pulse: None, sent: None, seq: 0, retry: None };
    loop {
        tokio::select! {
            changed = changed(&mut audio.pulse) => match changed {
                Some(()) => {
                    tokio::time::sleep(SETTLE).await;
                    audio.push().await;
                }
                None => audio.lost().await,
            },
            () = until(audio.retry) => audio.reconnect().await,
            event = events.recv() => match event {
                Ok(Event::AudioWatch { from, on: true }) => audio.watch(from.id).await,
                Ok(Event::AudioWatch { from, on: false }) => audio.unwatch(&from.id),
                Ok(Event::Disconnected { id }) => audio.unwatch(&id),
                Ok(Event::AudioCommand { command, .. }) => audio.command(command).await,
                Ok(_) | Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => break,
            },
        }
        audio.close_if_idle();
    }
}

struct Audio {
    node: Node,
    /// Devices that asked to be kept up to date.
    watchers: HashSet<Fingerprint>,
    /// The connection to the sound server: while someone watches, or a command is running.
    pulse: Option<Pulse>,
    /// What the watchers were last told, to stay quiet when a change doesn't show in it.
    sent: Option<AudioState>,
    seq: u64,
    /// When to try the sound server again, after losing it while somebody watches.
    retry: Option<Instant>,
}

impl Audio {
    /// A time-based `seq` that always grows, as the protocol wants.
    fn next_seq(&mut self) -> u64 {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
        self.seq = now.max(self.seq + 1);
        self.seq
    }

    /// Connect to the sound server unless already connected.
    async fn connect(&mut self) -> bool {
        if self.pulse.is_some() {
            return true;
        }
        match Pulse::start().await {
            Ok(p) => {
                self.pulse = Some(p);
                self.retry = None;
                true
            }
            Err(e) => {
                warn!("can't reach this computer's sound server: {e}");
                false
            }
        }
    }

    async fn snapshot(&mut self) -> Option<AudioState> {
        let pulse = self.pulse.as_ref()?;
        match pulse.snapshot().await {
            Ok(mut state) => {
                state.seq = self.next_seq();
                state.is_valid().then_some(state)
            }
            Err(e) => {
                debug!("can't read this computer's audio: {e}");
                None
            }
        }
    }

    /// Nothing to play on: no outputs, no apps (a device then shows that, rather than stale levels).
    fn nothing(&mut self) -> AudioState {
        AudioState { seq: self.next_seq(), default_sink: None, sinks: Vec::new(), streams: Vec::new() }
    }

    async fn watch(&mut self, id: Fingerprint) {
        self.watchers.insert(id);
        let state = if self.connect().await { self.snapshot().await } else { None };
        let state = match state {
            Some(state) => state,
            None => {
                // Look again later: the sound server may only be starting.
                self.retry.get_or_insert_with(|| Instant::now() + RETRY);
                self.nothing()
            }
        };
        let _ = self.node.send_audio_state(&id, &state).await;
        if self.sent.is_none() && self.pulse.is_some() {
            self.sent = Some(state);
        }
    }

    fn unwatch(&mut self, id: &Fingerprint) {
        self.watchers.remove(id);
    }

    /// Let go of the sound server when nobody needs it.
    fn close_if_idle(&mut self) {
        if self.watchers.is_empty() {
            self.pulse = None;
            self.sent = None;
            self.retry = None;
        }
    }

    /// Tell every watcher about a change, unless it doesn't show in what they know.
    async fn push(&mut self) {
        if let Some(p) = self.pulse.as_mut() {
            p.settle();
        }
        let Some(state) = self.snapshot().await else { return };
        if self.sent.as_ref().is_some_and(|s| same(s, &state)) {
            return;
        }
        self.broadcast(&state).await;
        self.sent = Some(state);
    }

    async fn broadcast(&mut self, state: &AudioState) {
        for id in self.watchers.clone() {
            if self.node.send_audio_state(&id, state).await.is_err() {
                self.watchers.remove(&id);
            }
        }
    }

    /// The connection to the sound server broke (it stopped, or restarted).
    async fn lost(&mut self) {
        self.pulse = None;
        self.sent = None;
        if self.watchers.is_empty() {
            return;
        }
        warn!("lost this computer's sound server");
        let state = self.nothing();
        self.broadcast(&state).await;
        self.retry = Some(Instant::now() + RETRY);
    }

    async fn reconnect(&mut self) {
        self.retry = None;
        if self.watchers.is_empty() {
            return;
        }
        if self.connect().await {
            self.push().await;
        } else {
            self.retry = Some(Instant::now() + RETRY);
        }
    }

    async fn command(&mut self, command: AudioCommand) {
        if !self.connect().await {
            return;
        }
        let Some(pulse) = self.pulse.as_ref() else { return };
        if let Err(e) = pulse.apply(command).await {
            debug!("audio command failed: {e}");
        }
        // The change comes back as a signal when someone watches; nothing else to do here.
    }
}

/// Resolves when the sound server reports a change, and with `None` when it is gone;
/// never while there is no connection.
async fn changed(pulse: &mut Option<Pulse>) -> Option<()> {
    match pulse.as_mut() {
        Some(p) => p.changed().await,
        None => std::future::pending().await,
    }
}

/// Resolves at `at`, never when there is none.
async fn until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at).await,
        None => std::future::pending().await,
    }
}

/// The same outputs, apps and levels (the `seq` always differs).
fn same(a: &AudioState, b: &AudioState) -> bool {
    a.default_sink == b.default_sink && a.sinks == b.sinks && a.streams == b.streams
}

#[cfg(test)]
mod tests {
    use super::*;
    use linksy_core::{AudioSink, AudioStream};

    #[test]
    fn a_new_seq_alone_is_no_change() {
        let sink = AudioSink { id: "s".into(), name: "S".into(), volume: 30, muted: false, form_factor: None };
        let a = AudioState { seq: 1, default_sink: Some("s".into()), sinks: vec![sink.clone()], streams: vec![] };
        let mut b = AudioState { seq: 2, ..a.clone() };
        assert!(same(&a, &b));
        b.sinks[0].volume = 31;
        assert!(!same(&a, &b));
        let mut c = AudioState { seq: 3, ..a.clone() };
        c.streams.push(AudioStream { id: "1".into(), app: "mpv".into(), title: String::new(), sink: "s".into(), volume: 100, muted: false, paused: false });
        assert!(!same(&a, &c));
    }

    async fn device(dir: &tempfile::TempDir, name: &str) -> Node {
        let mut config = linksy_core::Config::new(dir.path().join("data"), dir.path().join("downloads"), name);
        config.bind = std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST);
        config.discovery = false;
        let node = Node::start(config).await.unwrap();
        // Say yes to being paired with.
        let (answering, mut events) = (node.clone(), node.subscribe());
        tokio::spawn(async move {
            while let Ok(event) = events.recv().await {
                if let Event::PairingRequest { request, .. } = event {
                    answering.respond(request, true);
                }
            }
        });
        node
    }

    /// The next audio state that `fits`, from `events`.
    async fn state_where(events: &mut tokio::sync::broadcast::Receiver<Event>, fits: impl Fn(&AudioState) -> bool) -> AudioState {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Event::AudioState { state, .. } = events.recv().await.expect("event stream closed")
                    && fits(&state)
                {
                    return state;
                }
            }
        })
        .await
        .expect("timed out waiting for an audio state")
    }

    /// The whole thing, over a real sound server: a phone watches a computer's audio and
    /// controls it. Run by hand with two null outputs and an app playing (see
    /// `scripts/test-audio.sh`).
    #[tokio::test]
    #[ignore = "needs a running PulseAudio or PipeWire, with two outputs and `pacat` playing"]
    async fn a_paired_device_watches_and_controls_the_sound_server() {
        let (pc_dir, phone_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
        let (pc, phone) = (device(&pc_dir, "Computer").await, device(&phone_dir, "Phone").await);
        let mut connected = pc.subscribe();
        phone.pair(pc.local_addr().unwrap()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while !matches!(connected.recv().await, Ok(Event::Connected { .. })) {}
        })
        .await
        .expect("the phone to connect");
        tokio::spawn(run(pc.clone()));
        let (pc_id, mut on_phone) = (pc.info().id, phone.subscribe());

        // Watching: the first state arrives at once, with what the sound server has.
        phone.watch_audio(&pc_id, true).await.unwrap();
        let state = state_where(&mut on_phone, |_| true).await;
        assert!(state.is_valid() && state.sinks.len() >= 2 && state.default_sink.is_some(), "{state:?}");
        let app = state.streams.iter().find(|s| s.app == "pacat").expect("`pacat` playing").clone();

        // A command changes the sound server, and the change comes back as a new state.
        let command = |command| async { phone.audio_command(&pc_id, command).await.unwrap() };
        command(AudioCommand::SetSinkVolume { sink: linksy_core::protocol::DEFAULT_SINK.into(), volume: 20 }).await;
        let default = state.default_sink.clone().unwrap();
        let state = state_where(&mut on_phone, |s| s.sinks.iter().any(|k| k.id == default && k.volume == 20)).await;
        assert!(state.seq > 0);
        command(AudioCommand::SetStreamVolume { stream: app.id.clone(), volume: 55 }).await;
        state_where(&mut on_phone, |s| s.streams.iter().any(|k| k.id == app.id && k.volume == 55)).await;

        // Not watching, commands still work (and nothing is sent back).
        phone.watch_audio(&pc_id, false).await.unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        command(AudioCommand::MuteStream { stream: app.id.clone(), muted: true }).await;
        assert!(tokio::time::timeout(Duration::from_secs(1), state_where(&mut on_phone, |_| true)).await.is_err(), "sent to a device that stopped watching");
        // ...as the next look shows.
        phone.watch_audio(&pc_id, true).await.unwrap();
        state_where(&mut on_phone, |s| s.streams.iter().any(|k| k.id == app.id && k.muted)).await;
    }
}
