//! Decode a video file through `mediadecode`'s trait surface.
//!
//! Demonstrates the abstraction this crate is built around: callers
//! depend on `mediadecode::decoder::VideoStreamDecoder` and never
//! reach for `ffmpeg-next` types directly. The `FfmpegVideoStreamDecoder`
//! handles the HW probe, SW fallback, and frame conversion under the
//! hood; downstream code stays backend-neutral.
//!
//! ```sh
//! cargo run --release --example decode_via_trait -- /path/to/video.mp4
//! ```
//!
//! Compare with `examples/decode.rs`, which uses the lower-level
//! `VideoDecoder` (HW-probe wrapper) directly — no SW fallback, more
//! plumbing.
//!
//! What the example demonstrates that the lower-level one doesn't:
//! - **Backend-neutral consumer code** — the `decode_one_video` helper
//!   is generic over `VideoStreamDecoder<Adapter = Ffmpeg, Buffer =
//!   FfmpegBuffer>`. The same shape would work for a future
//!   `mediadecode-webcodecs` etc., bound on its own adapter type.
//! - **Transparent SW fallback** — when no HW backend can decode, the
//!   trait impl opens a software `ffmpeg::decoder::Video` and replays
//!   buffered packets. The caller sees no special case; the
//!   `is_hardware()` / `is_software()` accessors expose the path
//!   transition for logging.
//! - **Unified `mediadecode::PixelFormat`** — the per-frame pix_fmt
//!   prints the same lowercase FFmpeg name (`nv12`, `p010le`,
//!   `yuv420p`, …) regardless of which backend produced it.

use ffmpeg::{format, media};
use ffmpeg_next as ffmpeg;
use mediadecode::{
  Timebase,
  decoder::VideoStreamDecoder,
  frame::{Plane, VideoFrame},
  packet::VideoPacket,
};
use mediadecode_ffmpeg::{
  Ffmpeg, FfmpegBuffer, FfmpegVideoStreamDecoder,
  extras::{VideoFrameExtra, VideoPacketExtra},
};
use std::num::NonZeroU32;

fn main() -> Result<(), Box<dyn std::error::Error>> {
  let path = std::env::args()
    .nth(1)
    .ok_or("usage: decode_via_trait <video-file>")?;

  ffmpeg::init()?;

  let mut input = format::input(&path)?;
  let stream = input
    .streams()
    .best(media::Type::Video)
    .ok_or("no video stream")?;
  let stream_index = stream.index();
  let stream_tb = stream.time_base();
  let time_base = Timebase::new(
    stream_tb.numerator() as u32,
    NonZeroU32::new(stream_tb.denominator().max(1) as u32).ok_or("bad time base")?,
  );

  // Open through the trait-impl type. SW fallback is automatic; this
  // returns Err only if both HW and SW open fail.
  let mut decoder = FfmpegVideoStreamDecoder::open(stream.parameters(), time_base)?;
  println!(
    "decoder opened on the {} path",
    if decoder.is_hardware() {
      "hardware"
    } else {
      "software"
    }
  );

  let count = decode_one_video(&mut decoder, &mut input, stream_index)?;
  println!(
    "decoded {count} frame(s); final path: {}",
    if decoder.is_hardware() {
      "hardware"
    } else {
      "software"
    }
  );
  Ok(())
}

/// Generic helper bounded purely on the `mediadecode` trait. Any decoder
/// that satisfies `VideoStreamDecoder<Adapter = Ffmpeg, Buffer =
/// FfmpegBuffer>` works here — `FfmpegVideoStreamDecoder` is just one
/// instance.
fn decode_one_video<D>(
  decoder: &mut D,
  input: &mut format::context::Input,
  stream_index: usize,
) -> Result<u64, Box<dyn std::error::Error>>
where
  D: VideoStreamDecoder<Adapter = Ffmpeg, Buffer = FfmpegBuffer>,
  D::Error: std::error::Error + Send + Sync + 'static,
{
  let mut dst = empty_video_frame();
  let mut count: u64 = 0;

  let drain = |decoder: &mut D,
               dst: &mut VideoFrame<mediadecode::PixelFormat, VideoFrameExtra, FfmpegBuffer>,
               count: &mut u64|
   -> Result<(), Box<dyn std::error::Error>> {
    loop {
      match decoder.receive_frame(dst) {
        Ok(()) => {
          *count += 1;
          println!(
            "frame#{count} pts={:?} {}x{} pix_fmt={}",
            dst.pts().map(|t| t.pts()),
            dst.width(),
            dst.height(),
            dst.pixel_format(),
          );
        }
        // Any error from receive_frame ends the drain. The trait's
        // contract uses backend-specific errors for "no frame ready"
        // (EAGAIN-equivalent) and end-of-stream — we conservatively
        // treat all errors as terminal in this example.
        Err(_) => break,
      }
    }
    Ok(())
  };

  for (s, av_packet) in input.packets() {
    if s.index() != stream_index {
      continue;
    }
    let pkt = match wrap_av_packet(&av_packet) {
      Some(p) => p,
      None => continue,
    };
    if let Err(e) = decoder.send_packet(&pkt) {
      // EAGAIN: drain and retry, otherwise propagate.
      drain(decoder, &mut dst, &mut count)?;
      decoder.send_packet(&pkt).map_err(|_| e)?;
    }
    drain(decoder, &mut dst, &mut count)?;
  }
  let _ = decoder.send_eof();
  drain(decoder, &mut dst, &mut count)?;
  Ok(count)
}

/// Construct an empty `VideoFrame` to use as the `receive_frame`
/// destination. Real consumers can pool one of these and reuse across
/// frames.
fn empty_video_frame() -> VideoFrame<mediadecode::PixelFormat, VideoFrameExtra, FfmpegBuffer> {
  use ffmpeg::ffi::av_buffer_alloc;
  let placeholder = || {
    let raw = unsafe { av_buffer_alloc(1) };
    let buf = unsafe { FfmpegBuffer::take(raw) }.expect("placeholder buffer");
    Plane::new(buf, 0)
  };
  let planes = [placeholder(), placeholder(), placeholder(), placeholder()];
  VideoFrame::new(
    0,
    0,
    mediadecode::PixelFormat::Unknown,
    planes,
    0,
    VideoFrameExtra::default(),
  )
}

/// Wrap a borrowed `ffmpeg::Packet` as a `mediadecode::VideoPacket`.
/// Aliases the FFmpeg packet's underlying AVBufferRef via a refcount
/// bump (no copy).
fn wrap_av_packet(
  av_packet: &ffmpeg::Packet,
) -> Option<VideoPacket<VideoPacketExtra, FfmpegBuffer>> {
  use ffmpeg::packet::Ref;
  // SAFETY: AVPacket.buf is the documented refcounted backing for the
  // compressed bitstream; bumping the refcount via FfmpegBuffer::from_ref
  // is sound for the duration of the source packet's lifetime.
  let buf_ptr = unsafe { (*av_packet.as_ptr()).buf };
  let buf = unsafe { FfmpegBuffer::from_ref(buf_ptr) }?;
  let mut pkt = VideoPacket::new(buf, VideoPacketExtra::default());
  if let Some(p) = av_packet.pts() {
    pkt = pkt.with_pts(Some(mediadecode::Timestamp::new(
      p,
      mediadecode::Timebase::default(),
    )));
  }
  if let Some(d) = av_packet.dts() {
    pkt = pkt.with_dts(Some(mediadecode::Timestamp::new(
      d,
      mediadecode::Timebase::default(),
    )));
  }
  Some(pkt)
}
