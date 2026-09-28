//! Shared type metrics.
//!
//! The GTK client sized everything in `em` against the system GTK font
//! (`gtk-font-name`, default "Noto Sans, 10" ≈ 13px) and implemented zoom by
//! dividing that base, so every `em` value scaled with it. iced has no runtime
//! scale factor for a window, so the same thing is done by hand: every text
//! size goes through [`em`] (and the GTK spacing scale through [`space`]) with
//! the app's zoom.

/// 1em, in logical pixels.
pub const BASE_FONT_PX: f32 = 13.0;

/// `factor` em at `zoom`, rounded to whole pixels.
#[must_use]
pub fn em(factor: f32, zoom: f32) -> u32 {
    (factor * BASE_FONT_PX * zoom).round().max(1.0) as u32
}

/// `factor` em at `zoom`, for paddings and spacings.
#[must_use]
pub fn space(factor: f32, zoom: f32) -> f32 {
    factor * BASE_FONT_PX * zoom
}

/// A GTK pixel value as an em factor (`13px` -> `1.0`).
#[must_use]
pub fn px(value: f32) -> f32 {
    value / BASE_FONT_PX
}

/// GTK4's CSS `line-height` multiplies the font's *natural* line height
/// (ascent + descent, 1.38em for the interface font the captures pinned), while
/// iced's `LineHeight::Relative` multiplies the font size. Scaling GTK's
/// factors by this makes a stylesheet line-height render at the pitch GTK gave
/// it — without it every transcript line came out ~20% short, which is what
/// made the port's transcript fit three more rows than GTK's.
pub const GTK_LINE_HEIGHT_RATIO: f32 = 1.38;

/// A GTK CSS `line-height` factor as an iced [`LineHeight`].
///
/// [`LineHeight`]: cosmic::iced::core::text::LineHeight
#[must_use]
pub fn line_height(factor: f32) -> cosmic::iced::core::text::LineHeight {
    cosmic::iced::core::text::LineHeight::Relative(factor * GTK_LINE_HEIGHT_RATIO)
}

/// A 1em em-space value at zoom 1.0, for tests and default sizing.
#[must_use]
pub const fn em_base(factor: f32) -> u32 {
    (factor * BASE_FONT_PX) as u32
}
