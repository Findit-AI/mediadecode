//! End-to-end integration test that exercises `FfmpegVideoStreamDecoder`
//! through the `mediadecode::decoder::VideoStreamDecoder` trait — i.e.
//! the abstraction the rest of mediadecode is built around. Catches the
//! kind of regression where the trait impl compiles in isolation but the
//! generic dispatch path doesn't actually work.
//!
//! Two pieces:
//! 1. **Compile-time check** (always run): a generic helper that bounds
//!    on `VideoStreamDecoder<Adapter = Ffmpeg, Buffer = FfmpegBuffer>`
//!    and accepts a `FfmpegVideoStreamDecoder`. If the trait associated
//!    types or method signatures ever drift, this fails to build.
//! 2. **Runtime smoke test** (`#[ignore]`d unless
//!    `MEDIADECODE_SAMPLE_VIDEO` is set): opens a real video file, calls
//!    `send_packet` / `receive_frame` through the trait, and asserts the
//!    delivered `mediadecode::VideoFrame` carries sane width/height/
//!    pix_fmt.

use ffmpeg::{format, media};
use ffmpeg_next as ffmpeg;
use mediadecode::{Timebase, decoder::VideoStreamDecoder, frame::VideoFrame, packet::VideoPacket};
use mediadecode_ffmpeg::{
  Ffmpeg, FfmpegBuffer, FfmpegVideoStreamDecoder, boundary,
  extras::{VideoFrameExtra, VideoPacketExtra},
};
use std::num::NonZeroU32;

/// Generic helper bounded purely on the `mediadecode` trait — proves
/// `FfmpegVideoStreamDecoder` is reachable through the abstraction.
fn decode_through_trait<D>(
  decoder: &mut D,
  packet: &VideoPacket<VideoPacketExtra, FfmpegBuffer>,
  dst: &mut VideoFrame<mediadecode::PixelFormat, VideoFrameExtra, FfmpegBuffer>,
) -> Result<bool, D::Error>
where
  D: VideoStreamDecoder<Adapter = Ffmpeg, Buffer = FfmpegBuffer>,
{
  decoder.send_packet(packet)?;
  match decoder.receive_frame(dst) {
    Ok(()) => Ok(true),
    Err(_e) => {
      // EAGAIN — caller needs to send more packets. We surface the
      // "no frame yet" outcome as `Ok(false)` for this helper's
      // contract; production code would inspect the error variant.
      Ok(false)
    }
  }
}

/// Compile-time verification: `FfmpegVideoStreamDecoder` satisfies the
/// trait bound the helper requires. If the trait impl ever gets out of
/// sync with the trait definition, this won't compile.
#[test]
fn ffmpeg_video_stream_decoder_implements_trait() {
  fn _accepts<D>(_: D)
  where
    D: VideoStreamDecoder<Adapter = Ffmpeg, Buffer = FfmpegBuffer>,
  {
  }

  // The function never runs — just exists to force the trait check
  // through monomorphisation.
  fn _check_at_compile_time() {
    let opt: Option<FfmpegVideoStreamDecoder> = None;
    if let Some(d) = opt {
      _accepts(d);
    }
  }
}

const SAMPLE_ENV: &str = "MEDIADECODE_SAMPLE_VIDEO";

#[test]
#[ignore = "requires MEDIADECODE_SAMPLE_VIDEO env var pointing at a video file"]
fn decode_one_frame_through_trait() {
  let path = std::env::var_os(SAMPLE_ENV).unwrap_or_else(|| panic!("{SAMPLE_ENV} not set"));

  ffmpeg::init().expect("ffmpeg init");

  let mut input = format::input(&path).expect("open input");
  let stream = input
    .streams()
    .best(media::Type::Video)
    .expect("video stream");
  let stream_index = stream.index();
  let stream_tb = stream.time_base();
  let time_base = Timebase::new(
    stream_tb.numerator() as u32,
    NonZeroU32::new(stream_tb.denominator().max(1) as u32).expect("non-zero den"),
  );

  // Open through the high-level trait-impl type — note we never name
  // the inner HW decoder here.
  let mut decoder =
    FfmpegVideoStreamDecoder::open(stream.parameters(), time_base).expect("open decoder");

  eprintln!(
    "decoder opened — initial path: {}",
    if decoder.is_hardware() {
      "hardware"
    } else {
      "software"
    }
  );

  // Build an empty destination VideoFrame. We use empty placeholder
  // planes; the decoder fills `dst` with a real frame on success.
  let mut dst = make_empty_frame();

  let mut got_frame = false;
  for (s, av_packet) in input.packets() {
    if s.index() != stream_index {
      continue;
    }

    // Wrap the FFmpeg-side AVPacket in a mediadecode::VideoPacket so
    // we go through the trait. We use FfmpegBuffer::take to alias the
    // packet's existing AVBufferRef without copying (the buffer is
    // owned by the av_packet for the duration of this iteration).
    let pkt = match wrap_av_packet_as_mediadecode_packet(&av_packet) {
      Some(p) => p,
      None => continue, // empty packet, skip
    };

    match decode_through_trait(&mut decoder, &pkt, &mut dst) {
      Ok(true) => {
        eprintln!(
          "first frame: {}x{} pix_fmt={:?} (path = {})",
          dst.width(),
          dst.height(),
          dst.pixel_format(),
          if decoder.is_hardware() {
            "hardware"
          } else {
            "software"
          },
        );
        assert!(dst.width() > 0);
        assert!(dst.height() > 0);
        assert_ne!(*dst.pixel_format(), mediadecode::PixelFormat::Unknown);
        got_frame = true;
        break;
      }
      Ok(false) => continue,                   // EAGAIN — keep feeding
      Err(e) => panic!("decode error: {e:?}"), // hard fail
    }
  }

  assert!(got_frame, "no frame delivered through the trait surface");
}

/// Construct an empty `VideoFrame` with placeholder planes, suitable as
/// the `dst` parameter to `VideoStreamDecoder::receive_frame`.
fn make_empty_frame() -> VideoFrame<mediadecode::PixelFormat, VideoFrameExtra, FfmpegBuffer> {
  use ffmpeg_next::ffi::av_buffer_alloc;
  use mediadecode::frame::Plane;

  let make_placeholder = || {
    let raw = unsafe { av_buffer_alloc(1) };
    let buf = unsafe { FfmpegBuffer::take(raw) }.expect("placeholder buffer");
    Plane::new(buf, 0)
  };
  let planes = [
    make_placeholder(),
    make_placeholder(),
    make_placeholder(),
    make_placeholder(),
  ];
  VideoFrame::new(
    0,
    0,
    mediadecode::PixelFormat::Unknown,
    planes,
    0,
    VideoFrameExtra::default(),
  )
}

/// Wrap a borrowed `ffmpeg::Packet` into a `mediadecode::VideoPacket`
/// (parameterized by the FFmpeg adapter's extras + buffer types). Used
/// by the runtime test to feed packets through the trait API.
///
/// Returns `None` if the FFmpeg packet has no buffer attached (empty
/// packet — typical after EOF).
fn wrap_av_packet_as_mediadecode_packet(
  av_packet: &ffmpeg::Packet,
) -> Option<VideoPacket<VideoPacketExtra, FfmpegBuffer>> {
  use ffmpeg::packet::Ref;
  // SAFETY: AVPacket.buf is the refcounted backing storage for the
  // compressed payload; aliasing it via FfmpegBuffer::from_ref bumps
  // the refcount so the buffer outlives the source AVPacket if needed.
  let buf_ptr = unsafe { (*av_packet.as_ptr()).buf };
  let buf = unsafe { FfmpegBuffer::from_ref(buf_ptr) }?;
  let mut pkt = VideoPacket::new(buf, VideoPacketExtra::default());
  // Boundary helper handles the rest of the metadata mapping for the
  // copy-mode `send_packet` path; we replicate the salient fields here
  // so generic code that reads `pkt.pts()` etc. works too.
  let pts = av_packet
    .pts()
    .map(|p| mediadecode::Timestamp::new(p, mediadecode::Timebase::default()));
  if let Some(t) = pts {
    pkt = pkt.with_pts(Some(t));
  }
  // The `boundary` re-export is kept in the import block so this file
  // documents the public API surface; a future revision could add
  // `boundary::video_packet_from_ffmpeg(av_packet)` to mirror the
  // outbound conversion.
  let _ = boundary::is_hardware_pix_fmt(0);
  Some(pkt)
}
