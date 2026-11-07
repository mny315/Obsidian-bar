# Automatic media sources

Auto selects sources using active PipeWire playback streams instead of trusting
MPRIS `PlaybackStatus` alone. A stale `Playing` flag from a browser preview cannot
keep an application selected after its playback stream stops. The selected
player's transport state and capabilities always come from MPRIS, including
pauses made in the source application. A running stream can outlive a pause or
belong to another browser tab; muted playback can still be playing.

- A running output stream with nonzero volume and no mute is active. Capture
  streams, sinks, corked streams, and idle streams do not select a player.
- The most recently activated application wins. When it stops, Auto returns to
  an application that still has an active stream. With no active streams the
  indicator hides, except for a player explicitly paused from the bar, which
  remains available to resume.
- Stream process IDs and their parent processes are matched to the MPRIS D-Bus
  owner's process ID. Application names are a fallback when process information
  is unavailable. This also handles browser audio subprocesses.
- An active application without matching MPRIS controls is displayed by name
  with transport buttons disabled. Its audio must not redirect controls to an
  unrelated player.
- Choosing a source manually still pins it. Selecting Auto resumes automatic
  selection. If PipeWire monitoring is unavailable, selection falls back to
  MPRIS status.

The monitor uses one persistent `pw-dump --monitor --no-colors` process, restarted
after disconnects and terminated when the controller is dropped. The flake's
existing `OBSIDIAN_BAR_PW_DUMP_BIN` supplies its executable; no extra service or
browser extension is needed.

PipeWire identifies application streams, not individual browser tabs or video
elements. It also does not guarantee that samples contain audible sound: an
application can keep a running stream filled with silence. If a browser assigns
its MPRIS session to the wrong video inside the same application, the bar cannot
repair that assignment. Play/pause sends MPRIS `PlayPause` so that the source
toggles its actual state without the bar guessing from stream activity. Sources
that cannot pause receive `Play` when starting playback. The browser still owns
the command's target. Invalidated MPRIS properties are fetched again before
updating the controls.

## Checks

Run ordinary tests with `cargo test`. The following additional tests are ignored
by default because they require system services:

```sh
dbus-run-session -- cargo test audio_activity_drives_selection_and_transport_commands -- --ignored
dbus-run-session -- cargo test source_signals_and_transport_stay_in_sync_when_audio_activity_lags -- --ignored
cargo test live_monitor_follows_stream_creation_and_removal -- --ignored
```

The D-Bus tests use fixture MPRIS players and never control desktop applications.
The PipeWire test uses the current session and creates a temporary playback
stream containing zero samples; it checks discovery, removal, and monitor shutdown.
Set `CARGO_TARGET_DIR` to a temporary directory to keep test binaries out of the
source tree, and remove that directory after testing.
