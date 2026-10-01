use super::{CARD_HEIGHT, CARD_WIDTH, WallpaperError, is_video_wallpaper};
use crate::widgets::{command, run_background_async};
use gdk_pixbuf::{InterpType, Pixbuf};
use gio::glib;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, UNIX_EPOCH};
use std::{fs, thread};
use tracing::warn;

static FFMPEG: command::ExternalProgram = command::ExternalProgram::new(
    "OBSIDIAN_BAR_FFMPEG_BIN",
    option_env!("OBSIDIAN_BAR_FFMPEG_BIN"),
    "ffmpeg",
);
const THUMBNAIL_VERSION: &str = "cover-144x84-v2";
const VIDEO_STILL_VERSION: &str = "video-still-v1";
const FFMPEG_TIMEOUT: Duration = Duration::from_secs(15);
const IMAGE_THUMBNAIL_WORKERS: usize = 2;
const VIDEO_THUMBNAIL_WORKERS: usize = 2;
const THUMBNAIL_QUEUE_CAPACITY: usize = 256;

struct ThumbnailJob {
    source: PathBuf,
}

pub(super) type ThumbnailResult = Result<PathBuf, String>;
type ThumbnailWaiters = HashMap<PathBuf, Vec<async_channel::Sender<ThumbnailResult>>>;

fn thumbnail_waiters() -> &'static Mutex<ThumbnailWaiters> {
    static WAITERS: OnceLock<Mutex<ThumbnailWaiters>> = OnceLock::new();
    WAITERS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn thumbnail_job_needed(source: &Path) -> bool {
    let mut waiters = thumbnail_waiters()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let needed = waiters
        .get(source)
        .is_some_and(|waiters| waiters.iter().any(|waiter| !waiter.is_closed()));
    if !needed {
        // Remove while holding the same lock used to subscribe new views.
        // A later request can then enqueue its own job without being cancelled.
        waiters.remove(source);
    }
    needed
}

fn finish_thumbnail_job(source: &Path, result: ThumbnailResult) {
    let waiters = thumbnail_waiters()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(source)
        .unwrap_or_default();
    for waiter in waiters {
        let _ = waiter.send_blocking(result.clone());
    }
}

fn thumbnail_worker_queue(
    queue: &'static OnceLock<async_channel::Sender<ThumbnailJob>>,
    queue_name: &'static str,
    worker_count: usize,
    build: fn(&Path) -> Result<PathBuf, WallpaperError>,
) -> &'static async_channel::Sender<ThumbnailJob> {
    queue.get_or_init(|| {
        let (sender, receiver) = async_channel::bounded::<ThumbnailJob>(THUMBNAIL_QUEUE_CAPACITY);
        for worker_index in 0..worker_count {
            let receiver = receiver.clone();
            let spawn_result = thread::Builder::new()
                .name(format!("wallpaper-{queue_name}-thumb-{worker_index}"))
                .spawn(move || {
                    while let Ok(job) = receiver.recv_blocking() {
                        if !thumbnail_job_needed(&job.source) {
                            continue;
                        }
                        let result = build(&job.source).map_err(|error| error.to_string());
                        finish_thumbnail_job(&job.source, result);
                    }
                });
            if let Err(error) = spawn_result {
                warn!(%error, worker_index, "failed to start wallpaper thumbnail worker");
            }
        }
        sender
    })
}

fn image_thumbnail_queue() -> &'static async_channel::Sender<ThumbnailJob> {
    static QUEUE: OnceLock<async_channel::Sender<ThumbnailJob>> = OnceLock::new();
    thumbnail_worker_queue(&QUEUE, "image", IMAGE_THUMBNAIL_WORKERS, cached_thumbnail)
}

fn video_thumbnail_queue() -> &'static async_channel::Sender<ThumbnailJob> {
    static QUEUE: OnceLock<async_channel::Sender<ThumbnailJob>> = OnceLock::new();
    thumbnail_worker_queue(
        &QUEUE,
        "video",
        VIDEO_THUMBNAIL_WORKERS,
        cached_video_thumbnail,
    )
}

pub(super) fn request_thumbnail(source: PathBuf) -> async_channel::Receiver<ThumbnailResult> {
    let (queue, kind) = if is_video_wallpaper(&source) {
        (video_thumbnail_queue(), "video")
    } else {
        (image_thumbnail_queue(), "image")
    };
    let (sender, receiver) = async_channel::bounded::<ThumbnailResult>(1);
    let should_queue = {
        let mut waiters = thumbnail_waiters()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match waiters.entry(source.clone()) {
            std::collections::hash_map::Entry::Occupied(mut entry) => {
                entry.get_mut().push(sender);
                false
            }
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(vec![sender]);
                true
            }
        }
    };

    if should_queue
        && queue
            .try_send(ThumbnailJob {
                source: source.clone(),
            })
            .is_err()
    {
        finish_thumbnail_job(
            &source,
            Err(format!("{kind} thumbnail queue is full or unavailable")),
        );
    }

    receiver
}

fn cached_thumbnail(source: &Path) -> Result<PathBuf, WallpaperError> {
    let cache_path = thumbnail_cache_path(source)?;
    if cache_path.is_file() {
        return Ok(cache_path);
    }

    let cache_dir = cache_path.parent().ok_or(WallpaperError::StatePath)?;
    fs::create_dir_all(cache_dir).map_err(WallpaperError::Io)?;

    let (_, source_width, source_height) = Pixbuf::file_info(source)
        .ok_or_else(|| WallpaperError::InvalidWallpaper(source.to_path_buf()))?;
    if source_width <= 0 || source_height <= 0 {
        return Err(WallpaperError::InvalidWallpaper(source.to_path_buf()));
    }

    let (scaled_width, scaled_height) = cover_dimensions(source_width, source_height);
    let scaled = Pixbuf::from_file_at_scale(source, scaled_width, scaled_height, false)
        .map_err(WallpaperError::Glib)?;

    let crop_x = ((scaled.width() - CARD_WIDTH) / 2).max(0);
    let crop_y = ((scaled.height() - CARD_HEIGHT) / 2).max(0);
    let crop_width = CARD_WIDTH.min(scaled.width());
    let crop_height = CARD_HEIGHT.min(scaled.height());
    let cropped = scaled.new_subpixbuf(crop_x, crop_y, crop_width, crop_height);
    let thumbnail = if crop_width == CARD_WIDTH && crop_height == CARD_HEIGHT {
        cropped
    } else {
        cropped
            .scale_simple(CARD_WIDTH, CARD_HEIGHT, InterpType::Bilinear)
            .ok_or_else(|| WallpaperError::Thumbnail(source.to_path_buf()))?
    };

    let temporary = unique_temporary_path(&cache_path);
    let _ = fs::remove_file(&temporary);
    if let Err(error) = thumbnail.savev(&temporary, "png", &[]) {
        let _ = fs::remove_file(&temporary);
        return Err(WallpaperError::Glib(error));
    }
    if let Err(error) = fs::rename(&temporary, &cache_path) {
        let _ = fs::remove_file(&temporary);
        return Err(WallpaperError::Io(error));
    }
    Ok(cache_path)
}

fn cached_video_thumbnail(source: &Path) -> Result<PathBuf, WallpaperError> {
    let cache_path = thumbnail_cache_path(source)?;
    if cache_path.is_file() {
        return Ok(cache_path);
    }

    let cache_dir = cache_path.parent().ok_or(WallpaperError::StatePath)?;
    fs::create_dir_all(cache_dir).map_err(WallpaperError::Io)?;

    let filter = format!(
        "scale={CARD_WIDTH}:{CARD_HEIGHT}:force_original_aspect_ratio=increase,crop={CARD_WIDTH}:{CARD_HEIGHT}"
    );
    extract_video_frame(source, &cache_path, Some(&filter))?;
    Ok(cache_path)
}

fn cached_video_still(source: &Path) -> Result<PathBuf, WallpaperError> {
    let cache_path = video_still_cache_path(source)?;
    if cache_path.is_file() {
        return Ok(cache_path);
    }

    let cache_dir = cache_path.parent().ok_or(WallpaperError::StatePath)?;
    fs::create_dir_all(cache_dir).map_err(WallpaperError::Io)?;
    extract_video_frame(source, &cache_path, None)?;
    Ok(cache_path)
}

pub(super) async fn cached_video_still_async(source: PathBuf) -> Result<PathBuf, WallpaperError> {
    let cache_path = video_still_cache_path(&source)?;
    if cache_path.is_file() {
        return Ok(cache_path);
    }

    run_background_async(move || cached_video_still(&source).map_err(|error| error.to_string()))
        .await
        .ok_or_else(|| WallpaperError::Worker("video still worker stopped".into()))?
        .map_err(WallpaperError::Worker)
}

fn extract_video_frame(
    source: &Path,
    destination: &Path,
    video_filter: Option<&str>,
) -> Result<(), WallpaperError> {
    extract_video_frame_at(source, destination, Some(1.0), video_filter)
        .or_else(|_| extract_video_frame_at(source, destination, None, video_filter))
}

fn extract_video_frame_at(
    source: &Path,
    destination: &Path,
    position_seconds: Option<f64>,
    video_filter: Option<&str>,
) -> Result<(), WallpaperError> {
    let temporary = unique_temporary_path(destination);
    let _ = fs::remove_file(&temporary);

    let mut args = vec![
        OsString::from("-hide_banner"),
        OsString::from("-loglevel"),
        OsString::from("error"),
        OsString::from("-nostdin"),
    ];
    if let Some(seconds) = position_seconds {
        args.extend([
            OsString::from("-ss"),
            OsString::from(format!("{seconds:.6}")),
        ]);
    }
    args.push(OsString::from("-i"));
    args.push(source.as_os_str().to_owned());
    args.extend(
        ["-frames:v", "1", "-an", "-sn", "-dn"]
            .into_iter()
            .map(OsString::from),
    );
    if let Some(filter) = video_filter {
        args.extend([OsString::from("-vf"), OsString::from(filter)]);
    }
    match temporary.extension().and_then(OsStr::to_str) {
        Some(extension) if extension.eq_ignore_ascii_case("png") => {
            args.extend([OsString::from("-compression_level"), OsString::from("1")]);
        }
        Some(extension)
            if extension.eq_ignore_ascii_case("jpg") || extension.eq_ignore_ascii_case("jpeg") =>
        {
            args.extend([OsString::from("-q:v"), OsString::from("2")]);
        }
        _ => {}
    }
    args.push(OsString::from("-y"));
    args.push(temporary.as_os_str().to_owned());

    match command::status_inherited(FFMPEG.get(), &args, FFMPEG_TIMEOUT) {
        Ok(()) => {}
        Err(command::StatusError::Io(error)) => {
            let _ = fs::remove_file(&temporary);
            return Err(WallpaperError::Io(error));
        }
        Err(command::StatusError::TimedOut | command::StatusError::Failed) => {
            let _ = fs::remove_file(&temporary);
            return Err(WallpaperError::Ffmpeg(source.to_path_buf()));
        }
    }

    if let Err(error) = fs::rename(&temporary, destination) {
        let _ = fs::remove_file(&temporary);
        return Err(WallpaperError::Io(error));
    }
    Ok(())
}

fn unique_temporary_path(destination: &Path) -> PathBuf {
    static NEXT_ID: AtomicU64 = AtomicU64::new(0);

    let nonce = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let filename = destination
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or("wallpaper.png");
    let extension = destination
        .extension()
        .and_then(OsStr::to_str)
        .unwrap_or("tmp");
    destination.with_file_name(format!(
        ".{filename}.{}.{}.tmp.{extension}",
        std::process::id(),
        nonce
    ))
}

fn video_still_cache_path(source: &Path) -> Result<PathBuf, WallpaperError> {
    let digest = cache_fingerprint(source, VIDEO_STILL_VERSION)?;
    Ok(glib::user_cache_dir()
        .join("obsidian-bar")
        .join("wallpaper-stills")
        .join(format!("{digest}.png")))
}

pub(super) fn thumbnail_cache_path(source: &Path) -> Result<PathBuf, WallpaperError> {
    let digest = cache_fingerprint(source, THUMBNAIL_VERSION)?;
    Ok(glib::user_cache_dir()
        .join("obsidian-bar")
        .join("wallpaper-thumbs")
        .join(format!("{digest}.png")))
}

fn cache_fingerprint(source: &Path, version: &str) -> Result<String, WallpaperError> {
    let metadata = fs::metadata(source).map_err(WallpaperError::Io)?;
    let modified = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok());
    let modified_seconds = modified.map_or(0, |duration| duration.as_secs());
    let modified_nanos = modified.map_or(0, |duration| duration.subsec_nanos());
    let fingerprint = format!(
        "{}:{}:{}:{}:{}",
        source.to_string_lossy(),
        metadata.len(),
        modified_seconds,
        modified_nanos,
        version,
    );

    glib::compute_checksum_for_string(glib::ChecksumType::Sha256, fingerprint)
        .map(|digest| digest.to_string())
        .ok_or_else(|| WallpaperError::Thumbnail(source.to_path_buf()))
}

pub(super) fn cover_dimensions(source_width: i32, source_height: i32) -> (i32, i32) {
    if source_width <= 0 || source_height <= 0 {
        return (CARD_WIDTH, CARD_HEIGHT);
    }

    let source_width = i64::from(source_width);
    let source_height = i64::from(source_height);
    let card_width = i64::from(CARD_WIDTH);
    let card_height = i64::from(CARD_HEIGHT);
    let ceil_div = |numerator: i64, denominator: i64| {
        ((numerator + denominator - 1) / denominator).clamp(1, i64::from(i32::MAX)) as i32
    };

    if source_width * card_height >= card_width * source_height {
        (
            ceil_div(card_height * source_width, source_height).max(CARD_WIDTH),
            CARD_HEIGHT,
        )
    } else {
        (
            CARD_WIDTH,
            ceil_div(card_width * source_height, source_width).max(CARD_HEIGHT),
        )
    }
}
