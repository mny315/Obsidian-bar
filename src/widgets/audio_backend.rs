use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

use serde_json::{Map, Value};

use super::command;

const DEFAULT_SINK: &str = "@DEFAULT_AUDIO_SINK@";
const WPCTL_TIMEOUT: Duration = Duration::from_secs(3);
const PW_DUMP_TIMEOUT: Duration = Duration::from_secs(3);
const AUDIO_CACHE_TTL: Duration = Duration::from_millis(500);
type TimedCache<T> = OnceLock<Mutex<Option<(Instant, T)>>>;
static AUDIO_WRITE_LOCK: Mutex<()> = Mutex::new(());
static VOLUME_READ_LOCK: Mutex<()> = Mutex::new(());
static SINK_READ_LOCK: Mutex<()> = Mutex::new(());
static VOLUME_CACHE: TimedCache<VolumeState> = OnceLock::new();
static SINK_CACHE: TimedCache<Vec<SinkInfo>> = OnceLock::new();
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
    fn sinks(&self) -> Result<Vec<SinkInfo>, String>;
    fn set_volume(&self, value: f64) -> Result<(), String>;
    fn set_mute(&self, muted: bool) -> Result<(), String>;
    fn set_default_sink(&self, sink_id: &str) -> Result<(), String>;
}

#[derive(Default)]
struct WpctlBackend;

impl AudioBackend for WpctlBackend {
    fn volume(&self) -> Result<VolumeState, String> {
        if let Some(state) = cached_volume() {
            return Ok(state);
        }
        let _read_guard = VOLUME_READ_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(state) = cached_volume() {
            return Ok(state);
        }

        let output = command::output(WPCTL.get(), &["get-volume", DEFAULT_SINK], WPCTL_TIMEOUT)?;
        let state = parse_volume(&output)?;
        *volume_cache()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some((Instant::now(), state));
        Ok(state)
    }

    fn sinks(&self) -> Result<Vec<SinkInfo>, String> {
        if let Some(sinks) = cached_sinks() {
            return Ok(sinks);
        }
        let _read_guard = SINK_READ_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(sinks) = cached_sinks() {
            return Ok(sinks);
        }

        let snapshot = command::output(PW_DUMP.get(), &["-N"], PW_DUMP_TIMEOUT)?;
        let sinks = parse_sinks(&snapshot)?;
        *sink_cache()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some((Instant::now(), sinks.clone()));
        Ok(sinks)
    }

    fn set_volume(&self, value: f64) -> Result<(), String> {
        let _write_guard = audio_write_guard()?;
        let value = format!("{:.2}", value.clamp(0.0, 1.0));
        let result = command::status(
            WPCTL.get(),
            &["set-volume", DEFAULT_SINK, &value, "-l", "1"],
            WPCTL_TIMEOUT,
        );
        invalidate_volume_cache();
        result
    }

    fn set_mute(&self, muted: bool) -> Result<(), String> {
        let _write_guard = audio_write_guard()?;
        let result = command::status(
            WPCTL.get(),
            &["set-mute", DEFAULT_SINK, if muted { "1" } else { "0" }],
            WPCTL_TIMEOUT,
        );
        invalidate_volume_cache();
        result
    }

    fn set_default_sink(&self, sink_id: &str) -> Result<(), String> {
        let _write_guard = audio_write_guard()?;
        let result = command::status(WPCTL.get(), &["set-default", sink_id], WPCTL_TIMEOUT);
        invalidate_volume_cache();
        invalidate_sink_cache();
        result
    }
}

fn audio_write_guard() -> Result<std::sync::MutexGuard<'static, ()>, String> {
    match AUDIO_WRITE_LOCK.try_lock() {
        Ok(guard) => Ok(guard),
        Err(std::sync::TryLockError::Poisoned(poisoned)) => Ok(poisoned.into_inner()),
        Err(std::sync::TryLockError::WouldBlock) => {
            Err("another audio change is already in progress".to_owned())
        }
    }
}

fn volume_cache() -> &'static Mutex<Option<(Instant, VolumeState)>> {
    VOLUME_CACHE.get_or_init(|| Mutex::new(None))
}

fn sink_cache() -> &'static Mutex<Option<(Instant, Vec<SinkInfo>)>> {
    SINK_CACHE.get_or_init(|| Mutex::new(None))
}

fn cached_volume() -> Option<VolumeState> {
    volume_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
        .filter(|(loaded_at, _)| loaded_at.elapsed() < AUDIO_CACHE_TTL)
        .map(|(_, state)| *state)
}

fn cached_sinks() -> Option<Vec<SinkInfo>> {
    sink_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .as_ref()
        .filter(|(loaded_at, _)| loaded_at.elapsed() < AUDIO_CACHE_TTL)
        .map(|(_, sinks)| sinks.clone())
}

fn invalidate_volume_cache() {
    *volume_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
}

fn invalidate_sink_cache() {
    *sink_cache()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
}

pub(super) fn invalidate_audio_caches() {
    invalidate_volume_cache();
    invalidate_sink_cache();
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

#[derive(Clone, Debug, Eq, PartialEq)]
struct ParsedSinkNode {
    id: String,
    raw_name: String,
    node_name: String,
}

fn parse_sinks(snapshot: &str) -> Result<Vec<SinkInfo>, String> {
    let root: Value =
        serde_json::from_str(snapshot).map_err(|error| format!("invalid pw-dump JSON: {error}"))?;
    let objects = root
        .as_array()
        .ok_or_else(|| "invalid pw-dump JSON: expected a top-level array".to_owned())?;
    let default_sink = default_sink_name(objects);
    let parsed: Vec<_> = objects.iter().filter_map(parse_sink_node).collect();
    let mut visible_name_counts = HashMap::<String, usize>::new();
    for sink in &parsed {
        *visible_name_counts
            .entry(cleanup_sink_name(&sink.raw_name))
            .or_default() += 1;
    }

    let mut sinks: Vec<_> = parsed
        .into_iter()
        .map(|sink| {
            let name = cleanup_sink_name(&sink.raw_name);
            let node_name = sink.node_name;
            let node_key = normalized_property_key("node", &node_name);
            let legacy_key = stable_sink_key_base(&sink.raw_name);

            let mut keys = Vec::new();
            if let Some(key) = node_key.clone() {
                keys.push(key);
            }
            if !legacy_key.is_empty() && !keys.contains(&legacy_key) {
                keys.push(legacy_key.clone());
            }
            let persist_keys = node_key
                .map(|key| vec![key])
                .unwrap_or_else(|| vec![legacy_key.clone()]);
            let kind = sink_kind(&format!("{} {node_name}", sink.raw_name));
            let base_meta = sink_type_text(kind);
            let meta = if visible_name_counts.get(&name).copied().unwrap_or(0) > 1 {
                if node_name.is_empty() {
                    format!("{base_meta} · {}", sink.id)
                } else {
                    format!("{base_meta} · {node_name}")
                }
            } else {
                base_meta.to_owned()
            };

            SinkInfo {
                id: sink.id,
                keys,
                persist_keys,
                name,
                meta,
                current: default_sink.as_deref() == Some(node_name.as_str()),
                kind,
            }
        })
        .collect();

    sinks.sort_by(|left, right| {
        right
            .current
            .cmp(&left.current)
            .then_with(|| {
                left.name
                    .to_ascii_lowercase()
                    .cmp(&right.name.to_ascii_lowercase())
            })
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(sinks)
}

fn parse_sink_node(object: &Value) -> Option<ParsedSinkNode> {
    if object.get("type")?.as_str()? != "PipeWire:Interface:Node" {
        return None;
    }

    let props = object.get("info")?.get("props")?.as_object()?;
    if !string_property(props, "media.class")?.starts_with("Audio/Sink") {
        return None;
    }

    let id = json_id(object.get("id")?)?;
    let node_name = string_property(props, "node.name").unwrap_or_default();
    let raw_name = [
        "node.description",
        "node.nick",
        "device.description",
        "node.name",
    ]
    .into_iter()
    .find_map(|key| string_property(props, key))?;

    Some(ParsedSinkNode {
        id,
        raw_name: raw_name.to_owned(),
        node_name: node_name.to_owned(),
    })
}

fn default_sink_name(objects: &[Value]) -> Option<String> {
    objects
        .iter()
        .filter(|object| {
            object.get("type").and_then(Value::as_str) == Some("PipeWire:Interface:Metadata")
        })
        .filter(|object| {
            object
                .get("props")
                .and_then(Value::as_object)
                .and_then(|props| string_property(props, "metadata.name"))
                .is_none_or(|name| name == "default")
        })
        .filter_map(|object| object.get("metadata").and_then(Value::as_array))
        .flatten()
        .find(|entry| entry.get("key").and_then(Value::as_str) == Some("default.audio.sink"))
        .and_then(|entry| entry.get("value"))
        .and_then(metadata_node_name)
}

fn metadata_node_name(value: &Value) -> Option<String> {
    if let Some(name) = value.get("name").and_then(Value::as_str) {
        return non_empty(name).map(str::to_owned);
    }

    let encoded = value.as_str()?.trim();
    if encoded.is_empty() {
        return None;
    }
    if let Ok(decoded) = serde_json::from_str::<Value>(encoded) {
        return decoded
            .get("name")
            .and_then(Value::as_str)
            .and_then(non_empty)
            .map(str::to_owned);
    }

    (!encoded.starts_with('{')).then(|| encoded.to_owned())
}

fn string_property<'a>(props: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    props.get(key).and_then(Value::as_str).and_then(non_empty)
}

fn non_empty(value: &str) -> Option<&str> {
    let value = value.trim();
    (!value.is_empty()).then_some(value)
}

fn json_id(value: &Value) -> Option<String> {
    value
        .as_u64()
        .map(|id| id.to_string())
        .or_else(|| value.as_str().and_then(non_empty).map(str::to_owned))
}

fn strip_volume_suffix(value: &str) -> String {
    if let Some(index) = value.to_ascii_lowercase().rfind("[vol:") {
        value[..index].trim().to_owned()
    } else {
        value.trim().to_owned()
    }
}

fn cleanup_sink_name(raw: &str) -> String {
    let stripped = strip_volume_suffix(raw)
        .replace("PipeWire Node", " ")
        .replace("pipewire node", " ")
        .replace("Pro Audio", " ")
        .replace("pro audio", " ");
    let normalized = stripped.replace(['.', '_', '-'], " ");
    let mut words = Vec::new();

    for word in normalized.split_whitespace() {
        let lower = word.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "output" | "monitor" | "sink" | "pci" | "usb"
        ) {
            continue;
        }
        if lower == "alsa" || lower == "bluez" {
            continue;
        }
        words.push(word);
    }

    let simplified = words.join(" ");
    if simplified.is_empty() {
        strip_volume_suffix(raw)
    } else {
        simplified
    }
}

fn sink_kind(name: &str) -> SinkKind {
    let value = name.to_ascii_lowercase();
    if contains_any(&value, &["hyperx", "headset", "headphone", "earbud"]) {
        SinkKind::Headset
    } else if value.contains("hdmi") || value.contains("displayport") || has_token(&value, "dp") {
        SinkKind::Display
    } else if contains_any(&value, &["spdif", "iec958", "optical"]) {
        SinkKind::Digital
    } else if value.contains("speaker") || value.contains("analog") {
        SinkKind::Analog
    } else {
        SinkKind::Other
    }
}

fn contains_any(value: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| value.contains(needle))
}

fn has_token(value: &str, token: &str) -> bool {
    value
        .split(|character: char| !character.is_ascii_alphanumeric())
        .any(|part| part == token)
}

fn sink_type_text(kind: SinkKind) -> &'static str {
    match kind {
        SinkKind::Headset => "Headset",
        SinkKind::Display => "HDMI / DisplayPort",
        SinkKind::Digital => "SPDIF / optical",
        SinkKind::Analog => "Analog output",
        SinkKind::Other => "Audio output",
    }
}

fn normalized_property_key(kind: &str, value: &str) -> Option<String> {
    let normalized = value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase();
    (!normalized.is_empty()).then(|| format!("{kind}:{normalized}"))
}

fn stable_sink_key_base(raw: &str) -> String {
    cleanup_sink_name(raw)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
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

    #[test]
    fn parses_sinks_from_pipewire_json() {
        let snapshot = r#"[
          {
            "id": 7,
            "type": "PipeWire:Interface:Metadata",
            "props": { "metadata.name": "default" },
            "metadata": [
              {
                "subject": 0,
                "key": "default.audio.sink",
                "type": "Spa:String:JSON",
                "value": { "name": "alsa_output.usb-Kingston_HyperX_Cloud_III.analog-stereo" }
              }
            ]
          },
          {
            "id": 51,
            "type": "PipeWire:Interface:Node",
            "info": {
              "props": {
                "media.class": "Audio/Sink",
                "node.name": "alsa_output.pci-0000_03_00.1.hdmi-stereo",
                "node.description": "Navi HDMI Output"
              }
            }
          },
          {
            "id": 42,
            "type": "PipeWire:Interface:Node",
            "info": {
              "props": {
                "media.class": "Audio/Sink",
                "node.name": "alsa_output.usb-Kingston_HyperX_Cloud_III.analog-stereo",
                "node.description": "HyperX Cloud III"
              }
            }
          },
          {
            "id": 61,
            "type": "PipeWire:Interface:Node",
            "info": {
              "props": {
                "media.class": "Audio/Source",
                "node.name": "alsa_input.test",
                "node.description": "Microphone"
              }
            }
          }
        ]"#;

        let sinks = parse_sinks(snapshot).unwrap();
        assert_eq!(sinks.len(), 2);
        assert_eq!(sinks[0].id, "42");
        assert!(sinks[0].current);
        assert_eq!(sinks[0].name, "HyperX Cloud III");
        assert_eq!(sinks[0].kind, SinkKind::Headset);
        assert!(sinks[0].keys.iter().any(|key| key.starts_with("node:")));
        assert_eq!(sinks[1].id, "51");
        assert!(!sinks[1].current);
        assert_eq!(sinks[1].kind, SinkKind::Display);
    }

    #[test]
    fn parses_legacy_encoded_metadata_value() {
        let snapshot = r#"[
          {
            "id": 7,
            "type": "PipeWire:Interface:Metadata",
            "props": { "metadata.name": "default" },
            "metadata": [
              {
                "key": "default.audio.sink",
                "value": "{ \"name\": \"bluez_output.test\" }"
              }
            ]
          }
        ]"#;

        let root: Value = serde_json::from_str(snapshot).unwrap();
        assert_eq!(
            default_sink_name(root.as_array().unwrap()).as_deref(),
            Some("bluez_output.test")
        );
    }

    #[test]
    fn rejects_non_array_pw_dump_output() {
        assert!(parse_sinks(r#"{ "id": 42 }"#).is_err());
    }

    #[test]
    fn displayport_detection_uses_dp_as_a_token() {
        assert_eq!(sink_kind("DisplayPort DP-1"), SinkKind::Display);
        assert_eq!(sink_kind("HDMI-A-1"), SinkKind::Display);
        assert_ne!(sink_kind("SoundPort Analog"), SinkKind::Display);
    }
}
