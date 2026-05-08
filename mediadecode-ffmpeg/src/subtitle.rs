//! `mediadecode::SubtitleDecoder` impl backed by
//! `ffmpeg::decoder::Subtitle`.
//!
//! Subtitles use FFmpeg's legacy synchronous `decode()` API rather
//! than `send_packet`/`receive_frame`. We bridge the difference by
//! converting the produced `AVSubtitle` into a
//! [`mediadecode::SubtitleFrame`] inside [`SubtitleDecoder::send_packet`]
//! and stashing it in `pending` for the next [`SubtitleDecoder::receive_frame`]
//! call. This matches the trait's contract: `send_packet` enqueues
//! work, `receive_frame` drains one decoded frame at a time, and
//! `NoFrameReady` is signalled via [`SubtitleDecodeError::NoFrameReady`].

use std::option::Option;

use ffmpeg_next::codec::Parameters;
use mediadecode::{
  Timebase, decoder::SubtitleDecoder, frame::SubtitleFrame, packet::SubtitlePacket,
};

use crate::{
  Error, Ffmpeg, FfmpegBuffer, boundary, convert,
  extras::{SubtitleFrameExtra, SubtitlePacketExtra},
};

/// `mediadecode::SubtitleDecoder` impl wrapping `ffmpeg::decoder::Subtitle`.
///
/// Subtitle decoders are stateless from FFmpeg's perspective — each
/// `decode()` call consumes one packet and produces zero-or-one
/// `AVSubtitle`. The pending-frame buffer here is a one-slot queue
/// so the trait's `send_packet` / `receive_frame` split works.
pub struct FfmpegSubtitleStreamDecoder {
  decoder: ffmpeg_next::decoder::Subtitle,
  scratch: ffmpeg_next::Subtitle,
  pending: Option<SubtitleFrame<SubtitleFrameExtra, FfmpegBuffer>>,
  time_base: Timebase,
}

impl FfmpegSubtitleStreamDecoder {
  /// Opens a subtitle decoder for the given codec parameters.
  pub fn open(parameters: Parameters, time_base: Timebase) -> Result<Self, SubtitleDecodeError> {
    let ctx = ffmpeg_next::codec::Context::from_parameters(parameters)
      .map_err(|e| SubtitleDecodeError::Decode(Error::Ffmpeg(e)))?;
    let decoder = ctx
      .decoder()
      .subtitle()
      .map_err(|e| SubtitleDecodeError::Decode(Error::Ffmpeg(e)))?;
    Ok(Self {
      decoder,
      scratch: ffmpeg_next::Subtitle::new(),
      pending: None,
      time_base,
    })
  }

  /// Returns the time base associated with the source stream.
  pub fn time_base(&self) -> Timebase {
    self.time_base
  }

  /// Borrow the wrapped `ffmpeg::decoder::Subtitle`.
  pub fn inner(&self) -> &ffmpeg_next::decoder::Subtitle {
    &self.decoder
  }
}

impl SubtitleDecoder for FfmpegSubtitleStreamDecoder {
  type Adapter = Ffmpeg;
  type Buffer = FfmpegBuffer;
  type Error = SubtitleDecodeError;

  fn send_packet(
    &mut self,
    packet: &SubtitlePacket<SubtitlePacketExtra, Self::Buffer>,
  ) -> Result<(), Self::Error> {
    let av_pkt = boundary::ffmpeg_packet_from_subtitle_packet(packet);
    let got = self
      .decoder
      .decode(&av_pkt, &mut self.scratch)
      .map_err(|e| SubtitleDecodeError::Decode(Error::Ffmpeg(e)))?;
    if got {
      // SAFETY: scratch is a live AVSubtitle just filled by decode.
      let frame =
        unsafe { convert::av_subtitle_to_subtitle_frame(self.scratch.as_ptr(), self.time_base) }
          .map_err(SubtitleDecodeError::Convert)?;
      self.pending = Some(frame);
    }
    Ok(())
  }

  fn receive_frame(
    &mut self,
    dst: &mut SubtitleFrame<SubtitleFrameExtra, Self::Buffer>,
  ) -> Result<(), Self::Error> {
    match self.pending.take() {
      Some(frame) => {
        *dst = frame;
        Ok(())
      }
      None => Err(SubtitleDecodeError::NoFrameReady),
    }
  }

  fn send_eof(&mut self) -> Result<(), Self::Error> {
    // Subtitle decoders have no draining — the legacy decode() API
    // produces a frame inline with each packet. EOF is a no-op.
    Ok(())
  }

  fn flush(&mut self) -> Result<(), Self::Error> {
    self.decoder.flush();
    self.pending = None;
    Ok(())
  }
}

/// Errors from [`FfmpegSubtitleStreamDecoder`].
#[derive(thiserror::Error, Debug)]
pub enum SubtitleDecodeError {
  /// The wrapped `ffmpeg::decoder::Subtitle` reported an error.
  #[error("{0}")]
  Decode(#[from] Error),
  /// Conversion from FFmpeg's `AVSubtitle` to mediadecode's
  /// `SubtitleFrame` failed.
  #[error("subtitle conversion failed: {0}")]
  Convert(crate::convert::ConvertError),
  /// `receive_frame` was called with no buffered frame ready — caller
  /// should send another packet.
  #[error("no subtitle frame ready; send another packet first")]
  NoFrameReady,
}
