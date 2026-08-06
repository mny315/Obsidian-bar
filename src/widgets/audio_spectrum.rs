use std::{
    cell::{Cell, RefCell},
    f32::consts::TAU,
    fs,
    mem::size_of,
    rc::Rc,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use gtk::{gdk, glib, prelude::*};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use pipewire as pw;
use pw::{properties::properties, spa};
use spa::{
    param::format::{MediaSubtype, MediaType},
    pod::Pod,
};
use tracing::warn;

use super::detach_application_window;

const SETTINGS_GROUP: &str = "visualizer";
const SETTINGS_FILE: &str = "audio-spectrum.ini";
const NAMESPACE: &str = "desktop-audio-thread";

const SPECTRUM_BANDS: usize = 72;
const FFT_SIZE: usize = 4096;
const ANALYZER_FPS: usize = 30;
const RENDER_FPS: usize = 60;
const RENDER_FRAME_INTERVAL: Duration = Duration::from_micros(1_000_000 / RENDER_FPS as u64);
const WORKER_RETRY_BASE_DELAY: Duration = Duration::from_secs(1);
const WORKER_RETRY_MAX_DELAY: Duration = Duration::from_secs(30);
const MIN_FREQUENCY_HZ: f32 = 40.0;
const MAX_FREQUENCY_HZ: f32 = 16_000.0;
const SILENCE_RMS: f32 = 0.000_35;
const SPECTRUM_NOISE_FLOOR: f32 = 0.000_012;
const MIN_AUTO_PEAK: f32 = 0.002_5;
const LEVEL_ATTACK: f32 = 0.26;
const LEVEL_RELEASE: f32 = 0.13;
const LEVEL_EPSILON: f32 = 0.003;
const MIN_WINDOW_HEIGHT: i32 = 120;
const WINDOW_HEIGHT_FRACTION: f64 = 0.20;

#[derive(Clone, Copy, Debug, PartialEq)]
struct SpectrumFrame {
    levels: [f32; SPECTRUM_BANDS],
}

impl SpectrumFrame {
    const ZERO: Self = Self {
        levels: [0.0; SPECTRUM_BANDS],
    };

    fn is_visible(self) -> bool {
        self.levels.into_iter().any(|level| level > LEVEL_EPSILON)
    }
}

type StateSubscriber = Box<dyn Fn(bool) -> bool>;
type FrameSubscriber = Box<dyn Fn(SpectrumFrame) -> bool>;

pub struct AudioSpectrumController {
    enabled: Cell<bool>,
    worker: RefCell<Option<SpectrumWorker>>,
    worker_retry_pending: Cell<bool>,
    worker_retry_attempt: Cell<u32>,
    frame_sender: async_channel::Sender<SpectrumFrame>,
    state_subscribers: RefCell<Vec<StateSubscriber>>,
    frame_subscribers: RefCell<Vec<FrameSubscriber>>,
}

impl AudioSpectrumController {
    pub fn new() -> Rc<Self> {
        let (frame_sender, frame_receiver) = async_channel::bounded(1);
        let controller = Rc::new(Self {
            enabled: Cell::new(load_enabled()),
            worker: RefCell::new(None),
            worker_retry_pending: Cell::new(false),
            worker_retry_attempt: Cell::new(0),
            frame_sender,
            state_subscribers: RefCell::new(Vec::new()),
            frame_subscribers: RefCell::new(Vec::new()),
        });

        let weak_controller = Rc::downgrade(&controller);
        glib::MainContext::default().spawn_local(async move {
            while let Ok(frame) = frame_receiver.recv().await {
                let Some(controller) = weak_controller.upgrade() else {
                    frame_receiver.close();
                    break;
                };
                if controller.worker.borrow().is_some() {
                    controller.worker_retry_attempt.set(0);
                }
                controller.broadcast_frame(frame);
            }
        });

        controller
    }

    pub fn enabled(&self) -> bool {
        self.enabled.get()
    }

    pub fn start(self: &Rc<Self>) {
        if self.enabled() {
            self.ensure_worker();
        }
    }

    pub fn shutdown(&self) {
        self.enabled.set(false);
        self.stop_worker();
        self.clear_frames();
    }

    pub fn set_enabled(self: &Rc<Self>, enabled: bool) -> bool {
        if enabled == self.enabled() {
            return true;
        }

        if enabled {
            self.ensure_worker();
            if self.worker.borrow().is_none() {
                return false;
            }
        }

        if let Err(error) = save_enabled(enabled) {
            warn!(%error, "failed to save audio spectrum setting");
            if enabled {
                self.stop_worker();
            }
            return false;
        }

        self.enabled.set(enabled);
        if !enabled {
            self.stop_worker();
            self.clear_frames();
        }
        self.state_subscribers
            .borrow_mut()
            .retain(|subscriber| subscriber(enabled));
        true
    }

    pub fn subscribe_state(&self, callback: impl Fn(bool) -> bool + 'static) {
        if callback(self.enabled()) {
            self.state_subscribers.borrow_mut().push(Box::new(callback));
        }
    }

    fn subscribe_frames(&self, callback: impl Fn(SpectrumFrame) -> bool + 'static) {
        self.frame_subscribers.borrow_mut().push(Box::new(callback));
    }

    fn ensure_worker(self: &Rc<Self>) {
        if self.worker.borrow().is_some() {
            return;
        }

        let (stopped_sender, stopped_receiver) = async_channel::bounded::<()>(1);
        match SpectrumWorker::spawn(self.frame_sender.clone(), stopped_sender) {
            Ok(worker) => {
                *self.worker.borrow_mut() = Some(worker);
            }
            Err(error) => {
                warn!(%error, "failed to start audio spectrum capture");
                self.schedule_worker_restart();
                return;
            }
        }

        let weak = Rc::downgrade(self);
        glib::MainContext::default().spawn_local(async move {
            if stopped_receiver.recv().await.is_err() {
                return;
            }
            let Some(controller) = weak.upgrade() else {
                return;
            };
            drop(controller.worker.borrow_mut().take());
            controller.clear_frames();
            if controller.enabled() {
                controller.schedule_worker_restart();
            }
        });
    }

    fn schedule_worker_restart(self: &Rc<Self>) {
        if !self.enabled() || self.worker_retry_pending.replace(true) {
            return;
        }

        let attempt = self.worker_retry_attempt.get();
        self.worker_retry_attempt.set(attempt.saturating_add(1));
        let multiplier = 1_u32 << attempt.min(5);
        let delay = WORKER_RETRY_BASE_DELAY
            .saturating_mul(multiplier)
            .min(WORKER_RETRY_MAX_DELAY);

        let weak = Rc::downgrade(self);
        glib::timeout_add_local_once(delay, move || {
            let Some(controller) = weak.upgrade() else {
                return;
            };
            controller.worker_retry_pending.set(false);
            if controller.enabled() {
                controller.ensure_worker();
            }
        });
    }

    fn stop_worker(&self) {
        drop(self.worker.borrow_mut().take());
    }

    fn clear_frames(&self) {
        let _ = self.frame_sender.force_send(SpectrumFrame::ZERO);
        self.broadcast_frame(SpectrumFrame::ZERO);
    }

    fn broadcast_frame(&self, frame: SpectrumFrame) {
        let frame = if self.enabled() {
            frame
        } else {
            SpectrumFrame::ZERO
        };
        self.frame_subscribers
            .borrow_mut()
            .retain(|subscriber| subscriber(frame));
    }
}

pub struct AudioSpectrumView {
    window: gtk::ApplicationWindow,
}

impl AudioSpectrumView {
    pub fn new(
        application: &gtk::Application,
        monitor: &gdk::Monitor,
        controller: &Rc<AudioSpectrumController>,
    ) -> Self {
        let geometry = monitor.geometry();
        let height = ((f64::from(geometry.height()) * WINDOW_HEIGHT_FRACTION).round() as i32)
            .max(MIN_WINDOW_HEIGHT);

        let window = gtk::ApplicationWindow::builder()
            .application(application)
            .decorated(false)
            .build();
        window.set_focusable(false);
        window.add_css_class("audio-spectrum-window");
        window.init_layer_shell();
        window.set_namespace(Some(NAMESPACE));
        window.set_layer(Layer::Bottom);
        window.set_keyboard_mode(KeyboardMode::None);
        window.set_monitor(Some(monitor));
        window.set_anchor(Edge::Bottom, true);
        window.set_anchor(Edge::Left, true);
        window.set_anchor(Edge::Right, true);
        window.set_anchor(Edge::Top, false);
        window.set_exclusive_zone(-1);
        window.set_default_size(geometry.width().max(1), height);
        window.set_can_target(false);

        let area = gtk::DrawingArea::new();
        area.add_css_class("audio-spectrum-canvas");
        area.set_content_width(geometry.width().max(1));
        area.set_content_height(height);
        area.set_hexpand(true);
        area.set_vexpand(true);
        area.set_can_target(false);

        let render_state = Rc::new(RefCell::new(RenderState::default()));
        {
            let render_state = Rc::clone(&render_state);
            area.set_draw_func(move |area, context, width, height| {
                draw_spectrum(area, context, width, height, &mut render_state.borrow_mut());
            });
        }
        window.set_child(Some(&area));

        let ticking = Rc::new(Cell::new(false));
        {
            let weak_area = area.downgrade();
            let render_state = Rc::clone(&render_state);
            let ticking = Rc::clone(&ticking);
            controller.subscribe_frames(move |frame| {
                let Some(area) = weak_area.upgrade() else {
                    return false;
                };
                let should_tick = {
                    let mut state = render_state.borrow_mut();
                    state.target = frame.levels;
                    frame.is_visible()
                        || state.current.into_iter().any(|level| level > LEVEL_EPSILON)
                };
                if should_tick {
                    start_render_clock(&area, Rc::clone(&render_state), Rc::clone(&ticking));
                }
                true
            });
        }

        {
            let weak_window = window.downgrade();
            let weak_area = area.downgrade();
            let render_state = Rc::clone(&render_state);
            controller.subscribe_state(move |enabled| {
                let Some(window) = weak_window.upgrade() else {
                    return false;
                };

                if enabled {
                    window.present();
                } else {
                    render_state.borrow_mut().reset();
                    if let Some(area) = weak_area.upgrade() {
                        area.queue_draw();
                    }
                    window.set_visible(false);
                }
                true
            });
        }

        Self { window }
    }
}

impl Drop for AudioSpectrumView {
    fn drop(&mut self) {
        detach_application_window(&self.window);
    }
}

struct RenderState {
    target: [f32; SPECTRUM_BANDS],
    current: [f32; SPECTRUM_BANDS],
    smoothed: [f32; SPECTRUM_BANDS],
    curve_y: [f64; SPECTRUM_BANDS],
}

impl Default for RenderState {
    fn default() -> Self {
        Self {
            target: [0.0; SPECTRUM_BANDS],
            current: [0.0; SPECTRUM_BANDS],
            smoothed: [0.0; SPECTRUM_BANDS],
            curve_y: [0.0; SPECTRUM_BANDS],
        }
    }
}

impl RenderState {
    fn reset(&mut self) {
        self.target.fill(0.0);
        self.current.fill(0.0);
        self.smoothed.fill(0.0);
        self.curve_y.fill(0.0);
    }

    fn step(&mut self) -> bool {
        let mut active = false;
        for (current, target) in self.current.iter_mut().zip(self.target) {
            let speed = if target > *current {
                LEVEL_ATTACK
            } else {
                LEVEL_RELEASE
            };
            let next = *current + (target - *current) * speed;
            *current = if next.abs() < LEVEL_EPSILON {
                0.0
            } else {
                next
            };
            active |= *current > LEVEL_EPSILON || target > LEVEL_EPSILON;
        }

        if !active {
            self.reset();
        }
        active
    }

    fn update_curve(&mut self, height: f64) {
        for index in 0..SPECTRUM_BANDS {
            let value = self.current[index];
            let previous = self.current[index.saturating_sub(1)];
            let next = self.current[(index + 1).min(SPECTRUM_BANDS - 1)];
            self.smoothed[index] = previous * 0.12 + value * 0.76 + next * 0.12;
        }

        let baseline = height - 1.0;
        let amplitude = height * 0.88;
        for (y, level) in self.curve_y.iter_mut().zip(self.smoothed) {
            *y = baseline - f64::from(level.clamp(0.0, 1.0)) * amplitude;
        }
    }
}

fn start_render_clock(
    area: &gtk::DrawingArea,
    render_state: Rc<RefCell<RenderState>>,
    ticking: Rc<Cell<bool>>,
) {
    if ticking.replace(true) {
        return;
    }

    let last_frame = Cell::new(None);
    area.add_tick_callback(move |area, _| {
        let now = Instant::now();
        if last_frame
            .get()
            .is_some_and(|previous| now.duration_since(previous) < RENDER_FRAME_INTERVAL)
        {
            return glib::ControlFlow::Continue;
        }
        last_frame.set(Some(now));

        let active = render_state.borrow_mut().step();
        area.queue_draw();
        if active {
            glib::ControlFlow::Continue
        } else {
            ticking.set(false);
            glib::ControlFlow::Break
        }
    });
}

fn draw_spectrum(
    area: &gtk::DrawingArea,
    context: &gtk::cairo::Context,
    width: i32,
    height: i32,
    state: &mut RenderState,
) {
    if width <= 2 || height <= 2 {
        return;
    }

    let width = f64::from(width);
    let height = f64::from(height);
    state.update_curve(height);
    if state
        .current
        .into_iter()
        .all(|level| level <= LEVEL_EPSILON)
    {
        return;
    }
    let color = area.color();

    let _ = context.save();
    context.set_line_join(gtk::cairo::LineJoin::Round);
    context.set_line_cap(gtk::cairo::LineCap::Round);

    append_curve(context, width, &state.curve_y);
    context.set_source_rgba(
        f64::from(color.red()),
        f64::from(color.green()),
        f64::from(color.blue()),
        f64::from(color.alpha()) * 0.86,
    );
    context.set_line_width(2.1);
    let _ = context.stroke();
    let _ = context.restore();
}

fn append_curve(context: &gtk::cairo::Context, width: f64, y: &[f64; SPECTRUM_BANDS]) {
    let x_step = width / (SPECTRUM_BANDS - 1) as f64;
    context.move_to(0.0, y[0]);

    for index in 0..SPECTRUM_BANDS - 1 {
        let p0 = index.saturating_sub(1);
        let p1 = index;
        let p2 = index + 1;
        let p3 = (index + 2).min(SPECTRUM_BANDS - 1);

        let p1x = index as f64 * x_step;
        let p2x = (index + 1) as f64 * x_step;
        let control_1_x = p1x + (p2x - p0 as f64 * x_step) / 6.0;
        let control_1_y = y[p1] + (y[p2] - y[p0]) / 6.0;
        let control_2_x = p2x - (p3 as f64 * x_step - p1x) / 6.0;
        let control_2_y = y[p2] - (y[p3] - y[p1]) / 6.0;

        context.curve_to(
            control_1_x,
            control_1_y,
            control_2_x,
            control_2_y,
            p2x,
            y[p2],
        );
    }
}

struct SpectrumWorker {
    stop: pw::channel::Sender<()>,
    thread: Option<JoinHandle<()>>,
}

impl SpectrumWorker {
    fn spawn(
        frame_sender: async_channel::Sender<SpectrumFrame>,
        stopped_sender: async_channel::Sender<()>,
    ) -> Result<Self, String> {
        let (stop, stop_receiver) = pw::channel::channel();
        let thread = thread::Builder::new()
            .name("pipewire-audio-spectrum".to_owned())
            .spawn(move || {
                if let Err(error) = run_capture(frame_sender, stop_receiver) {
                    warn!(%error, "PipeWire audio spectrum capture stopped");
                }
                let _ = stopped_sender.try_send(());
            })
            .map_err(|error| format!("failed to spawn audio spectrum thread: {error}"))?;

        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Drop for SpectrumWorker {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct CaptureData {
    format: spa::param::audio::AudioInfoRaw,
    analyzer: SpectrumAnalyzer,
    frame_sender: async_channel::Sender<SpectrumFrame>,
}

fn run_capture(
    frame_sender: async_channel::Sender<SpectrumFrame>,
    stop_receiver: pw::channel::Receiver<()>,
) -> Result<(), String> {
    pw::init();

    let main_loop = pw::main_loop::MainLoopRc::new(None)
        .map_err(|error| format!("failed to create PipeWire main loop: {error}"))?;
    let context = pw::context::ContextRc::new(&main_loop, None)
        .map_err(|error| format!("failed to create PipeWire context: {error}"))?;
    let core = context
        .connect_rc(None)
        .map_err(|error| format!("failed to connect to PipeWire: {error}"))?;

    let _stop_receiver = stop_receiver.attach(main_loop.loop_(), {
        let main_loop = main_loop.clone();
        move |_| main_loop.quit()
    });

    let mut props = properties! {
        *pw::keys::MEDIA_TYPE => "Audio",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Music",
    };
    props.insert(*pw::keys::STREAM_CAPTURE_SINK, "true");
    let stream = pw::stream::StreamBox::new(&core, "obsidian-bar-spectrum", props)
        .map_err(|error| format!("failed to create PipeWire stream: {error}"))?;

    let data = CaptureData {
        format: Default::default(),
        analyzer: SpectrumAnalyzer::new(),
        frame_sender,
    };

    let _listener = stream
        .add_local_listener_with_user_data(data)
        .param_changed(|_, data, id, param| {
            let Some(param) = param else {
                return;
            };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }

            let Ok((media_type, media_subtype)) = spa::param::format_utils::parse_format(param)
            else {
                return;
            };
            if media_type != MediaType::Audio || media_subtype != MediaSubtype::Raw {
                return;
            }
            if data.format.parse(param).is_ok() {
                data.analyzer.set_sample_rate(data.format.rate());
            }
        })
        .process(|stream, data| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let Some(audio_data) = buffer.datas_mut().first_mut() else {
                return;
            };
            let channels = data.format.channels() as usize;
            if channels == 0 {
                return;
            }

            let offset = audio_data.chunk().offset() as usize;
            let size = audio_data.chunk().size() as usize;
            let Some(bytes) = audio_data.data() else {
                return;
            };
            let end = offset.saturating_add(size).min(bytes.len());
            if offset >= end {
                return;
            }

            let frame_size = channels.saturating_mul(size_of::<f32>());
            if frame_size == 0 {
                return;
            }
            for frame in bytes[offset..end].chunks_exact(frame_size) {
                let mut mono = 0.0;
                for channel in frame.chunks_exact(size_of::<f32>()) {
                    mono += f32::from_le_bytes([channel[0], channel[1], channel[2], channel[3]]);
                }
                mono /= channels as f32;

                if let Some(spectrum) = data.analyzer.push_sample(mono) {
                    let _ = data.frame_sender.force_send(spectrum);
                }
            }
        })
        .register()
        .map_err(|error| format!("failed to register PipeWire stream listener: {error}"))?;

    let mut audio_info = spa::param::audio::AudioInfoRaw::new();
    audio_info.set_format(spa::param::audio::AudioFormat::F32LE);
    let object = pw::spa::pod::Object {
        type_: pw::spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: pw::spa::param::ParamType::EnumFormat.as_raw(),
        properties: audio_info.into(),
    };
    let values = pw::spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &pw::spa::pod::Value::Object(object),
    )
    .map_err(|error| format!("failed to serialize PipeWire audio format: {error}"))?
    .0
    .into_inner();
    let format = Pod::from_bytes(&values)
        .ok_or_else(|| "failed to build PipeWire audio format".to_owned())?;
    let mut params = [format];

    stream
        .connect(
            spa::utils::Direction::Input,
            None,
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(|error| format!("failed to connect PipeWire audio stream: {error}"))?;

    main_loop.run();
    Ok(())
}

#[derive(Clone, Copy, Default)]
struct Complex32 {
    re: f32,
    im: f32,
}

impl Complex32 {
    fn norm_sqr(self) -> f32 {
        self.re.mul_add(self.re, self.im * self.im)
    }

    fn multiply(self, other: Self) -> Self {
        Self {
            re: self.re.mul_add(other.re, -self.im * other.im),
            im: self.re.mul_add(other.im, self.im * other.re),
        }
    }
}

struct Radix2Fft {
    bit_reversed: Vec<usize>,
    twiddles: Vec<Complex32>,
}

impl Radix2Fft {
    fn new(size: usize) -> Self {
        assert!(size.is_power_of_two());
        let bits = size.trailing_zeros();
        let bit_reversed = (0..size)
            .map(|index| index.reverse_bits() >> (usize::BITS - bits))
            .collect();
        let twiddles = (0..size / 2)
            .map(|index| {
                let angle = -TAU * index as f32 / size as f32;
                Complex32 {
                    re: angle.cos(),
                    im: angle.sin(),
                }
            })
            .collect();
        Self {
            bit_reversed,
            twiddles,
        }
    }

    fn process(&self, buffer: &mut [Complex32]) {
        let size = buffer.len();
        debug_assert_eq!(size, self.bit_reversed.len());
        for (index, &reversed) in self.bit_reversed.iter().enumerate() {
            if index < reversed {
                buffer.swap(index, reversed);
            }
        }

        let mut length = 2;
        while length <= size {
            let half = length / 2;
            let twiddle_step = size / length;
            for base in (0..size).step_by(length) {
                for index in 0..half {
                    let even = buffer[base + index];
                    let odd =
                        buffer[base + index + half].multiply(self.twiddles[index * twiddle_step]);
                    buffer[base + index] = Complex32 {
                        re: even.re + odd.re,
                        im: even.im + odd.im,
                    };
                    buffer[base + index + half] = Complex32 {
                        re: even.re - odd.re,
                        im: even.im - odd.im,
                    };
                }
            }
            length *= 2;
        }
    }
}

#[derive(Clone, Copy, Default)]
struct BandRange {
    start: usize,
    end: usize,
    weight: f32,
}

struct SpectrumAnalyzer {
    ring: Vec<f32>,
    write_index: usize,
    filled: usize,
    samples_since_frame: usize,
    hop_size: usize,
    window: Vec<f32>,
    fft_buffer: Vec<Complex32>,
    fft: Radix2Fft,
    bands: [BandRange; SPECTRUM_BANDS],
    auto_peak: f32,
}

impl SpectrumAnalyzer {
    fn new() -> Self {
        let window = (0..FFT_SIZE)
            .map(|index| 0.5 - 0.5 * (TAU * index as f32 / (FFT_SIZE - 1) as f32).cos())
            .collect();
        let mut analyzer = Self {
            ring: vec![0.0; FFT_SIZE],
            write_index: 0,
            filled: 0,
            samples_since_frame: 0,
            hop_size: 1_600,
            window,
            fft_buffer: vec![Complex32::default(); FFT_SIZE],
            fft: Radix2Fft::new(FFT_SIZE),
            bands: [BandRange::default(); SPECTRUM_BANDS],
            auto_peak: MIN_AUTO_PEAK,
        };
        analyzer.set_sample_rate(48_000);
        analyzer
    }

    fn set_sample_rate(&mut self, sample_rate: u32) {
        if sample_rate == 0 {
            return;
        }
        self.hop_size = (sample_rate as usize / ANALYZER_FPS).max(1);

        let nyquist = sample_rate as f32 * 0.5;
        let max_frequency = MAX_FREQUENCY_HZ.min(nyquist * 0.94);
        let ratio = max_frequency / MIN_FREQUENCY_HZ;
        for (index, band) in self.bands.iter_mut().enumerate() {
            let low = MIN_FREQUENCY_HZ * ratio.powf(index as f32 / SPECTRUM_BANDS as f32);
            let high = MIN_FREQUENCY_HZ * ratio.powf((index + 1) as f32 / SPECTRUM_BANDS as f32);
            let start = ((low * FFT_SIZE as f32 / sample_rate as f32).floor() as usize)
                .clamp(1, FFT_SIZE / 2 - 1);
            let end = ((high * FFT_SIZE as f32 / sample_rate as f32).ceil() as usize)
                .clamp(start + 1, FFT_SIZE / 2);
            let position = index as f32 / (SPECTRUM_BANDS - 1) as f32;
            let weight = 1.04 - position * 0.08;
            *band = BandRange { start, end, weight };
        }
    }

    fn push_sample(&mut self, sample: f32) -> Option<SpectrumFrame> {
        self.ring[self.write_index] = sample;
        self.write_index = (self.write_index + 1) % FFT_SIZE;
        self.filled = (self.filled + 1).min(FFT_SIZE);
        self.samples_since_frame += 1;

        if self.filled < FFT_SIZE || self.samples_since_frame < self.hop_size {
            return None;
        }
        self.samples_since_frame = 0;
        Some(self.analyze())
    }

    fn analyze(&mut self) -> SpectrumFrame {
        let mut sum_squares = 0.0;
        for index in 0..FFT_SIZE {
            let sample = self.ring[(self.write_index + index) % FFT_SIZE];
            sum_squares = sample.mul_add(sample, sum_squares);
        }

        let rms = (sum_squares / FFT_SIZE as f32).sqrt();
        if rms < SILENCE_RMS {
            self.auto_peak = (self.auto_peak * 0.98).max(MIN_AUTO_PEAK);
            return SpectrumFrame::ZERO;
        }

        for index in 0..FFT_SIZE {
            let sample = self.ring[(self.write_index + index) % FFT_SIZE];
            self.fft_buffer[index] = Complex32 {
                re: sample * self.window[index],
                im: 0.0,
            };
        }
        self.fft.process(&mut self.fft_buffer);
        let scale = 2.0 / FFT_SIZE as f32;
        let mut raw = [0.0; SPECTRUM_BANDS];
        let mut frame_peak = 0.0_f32;

        for (level, band) in raw.iter_mut().zip(self.bands) {
            let mut sum = 0.0;
            let mut peak = 0.0_f32;
            for bin in band.start..band.end {
                let magnitude = self.fft_buffer[bin].norm_sqr().sqrt() * scale;
                sum = magnitude.mul_add(magnitude, sum);
                peak = peak.max(magnitude);
            }
            let count = (band.end - band.start).max(1) as f32;
            let rms = (sum / count).sqrt();
            *level = (peak * 0.72 + rms * 0.28) * band.weight;
            frame_peak = frame_peak.max(*level);
        }

        if frame_peak > self.auto_peak {
            self.auto_peak = self.auto_peak * 0.25 + frame_peak * 0.75;
        } else {
            self.auto_peak = (self.auto_peak * 0.985)
                .max(frame_peak * 1.08)
                .max(MIN_AUTO_PEAK);
        }

        let mut levels = [0.0; SPECTRUM_BANDS];
        for (output, value) in levels.iter_mut().zip(raw) {
            let normalized =
                ((value - SPECTRUM_NOISE_FLOOR).max(0.0) / self.auto_peak).clamp(0.0, 1.0);
            *output = normalized.powf(0.72) * 0.92;
        }
        SpectrumFrame { levels }
    }
}

fn load_enabled() -> bool {
    let key_file = glib::KeyFile::new();
    if key_file
        .load_from_file(settings_path(), glib::KeyFileFlags::NONE)
        .is_err()
    {
        return false;
    }
    key_file.boolean(SETTINGS_GROUP, "enabled").unwrap_or(false)
}

fn save_enabled(enabled: bool) -> Result<(), String> {
    let path = settings_path();
    let parent = path
        .parent()
        .ok_or_else(|| "audio spectrum settings path has no parent".to_owned())?;
    fs::create_dir_all(parent)
        .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;

    let key_file = glib::KeyFile::new();
    key_file.set_boolean(SETTINGS_GROUP, "enabled", enabled);
    let temporary = path.with_extension("ini.tmp");
    if let Err(error) = key_file.save_to_file(&temporary) {
        let _ = fs::remove_file(&temporary);
        return Err(format!("failed to write {}: {error}", temporary.display()));
    }
    if let Err(error) = fs::rename(&temporary, &path) {
        let _ = fs::remove_file(&temporary);
        return Err(format!("failed to replace {}: {error}", path.display()));
    }
    Ok(())
}

fn settings_path() -> std::path::PathBuf {
    glib::user_state_dir()
        .join("obsidian-bar")
        .join(SETTINGS_FILE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_produces_zero_spectrum() {
        let mut analyzer = SpectrumAnalyzer::new();
        let mut frame = None;
        for _ in 0..FFT_SIZE + analyzer.hop_size {
            frame = analyzer.push_sample(0.0).or(frame);
        }
        assert_eq!(frame, Some(SpectrumFrame::ZERO));
    }

    #[test]
    fn sine_wave_produces_visible_peak() {
        let mut analyzer = SpectrumAnalyzer::new();
        let sample_rate = 48_000.0;
        let frequency = 440.0;
        let mut frame = None;
        for index in 0..FFT_SIZE + analyzer.hop_size {
            let sample = (TAU * frequency * index as f32 / sample_rate).sin() * 0.4;
            frame = analyzer.push_sample(sample).or(frame);
        }
        let frame = frame.expect("analyzer should emit a spectrum frame");
        let peak = frame.levels.into_iter().fold(0.0_f32, f32::max);
        assert!(peak > 0.5, "unexpectedly weak peak: {peak}");
    }
}
