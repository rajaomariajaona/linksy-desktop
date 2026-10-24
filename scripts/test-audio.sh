#!/bin/sh
# Check the daemon's audio backend against a real sound server: a private PulseAudio with
# two null outputs and one app playing, away from your own audio. Needs pulseaudio and
# pactl/pacat (pulseaudio-utils), and a user that isn't root.
#
#   scripts/test-audio.sh                 runs the daemon's ignored audio tests with cargo
#   scripts/test-audio.sh CMD [ARGS…]     runs CMD instead, with the sound server around it
set -eu
cd "$(dirname "$0")/.."

dir=$(mktemp -d)
pids=""
cleanup() {
    for pid in $pids; do kill "$pid" 2>/dev/null || true; done
    rm -rf "$dir"
}
trap cleanup EXIT INT TERM

# -n: no default config, nothing of yours; only what is loaded here.
pulseaudio -n --daemonize=no --use-pid-file=no --exit-idle-time=-1 --disable-shm \
    --log-target=stderr \
    -L "module-native-protocol-unix socket=$dir/native" \
    -L "module-null-sink sink_name=linksy_a sink_properties=device.description=Linksy_A" \
    -L "module-null-sink sink_name=linksy_b sink_properties=device.description=Linksy_B" \
    2>"$dir/pulseaudio.log" &
pids="$pids $!"
export PULSE_SERVER="unix:$dir/native"

for _ in $(seq 1 50); do
    pactl info >/dev/null 2>&1 && break
    sleep 0.1
done
pactl info >/dev/null || { cat "$dir/pulseaudio.log"; echo "the private PulseAudio did not start" >&2; exit 1; }
pactl set-default-sink linksy_a

# An app playing silence on the first output.
pacat --playback --device=linksy_a --rate=8000 --channels=1 --format=s16le </dev/zero &
pids="$pids $!"
sleep 0.5

if [ $# -gt 0 ]; then
    "$@"
else
    cargo test -p linksyd -- --ignored --nocapture
fi
