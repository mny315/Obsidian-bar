Optimization audit, 2026-10-17.

The equalizer update implementation (`src/widgets/audio_spectrum.rs`) and
dependency versions are unchanged. The work targets redundant backend reads,
allocations, scheduled callbacks and hidden-widget updates. The requested
monitor dropdown animations retain the shared glass surface; directory list
rows keep their appearance while their widgets are now recycled.

- Shared cancelable timers replace obsolete callbacks left queued after debounce
  replacements. Backend caches serialize concurrent reads and reject stale cache
  writes after invalidation. Snapshot consumers share immutable audio, network,
  monitor and tray data.
- Wi-Fi scan completion timers cancel when scanning finishes or tracking is
  reset. Bluetooth discovery deadlines, stop retries and notice expiry replace
  or cancel their pending source instead of leaving obsolete wakeups queued.
- Clock minute updates have an owner-held timer, so removing a clock cancels its
  pending update immediately. The calendar fixture also checks controller and
  popup release while a minute update is still pending.
- Command capture drains stdout and stderr on the calling worker instead of
  creating reader and waiter threads for every command. Output limits, process
  group termination, deadlines and the fallback without pidfds remain covered.
- Audio-output parsing reads one graph object at a time and skips unused
  parameter trees and properties during deserialization. Sink identities,
  ordering and the first default-output metadata entry remain unchanged;
  irregular irrelevant entries and malformed JSON are covered.
- The system monitor reads network counters and interface choices together,
  defers hidden GTK updates, preserves section widgets when changing dimensions,
  and scans Intel GPU clients once per sample across devices. Frequency fallback
  directory walks run only when earlier paths fail.
- New monitor samples copy retained readings rather than cloning collections
  that are immediately replaced. CPU usage reads stop after the aggregate
  `/proc/stat` row. Metric option lists skip value formatting, and unchanged
  default metric names avoid allocating replacement strings.
- CPU collection has its own module and skips sources for known hidden metrics.
  Load average and process counts share one read. Initial discovery still reads
  every source, and missing readings retry until newly supported metrics appear.
  Usage counters stay warm while the CPU section is active, preserving the
  current sampling interval when the usage row is shown again.
- Memory, mounted storage and shared filesystem readers are independent of
  sampler orchestration. System battery capacity and aggregation live beside
  the peripheral battery sources; the existing weighted-capacity and
  device-exclusion checks remain in place.
- Sensor discovery caches descriptors and labels, while each sample still
  reads fresh values and checks runtime suspension before hardware reads.
  Top-level device identities detect hotplug, removal and replacement. Metadata
  refreshes after 30 seconds; incomplete discovery retries on the next sample.
  GPU PCI classification caches both GPU and non-GPU devices by inode identity.
- Mount parsing rejects irrelevant filesystems before decoding fields, borrows
  paths without escapes and decodes escaped paths in one pass. Mount ordering,
  bind-mount deduplication, Btrfs subvolumes and literal backslashes are covered.
- Niri sends compact window snapshots and suppresses changes to unused fields.
  GTK receives combined bursts with bounded processing between yields. Invalid
  object references reconnect instead of terminating the listener thread.
- Wi-Fi reads use direct D-Bus calls for access points and saved profiles.
  Wi-Fi, Bluetooth, player and tray notifications inspect relevant property
  names without unpacking unused metadata. Hidden player source menus refresh
  before opening. Workspace changes retain surviving button parents.
- Urgent workspace blinking cancels its timer while the chip is unmapped and
  resumes on map. Visible blinking keeps a single periodic 600 ms source. The
  GTK fixture verifies pause, resume, clearing urgency and owner release without
  switching actual workspaces.
- Player and tray property notifications keep at most one pending update per
  source and process the latest cached properties. Consumers yield after 32
  events so a busy D-Bus queue does not monopolize GTK. Discovery and ownership
  events retain their ordering.
- Launcher metadata file reads run in the background; duplicate notifications
  before a catalog scan begins do not request a second scan. Favorites do not
  construct the full application list until it is needed.
- Launcher catalog changes retain unchanged desktop file monitors. Device and
  inode identity force replacement when a directory or symlink target changes
  at the same path; removed monitors cancel explicitly. An isolated temporary
  directory fixture verifies retained event delivery, directory replacement and
  cancellation on removal and owner drop. Watch discovery has its own module.
- Launcher catalog changes retain and reorder surviving application rows.
  Labels and icons update only when changed. Launch callbacks resolve the latest
  application metadata instead of retaining the previous desktop entry.
- Favorites also retain their tiles and row parents when catalog metadata changes
  without changing their keys or order. Their name and icon refresh in place;
  repeated refreshes without metadata changes skip that work. Tile callbacks keep
  only application keys and launch the latest catalog entry. Favorite tile UI has
  its own module.
- Thumbnail queues wait for capacity, share duplicate requests, discard canceled
  cards and survive a failed builder. Cache keys distinguish raw filenames while
  preserving existing keys for UTF-8 names. Directory scans use entry types and
  continue following symbolic links. Wallpaper process shutdown waits for exit
  notifications instead of polling every 10 ms.
- Superseded wallpaper and folder scans now stop before starting and between
  entries. Cancellation callbacks can start a newer scan without a mutable-borrow
  panic or losing that request. Closing their page cancels pending scans. Folder
  navigation uses a virtual list; the 5,001-folder fixture constructs fewer than
  512 buttons and confirms that recycled rows navigate to their current binding,
  including a folder with non-UTF-8 bytes in its name.
- Extremely thin images bypass a huge intermediate upscale when producing
  cover thumbnails. The direct crop matches the previous visible pixels for
  tested PNG/JPEG inputs, RGB and RGBA, and both image orientations.
- Gallery deletion refreshes build one index of remaining paths instead of
  searching the full list for each retained card. Active styling visits live
  cards rather than every file. The fixture deletes a temporary image and checks
  thumbnail identity, active selection and scroll preservation.
- Shared frame callback ownership immediately removes replaced animations,
  including while a surface receives no frames. This covers popup reveals,
  monitor sliding and deferred font measurement, track titles, launcher width,
  OSD values and smooth scrolling. An external scroll reset or unmap cancels the
  previous scroll target. Gallery refresh indicators no longer retain the model
  for their two-second display interval. Popup and OSD effects honor GTK's
  animation preference.
- The main bar slide also cancels replaced frame callbacks immediately. Closing
  the bar cancels its startup delay and any active slide. Existing durations,
  easing and margin/exclusive-zone geometry remain unchanged; disabling GTK
  animations now applies the endpoint immediately. A separate glass layer
  fixture checks 1,000 unmapped replacements, reversal from the current margin,
  both endpoints, disabled animations and owner release without reserving space
  or changing real workspaces.
- Native tray menus and monitor dropdowns share an entrance helper that starts
  before first paint, cancels obsolete cleanup timers on unmap and honors the
  GTK animation preference. Closing a menu does not retain its surface.

Background execution, timers, caches, popup and inline panel helpers, launcher
catalog discovery, brightness backends, sensor/network collection, PipeWire sink
parsing and wallpaper folder navigation have separate modules.

Monitor dropdowns use the shared translucent `@popup_surface_bg`, rounded
borders and a 200 ms entrance animation. The monitor content layer now uses
`obsidian-bar-system-monitor`, matching the other panel namespaces. The existing
local Niri rule for `^obsidian-bar-.*$` includes popup blur, whereas the previous
monitor-only rule applied blur only to the main surface. A Wayland trace confirms
that both GTK dropdowns register as popups of the matching layer surface. No Niri
configuration files were edited. GTK snapshots validate transparency, repeated
opening and disabled animations; they do not capture compositor blur pixels.

CPU readings now have a model heading matching the GPU device headings, both
in the panel and its metric settings. The name is read once from `/proc/cpuinfo`
and shared between snapshots, including when a section is paused. Missing model
information leaves the readings available without repeatedly reading the file.
The core-count suffix is omitted, displaying `AMD Ryzen 9 9950X` locally.
The temperature list shows `GPU Temperature` alongside the other sensor rows,
without repeating the device model heading from the GPU section.

Local measurements use five alternating runs per implementation and report
medians. Parser benchmarks compile the harness with `rustc -O`, link the current
dependency build, and process the same fixed input 100 times in each process.
Peak RSS is the benchmark process's Linux `VmHWM`, including its input buffer.

| Case | Before | After |
| --- | ---: | ---: |
| `true` command lifecycle, average per command in batches of 300 | 318.407 µs | 288.591 µs |
| Sink parsing, synthetic 818,656-byte graph, average per parse | 3.98373 ms | 2.81748 ms |
| Synthetic parser process peak RSS | 6,152 KiB | 3,572 KiB |
| Sink parsing, local 343,764-byte graph, average per parse | 1.61026 ms | 0.96735 ms |
| Local parser process peak RSS | 5,012 KiB | 3,176 KiB |
| PCI GPU discovery, 500 scans per run, average per scan | 159.046 µs | 55.528 µs |
| Cover thumbnail, fixed 4097 × 1 RGBA PNG, average per thumbnail | 228.5931 ms | 0.1285 ms |
| Thin-image thumbnail benchmark process peak RSS | 230,292 KiB | 6,628 KiB |
| Mount parsing, fixed 2,759-byte snapshot, average per parse | 6.3541 µs | 1.3941 µs |
| Mount parser allocations per parse | 189 | 15 |
| Gallery path pruning, 10,000 synthetic paths and two maps of 220 cards | 74.9196 ms | 0.3781 ms |
| Desktop monitor refresh, 43 unchanged local watch paths | 67.991 µs | 21.769 µs |

Both dumps produced identical complete sink lists with the original and final
parsers, including all five local outputs and default selection. On the local
input, parse time fell by about 40% and benchmark process peak RSS by about 37%.
These measurements do not establish a percentage improvement for the entire bar.

PCI and thumbnail harnesses compile with Rust edition 2024 and optimizations.
PCI runs produce identical device lists; thin-image runs produce identical
visible pixel hashes. The thumbnail result describes an extreme aspect ratio,
not typical wallpaper images or whole-gallery performance.

Mount parsing timings use five alternating runs of 10,000 parses with the
normal allocator. A separately compiled counting allocator measures allocations,
so its overhead does not enter the timing result. Both parsers return identical
mount lists. Reordering also skips key construction when no custom order exists.

The gallery pruning harness uses Rust edition 2024 and `-O`, five alternating
runs of ten updates, and times only the two map-retention operations with their
shared index. Map cloning is outside the timed region. Both implementations
retain the same keys. The result excludes rendering, scanning and other refresh
work, and does not describe the full gallery's update time.

Desktop monitor timings use Rust edition 2024 and `-O`, five alternating runs
of 100 updates per process, and the same GTK/GIO dependency build. The input is
the same 43 local watch paths; discovery is outside the timed region. The
original recreation loop and final retention loop install the same number of
monitors. The benchmark performs no writes to application directories and does
not measure catalog discovery or whole-bar performance.

For 100 real hwmon samples, `strace -f -c` counted 50,609 syscalls before metadata
caching and 25,668 after it. `readlink` calls fell from 6,300 to 63. Both runs
reported 15 temperature sensors and 10 fans. This is about 49% fewer syscalls;
hardware read latency still dominates, so it is not a 49% sampling-time claim.

A separate 30-minute launcher run applied about 3,600 catalog replacements,
checked retained row parents and obsolete row release, then launched the latest
temporary desktop entry. Sampled RSS remained around 60 MiB and ended at
60,876 KiB. This is a fixture process measurement, not total desktop bar memory.

Validation passed with:

```sh
nix develop --command sh -c 'cargo fmt --check && cargo clippy --all-targets -- -D warnings && cargo test && cargo build'
```

There are 200 passing ordinary tests. Twenty-four additional GTK/D-Bus tests passed
individually with isolated state directories: popup lifetime, calendar animation,
player source actions and lazy updates, launcher favorites, Bluetooth row reuse,
monitor drawer animation, monitor pin restoration, metric selection and scrolling,
battery label layout, player discovery retry, tray menu behavior, direct D-Bus
reads, gallery thumbnails and widget lifetime, workspace button retention, latest
launcher metadata and row retention, scroll cancellation, property signal bursts,
glass dropdowns, large folder navigation, frame callback ownership, synthetic
OSD animation, favorite tile metadata/lifetime, native popup reopening and main
bar slide replacement/reversal. The hwmon diagnostic and launcher soak also passed.
Five other ignored tests require
hardware queries or move the real pointer and were not run.

Final ordinary tests, Clippy, formatting and debug/release builds passed after
the clock timer cleanup; the calendar lifetime check also passed. At the user's
request, local compiled binaries and the `target/` build cache were then removed
before publishing the source changes. No further local build is required for
the handoff to the flake.

The local debug bar was launched at the user's request and restarted with the
glass dropdowns and shared Niri blur rule. After animation checks passed, the
current release binary was launched at the user's request. Release ordinary
tests and the drawer animation check also passed. The release was restarted at
13:13:41 UTC with the current batch, including CPU model headings, favorite tile
retention, native menu cleanup, gallery pruning, hidden urgency timers and filtered
audio-output parsing. Startup completed without errors. The running process is
not replaced after every edit. It was restarted again at 13:21:20 UTC with the
temperature heading adjustment and monitor module separation; startup completed
and the first hardware snapshot arrived.
The release was restarted at 13:32:36 UTC after release tests passed, including
the main bar slide, wireless timer cancellation and retained desktop monitors.
Startup and the first hardware snapshot completed successfully.

For the gallery check, isolate both state and thumbnail caches:

```sh
nix develop --command env \
  XDG_STATE_HOME=/tmp/obsidian-monitor-items-test-state/audit \
  XDG_CACHE_HOME=/tmp/obsidian-gallery-cache/audit \
  cargo test gallery_loads_thumbnails_after_scrolling_reuses_cards_and_releases_widgets \
  -- --ignored --test-threads=1
```
