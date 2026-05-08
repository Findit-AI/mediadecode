//! Conversions from FFmpeg's [`ffmpeg_next::ChannelLayout`] /
//! [`ffmpeg_next::ffi::AVChannelOrder`] to the channel-layout types
//! mediadecode owns ([`mediadecode::channel::ChannelLayoutKind`],
//! [`mediadecode::channel::AudioChannelOrderKind`],
//! [`mediadecode::channel::AudioChannelSpec`],
//! [`mediadecode::channel::AudioChannelLayout`]).
//!
//! These live as **free functions** (not `From` trait impls) because of
//! Rust's orphan rule: this crate owns neither `From` nor
//! `mediadecode::channel::*`, so we can't write the `impl` here. Calling
//! `mediadecode_ffmpeg::audio_channel_layout_from_ffmpeg(layout)` is the
//! ergonomic boundary instead.

use core::slice;

use ffmpeg_next::{ChannelLayout, ffi};
use mediadecode::channel::{
  AudioChannelLayout, AudioChannelOrderKind, AudioChannelSpec, ChannelLayoutKind,
};
use smol_str::SmolStr;
use std::vec::Vec;

/// Maps an FFmpeg [`ChannelLayout`] to the high-level
/// [`ChannelLayoutKind`] tag.
///
/// Returns [`ChannelLayoutKind::Unknown`] for layouts that don't match
/// one of FFmpeg's named-layout constants.
pub fn channel_layout_kind_from_ffmpeg(value: &ChannelLayout) -> ChannelLayoutKind {
  match () {
    () if value.eq(&ChannelLayout::MONO) => ChannelLayoutKind::Mono,
    () if value.eq(&ChannelLayout::STEREO) => ChannelLayoutKind::Stereo,
    () if value.eq(&ChannelLayout::STEREO_DOWNMIX) => ChannelLayoutKind::StereoDownmix,
    () if value.eq(&ChannelLayout::SURROUND) => ChannelLayoutKind::Surround,
    () if value.eq(&ChannelLayout::QUAD) => ChannelLayoutKind::Quad,
    () if value.eq(&ChannelLayout::HEXAGONAL) => ChannelLayoutKind::Hexagonal,
    () if value.eq(&ChannelLayout::OCTAGONAL) => ChannelLayoutKind::Octagonal,
    () if value.eq(&ChannelLayout::HEXADECAGONAL) => ChannelLayoutKind::Hexadecagonal,
    () if value.eq(&ChannelLayout::CUBE) => ChannelLayoutKind::Cube,
    () if value.eq(&ChannelLayout::_2POINT1) => ChannelLayoutKind::Ch2_1,
    () if value.eq(&ChannelLayout::_2_1) => ChannelLayoutKind::Ch2_1Alt,
    () if value.eq(&ChannelLayout::_2_2) => ChannelLayoutKind::Ch2_2,
    () if value.eq(&ChannelLayout::_3POINT1) => ChannelLayoutKind::Ch3_1,
    () if value.eq(&ChannelLayout::_3POINT1POINT2) => ChannelLayoutKind::Ch3_1_2,
    () if value.eq(&ChannelLayout::_4POINT0) => ChannelLayoutKind::Ch4_0,
    () if value.eq(&ChannelLayout::_4POINT1) => ChannelLayoutKind::Ch4_1,
    () if value.eq(&ChannelLayout::_5POINT0) => ChannelLayoutKind::Ch5_0,
    () if value.eq(&ChannelLayout::_5POINT0_BACK) => ChannelLayoutKind::Ch5_0Back,
    () if value.eq(&ChannelLayout::_5POINT1) => ChannelLayoutKind::Ch5_1,
    () if value.eq(&ChannelLayout::_5POINT1_BACK) => ChannelLayoutKind::Ch5_1Back,
    () if value.eq(&ChannelLayout::_5POINT1POINT2_BACK) => ChannelLayoutKind::Ch5_1_2Back,
    () if value.eq(&ChannelLayout::_5POINT1POINT4_BACK) => ChannelLayoutKind::Ch5_1_4Back,
    () if value.eq(&ChannelLayout::_6POINT0) => ChannelLayoutKind::Ch6_0,
    () if value.eq(&ChannelLayout::_6POINT0_FRONT) => ChannelLayoutKind::Ch6_0Front,
    () if value.eq(&ChannelLayout::_6POINT1) => ChannelLayoutKind::Ch6_1,
    () if value.eq(&ChannelLayout::_6POINT1_BACK) => ChannelLayoutKind::Ch6_1Back,
    () if value.eq(&ChannelLayout::_6POINT1_FRONT) => ChannelLayoutKind::Ch6_1Front,
    () if value.eq(&ChannelLayout::_7POINT0) => ChannelLayoutKind::Ch7_0,
    () if value.eq(&ChannelLayout::_7POINT0_FRONT) => ChannelLayoutKind::Ch7_0Front,
    () if value.eq(&ChannelLayout::_7POINT1) => ChannelLayoutKind::Ch7_1,
    () if value.eq(&ChannelLayout::_7POINT1_WIDE) => ChannelLayoutKind::Ch7_1Wide,
    () if value.eq(&ChannelLayout::_7POINT1_WIDE_BACK) => ChannelLayoutKind::Ch7_1WideBack,
    () if value.eq(&ChannelLayout::_7POINT1_TOP_BACK) => ChannelLayoutKind::Ch7_1TopBack,
    () if value.eq(&ChannelLayout::_7POINT1POINT2) => ChannelLayoutKind::Ch7_1_2,
    () if value.eq(&ChannelLayout::_7POINT1POINT4_BACK) => ChannelLayoutKind::Ch7_1_4Back,
    () if value.eq(&ChannelLayout::_7POINT2POINT3) => ChannelLayoutKind::Ch7_2_3,
    () if value.eq(&ChannelLayout::_9POINT1POINT4_BACK) => ChannelLayoutKind::Ch9_1_4Back,
    () if value.eq(&ChannelLayout::_22POINT2) => ChannelLayoutKind::Ch22_2,
    () => ChannelLayoutKind::Unknown,
  }
}

/// Maps FFmpeg's [`AVChannelOrder`](ffi::AVChannelOrder) to the
/// [`AudioChannelOrderKind`] tag.
pub fn audio_channel_order_kind_from_ffmpeg(value: ffi::AVChannelOrder) -> AudioChannelOrderKind {
  match value {
    ffi::AVChannelOrder::AV_CHANNEL_ORDER_NATIVE => AudioChannelOrderKind::Native,
    ffi::AVChannelOrder::AV_CHANNEL_ORDER_CUSTOM => AudioChannelOrderKind::Custom,
    ffi::AVChannelOrder::AV_CHANNEL_ORDER_AMBISONIC => AudioChannelOrderKind::Ambisonic,
    _ => AudioChannelOrderKind::Unspecified,
  }
}

/// Builds a fully-populated [`AudioChannelLayout`] from an FFmpeg
/// [`ChannelLayout`].
///
/// - Native / Ambisonic layouts populate `native_mask` from
///   [`ChannelLayout::bits`] (clearing it to `None` if zero).
/// - Custom layouts populate `custom_channels` from FFmpeg's per-channel
///   list (`AVChannelLayout.u.map`), with each label drawn from
///   `AVChannelCustom.name`.
/// - `description` carries the result of `av_channel_layout_describe`
///   (FFmpeg's human-readable rendering — e.g. `"5.1(side)"`).
pub fn audio_channel_layout_from_ffmpeg(value: &ChannelLayout) -> AudioChannelLayout {
  let order = audio_channel_order_kind_from_ffmpeg(value.0.order);
  let native_mask = match order {
    AudioChannelOrderKind::Native | AudioChannelOrderKind::Ambisonic => {
      Some(value.bits()).filter(|mask| *mask != 0)
    }
    _ => None,
  };

  AudioChannelLayout::new(value.channels().max(0) as u32)
    .with_order(order)
    .with_known_kind(channel_layout_kind_from_ffmpeg(value))
    .with_native_mask(native_mask)
    .with_custom_channels(custom_channels(value))
    .with_description(describe_layout(value))
}

fn custom_channels(layout: &ChannelLayout) -> Vec<AudioChannelSpec> {
  if layout.0.order != ffi::AVChannelOrder::AV_CHANNEL_ORDER_CUSTOM {
    return Vec::new();
  }
  let count = layout.0.nb_channels.max(0) as usize;
  if count == 0 {
    return Vec::new();
  }
  // SAFETY: The `u` field is a union; reading `.map` is sound when
  // `order == CUSTOM` per FFmpeg's documented contract. The pointer
  // may still be null on a malformed layout — guard explicitly.
  let ptr = unsafe { layout.0.u.map };
  if ptr.is_null() {
    return Vec::new();
  }
  // SAFETY: AVChannelLayout's contract says `.u.map` points to
  // `nb_channels` valid `AVChannelCustom` entries when order == CUSTOM.
  let slice_ref = unsafe { slice::from_raw_parts(ptr, count) };
  slice_ref
    .iter()
    .enumerate()
    .map(|(index, channel)| {
      AudioChannelSpec::new(index as u32, channel.id as u32)
        .with_label(custom_channel_label(channel))
    })
    .collect()
}

fn custom_channel_label(channel: &ffi::AVChannelCustom) -> SmolStr {
  // SAFETY: AVChannelCustom.name is a fixed-size [c_char; 16] inline
  // buffer. Re-interpreting as bytes for UTF-8 lossy decoding is sound.
  let bytes =
    unsafe { slice::from_raw_parts(channel.name.as_ptr() as *const u8, channel.name.len()) };
  let end = bytes
    .iter()
    .position(|byte| *byte == 0)
    .unwrap_or(bytes.len());
  if end == 0 {
    return SmolStr::default();
  }
  SmolStr::new(std::string::String::from_utf8_lossy(&bytes[..end]))
}

fn describe_layout(layout: &ChannelLayout) -> SmolStr {
  // `av_channel_layout_describe` returns the number of bytes needed
  // (excluding the NUL terminator). Start with a 128-byte buffer —
  // comfortably bigger than every named layout — and grow once if it
  // wasn't enough.
  let mut buf = std::vec![0i8; 128];
  let mut needed = unsafe {
    ffi::av_channel_layout_describe(&layout.0 as *const _, buf.as_mut_ptr(), buf.len())
  };
  if needed < 0 {
    return SmolStr::default();
  }
  if needed as usize >= buf.len() {
    buf.resize(needed as usize + 1, 0);
    needed = unsafe {
      ffi::av_channel_layout_describe(&layout.0 as *const _, buf.as_mut_ptr(), buf.len())
    };
    if needed < 0 {
      return SmolStr::default();
    }
  }
  // SAFETY: buf is heap-allocated, NUL-terminated by FFmpeg's contract.
  let bytes = unsafe { slice::from_raw_parts(buf.as_ptr() as *const u8, buf.len()) };
  let end = bytes
    .iter()
    .position(|byte| *byte == 0)
    .unwrap_or(needed as usize);
  if end == 0 {
    return SmolStr::default();
  }
  SmolStr::new(std::string::String::from_utf8_lossy(&bytes[..end]))
}
