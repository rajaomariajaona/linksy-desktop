//! `linksy audio`: look at another device's audio and change it.
//!
//! ```text
//! linksy audio <device>                          outputs, and the apps playing
//! linksy audio <device> volume <0-100> [--output NAME | --app NAME]
//! linksy audio <device> mute|unmute [--output NAME | --app NAME]
//! linksy audio <device> default <output>
//! linksy audio <device> move <app> <output>
//! ```
//!
//! Without `--output` or `--app`, volume and mute act on the default output. Names are
//! matched without regard to case, as a whole or as a part that only one output or app has.

use linksy_ipc::linksy_core::protocol::DEFAULT_SINK;
use linksy_ipc::linksy_core::{AudioCommand, AudioState};
use linksy_ipc::{Client, ClientError};

pub const USAGE: &str = "Usage:
  linksy audio <device>
  linksy audio <device> volume <0-100> [--output NAME | --app NAME]
  linksy audio <device> mute|unmute [--output NAME | --app NAME]
  linksy audio <device> default <output>
  linksy audio <device> move <app> <output>";

#[derive(Debug, PartialEq)]
enum Target {
    Default,
    Output(String),
    App(String),
}

#[derive(Debug, PartialEq)]
enum Action {
    Show,
    Volume(u8, Target),
    Mute(bool, Target),
    Default(String),
    Move(String, String),
}

pub async fn run(c: &Client, device: &str, rest: &[String]) -> Result<(), String> {
    let err = |e: ClientError| e.to_string();
    let action = parse(rest)?;
    if action == Action::Show {
        let state = c.audio_state(device).await.map_err(err)?;
        print!("{}", show(&state));
        return Ok(());
    }
    // Only a name needs the device's list; the default output goes by its own id.
    let state = if needs_names(&action) { c.audio_state(device).await.map_err(err)? } else { AudioState::default() };
    let command = build(&action, &state)?;
    c.audio_command(device, command).await.map_err(err)?;
    println!("Asked {device} to {}.", describe(&action));
    Ok(())
}

fn parse(args: &[String]) -> Result<Action, String> {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        [] => Ok(Action::Show),
        ["volume", value, target @ ..] => Ok(Action::Volume(percent(value)?, target_of(target)?)),
        ["mute", target @ ..] => Ok(Action::Mute(true, target_of(target)?)),
        ["unmute", target @ ..] => Ok(Action::Mute(false, target_of(target)?)),
        ["default", output] => Ok(Action::Default(output.to_string())),
        ["move", app, output] => Ok(Action::Move(app.to_string(), output.to_string())),
        _ => Err(USAGE.to_string()),
    }
}

/// `40` or `40%`; nothing over 100 (nobody makes a device louder than full scale).
fn percent(value: &str) -> Result<u8, String> {
    value
        .trim_end_matches('%')
        .parse::<u8>()
        .ok()
        .filter(|v| *v <= 100)
        .ok_or_else(|| format!("A volume is a number from 0 to 100, not \"{value}\"."))
}

fn target_of(args: &[&str]) -> Result<Target, String> {
    match args {
        [] => Ok(Target::Default),
        ["--output", name] => Ok(Target::Output(name.to_string())),
        ["--app", name] => Ok(Target::App(name.to_string())),
        _ => Err(USAGE.to_string()),
    }
}

fn needs_names(action: &Action) -> bool {
    !matches!(action, Action::Show | Action::Volume(_, Target::Default) | Action::Mute(_, Target::Default))
}

/// The command for `action`, with names turned into the ids `state` knows them by.
fn build(action: &Action, state: &AudioState) -> Result<AudioCommand, String> {
    Ok(match action {
        Action::Show => return Err(USAGE.to_string()),
        Action::Volume(volume, target) => match target {
            Target::Default => AudioCommand::SetSinkVolume { sink: DEFAULT_SINK.into(), volume: *volume },
            Target::Output(name) => AudioCommand::SetSinkVolume { sink: output(state, name)?, volume: *volume },
            Target::App(name) => AudioCommand::SetStreamVolume { stream: app(state, name)?, volume: *volume },
        },
        Action::Mute(muted, target) => match target {
            Target::Default => AudioCommand::MuteSink { sink: DEFAULT_SINK.into(), muted: *muted },
            Target::Output(name) => AudioCommand::MuteSink { sink: output(state, name)?, muted: *muted },
            Target::App(name) => AudioCommand::MuteStream { stream: app(state, name)?, muted: *muted },
        },
        Action::Default(name) => AudioCommand::SetDefaultSink { sink: output(state, name)? },
        Action::Move(name, to) => AudioCommand::MoveStream { stream: app(state, name)?, sink: output(state, to)? },
    })
}

fn describe(action: &Action) -> String {
    let of = |target: &Target| match target {
        Target::Default => "the default output".to_string(),
        Target::Output(name) => format!("output \"{name}\""),
        Target::App(name) => format!("\"{name}\""),
    };
    match action {
        Action::Show => String::new(),
        Action::Volume(v, target) => format!("set the volume of {} to {v}%", of(target)),
        Action::Mute(true, target) => format!("mute {}", of(target)),
        Action::Mute(false, target) => format!("unmute {}", of(target)),
        Action::Default(name) => format!("make \"{name}\" the default output"),
        Action::Move(name, to) => format!("play \"{name}\" on \"{to}\""),
    }
}

fn output(state: &AudioState, wanted: &str) -> Result<String, String> {
    pick("output", wanted, &state.sinks, |s| [&s.id, &s.name], |s| format!("{} ({})", s.name, s.id)).map(|s| s.id.clone())
}

fn app(state: &AudioState, wanted: &str) -> Result<String, String> {
    pick("app", wanted, &state.streams, |s| [&s.app, &s.id], |s| format!("{} (#{})", s.app, s.id)).map(|s| s.id.clone())
}

/// The one item `wanted` names: a whole name, else a part of a name that only one item has.
fn pick<'a, T>(kind: &str, wanted: &str, items: &'a [T], names: impl Fn(&T) -> [&str; 2], label: impl Fn(&T) -> String) -> Result<&'a T, String> {
    let wanted_lower = wanted.to_lowercase();
    let matching = |whole: bool| -> Vec<&'a T> {
        items
            .iter()
            .filter(|item| {
                names(item).iter().any(|n| {
                    let n = n.to_lowercase();
                    if whole { n == wanted_lower } else { n.contains(&wanted_lower) }
                })
            })
            .collect()
    };
    let found = match matching(true) {
        whole if !whole.is_empty() => whole,
        _ => matching(false),
    };
    match found.as_slice() {
        [one] => Ok(one),
        [] => Err(format!("No {kind} matches \"{wanted}\". See them with: linksy audio <device>")),
        several => {
            let list: Vec<String> = several.iter().map(|i| label(i)).collect();
            Err(format!("\"{wanted}\" matches several {kind}s: {}. Say which, by its name or its #id.", list.join(", ")))
        }
    }
}

/// What to print for `linksy audio <device>`.
fn show(state: &AudioState) -> String {
    let mut out = String::new();
    let width = state.sinks.iter().map(|s| s.name.chars().count()).chain(state.streams.iter().map(|s| label(s).chars().count())).max().unwrap_or(0);
    if state.sinks.is_empty() {
        out.push_str("No audio outputs.\n");
    } else {
        out.push_str("Outputs\n");
        for s in &state.sinks {
            let default = if state.default_sink.as_deref() == Some(s.id.as_str()) { "●" } else { " " };
            let muted = if s.muted { "  muted" } else { "" };
            out.push_str(&format!("  {default} {:<width$}  {:>3}%{muted}\n", s.name, s.volume));
        }
    }
    if state.streams.is_empty() {
        out.push_str("Nothing is playing.\n");
    } else {
        out.push_str("Apps\n");
        for s in &state.streams {
            let mut notes = Vec::new();
            if s.muted {
                notes.push("muted".to_string());
            }
            if s.paused {
                notes.push("paused".to_string());
            }
            let on = state.sinks.iter().find(|k| k.id == s.sink).map(|k| k.name.as_str()).unwrap_or(&s.sink);
            notes.push(format!("on {on}"));
            out.push_str(&format!("    {:<width$}  {:>3}%  {}\n", label(s), s.volume, notes.join(", ")));
        }
    }
    out
}

fn label(s: &linksy_ipc::linksy_core::AudioStream) -> String {
    if s.title.is_empty() { s.app.clone() } else { format!("{} – {}", s.app, s.title) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use linksy_ipc::linksy_core::{AudioSink, AudioStream};

    fn words(line: &str) -> Vec<String> {
        line.split_whitespace().map(String::from).collect()
    }

    fn state() -> AudioState {
        let sink = |id: &str, name: &str, volume, muted| AudioSink { id: id.into(), name: name.into(), volume, muted, form_factor: None };
        let stream = |id: &str, app: &str, title: &str, sink: &str, volume, paused| AudioStream {
            id: id.into(),
            app: app.into(),
            title: title.into(),
            sink: sink.into(),
            volume,
            muted: false,
            paused,
        };
        AudioState {
            seq: 1,
            default_sink: Some("alsa_output.analog".into()),
            sinks: vec![sink("alsa_output.analog", "Built-in Audio Analog Stereo", 40, false), sink("alsa_output.hdmi", "HDMI / DisplayPort", 100, true)],
            streams: vec![
                stream("57", "Firefox", "Song", "alsa_output.analog", 100, false),
                stream("61", "Firefox", "", "alsa_output.hdmi", 70, true),
                stream("62", "mpv", "", "alsa_output.analog", 25, false),
            ],
        }
    }

    #[test]
    fn commands_parse() {
        assert_eq!(parse(&words("")), Ok(Action::Show));
        assert_eq!(parse(&words("volume 40")), Ok(Action::Volume(40, Target::Default)));
        assert_eq!(parse(&words("volume 40% --app mpv")), Ok(Action::Volume(40, Target::App("mpv".into()))));
        assert_eq!(parse(&words("mute --output hdmi")), Ok(Action::Mute(true, Target::Output("hdmi".into()))));
        assert_eq!(parse(&words("unmute")), Ok(Action::Mute(false, Target::Default)));
        assert_eq!(parse(&words("default hdmi")), Ok(Action::Default("hdmi".into())));
        assert_eq!(parse(&words("move mpv hdmi")), Ok(Action::Move("mpv".into(), "hdmi".into())));
        // Nothing above full scale, nothing that isn't a number, nothing half-said.
        for bad in ["volume 101", "volume -5", "volume loud", "volume", "mute --app", "mute --both x", "default", "move mpv", "shout"] {
            assert!(parse(&words(bad)).is_err(), "{bad}");
        }
    }

    #[test]
    fn only_names_need_the_devices_list() {
        assert!(!needs_names(&parse(&words("volume 40")).unwrap()));
        assert!(!needs_names(&parse(&words("mute")).unwrap()));
        assert!(needs_names(&parse(&words("volume 40 --app mpv")).unwrap()));
        assert!(needs_names(&parse(&words("default hdmi")).unwrap()));
    }

    #[test]
    fn names_become_ids() {
        let state = state();
        let go = |line: &str| build(&parse(&words(line)).unwrap(), &state);
        // No state needed for the default output: it goes by its stand-in id.
        assert_eq!(go("volume 30"), Ok(AudioCommand::SetSinkVolume { sink: DEFAULT_SINK.into(), volume: 30 }));
        // A part of a name, in any case, when only one output has it.
        assert_eq!(go("volume 30 --output hdmi"), Ok(AudioCommand::SetSinkVolume { sink: "alsa_output.hdmi".into(), volume: 30 }));
        assert_eq!(go("mute --output BUILT-IN"), Ok(AudioCommand::MuteSink { sink: "alsa_output.analog".into(), muted: true }));
        assert_eq!(go("default displayport"), Ok(AudioCommand::SetDefaultSink { sink: "alsa_output.hdmi".into() }));
        assert_eq!(go("unmute --app mpv"), Ok(AudioCommand::MuteStream { stream: "62".into(), muted: false }));
        assert_eq!(go("move mpv hdmi"), Ok(AudioCommand::MoveStream { stream: "62".into(), sink: "alsa_output.hdmi".into() }));
        // Two Firefox streams: the name alone is ambiguous, the id isn't.
        let err = go("volume 10 --app firefox").unwrap_err();
        assert!(err.contains("several apps") && err.contains("#57") && err.contains("#61"), "{err}");
        assert_eq!(go("volume 10 --app 61"), Ok(AudioCommand::SetStreamVolume { stream: "61".into(), volume: 10 }));
        // Nothing by that name.
        assert!(go("mute --app spotify").unwrap_err().contains("No app matches"));
        assert!(go("default bluetooth").unwrap_err().contains("No output matches"));
    }

    #[test]
    fn a_whole_name_beats_a_part_of_another() {
        let mut state = state();
        state.sinks[1].name = "HDMI".into();
        state.sinks.push(AudioSink { id: "alsa_output.hdmi2".into(), name: "HDMI 2".into(), volume: 10, muted: false, form_factor: None });
        // "hdmi" is a part of both names but all of one.
        assert_eq!(output(&state, "hdmi"), Ok("alsa_output.hdmi".into()));
        assert_eq!(output(&state, "hdmi 2"), Ok("alsa_output.hdmi2".into()));
    }

    #[test]
    fn the_listing_reads_well() {
        let shown = show(&state());
        let lines: Vec<&str> = shown.lines().collect();
        assert_eq!(lines[0], "Outputs");
        assert!(lines[1].starts_with("  ● Built-in Audio Analog Stereo") && lines[1].ends_with(" 40%"), "{}", lines[1]);
        assert!(lines[2].starts_with("    HDMI / DisplayPort") && lines[2].ends_with("100%  muted"), "{}", lines[2]);
        assert_eq!(lines[3], "Apps");
        assert!(lines[4].contains("Firefox – Song") && lines[4].ends_with("100%  on Built-in Audio Analog Stereo"), "{}", lines[4]);
        assert!(lines[5].ends_with(" 70%  paused, on HDMI / DisplayPort"), "{}", lines[5]);
        assert_eq!(show(&AudioState::default()), "No audio outputs.\nNothing is playing.\n");
    }
}
