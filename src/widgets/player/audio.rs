//! Observe actual playback streams. MPRIS PlaybackStatus alone can be stale.
use std::{
    collections::HashMap,
    io::BufReader,
    process::{Child, Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use super::{PlayerEvent, enqueue_player_event};
use crate::widgets::command::ExternalProgram;
use serde_json::Value;

mod meter;
use meter::PlaybackMeter;

static PW_DUMP: ExternalProgram = ExternalProgram::new(
    "OBSIDIAN_BAR_PW_DUMP_BIN",
    option_env!("OBSIDIAN_BAR_PW_DUMP_BIN"),
    "pw-dump",
);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct AudioSource {
    pub key: String,
    pub name: String,
    pub app_id: String,
    pub binary: String,
    pub process_ids: Vec<u32>,
}

impl AudioSource {
    pub fn matches(&self, pid: Option<u32>, names: &[&str]) -> bool {
        // PID ancestry distinguishes browser instances and handles their audio
        // service subprocesses. Names are only a fallback when PIDs are absent.
        if let Some(pid) = pid
            && !self.process_ids.is_empty()
        {
            return self.process_ids.contains(&pid);
        }
        [&self.name, &self.app_id, &self.binary]
            .into_iter()
            .any(|name| {
                let name = normalized_name(name);
                !name.is_empty() && names.iter().any(|other| normalized_name(other) == name)
            })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PlaybackStream {
    serial: u64,
    source: AudioSource,
}

type PlaybackStreams = Arc<Mutex<Option<Arc<[PlaybackStream]>>>>;

fn normalized_name(name: &str) -> String {
    let name = name
        .trim()
        .trim_end_matches(".desktop")
        .trim_end_matches("-wrapped")
        .trim_start_matches('.')
        .rsplit('/')
        .next()
        .unwrap_or(name);
    let normalized: String = name
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .flat_map(char::to_lowercase)
        .collect();
    match normalized.as_str() {
        "googlechrome" | "googlechromestable" | "chromium" | "chromiumbrowser" => "chrome".into(),
        _ => normalized,
    }
}

#[derive(Default)]
struct Graph {
    objects: HashMap<u32, Value>,
    ancestors: HashMap<u32, Vec<u32>>,
}

fn merge(target: &mut Value, update: Value) {
    if let (Some(target), Some(update)) = (target.as_object_mut(), update.as_object()) {
        for (key, value) in update {
            merge(target.entry(key).or_insert(Value::Null), value.clone());
        }
    } else {
        *target = update;
    }
}

impl Graph {
    fn update(&mut self, updates: Vec<Value>) {
        for update in updates {
            let Some(id) = update["id"].as_u64().and_then(|id| u32::try_from(id).ok()) else {
                continue;
            };
            if update.get("info") == Some(&Value::Null) {
                self.objects.remove(&id);
                // A recreated stream may reuse a process ID later.
                self.ancestors.clear();
            } else if self.objects.contains_key(&id)
                || matches!(
                    update["type"].as_str(),
                    Some("PipeWire:Interface:Node" | "PipeWire:Interface:Client")
                )
            {
                merge(self.objects.entry(id).or_insert(Value::Null), update);
            }
        }
    }

    fn playback_streams(&mut self) -> Vec<PlaybackStream> {
        let mut streams = Vec::new();
        for (id, node) in &self.objects {
            let info = &node["info"];
            let props = &info["props"];
            if props["media.class"] != "Stream/Output/Audio"
                || info["state"] != "running"
                || props["pulse.corked"].as_bool() == Some(true)
                || matches!(props["pulse.corked"].as_str(), Some("true" | "1"))
            {
                continue;
            }
            let Some(serial) = unsigned(&props["object.serial"]) else {
                continue;
            };
            let controls = info["params"]["Props"].as_array().into_iter().flatten();
            if controls.clone().any(|p| {
                p["mute"].as_bool() == Some(true)
                    || p["softMute"].as_bool() == Some(true)
                    || p["volume"].as_f64() == Some(0.0)
                    || ["channelVolumes", "softVolumes"].iter().any(|key| {
                        p[key].as_array().is_some_and(|v| {
                            !v.is_empty() && v.iter().all(|v| v.as_f64() == Some(0.0))
                        })
                    })
            }) {
                continue;
            }
            let client = number(&props["client.id"]).and_then(|id| self.objects.get(&id));
            let property = |key: &str| {
                props[key]
                    .as_str()
                    .or_else(|| client.and_then(|c| c["info"]["props"][key].as_str()))
                    .unwrap_or("")
            };
            let pid = number(&props["application.process.id"]).or_else(|| {
                client.and_then(|c| number(&c["info"]["props"]["application.process.id"]))
            });
            let process_ids = pid
                .map(|pid| {
                    self.ancestors
                        .entry(pid)
                        .or_insert_with(|| process_ancestors(pid))
                        .clone()
                })
                .unwrap_or_default();
            let name = [
                property("application.name"),
                property("application.process.binary"),
                property("node.description"),
                property("node.name"),
            ]
            .into_iter()
            .find(|name| !name.is_empty())
            .unwrap_or("Audio")
            .to_owned();
            let key = pid
                .map(|pid| format!("audio:pid:{pid}"))
                .unwrap_or_else(|| {
                    format!(
                        "audio:client:{}",
                        number(&props["client.id"]).unwrap_or(*id)
                    )
                });
            streams.push(PlaybackStream {
                serial,
                source: AudioSource {
                    key,
                    name,
                    app_id: property("application.id").into(),
                    binary: property("application.process.binary").into(),
                    process_ids,
                },
            });
        }
        streams.sort_by(|a, b| {
            a.source
                .key
                .cmp(&b.source.key)
                .then(a.serial.cmp(&b.serial))
        });
        streams
    }
}

fn number(value: &Value) -> Option<u32> {
    u32::try_from(unsigned(value)?).ok()
}

fn unsigned(value: &Value) -> Option<u64> {
    value.as_u64().or_else(|| value.as_str()?.parse().ok())
}

fn process_ancestors(mut pid: u32) -> Vec<u32> {
    let mut ids = Vec::new();
    while pid > 1 && ids.len() < 32 && !ids.contains(&pid) {
        ids.push(pid);
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            break;
        };
        // comm is parenthesized and can itself contain spaces or parentheses.
        let Some(parent) = stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().nth(1))
            .and_then(|s| s.parse().ok())
        else {
            break;
        };
        pid = parent;
    }
    ids
}

pub(super) struct AudioMonitor {
    stopping: Arc<AtomicBool>,
    child: Arc<Mutex<Option<Child>>>,
    wake: mpsc::Sender<()>,
    thread: Option<JoinHandle<()>>,
    _meter: PlaybackMeter,
}

impl AudioMonitor {
    pub fn start(events: async_channel::Sender<PlayerEvent>) -> std::io::Result<Self> {
        let streams: PlaybackStreams = Arc::new(Mutex::new(Some(Arc::from([]))));
        let meter = PlaybackMeter::start(streams.clone(), events)?;
        let stopping = Arc::new(AtomicBool::new(false));
        let child = Arc::new(Mutex::new(None::<Child>));
        let (wake, wait) = mpsc::channel();
        let stop = stopping.clone();
        let process = child.clone();
        let thread = thread::Builder::new()
            .name("player-audio-activity".into())
            .spawn(move || {
                while !stop.load(Ordering::Acquire) {
                    let result = Command::new(PW_DUMP.get())
                        .args(["--monitor", "--no-colors"])
                        .stdin(Stdio::null())
                        .stdout(Stdio::piped())
                        .stderr(Stdio::null())
                        .spawn();
                    if let Ok(mut child) = result {
                        let stdout = child.stdout.take().expect("piped pw-dump output");
                        {
                            let mut slot = process.lock().unwrap();
                            if stop.load(Ordering::Acquire) {
                                let _ = child.kill();
                            }
                            *slot = Some(child);
                        }
                        let mut graph = Graph::default();
                        let mut previous = None;
                        for updates in serde_json::Deserializer::from_reader(BufReader::new(stdout))
                            .into_iter::<Vec<Value>>()
                        {
                            let Ok(updates) = updates else { break };
                            graph.update(updates);
                            let current = graph.playback_streams();
                            if previous.as_deref() != Some(current.as_slice()) {
                                let current: Arc<[PlaybackStream]> = current.into();
                                previous = Some(current.clone());
                                *streams.lock().unwrap() = Some(current);
                            }
                            if stop.load(Ordering::Acquire) {
                                break;
                            }
                        }
                        if let Some(mut child) = process.lock().unwrap().take() {
                            let _ = child.kill();
                            let _ = child.wait();
                        }
                    }
                    *streams.lock().unwrap() = None;
                    if stop.load(Ordering::Acquire)
                        || wait.recv_timeout(Duration::from_secs(2)).is_ok()
                    {
                        break;
                    }
                }
            })?;
        Ok(Self {
            stopping,
            child,
            wake,
            thread: Some(thread),
            _meter: meter,
        })
    }
}

impl Drop for AudioMonitor {
    fn drop(&mut self) {
        self.stopping.store(true, Ordering::Release);
        if let Some(child) = self.child.lock().unwrap().as_mut() {
            let _ = child.kill();
        }
        let _ = self.wake.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn tracks_stream_state_mute_zero_volume_and_removal() {
        let mut graph = Graph::default();
        graph.update(vec![json!({"id":10,"type":"PipeWire:Interface:Node","info":{
            "state":"running","props":{"media.class":"Stream/Output/Audio","object.serial":"100","application.name":"Player"},
            "params":{"Props":[{"mute":false,"channelVolumes":[0.5,0.5]}]}}})]);
        assert_eq!(graph.playback_streams()[0].source.name, "Player");
        for update in [
            json!({"state":"idle"}),
            json!({"state":"running","params":{"Props":[{"mute":true}]}}),
            json!({"params":{"Props":[{"mute":false,"channelVolumes":[0.0,0.0]}]}}),
        ] {
            graph.update(vec![json!({"id":10,"info":update})]);
            assert!(graph.playback_streams().is_empty());
        }
        graph.update(vec![json!({"id":10,"info":{"params":{"Props":[{"mute":false,"channelVolumes":[0.3,0.0]}]}}})]);
        assert_eq!(graph.playback_streams().len(), 1);
        graph.update(vec![json!({"id":10,"info":null})]);
        assert!(graph.playback_streams().is_empty());
    }

    #[test]
    fn resolves_client_identity_and_ignores_capture_and_sinks() {
        let mut graph = Graph::default();
        graph.update(vec![json!({"id":7,"type":"PipeWire:Interface:Client","info":{"props":{"application.name":"Native player"}}})]);
        for (id, class) in [
            (1, "Stream/Output/Audio"),
            (2, "Stream/Input/Audio"),
            (3, "Audio/Sink"),
        ] {
            graph.update(vec![json!({"id":id,"type":"PipeWire:Interface:Node","info":{"state":"running","props":{"media.class":class,"client.id":7,"object.serial":id + 100}}})]);
        }
        assert_eq!(graph.playback_streams().len(), 1);
        assert_eq!(graph.playback_streams()[0].source.name, "Native player");
    }

    #[test]
    fn pid_match_handles_audio_subprocess_without_confusing_browser_instances() {
        let source = AudioSource {
            key: "test".into(),
            name: "Google Chrome".into(),
            app_id: String::new(),
            binary: "chrome".into(),
            process_ids: vec![120, 100],
        };
        assert!(source.matches(Some(100), &["chromium"]));
        assert!(!source.matches(Some(200), &["chromium"]));
        assert!(source.matches(None, &["chromium"]));
        assert!(!source.matches(None, &["Materialgram"]));
    }

    #[test]
    #[ignore = "requires a live PipeWire session with a test sink for synthetic audio"]
    fn live_monitor_follows_signal_in_each_playback_stream() {
        use std::{io::Write, time::Instant};

        struct Playback {
            child: Child,
            signal: Arc<AtomicBool>,
            writer: Option<JoinHandle<()>>,
        }

        impl Playback {
            fn start() -> Self {
                let binary = std::path::Path::new(PW_DUMP.get()).with_file_name("pw-cat");
                let target = std::env::var_os("OBSIDIAN_BAR_TEST_AUDIO_SINK")
                    .expect("set OBSIDIAN_BAR_TEST_AUDIO_SINK to the name of a test sink");
                let mut child = Command::new(binary)
                    .arg("--target")
                    .arg(target)
                    .args([
                        "--playback",
                        "--raw",
                        "--rate",
                        "48000",
                        "--channels",
                        "2",
                        "--format",
                        "f32",
                        "-",
                    ])
                    .stdin(Stdio::piped())
                    .stdout(Stdio::null())
                    .spawn()
                    .unwrap();
                let mut input = child.stdin.take().unwrap();
                let signal = Arc::new(AtomicBool::new(false));
                let writer = thread::spawn({
                    let signal = signal.clone();
                    move || {
                        let silence = [0; 3840];
                        let tone: Vec<_> = (0..480)
                            .flat_map(|frame| {
                                let level = if frame % 96 < 48 { 0.02_f32 } else { -0.02 };
                                [level, -level].into_iter().flat_map(f32::to_le_bytes)
                            })
                            .collect();
                        while input
                            .write_all(if signal.load(Ordering::Relaxed) {
                                &tone
                            } else {
                                &silence
                            })
                            .is_ok()
                        {}
                    }
                });
                Self {
                    child,
                    signal,
                    writer: Some(writer),
                }
            }
        }

        impl Drop for Playback {
            fn drop(&mut self) {
                let _ = self.child.kill();
                let _ = self.child.wait();
                self.writer.take().unwrap().join().unwrap();
            }
        }

        let (events, receiver) = async_channel::unbounded();
        let monitor = AudioMonitor::start(events).unwrap();
        let first = Playback::start();
        let first_pid = first.child.id();
        first.signal.store(true, Ordering::Relaxed);
        let wait_for = |expected: &[u32]| {
            let deadline = Instant::now() + Duration::from_secs(8);
            while Instant::now() < deadline {
                if let Ok(PlayerEvent::AudioChanged(Some(sources))) = receiver.try_recv() {
                    let actual: Vec<_> = sources
                        .iter()
                        .filter_map(|source| source.process_ids.first().copied())
                        .collect();
                    if actual.len() == expected.len()
                        && expected.iter().all(|pid| actual.contains(pid))
                    {
                        return;
                    }
                }
                thread::sleep(Duration::from_millis(20));
            }
            panic!("audible stream IDs did not become {expected:?}");
        };
        wait_for(&[first_pid]);

        let second = Playback::start();
        let second_pid = second.child.id();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let dump = Command::new(PW_DUMP.get())
                .arg("--no-colors")
                .output()
                .unwrap();
            let mut graph = Graph::default();
            graph.update(serde_json::from_slice(&dump.stdout).unwrap());
            if graph
                .playback_streams()
                .iter()
                .any(|stream| stream.source.process_ids.first() == Some(&second_pid))
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "silent stream must already be running"
            );
            thread::sleep(Duration::from_millis(20));
        }
        thread::sleep(Duration::from_millis(600));
        assert!(
            receiver.try_recv().is_err(),
            "a new silent stream must not change the audible sources"
        );

        second.signal.store(true, Ordering::Relaxed);
        wait_for(&[first_pid, second_pid]);
        second.signal.store(false, Ordering::Relaxed);
        wait_for(&[first_pid]);
        drop(first);
        wait_for(&[]);
        let stop = Instant::now();
        drop(monitor);
        assert!(stop.elapsed() < Duration::from_secs(2));
    }
}
