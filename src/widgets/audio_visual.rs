pub(super) const ICON_MUTED: &str = "\u{f0581}";
pub(super) const ICON_HIGH: &str = "\u{f057e}";

const ICON_ZERO: &str = "\u{f075f}";
const ICON_LOW: &str = "\u{f057f}";

pub(super) fn volume_icon(volume: f64, muted: bool) -> &'static str {
    if muted {
        ICON_MUTED
    } else if volume <= 0.01 {
        ICON_ZERO
    } else if volume < 0.5 {
        ICON_LOW
    } else {
        ICON_HIGH
    }
}
