use super::{CARD_HEIGHT, CARD_WIDTH, WallpaperError, is_video_wallpaper};
use crate::widgets::{command, run_background_async};
use gdk_pixbuf::{InterpType, Pixbuf};
use gio::glib;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, UNIX_EPOCH};

mod queue;
use queue::ThumbnailQueue;

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
const MAX_COVER_INTERMEDIATE_PIXELS: i64 = 4 * 1024 * 1024;

pub(super) type ThumbnailResult = Result<PathBuf, String>;

pub(super) fn request_thumbnail(source: PathBuf) -> async_channel::Receiver<ThumbnailResult> {
    let queue = if is_video_wallpaper(&source) {
        static QUEUE: OnceLock<ThumbnailQueue> = OnceLock::new();
        QUEUE.get_or_init(|| {
            ThumbnailQueue::new(
                "video",
                VIDEO_THUMBNAIL_WORKERS,
                THUMBNAIL_QUEUE_CAPACITY,
                |source| cached_video_thumbnail(source).map_err(|error| error.to_string()),
            )
        })
    } else {
        static QUEUE: OnceLock<ThumbnailQueue> = OnceLock::new();
        QUEUE.get_or_init(|| {
            ThumbnailQueue::new(
                "image",
                IMAGE_THUMBNAIL_WORKERS,
                THUMBNAIL_QUEUE_CAPACITY,
                |source| cached_thumbnail(source).map_err(|error| error.to_string()),
            )
        })
    };
    queue.request(source)
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

    let thumbnail = load_cover_thumbnail(source, source_width, source_height)?;

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

fn load_cover_thumbnail(
    source: &Path,
    source_width: i32,
    source_height: i32,
) -> Result<Pixbuf, WallpaperError> {
    let (scaled_width, scaled_height) = cover_dimensions(source_width, source_height);
    if i64::from(scaled_width) * i64::from(scaled_height) > MAX_COVER_INTERMEDIATE_PIXELS
        && (scaled_width > source_width || scaled_height > source_height)
    {
        // A very thin image can require an enormous upscaled surface just to
        // crop 144×84 pixels. Scale directly into that crop instead. Ordinary
        // images retain the loader's existing downsampling path.
        let original = Pixbuf::from_file(source).map_err(WallpaperError::Glib)?;
        return scale_cover_crop(&original, scaled_width, scaled_height)
            .ok_or_else(|| WallpaperError::Thumbnail(source.to_path_buf()));
    }
    let scaled = Pixbuf::from_file_at_scale(source, scaled_width, scaled_height, false)
        .map_err(WallpaperError::Glib)?;

    let crop_x = ((scaled.width() - CARD_WIDTH) / 2).max(0);
    let crop_y = ((scaled.height() - CARD_HEIGHT) / 2).max(0);
    let crop_width = CARD_WIDTH.min(scaled.width());
    let crop_height = CARD_HEIGHT.min(scaled.height());
    let cropped = scaled.new_subpixbuf(crop_x, crop_y, crop_width, crop_height);
    if crop_width == CARD_WIDTH && crop_height == CARD_HEIGHT {
        Ok(cropped)
    } else {
        cropped
            .scale_simple(CARD_WIDTH, CARD_HEIGHT, InterpType::Bilinear)
            .ok_or_else(|| WallpaperError::Thumbnail(source.to_path_buf()))
    }
}

fn scale_cover_crop(source: &Pixbuf, scaled_width: i32, scaled_height: i32) -> Option<Pixbuf> {
    let thumbnail = Pixbuf::new(
        source.colorspace(),
        source.has_alpha(),
        source.bits_per_sample(),
        CARD_WIDTH,
        CARD_HEIGHT,
    )?;
    let crop_x = (scaled_width - CARD_WIDTH) / 2;
    let crop_y = (scaled_height - CARD_HEIGHT) / 2;
    source.scale(
        &thumbnail,
        0,
        0,
        CARD_WIDTH,
        CARD_HEIGHT,
        -f64::from(crop_x),
        -f64::from(crop_y),
        f64::from(scaled_width) / f64::from(source.width()),
        f64::from(scaled_height) / f64::from(source.height()),
        InterpType::Bilinear,
    );
    Some(thumbnail)
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
    let suffix = format!(
        ":{}:{}:{}:{}",
        metadata.len(),
        modified_seconds,
        modified_nanos,
        version,
    );

    let mut checksum = glib::Checksum::new(glib::ChecksumType::Sha256)
        .ok_or_else(|| WallpaperError::Thumbnail(source.to_path_buf()))?;
    checksum.update(source.as_os_str().as_encoded_bytes());
    checksum.update(suffix.as_bytes());
    checksum
        .string()
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

#[cfg(test)]
mod tests {
    use super::*;

    fn pixels(pixbuf: &Pixbuf) -> Vec<u8> {
        let bytes = pixbuf.read_pixel_bytes();
        let width = (pixbuf.width() * pixbuf.n_channels()) as usize;
        let stride = pixbuf.rowstride() as usize;
        (0..pixbuf.height() as usize)
            .flat_map(|row| bytes[row * stride..row * stride + width].iter().copied())
            .collect()
    }

    #[test]
    fn direct_cover_crop_matches_full_scaling_for_thin_images() {
        let root = std::env::temp_dir().join(format!("obsidian-cover-crop-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        for (width, height) in [(4097, 1), (1, 1025), (17, 11)] {
            for alpha in [false, true] {
                let channels = if alpha { 4 } else { 3 };
                let data = (0..width * height * channels)
                    .map(|index| ((index * 31 + index / channels * 13) % 256) as u8)
                    .collect::<Vec<_>>();
                let original = Pixbuf::from_mut_slice(
                    data,
                    gdk_pixbuf::Colorspace::Rgb,
                    alpha,
                    8,
                    width,
                    height,
                    width * channels,
                );
                let (scaled_width, scaled_height) = cover_dimensions(width, height);
                let reference = original
                    .scale_simple(scaled_width, scaled_height, InterpType::Bilinear)
                    .unwrap();
                let cropped = reference.new_subpixbuf(
                    (scaled_width - CARD_WIDTH) / 2,
                    (scaled_height - CARD_HEIGHT) / 2,
                    CARD_WIDTH,
                    CARD_HEIGHT,
                );
                let direct = scale_cover_crop(&original, scaled_width, scaled_height).unwrap();
                assert!(
                    pixels(&direct) == pixels(&cropped),
                    "direct crop: {width}×{height}, alpha={alpha}"
                );
                drop(cropped);
                drop(reference);
                for format in ["png", "jpeg"] {
                    let file = root.join(format!("{width}-{height}-{alpha}.{format}"));
                    original.savev(&file, format, &[]).unwrap();
                    let loaded = load_cover_thumbnail(&file, width, height).unwrap();
                    let reference =
                        Pixbuf::from_file_at_scale(&file, scaled_width, scaled_height, false)
                            .unwrap();
                    let cropped = reference.new_subpixbuf(
                        (scaled_width - CARD_WIDTH) / 2,
                        (scaled_height - CARD_HEIGHT) / 2,
                        CARD_WIDTH,
                        CARD_HEIGHT,
                    );
                    assert!(
                        pixels(&loaded) == pixels(&cropped),
                        "{format} loader: {width}×{height}, alpha={alpha}"
                    );
                }
            }
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn cache_fingerprints_preserve_utf8_keys_and_distinguish_raw_filenames() {
        use std::os::unix::ffi::OsStringExt;
        let root =
            std::env::temp_dir().join(format!("obsidian-thumbnail-key-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        let plain = root.join("plain.png");
        let first = root.join(OsString::from_vec(b"image\xff.png".to_vec()));
        let second = root.join(OsString::from_vec(b"image\xfe.png".to_vec()));
        fs::write(&plain, b"fixture").unwrap();
        fs::hard_link(&plain, &first).unwrap();
        fs::hard_link(&plain, &second).unwrap();
        assert_eq!(first.to_string_lossy(), second.to_string_lossy());
        assert_ne!(
            cache_fingerprint(&first, THUMBNAIL_VERSION).unwrap(),
            cache_fingerprint(&second, THUMBNAIL_VERSION).unwrap()
        );
        let metadata = fs::metadata(&plain).unwrap();
        let modified = metadata
            .modified()
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap();
        let old = glib::compute_checksum_for_string(
            glib::ChecksumType::Sha256,
            format!(
                "{}:{}:{}:{}:{}",
                plain.to_string_lossy(),
                metadata.len(),
                modified.as_secs(),
                modified.subsec_nanos(),
                THUMBNAIL_VERSION
            ),
        )
        .unwrap();
        assert_eq!(
            cache_fingerprint(&plain, THUMBNAIL_VERSION).unwrap(),
            old.as_str()
        );
        fs::remove_dir_all(root).unwrap();
    }
}
