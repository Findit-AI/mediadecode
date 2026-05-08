//! Conversion helpers from FFmpeg `AVFrame` / `AVPacket` to the
//! `mediadecode` types parameterized by [`crate::Ffmpeg`] and
//! [`crate::FfmpegBuffer`].
//!
//! The video-frame conversion is **zero-copy**: each plane is exposed
//! as an `FfmpegBuffer` view into the underlying `AVBufferRef`, so the
//! FFmpeg-allocated pixel memory is shared between the source frame
//! and the produced `VideoFrame`. Cloning the resulting `VideoFrame`
//! bumps refcounts; dropping releases them.

use ffmpeg_next::ffi::{
  AV_NOPTS_VALUE, AVChromaLocation, AVColorPrimaries, AVColorRange, AVColorSpace,
  AVColorTransferCharacteristic, AVFrame, AVPictureType,
};
use mediadecode::{
  PixelFormat, Timebase, Timestamp,
  channel::AudioChannelLayout,
  color::{ChromaLocation, ColorInfo, ColorMatrix, ColorPrimaries, ColorRange, ColorTransfer},
  frame::{AudioFrame, Plane, Rect, SubtitleFrame, VideoFrame},
  subtitle::SubtitlePayload,
};

use crate::{
  FfmpegBuffer, boundary,
  channel_layout::audio_channel_layout_from_ffmpeg,
  extras::{AudioFrameExtra, PictureType, SideDataEntry, SubtitleFrameExtra, VideoFrameExtra},
  frame::{is_supported_cpu_pix_fmt, plane_height_for},
  sample_format::SampleFormat,
};

/// Errors from [`av_frame_to_video_frame`].
#[derive(Debug)]
#[non_exhaustive]
pub enum ConvertError {
  /// `av_frame` was null.
  NullFrame,
  /// The frame's pixel format isn't in the closed CPU-format set this
  /// crate supports for safe per-plane access.
  UnsupportedPixelFormat(PixelFormat),
  /// A plane reported `linesize <= 0` or otherwise inconsistent layout.
  InvalidPlaneLayout {
    /// Plane index.
    plane: usize,
  },
  /// Failed to acquire an `AVBufferRef` for a plane (out of memory, or
  /// the frame's `data[i]` pointer doesn't lie inside any of `buf[]`).
  BufferAcquireFailed {
    /// Plane index whose buffer couldn't be acquired.
    plane: usize,
  },
}

impl core::fmt::Display for ConvertError {
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    match self {
      Self::NullFrame => write!(f, "convert: AVFrame pointer was null"),
      Self::UnsupportedPixelFormat(pf) => {
        write!(f, "convert: unsupported pixel format {pf:?}")
      }
      Self::InvalidPlaneLayout { plane } => {
        write!(f, "convert: invalid layout on plane {plane}")
      }
      Self::BufferAcquireFailed { plane } => {
        write!(f, "convert: could not acquire buffer ref for plane {plane}")
      }
    }
  }
}

impl core::error::Error for ConvertError {}

/// Safe wrapper around [`av_frame_to_video_frame`] taking a borrowed
/// [`ffmpeg::Frame`](ffmpeg_next::Frame). Recommended entry point for
/// most callers — equivalent to passing `frame.as_ptr()` to the
/// unsafe variant, but the FFmpeg side keeps the frame alive for the
/// duration of the call so the safety contract is satisfied
/// internally.
pub fn video_frame_from(
  frame: &ffmpeg_next::Frame,
  time_base: Timebase,
) -> Result<VideoFrame<mediadecode::PixelFormat, VideoFrameExtra, FfmpegBuffer>, ConvertError> {
  // SAFETY: `&frame` keeps the AVFrame alive for the duration of this
  // call; the unsafe convert just reads through the pointer.
  unsafe { av_frame_to_video_frame(frame.as_ptr(), time_base) }
}

/// Safe wrapper around [`av_frame_to_audio_frame`] taking a borrowed
/// [`ffmpeg::frame::Audio`](ffmpeg_next::frame::Audio).
pub fn audio_frame_from(
  frame: &ffmpeg_next::frame::Audio,
  time_base: Timebase,
) -> Result<AudioFrame<SampleFormat, AudioChannelLayout, AudioFrameExtra, FfmpegBuffer>, ConvertError>
{
  // SAFETY: `&frame` keeps the AVFrame alive for the duration of this
  // call.
  unsafe { av_frame_to_audio_frame(frame.as_ptr(), time_base) }
}

/// Safe wrapper around [`av_subtitle_to_subtitle_frame`] taking a
/// borrowed [`ffmpeg::Subtitle`](ffmpeg_next::Subtitle).
pub fn subtitle_frame_from(
  subtitle: &ffmpeg_next::Subtitle,
  time_base: Timebase,
) -> Result<SubtitleFrame<SubtitleFrameExtra, FfmpegBuffer>, ConvertError> {
  // SAFETY: `&subtitle` keeps the AVSubtitle alive for the duration
  // of this call.
  unsafe { av_subtitle_to_subtitle_frame(subtitle.as_ptr(), time_base) }
}

/// Converts an FFmpeg `AVFrame` (CPU-side, post-`av_hwframe_transfer_data`
/// or from a software decoder) into a `mediadecode::VideoFrame`
/// parameterized by [`crate::Ffmpeg`] / [`crate::FfmpegBuffer`].
///
/// `time_base` is the source stream's time base, used to label
/// `pts`/`duration` as mediatime [`Timestamp`]s.
///
/// # Safety
///
/// `av_frame` must be a live `*const AVFrame` for the duration of this
/// call. The frame's `buf[]` references are not consumed; the produced
/// `VideoFrame` holds its own refcounts on each underlying buffer.
pub unsafe fn av_frame_to_video_frame(
  av_frame: *const AVFrame,
  time_base: Timebase,
) -> Result<VideoFrame<mediadecode::PixelFormat, VideoFrameExtra, FfmpegBuffer>, ConvertError> {
  if av_frame.is_null() {
    return Err(ConvertError::NullFrame);
  }
  // SAFETY: Caller guarantees liveness for the duration of the call.
  let frame = unsafe { &*av_frame };

  let pix_fmt = boundary::from_av_pixel_format(frame.format);
  let width = frame.width.max(0) as u32;
  let height = frame.height.max(0) as u32;

  // Build planes. We support the closed CPU-format set for which we
  // know the per-plane height (NV*, P0xx/P2xx/P4xx). Unknown formats
  // would let us read garbage `linesize * height` bytes — refuse.
  if !is_supported_cpu_pix_fmt(pix_fmt) {
    return Err(ConvertError::UnsupportedPixelFormat(pix_fmt));
  }

  let mut planes_out: [Plane<FfmpegBuffer>; 4] = [
    plane_placeholder()?,
    plane_placeholder()?,
    plane_placeholder()?,
    plane_placeholder()?,
  ];
  let mut plane_count: u8 = 0;

  for plane_idx in 0..4 {
    let linesize = frame.linesize[plane_idx];
    if linesize <= 0 {
      // Either we ran past the active plane count (linesize == 0) or
      // the frame uses negative-stride vertical-flip (which our safe
      // accessors refuse).
      if linesize == 0 {
        break;
      }
      return Err(ConvertError::InvalidPlaneLayout { plane: plane_idx });
    }
    let data_ptr = frame.data[plane_idx];
    if data_ptr.is_null() {
      return Err(ConvertError::InvalidPlaneLayout { plane: plane_idx });
    }
    let plane_h = plane_height_for(pix_fmt, plane_idx, height as usize)
      .ok_or(ConvertError::InvalidPlaneLayout { plane: plane_idx })?;
    let plane_bytes = (linesize as usize)
      .checked_mul(plane_h)
      .ok_or(ConvertError::InvalidPlaneLayout { plane: plane_idx })?;

    // Locate the AVBufferRef that backs `data_ptr`. FFmpeg packs
    // multi-plane frames into one or more AVBufferRefs; we scan the
    // buf[] array and pick the one whose data range contains data_ptr.
    let buf = find_backing_buffer(frame, data_ptr, plane_bytes)
      .ok_or(ConvertError::BufferAcquireFailed { plane: plane_idx })?;
    let offset =
      // SAFETY: find_backing_buffer ensures data_ptr lies in [buf.data,
      // buf.data + buf.size); the offset is therefore representable as
      // usize.
      unsafe { (data_ptr as *const u8).offset_from((*buf).data as *const u8) as usize };

    // SAFETY: `buf` is non-null and live for the duration of the call;
    // offset + plane_bytes <= buf.size by find_backing_buffer's check.
    let view = unsafe { FfmpegBuffer::from_ref_view(buf, offset, plane_bytes) }
      .ok_or(ConvertError::BufferAcquireFailed { plane: plane_idx })?;

    planes_out[plane_idx] = Plane::new(view, linesize as u32);
    plane_count = (plane_idx + 1) as u8;
  }

  // pts / duration / time_base
  let pts = if frame.pts != AV_NOPTS_VALUE {
    Some(Timestamp::new(frame.pts, time_base))
  } else {
    None
  };
  let duration = if frame.duration > 0 {
    Some(Timestamp::new(frame.duration, time_base))
  } else {
    None
  };

  // Visible rect (FFmpeg crop).
  let visible_rect = build_visible_rect(frame, width, height);

  // Color metadata (the universal cross-backend bits).
  let color = ColorInfo::UNSPECIFIED
    .with_primaries(map_primaries(frame.color_primaries as i32))
    .with_transfer(map_transfer(frame.color_trc as i32))
    .with_matrix(map_matrix(frame.colorspace as i32))
    .with_range(map_range(frame.color_range as i32))
    .with_chroma_location(map_chroma_loc(frame.chroma_location as i32));

  // Backend-specific extras.
  let extra = build_video_frame_extra(frame);

  // pix_fmt is already mediadecode::PixelFormat thanks to the boundary
  // function above, so we just pass it through.
  let mut out = VideoFrame::new(width, height, pix_fmt, planes_out, plane_count, extra)
    .with_pts(pts)
    .with_duration(duration)
    .with_color(color);
  if let Some(r) = visible_rect {
    out = out.with_visible_rect(Some(r));
  }
  Ok(out)
}

fn plane_placeholder() -> Result<Plane<FfmpegBuffer>, ConvertError> {
  // Allocate a zero-byte AVBufferRef as a placeholder for unused plane
  // slots. `[Plane<B>; 4]` requires four populated entries; we only
  // expose `plane_count` of them through `VideoFrame::planes()`.
  use ffmpeg_next::ffi::av_buffer_alloc;
  let raw = unsafe { av_buffer_alloc(0) };
  // `av_buffer_alloc(0)` is allowed to return null on some platforms;
  // fall back to allocating 1 byte if so.
  let raw = if raw.is_null() {
    unsafe { av_buffer_alloc(1) }
  } else {
    raw
  };
  if raw.is_null() {
    // Truly OOM. Return an error by way of a poisoned plane.
    return Err(ConvertError::BufferAcquireFailed { plane: 4 });
  }
  let buf =
    unsafe { FfmpegBuffer::take(raw) }.ok_or(ConvertError::BufferAcquireFailed { plane: 4 })?;
  Ok(Plane::new(buf, 0))
}

fn build_visible_rect(frame: &AVFrame, width: u32, height: u32) -> Option<Rect> {
  let crop_left = frame.crop_left as u32;
  let crop_top = frame.crop_top as u32;
  let crop_right = frame.crop_right as u32;
  let crop_bottom = frame.crop_bottom as u32;
  if crop_left == 0 && crop_top == 0 && crop_right == 0 && crop_bottom == 0 {
    return None;
  }
  let x = crop_left;
  let y = crop_top;
  let w = width.saturating_sub(crop_left).saturating_sub(crop_right);
  let h = height.saturating_sub(crop_top).saturating_sub(crop_bottom);
  Some(Rect::new(x, y, w, h))
}

fn build_video_frame_extra(frame: &AVFrame) -> VideoFrameExtra {
  let mut out = VideoFrameExtra::default();
  // SAR.
  let sar_num = frame.sample_aspect_ratio.num;
  let sar_den = frame.sample_aspect_ratio.den;
  if sar_num > 0 && sar_den > 0 && (sar_num != 1 || sar_den != 1) {
    out.sample_aspect_ratio = Some((sar_num as u32, sar_den as u32));
  }
  // Picture type.
  out.picture_type = map_picture_type(frame.pict_type);
  // Key frame and interlace flags. AVFrame.flags has dedicated bits
  // for these in recent FFmpeg; the deprecated fields (key_frame,
  // interlaced_frame, top_field_first) still mirror them.
  out.key_frame = frame.flags & ffmpeg_next::ffi::AV_FRAME_FLAG_KEY != 0;
  out.interlaced = frame.flags & ffmpeg_next::ffi::AV_FRAME_FLAG_INTERLACED != 0;
  out.top_field_first = frame.flags & ffmpeg_next::ffi::AV_FRAME_FLAG_TOP_FIELD_FIRST != 0;
  // Best-effort timestamp.
  if frame.best_effort_timestamp != AV_NOPTS_VALUE {
    out.best_effort_timestamp = Some(frame.best_effort_timestamp);
  }
  // Side data — passthrough as raw bytes. Structured parsing for the
  // well-known HDR / timecode entries is left for downstream consumers
  // (ffmpeg-next exposes the raw bytes in `side_data[i]`); a future
  // commit can add `mastering_display` / `content_light_level` /
  // `smpte_timecode` parsing once we wire up the FFmpeg metadata
  // structs.
  out.side_data = unsafe { collect_side_data(frame) };
  out
}

unsafe fn collect_side_data(frame: &AVFrame) -> std::vec::Vec<SideDataEntry> {
  let count = frame.nb_side_data as usize;
  if count == 0 || frame.side_data.is_null() {
    return Vec::new();
  }
  let mut out = Vec::with_capacity(count);
  for i in 0..count {
    let sd = unsafe { *frame.side_data.add(i) };
    if sd.is_null() {
      continue;
    }
    let sd_ref = unsafe { &*sd };
    let kind = sd_ref.type_ as i32;
    let size = sd_ref.size;
    let data_slice = if size == 0 || sd_ref.data.is_null() {
      Vec::new()
    } else {
      unsafe { core::slice::from_raw_parts(sd_ref.data, size).to_vec() }
    };
    out.push(SideDataEntry {
      kind,
      data: data_slice,
    });
  }
  out
}

/// Locate the `AVBufferRef` in `frame.buf[]` that backs `data_ptr`,
/// confirming the requested `bytes` fit inside the buffer.
fn find_backing_buffer(
  frame: &AVFrame,
  data_ptr: *const u8,
  bytes: usize,
) -> Option<*mut ffmpeg_next::ffi::AVBufferRef> {
  for i in 0..frame.buf.len() {
    let buf = frame.buf[i];
    if buf.is_null() {
      continue;
    }
    let buf_data = unsafe { (*buf).data as *const u8 };
    let buf_size = unsafe { (*buf).size };
    if buf_data.is_null() {
      continue;
    }
    let start = buf_data as usize;
    let end = start + buf_size;
    let dp = data_ptr as usize;
    if dp >= start && dp + bytes <= end {
      return Some(buf);
    }
  }
  None
}

fn map_primaries(raw: i32) -> ColorPrimaries {
  match raw {
    x if x == AVColorPrimaries::AVCOL_PRI_BT709 as i32 => ColorPrimaries::Bt709,
    x if x == AVColorPrimaries::AVCOL_PRI_UNSPECIFIED as i32 => ColorPrimaries::Unspecified,
    x if x == AVColorPrimaries::AVCOL_PRI_BT470M as i32 => ColorPrimaries::Bt470M,
    x if x == AVColorPrimaries::AVCOL_PRI_BT470BG as i32 => ColorPrimaries::Bt470Bg,
    x if x == AVColorPrimaries::AVCOL_PRI_SMPTE170M as i32 => ColorPrimaries::Smpte170M,
    x if x == AVColorPrimaries::AVCOL_PRI_SMPTE240M as i32 => ColorPrimaries::Smpte240M,
    x if x == AVColorPrimaries::AVCOL_PRI_FILM as i32 => ColorPrimaries::Film,
    x if x == AVColorPrimaries::AVCOL_PRI_BT2020 as i32 => ColorPrimaries::Bt2020,
    x if x == AVColorPrimaries::AVCOL_PRI_SMPTE428 as i32 => ColorPrimaries::SmpteSt428,
    x if x == AVColorPrimaries::AVCOL_PRI_SMPTE431 as i32 => ColorPrimaries::SmpteRp431,
    x if x == AVColorPrimaries::AVCOL_PRI_SMPTE432 as i32 => ColorPrimaries::SmpteEg432,
    x if x == AVColorPrimaries::AVCOL_PRI_EBU3213 as i32 => ColorPrimaries::Ebu3213E,
    _ => ColorPrimaries::Unspecified,
  }
}

fn map_transfer(raw: i32) -> ColorTransfer {
  match raw {
    x if x == AVColorTransferCharacteristic::AVCOL_TRC_BT709 as i32 => ColorTransfer::Bt709,
    x if x == AVColorTransferCharacteristic::AVCOL_TRC_UNSPECIFIED as i32 => {
      ColorTransfer::Unspecified
    }
    x if x == AVColorTransferCharacteristic::AVCOL_TRC_GAMMA22 as i32 => ColorTransfer::Bt470M,
    x if x == AVColorTransferCharacteristic::AVCOL_TRC_GAMMA28 as i32 => ColorTransfer::Bt470Bg,
    x if x == AVColorTransferCharacteristic::AVCOL_TRC_SMPTE170M as i32 => ColorTransfer::Smpte170M,
    x if x == AVColorTransferCharacteristic::AVCOL_TRC_SMPTE240M as i32 => ColorTransfer::Smpte240M,
    x if x == AVColorTransferCharacteristic::AVCOL_TRC_LINEAR as i32 => ColorTransfer::Linear,
    x if x == AVColorTransferCharacteristic::AVCOL_TRC_LOG as i32 => ColorTransfer::Log100,
    x if x == AVColorTransferCharacteristic::AVCOL_TRC_LOG_SQRT as i32 => ColorTransfer::Log316,
    x if x == AVColorTransferCharacteristic::AVCOL_TRC_IEC61966_2_4 as i32 => {
      ColorTransfer::Iec6196624
    }
    x if x == AVColorTransferCharacteristic::AVCOL_TRC_BT1361_ECG as i32 => {
      ColorTransfer::Bt1361Ecg
    }
    x if x == AVColorTransferCharacteristic::AVCOL_TRC_IEC61966_2_1 as i32 => {
      ColorTransfer::Iec6196621
    }
    x if x == AVColorTransferCharacteristic::AVCOL_TRC_BT2020_10 as i32 => {
      ColorTransfer::Bt2020_10Bit
    }
    x if x == AVColorTransferCharacteristic::AVCOL_TRC_BT2020_12 as i32 => {
      ColorTransfer::Bt2020_12Bit
    }
    x if x == AVColorTransferCharacteristic::AVCOL_TRC_SMPTE2084 as i32 => {
      ColorTransfer::SmpteSt2084Pq
    }
    x if x == AVColorTransferCharacteristic::AVCOL_TRC_SMPTE428 as i32 => ColorTransfer::SmpteSt428,
    x if x == AVColorTransferCharacteristic::AVCOL_TRC_ARIB_STD_B67 as i32 => {
      ColorTransfer::AribStdB67Hlg
    }
    _ => ColorTransfer::Unspecified,
  }
}

fn map_matrix(raw: i32) -> ColorMatrix {
  match raw {
    x if x == AVColorSpace::AVCOL_SPC_BT709 as i32 => ColorMatrix::Bt709,
    x if x == AVColorSpace::AVCOL_SPC_BT2020_NCL as i32 => ColorMatrix::Bt2020Ncl,
    x if x == AVColorSpace::AVCOL_SPC_SMPTE170M as i32 => ColorMatrix::Bt601,
    x if x == AVColorSpace::AVCOL_SPC_BT470BG as i32 => ColorMatrix::Bt601,
    x if x == AVColorSpace::AVCOL_SPC_SMPTE240M as i32 => ColorMatrix::Smpte240m,
    x if x == AVColorSpace::AVCOL_SPC_FCC as i32 => ColorMatrix::Fcc,
    x if x == AVColorSpace::AVCOL_SPC_YCGCO as i32 => ColorMatrix::YCgCo,
    _ => ColorMatrix::Bt709, // ColorMatrix has no Unspecified; Bt709 is FFmpeg's height>=720 default
  }
}

fn map_range(raw: i32) -> ColorRange {
  match raw {
    x if x == AVColorRange::AVCOL_RANGE_JPEG as i32 => ColorRange::Full,
    x if x == AVColorRange::AVCOL_RANGE_MPEG as i32 => ColorRange::Limited,
    _ => ColorRange::Unspecified,
  }
}

fn map_chroma_loc(raw: i32) -> ChromaLocation {
  match raw {
    x if x == AVChromaLocation::AVCHROMA_LOC_LEFT as i32 => ChromaLocation::Left,
    x if x == AVChromaLocation::AVCHROMA_LOC_CENTER as i32 => ChromaLocation::Center,
    x if x == AVChromaLocation::AVCHROMA_LOC_TOPLEFT as i32 => ChromaLocation::TopLeft,
    x if x == AVChromaLocation::AVCHROMA_LOC_TOP as i32 => ChromaLocation::Top,
    x if x == AVChromaLocation::AVCHROMA_LOC_BOTTOMLEFT as i32 => ChromaLocation::BottomLeft,
    x if x == AVChromaLocation::AVCHROMA_LOC_BOTTOM as i32 => ChromaLocation::Bottom,
    _ => ChromaLocation::Unspecified,
  }
}

/// Converts an FFmpeg audio `AVFrame` into a `mediadecode::AudioFrame`.
///
/// The plane payloads are zero-copy views into the source frame's
/// `AVBufferRef` entries (the corresponding `data[i]` is always
/// covered by exactly one of `buf[i]` per FFmpeg's contract). Channel
/// counts above 8 (which would spill into `extended_buf`) are clamped
/// to 8 — the rare cases where this matters can read the source
/// `AVFrame` directly.
///
/// # Safety
///
/// `av_frame` must be a live `*const AVFrame` for the duration of this
/// call and must describe an audio frame (`format` is an
/// `AVSampleFormat`, `nb_samples > 0`, and `data[]` / `buf[]` populated).
pub unsafe fn av_frame_to_audio_frame(
  av_frame: *const AVFrame,
  time_base: Timebase,
) -> Result<AudioFrame<SampleFormat, AudioChannelLayout, AudioFrameExtra, FfmpegBuffer>, ConvertError>
{
  if av_frame.is_null() {
    return Err(ConvertError::NullFrame);
  }
  // SAFETY: caller upholds liveness for the duration of the call.
  let frame = unsafe { &*av_frame };

  let sample_format = SampleFormat::from_raw(frame.format);
  let sample_rate = frame.sample_rate.max(0) as u32;
  let nb_samples = frame.nb_samples.max(0) as u32;

  // ffmpeg_next::ChannelLayout is a #[repr(transparent)] tuple struct
  // around AVChannelLayout. AVChannelLayout is Copy, so we can wrap a
  // copy of the embedded layout for the boundary helper without
  // disturbing the source frame.
  let layout = ffmpeg_next::ChannelLayout(frame.ch_layout);
  let channel_layout = audio_channel_layout_from_ffmpeg(&layout);
  let channel_count_full = channel_layout.channels();
  let channel_count = channel_count_full.min(255) as u8;

  // Plane count: 1 for packed, channel_count for planar (capped at 8).
  let is_planar = sample_format.is_planar();
  let plane_count_full = if is_planar { channel_count as usize } else { 1 };
  let plane_count = plane_count_full.min(8) as u8;

  // Per-plane size in bytes. For audio, FFmpeg only sets `linesize[0]`;
  // every planar plane has the same size, every packed buffer is the
  // total size for all channels.
  let plane_bytes = frame.linesize[0].max(0) as usize;

  let mut planes_out: [Plane<FfmpegBuffer>; 8] = [
    audio_plane_placeholder()?,
    audio_plane_placeholder()?,
    audio_plane_placeholder()?,
    audio_plane_placeholder()?,
    audio_plane_placeholder()?,
    audio_plane_placeholder()?,
    audio_plane_placeholder()?,
    audio_plane_placeholder()?,
  ];

  for plane_idx in 0..plane_count as usize {
    let data_ptr = frame.data[plane_idx];
    if data_ptr.is_null() {
      // Decoder produced fewer planes than the channel count claimed
      // (rare, but possible with some codecs at EOF). Stop here.
      break;
    }
    let buf = find_audio_backing_buffer(frame, data_ptr, plane_bytes)
      .ok_or(ConvertError::BufferAcquireFailed { plane: plane_idx })?;
    let offset =
      // SAFETY: find_audio_backing_buffer guarantees data_ptr lies in
      // [buf.data, buf.data + buf.size); the offset fits in usize.
      unsafe { (data_ptr as *const u8).offset_from((*buf).data as *const u8) as usize };
    // SAFETY: `buf` is non-null and live; offset + plane_bytes <= buf.size
    // by find_audio_backing_buffer's bounds check.
    let view = unsafe { FfmpegBuffer::from_ref_view(buf, offset, plane_bytes) }
      .ok_or(ConvertError::BufferAcquireFailed { plane: plane_idx })?;
    planes_out[plane_idx] = Plane::new(view, plane_bytes as u32);
  }

  let pts = if frame.pts != AV_NOPTS_VALUE {
    Some(Timestamp::new(frame.pts, time_base))
  } else {
    None
  };
  let duration = if frame.duration > 0 {
    Some(Timestamp::new(frame.duration, time_base))
  } else {
    None
  };

  let mut extra = AudioFrameExtra::default();
  if frame.best_effort_timestamp != AV_NOPTS_VALUE {
    extra.best_effort_timestamp = Some(frame.best_effort_timestamp);
  }
  // SAFETY: caller upholds liveness for the duration of the call;
  // collect_side_data only reads through valid AVFrameSideData entries.
  extra.side_data = unsafe { collect_side_data(frame) };

  Ok(
    AudioFrame::new(
      sample_rate,
      nb_samples,
      channel_count,
      sample_format,
      channel_layout,
      planes_out,
      plane_count,
      extra,
    )
    .with_pts(pts)
    .with_duration(duration),
  )
}

fn audio_plane_placeholder() -> Result<Plane<FfmpegBuffer>, ConvertError> {
  use ffmpeg_next::ffi::av_buffer_alloc;
  let raw = unsafe { av_buffer_alloc(1) };
  if raw.is_null() {
    return Err(ConvertError::BufferAcquireFailed { plane: 8 });
  }
  let buf =
    unsafe { FfmpegBuffer::take(raw) }.ok_or(ConvertError::BufferAcquireFailed { plane: 8 })?;
  Ok(Plane::new(buf, 0))
}

fn find_audio_backing_buffer(
  frame: &AVFrame,
  data_ptr: *const u8,
  bytes: usize,
) -> Option<*mut ffmpeg_next::ffi::AVBufferRef> {
  // Audio frames pack each plane into a separate AVBufferRef in buf[].
  // Same scan as the video path — finds whichever buffer's data range
  // contains data_ptr.
  for i in 0..frame.buf.len() {
    let buf = frame.buf[i];
    if buf.is_null() {
      continue;
    }
    let buf_data = unsafe { (*buf).data as *const u8 };
    let buf_size = unsafe { (*buf).size };
    if buf_data.is_null() {
      continue;
    }
    let start = buf_data as usize;
    let end = start + buf_size;
    let dp = data_ptr as usize;
    if dp >= start && dp + bytes <= end {
      return Some(buf);
    }
  }
  None
}

/// Converts an FFmpeg `AVSubtitle` into a `mediadecode::SubtitleFrame`.
///
/// Strategy:
/// - If the subtitle contains any text/ASS rects, produce a
///   [`SubtitlePayload::Text`] whose buffer is the concatenation of
///   their UTF-8 contents (newline-separated).
/// - Otherwise, if the subtitle contains bitmap rects, produce a
///   [`SubtitlePayload::Bitmap`] with one [`mediadecode::subtitle::BitmapRegion`]
///   per rect (paletted indices and RGBA palette copied into fresh
///   refcounted FfmpegBuffers, since `AVSubtitleRect` data is not
///   refcounted).
/// - An empty subtitle (no rects) becomes an empty `Text` payload.
///
/// `time_base` is the source stream's time base, used to label
/// `pts` / `duration`. The duration is computed as
/// `(end_display_time - start_display_time)` in milliseconds, then
/// rescaled into `time_base`.
///
/// # Safety
///
/// `av_subtitle` must be a live `*const AVSubtitle` for the duration
/// of this call; the rect array (`av_subtitle.rects`) must be valid
/// for `av_subtitle.num_rects` entries.
pub unsafe fn av_subtitle_to_subtitle_frame(
  av_subtitle: *const ffmpeg_next::ffi::AVSubtitle,
  time_base: Timebase,
) -> Result<SubtitleFrame<SubtitleFrameExtra, FfmpegBuffer>, ConvertError> {
  if av_subtitle.is_null() {
    return Err(ConvertError::NullFrame);
  }
  // SAFETY: caller upholds liveness.
  let sub = unsafe { &*av_subtitle };

  let mut text_chunks: std::vec::Vec<u8> = std::vec::Vec::new();
  let mut bitmap_regions: std::vec::Vec<mediadecode::subtitle::BitmapRegion<FfmpegBuffer>> =
    std::vec::Vec::new();

  let count = sub.num_rects as usize;
  for i in 0..count {
    if sub.rects.is_null() {
      break;
    }
    // SAFETY: sub.rects points to num_rects valid entries per FFmpeg's
    // contract.
    let rect_ptr = unsafe { *sub.rects.add(i) };
    if rect_ptr.is_null() {
      continue;
    }
    let rect = unsafe { &*rect_ptr };

    use ffmpeg_next::ffi::AVSubtitleType::*;
    match rect.type_ {
      SUBTITLE_TEXT if !rect.text.is_null() => {
        // SAFETY: `rect.text` is documented as a 0-terminated UTF-8
        // string, owned by FFmpeg for the lifetime of the AVSubtitle.
        let bytes = unsafe { core::ffi::CStr::from_ptr(rect.text) }.to_bytes();
        if !text_chunks.is_empty() {
          text_chunks.push(b'\n');
        }
        text_chunks.extend_from_slice(bytes);
      }
      SUBTITLE_ASS if !rect.ass.is_null() => {
        // SAFETY: `rect.ass` is documented as 0-terminated UTF-8.
        let bytes = unsafe { core::ffi::CStr::from_ptr(rect.ass) }.to_bytes();
        if !text_chunks.is_empty() {
          text_chunks.push(b'\n');
        }
        text_chunks.extend_from_slice(bytes);
      }
      SUBTITLE_BITMAP => {
        // Bitmap region. data[0] = paletted indices, data[1] = RGBA
        // palette (256 entries × 4 bytes = 1024 bytes max). Both are
        // owned by FFmpeg and not refcounted; copy into fresh buffers.
        let w = rect.w.max(0) as u32;
        let h = rect.h.max(0) as u32;
        let stride = rect.linesize[0].max(0) as u32;
        if rect.data[0].is_null() || stride == 0 || h == 0 {
          continue;
        }
        let data_len = (stride as usize).saturating_mul(h as usize);
        // SAFETY: caller-validated AVSubtitleRect — data[0] is valid
        // for `linesize[0] * h` bytes.
        let data_slice = unsafe { core::slice::from_raw_parts(rect.data[0], data_len) };
        let data_buf = FfmpegBuffer::copy_from_slice(data_slice)
          .ok_or(ConvertError::BufferAcquireFailed { plane: 0 })?;
        // Palette: 256 RGBA entries (1024 bytes) per FFmpeg's contract;
        // some encoders use fewer, but the buffer is always 1024 bytes
        // wide. nb_colors tells us how many of those entries are used.
        let palette_len = 256 * 4;
        let palette_buf = if rect.data[1].is_null() {
          FfmpegBuffer::copy_from_slice(&[])
            .ok_or(ConvertError::BufferAcquireFailed { plane: 1 })?
        } else {
          // SAFETY: palette buffer is 256*4 bytes per FFmpeg's contract.
          let p = unsafe { core::slice::from_raw_parts(rect.data[1], palette_len) };
          FfmpegBuffer::copy_from_slice(p).ok_or(ConvertError::BufferAcquireFailed { plane: 1 })?
        };
        bitmap_regions.push(mediadecode::subtitle::BitmapRegion::new(
          rect.x.max(0) as u32,
          rect.y.max(0) as u32,
          w,
          h,
          stride,
          data_buf,
          palette_buf,
        ));
      }
      _ => {}
    }
  }

  let payload = if !text_chunks.is_empty() {
    let buf = FfmpegBuffer::copy_from_slice(&text_chunks)
      .ok_or(ConvertError::BufferAcquireFailed { plane: 0 })?;
    SubtitlePayload::Text {
      text: buf,
      language: None,
    }
  } else if !bitmap_regions.is_empty() {
    SubtitlePayload::Bitmap {
      regions: bitmap_regions,
    }
  } else {
    // No rects (or only `None`-typed) — empty text payload.
    let buf =
      FfmpegBuffer::copy_from_slice(&[]).ok_or(ConvertError::BufferAcquireFailed { plane: 0 })?;
    SubtitlePayload::Text {
      text: buf,
      language: None,
    }
  };

  let pts = if sub.pts != AV_NOPTS_VALUE {
    Some(Timestamp::new(sub.pts, time_base))
  } else {
    None
  };

  let extra = SubtitleFrameExtra {
    start_display_time: sub.start_display_time,
    end_display_time: sub.end_display_time,
  };

  Ok(SubtitleFrame::new(payload, extra).with_pts(pts))
}

fn map_picture_type(raw: AVPictureType) -> PictureType {
  match raw {
    AVPictureType::AV_PICTURE_TYPE_I => PictureType::I,
    AVPictureType::AV_PICTURE_TYPE_P => PictureType::P,
    AVPictureType::AV_PICTURE_TYPE_B => PictureType::B,
    AVPictureType::AV_PICTURE_TYPE_S => PictureType::S,
    AVPictureType::AV_PICTURE_TYPE_SI => PictureType::Si,
    AVPictureType::AV_PICTURE_TYPE_SP => PictureType::Sp,
    AVPictureType::AV_PICTURE_TYPE_BI => PictureType::Bi,
    _ => PictureType::Unspecified,
  }
}
