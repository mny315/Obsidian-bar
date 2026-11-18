# Automatic media sources

Auto selects sources using audio levels measured separately for each PipeWire
playback stream. An open stream carrying silence and a stale MPRIS `Playing`
flag cannot take over selection while the monitor is available. The selected
player's transport state and capabilities always come from MPRIS, including
pauses made in the source application. A running stream can outlive a pause or
belong to another browser tab; muted playback can still be playing.

- A running, unmuted output stream with nonzero volume becomes active when
  its samples exceed the playback meter's noise floor. Brief gaps of up to
  400 ms do not change the source. Capture streams, sinks, corked streams,
  and idle streams do not select a player.
- The most recently activated application wins. When it stops, Auto returns to
  an application that still has an active stream. With no active streams the
  last audible player remains available to resume while paused, including pauses
  made in the source application. This works regardless of whether the pause
  notification arrives before or after the stream stops. An idle player with
  only a stale `Playing` flag does not keep the indicator visible.
- Stream process IDs and their parent processes are matched to the MPRIS D-Bus
  owner's process ID. Application names are a fallback when process information
  is unavailable. This also handles browser audio subprocesses.
- An active application without matching MPRIS controls is displayed by name
  with transport buttons disabled. Its audio must not redirect controls to an
  unrelated player.
- Choosing a source manually still pins it. Selecting Auto resumes automatic
  selection. If PipeWire monitoring is unavailable, selection falls back to
  MPRIS status.

The monitor uses one persistent `pw-dump --monitor --no-colors` process to
discover playback nodes. It measures each eligible node using a passive PipeWire
connection with an explicit target. The connections yield peak levels in memory
and are released when the controller is dropped. The flake supplies `pw-dump`
through `OBSIDIAN_BAR_PW_DUMP_BIN`.

PipeWire identifies playback streams and their applications. The browser owns
the mapping from its MPRIS session to a tab or video. Play/pause sends MPRIS
`PlayPause` so that the source
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
OBSIDIAN_BAR_TEST_AUDIO_SINK=obsidian-test-sink cargo test live_monitor_follows_signal_in_each_playback_stream -- --ignored
```

The D-Bus tests use fixture MPRIS players and never control desktop applications.
Run the PipeWire check in an isolated PipeWire session with a null sink named
`obsidian-test-sink`. The test checks that a second running stream remains
unselected while silent, appears when it emits a tone, and goes away when it
returns to silence. It also checks stream removal and monitor shutdown.
Set `CARGO_TARGET_DIR` to a temporary directory to keep test binaries out of the
source tree, and remove that directory after testing.
