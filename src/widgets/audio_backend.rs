use std::{
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};

mod sinks;
use sinks::parse_sinks;

use super::{command, snapshot_cache::SnapshotCache};

const DEFAULT_SINK: &str = "@DEFAULT_AUDIO_SINK@";
const WPCTL_TIMEOUT: Duration = Duration::from_secs(3);
const PW_DUMP_TIMEOUT: Duration = Duration::from_secs(3);
const AUDIO_CACHE_TTL: Duration = Duration::from_millis(500);
static AUDIO_WRITE_LOCK: Mutex<()> = Mutex::new(());
static VOLUME_CACHE: SnapshotCache<VolumeState> = SnapshotCache::new(AUDIO_CACHE_TTL);
static SINK_CACHE: SnapshotCache<Arc<[SinkInfo]>> = SnapshotCache::new(AUDIO_CACHE_TTL);
static WPCTL: command::ExternalProgram = command::ExternalProgram::new(
    "OBSIDIAN_BAR_WPCTL_BIN",
    option_env!("OBSIDIAN_BAR_WPCTL_BIN"),
    "wpctl",
);
static PW_DUMP: command::ExternalProgram = command::ExternalProgram::new(
    "OBSIDIAN_BAR_PW_DUMP_BIN",
    option_env!("OBSIDIAN_BAR_PW_DUMP_BIN"),
    "pw-dump",
);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SinkKind {
    Headset,
    Display,
    Digital,
    Analog,
    Other,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct SinkInfo {
    pub(super) id: String,
    pub(super) keys: Vec<String>,
    pub(super) persist_keys: Vec<String>,
    pub(super) name: String,
    pub(super) meta: String,
    pub(super) current: bool,
    pub(super) kind: SinkKind,
}

#[derive(Clone, Copy, Debug)]
pub(super) struct VolumeState {
    pub(super) volume: f64,
    pub(super) muted: bool,
}

pub(super) trait AudioBackend: Send + Sync {
    fn volume(&self) -> Result<VolumeState, String>;
    fn sinks(&self) -> Result<Arc<[SinkInfo]>, String>;
    fn set_volume(&self, value: f64) -> Result<(), String>;
    fn set_mute(&self, muted: bool) -> Result<(), String>;
    fn set_default_sink(&self, sink_id: &str) -> Result<(), String>;
}

#[derive(Default)]
struct WpctlBackend;

impl AudioBackend for WpctlBackend {
    fn volume(&self) -> Result<VolumeState, String> {
        VOLUME_CACHE.get_or_try_update(|| {
            let output =
                command::output(WPCTL.get(), &["get-volume", DEFAULT_SINK], WPCTL_TIMEOUT)?;
            parse_volume(&output)
        })
    }

    fn sinks(&self) -> Result<Arc<[SinkInfo]>, String> {
        SINK_CACHE.get_or_try_update(|| {
            let snapshot = command::output(PW_DUMP.get(), &["-N"], PW_DUMP_TIMEOUT)?;
            parse_sinks(&snapshot).map(Arc::from)
        })
    }

    fn set_volume(&self, value: f64) -> Result<(), String> {
        let _write_guard = audio_write_guard();
        let value = format!("{:.2}", value.clamp(0.0, 1.0));
        let result = command::status(
            WPCTL.get(),
            &["set-volume", DEFAULT_SINK, &value, "-l", "1"],
            WPCTL_TIMEOUT,
        );
        VOLUME_CACHE.invalidate();
        result
    }

    fn set_mute(&self, muted: bool) -> Result<(), String> {
        let _write_guard = audio_write_guard();
        let result = command::status(
            WPCTL.get(),
            &["set-mute", DEFAULT_SINK, if muted { "1" } else { "0" }],
            WPCTL_TIMEOUT,
        );
        VOLUME_CACHE.invalidate();
        result
    }

    fn set_default_sink(&self, sink_id: &str) -> Result<(), String> {
        let _write_guard = audio_write_guard();
        let result = command::status(WPCTL.get(), &["set-default", sink_id], WPCTL_TIMEOUT);
        VOLUME_CACHE.invalidate();
        SINK_CACHE.invalidate();
        result
    }
}

fn audio_write_guard() -> std::sync::MutexGuard<'static, ()> {
    AUDIO_WRITE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(super) fn invalidate_audio_caches() {
    VOLUME_CACHE.invalidate();
    SINK_CACHE.invalidate();
}

static DEFAULT_BACKEND: OnceLock<Arc<dyn AudioBackend>> = OnceLock::new();

pub(super) fn default_audio_backend() -> Arc<dyn AudioBackend> {
    Arc::clone(DEFAULT_BACKEND.get_or_init(|| Arc::new(WpctlBackend)))
}

fn parse_volume(output: &str) -> Result<VolumeState, String> {
    let mut fields = output.split_whitespace();
    if fields.next() != Some("Volume:") {
        return Err(format!("unexpected wpctl volume output: {output}"));
    }

    let volume = fields
        .next()
        .ok_or_else(|| format!("missing wpctl volume value: {output}"))?
        .parse::<f64>()
        .map_err(|error| format!("invalid wpctl volume: {error}"))?;
    if !volume.is_finite() {
        return Err(format!("invalid wpctl volume value: {volume}"));
    }
    let muted = fields.any(|field| field.eq_ignore_ascii_case("[MUTED]"));

    Ok(VolumeState {
        volume: volume.clamp(0.0, 1.0),
        muted,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_wpctl_volume() {
        let state = parse_volume("Volume: 0.42").unwrap();
        assert_eq!(state.volume, 0.42);
        assert!(!state.muted);

        let state = parse_volume("Volume: 0.75 [MUTED]").unwrap();
        assert_eq!(state.volume, 0.75);
        assert!(state.muted);

        assert!(parse_volume("garbage 0.42").is_err());
        assert!(parse_volume("Volume: NaN").is_err());
        assert!(parse_volume("Volume: inf").is_err());
    }
}
