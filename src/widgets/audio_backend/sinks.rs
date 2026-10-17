//! PipeWire dump parsing and stable audio-output identities.
use super::{SinkInfo, SinkKind};
use serde::{
    Deserialize, Deserializer,
    de::{SeqAccess, Visitor},
};
use serde_json::{Map, Value};
use std::{collections::HashMap, fmt};

mod filtered;

#[derive(Clone, Debug, Eq, PartialEq)]
struct ParsedSinkNode {
    id: String,
    raw_name: String,
    node_name: String,
}

struct ParsedDump {
    sinks: Vec<ParsedSinkNode>,
    default_sink: Option<String>,
}

impl<'de> Deserialize<'de> for ParsedDump {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct DumpVisitor;
        impl<'de> Visitor<'de> for DumpVisitor {
            type Value = ParsedDump;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("a pw-dump object array")
            }

            fn visit_seq<A: SeqAccess<'de>>(self, mut objects: A) -> Result<Self::Value, A::Error> {
                let mut dump = ParsedDump {
                    sinks: Vec::new(),
                    default_sink: None,
                };
                let mut default_seen = false;
                // Skip unused parameter trees while reading each graph object.
                while let Some(filtered::DumpObject(object)) = objects.next_element()? {
                    if !default_seen && let Some(entry) = default_sink_entry(&object) {
                        default_seen = true;
                        dump.default_sink = entry.get("value").and_then(metadata_node_name);
                    }
                    if let Some(sink) = parse_sink_node(&object) {
                        dump.sinks.push(sink);
                    }
                }
                Ok(dump)
            }
        }
        deserializer.deserialize_seq(DumpVisitor)
    }
}

pub(super) fn parse_sinks(snapshot: &str) -> Result<Vec<SinkInfo>, String> {
    let ParsedDump {
        sinks: parsed,
        default_sink,
    } = serde_json::from_str(snapshot).map_err(|error| format!("invalid pw-dump JSON: {error}"))?;
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

fn default_sink_entry(object: &Value) -> Option<&Value> {
    if object.get("type").and_then(Value::as_str) != Some("PipeWire:Interface:Metadata")
        || object
            .get("props")
            .and_then(Value::as_object)
            .and_then(|props| string_property(props, "metadata.name"))
            .is_some_and(|name| name != "default")
    {
        return None;
    }
    object
        .get("metadata")?
        .as_array()?
        .iter()
        .find(|entry| entry.get("key").and_then(Value::as_str) == Some("default.audio.sink"))
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
    fn metadata_order_and_malformed_objects_preserve_default_selection() {
        let node = serde_json::json!({"id": " 42 ", "type": "PipeWire:Interface:Node", "info": {"props": {
            "media.class": " Audio/Sink ", "node.name": "alsa_output.test", "node.nick": " Test speakers "
        }}});
        let metadata = serde_json::json!({"type": "PipeWire:Interface:Metadata", "metadata": [{
            "key": "default.audio.sink", "value": "alsa_output.test"
        }]});
        for objects in [
            vec![node.clone(), metadata.clone()],
            vec![metadata.clone(), node.clone()],
        ] {
            let mut objects = objects;
            objects.extend([
                Value::Null,
                serde_json::json!(false),
                serde_json::json!({"type": "PipeWire:Interface:Node", "info": []}),
            ]);
            let sinks = parse_sinks(&serde_json::to_string(&objects).unwrap()).unwrap();
            assert_eq!(sinks.len(), 1);
            assert_eq!(sinks[0].id, "42");
            assert_eq!(sinks[0].name, "Test speakers");
            assert!(sinks[0].current);
        }
        let malformed_default = serde_json::json!({"type": "PipeWire:Interface:Metadata", "metadata": [{"key": "default.audio.sink", "value": 17}]});
        let sinks =
            parse_sinks(&serde_json::to_string(&[node, malformed_default, metadata]).unwrap())
                .unwrap();
        assert!(
            !sinks[0].current,
            "the first default entry remains authoritative, even if malformed"
        );
    }

    #[test]
    fn streaming_parser_still_rejects_a_truncated_or_trailing_document() {
        for json in ["[{}", "[{}] trailing", "[null, true, {}] []"] {
            assert!(parse_sinks(json).is_err());
        }
        assert!(parse_sinks("[null, true, {}]").unwrap().is_empty());
    }

    #[test]
    fn filtered_parser_matches_full_objects_with_unused_trees_and_irregular_types() {
        let snapshot = serde_json::json!([
            null, false, 7, 0.5, "ignored entry", [1, {"unused": [2, 3]}],
            {"id": 1, "type": "PipeWire:Interface:Node", "info": false},
            {"id": 2, "type": "PipeWire:Interface:Node", "info": ["ignored"]},
            {"id": 3, "type": "PipeWire:Interface:Node", "info": "ignored"},
            {"id": 4, "type": "PipeWire:Interface:Node", "info": 12},
            {
                "ignored": [{"nested": ["escaped \\\" text", null, true, 1.5]}],
                "id": "42", "type": "PipeWire:Interface:Node",
                "info": {
                    "params": {"large": (0..1000).map(|index| serde_json::json!({"value": index})).collect::<Vec<_>>()},
                    "props": {
                        "media.class": "Audio/Sink", "node.name": "output.test", "node.description": "Test speakers",
                        "unused.property": [{"nested": [null, true, 123, "value"]}]
                    },
                    "state": "running"
                }
            },
            {"type": "PipeWire:Interface:Metadata", "props": false, "metadata": [{"key": "default.audio.sink", "value": {"name": "output.test"}}]}
        ]);
        let json = serde_json::to_string(&snapshot).unwrap();
        let objects = snapshot.as_array().unwrap();
        let expected_nodes = objects
            .iter()
            .filter_map(parse_sink_node)
            .collect::<Vec<_>>();
        let expected_default = objects
            .iter()
            .find_map(default_sink_entry)
            .and_then(|entry| entry.get("value"))
            .and_then(metadata_node_name);
        let parsed: ParsedDump = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.sinks, expected_nodes);
        assert_eq!(parsed.default_sink, expected_default);
        let sinks = parse_sinks(&json).unwrap();
        assert_eq!(sinks.len(), 1);
        assert!(sinks[0].current);
        for malformed in [
            r#"[{"unused": [1, {"x": true}}]}]"#,
            r#"[{"info": {"params": [1, 2}}}]"#,
        ] {
            assert!(parse_sinks(malformed).is_err());
        }
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

        let dump: ParsedDump = serde_json::from_str(snapshot).unwrap();
        assert_eq!(dump.default_sink.as_deref(), Some("bluez_output.test"));
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
