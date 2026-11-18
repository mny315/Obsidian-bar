//! Measure each playback stream independently without waking its target.
use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    io::Cursor,
    rc::Rc,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use pipewire as pw;
use pw::{properties::properties, spa};
use spa::{
    param::{
        ParamType,
        audio::{AudioFormat, AudioInfoRaw},
    },
    pod::{Object, Pod, Value, serialize::PodSerializer},
    utils::{Direction, SpaTypes},
};
use tracing::{debug, warn};

use super::{AudioSource, PlaybackStream, PlaybackStreams, PlayerEvent, enqueue_player_event};

const POLL_INTERVAL: Duration = Duration::from_millis(75);
const SILENCE_GRACE: Duration = Duration::from_millis(400);
const RETRY_INTERVAL: Duration = Duration::from_secs(2);
const MIN_PEAK: f32 = 0.0005;

pub(super) struct PlaybackMeter {
    stop: pw::channel::Sender<()>,
    thread: Option<JoinHandle<()>>,
}

impl PlaybackMeter {
    pub(super) fn start(
        streams: PlaybackStreams,
        events: async_channel::Sender<PlayerEvent>,
    ) -> std::io::Result<Self> {
        let (stop, stop_receiver) = pw::channel::channel();
        let thread = thread::Builder::new()
            .name("player-audio-meter".into())
            .spawn(move || {
                if let Err(error) = run(streams, &events, stop_receiver) {
                    warn!(%error, "cannot measure playback activity");
                    enqueue_player_event(&events, PlayerEvent::AudioChanged(None));
                }
            })?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for PlaybackMeter {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run(
    streams: PlaybackStreams,
    events: &async_channel::Sender<PlayerEvent>,
    stop: pw::channel::Receiver<()>,
) -> Result<(), String> {
    pw::init();
    let main_loop = pw::main_loop::MainLoopRc::new(None).map_err(|err| err.to_string())?;
    let context = pw::context::ContextRc::new(&main_loop, None).map_err(|err| err.to_string())?;
    let _stop = stop.attach(main_loop.loop_(), {
        let main_loop = main_loop.clone();
        move |_| main_loop.quit()
    });

    let session = RefCell::new(None::<MeterSession>);
    let retry_at = Cell::new(Instant::now());
    let previous = RefCell::new(Some(Vec::new()));
    let events = events.clone();
    let timer = main_loop.loop_().add_timer(move |_| {
        let now = Instant::now();
        let mut session = session.borrow_mut();
        if session
            .as_ref()
            .is_some_and(|session| session.disconnected.get())
        {
            *session = None;
        }
        if session.is_none() && now >= retry_at.get() {
            retry_at.set(now + RETRY_INTERVAL);
            match context.connect_rc(None) {
                Ok(core) => *session = Some(MeterSession::new(core)),
                Err(error) => debug!(%error, "playback meter connection failed"),
            }
        }

        let streams = streams.lock().unwrap().clone();
        let active = session.as_mut().and_then(|session| {
            let active = session.measure(streams.as_deref().unwrap_or_default(), now);
            streams.is_some().then_some(active)
        });
        let mut previous = previous.borrow_mut();
        if active != *previous {
            previous.clone_from(&active);
            enqueue_player_event(&events, PlayerEvent::AudioChanged(active));
        }
    });
    timer
        .update_timer(Some(POLL_INTERVAL), Some(POLL_INTERVAL))
        .into_result()
        .map_err(|err| err.to_string())?;
    main_loop.run();
    Ok(())
}

struct MeterSession {
    streams: HashMap<u64, (Instant, Option<StreamMeter>)>,
    _listener: pw::core::Listener,
    core: pw::core::CoreRc,
    disconnected: Rc<Cell<bool>>,
}

impl MeterSession {
    fn new(core: pw::core::CoreRc) -> Self {
        let disconnected = Rc::new(Cell::new(false));
        let listener = core
            .add_listener_local()
            .error({
                let disconnected = disconnected.clone();
                move |id, _, _, message| {
                    if id == pw::core::PW_ID_CORE {
                        debug!(%message, "playback meter disconnected");
                        disconnected.set(true);
                    }
                }
            })
            .register();
        Self {
            streams: HashMap::new(),
            _listener: listener,
            core,
            disconnected,
        }
    }

    fn measure(&mut self, streams: &[PlaybackStream], now: Instant) -> Vec<AudioSource> {
        self.streams
            .retain(|serial, _| streams.iter().any(|stream| stream.serial == *serial));
        let mut active = Vec::new();
        for stream in streams {
            let (retry_at, meter) = self.streams.entry(stream.serial).or_insert((now, None));
            if now >= *retry_at && meter.as_ref().is_none_or(StreamMeter::disconnected) {
                *retry_at = now + RETRY_INTERVAL;
                *meter = StreamMeter::new(self.core.clone(), stream.serial)
                    .inspect_err(|error| debug!(%error, serial = stream.serial, "cannot measure playback stream"))
                    .ok();
            }
            if meter.as_ref().is_some_and(|meter| meter.audible(now))
                && active
                    .last()
                    .is_none_or(|source: &AudioSource| source.key != stream.source.key)
            {
                active.push(stream.source.clone());
            }
        }
        active
    }
}

struct StreamMeter {
    _listener: pw::stream::StreamListener<()>,
    stream: pw::stream::StreamRc,
    last_signal: Rc<Cell<Option<Instant>>>,
}

impl StreamMeter {
    fn new(core: pw::core::CoreRc, serial: u64) -> Result<Self, String> {
        let props = properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::NODE_NAME => "obsidian-bar-playback-meter",
            "target.object" => serial.to_string(),
            *pw::keys::NODE_DONT_RECONNECT => "true",
            *pw::keys::NODE_PASSIVE => "true",
            "node.dont-fallback" => "true",
            "node.dont-move" => "true",
        };
        let stream = pw::stream::StreamRc::new(core, "Obsidian Bar playback meter", props)
            .map_err(|err| err.to_string())?;
        let last_signal = Rc::new(Cell::new(None));
        let listener = stream
            .add_local_listener_with_user_data(())
            .process({
                let last_signal = last_signal.clone();
                move |stream, _| {
                    let Some(mut buffer) = stream.dequeue_buffer() else {
                        return;
                    };
                    // Scan channels independently so opposite phases cannot cancel.
                    for data in buffer.datas_mut() {
                        let offset = data.chunk().offset() as usize;
                        let size = data.chunk().size() as usize;
                        let Some(bytes) = data.data() else { continue };
                        let end = offset.saturating_add(size).min(bytes.len());
                        if bytes.get(offset..end).is_some_and(has_signal) {
                            last_signal.set(Some(Instant::now()));
                            break;
                        }
                    }
                }
            })
            .register()
            .map_err(|err| err.to_string())?;

        let mut audio = AudioInfoRaw::new();
        audio.set_format(AudioFormat::F32LE);
        let format = Value::Object(Object {
            type_: SpaTypes::ObjectParamFormat.as_raw(),
            id: ParamType::EnumFormat.as_raw(),
            properties: audio.into(),
        });
        let (bytes, _) = PodSerializer::serialize(Cursor::new(Vec::new()), &format)
            .map_err(|err| err.to_string())?;
        let bytes = bytes.into_inner();
        let pod = Pod::from_bytes(&bytes).ok_or("invalid playback meter format")?;
        stream
            .connect(
                Direction::Input,
                None,
                pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
                &mut [pod],
            )
            .map_err(|err| err.to_string())?;

        Ok(Self {
            _listener: listener,
            stream,
            last_signal,
        })
    }

    fn disconnected(&self) -> bool {
        matches!(
            self.stream.state(),
            pw::stream::StreamState::Unconnected | pw::stream::StreamState::Error(_)
        )
    }

    fn audible(&self, now: Instant) -> bool {
        self.last_signal
            .get()
            .is_some_and(|last| now.duration_since(last) < SILENCE_GRACE)
    }
}

fn has_signal(bytes: &[u8]) -> bool {
    bytes.chunks_exact(size_of::<f32>()).any(|sample| {
        let level = f32::from_le_bytes(sample.try_into().unwrap());
        level.is_finite() && level.abs() >= MIN_PEAK
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_does_not_count_as_playback() {
        let samples = [0.0_f32, -0.0, MIN_PEAK / 2.0, f32::NAN, f32::INFINITY];
        assert!(!has_signal(&samples.map(f32::to_le_bytes).concat()));
        assert!(has_signal(&[0.1_f32, -0.1].map(f32::to_le_bytes).concat()));
    }
}
