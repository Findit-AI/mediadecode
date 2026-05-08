//! Decoder traits — push-style streams (FFmpeg / WebCodecs / ProRes
//! RAW via VTDecompressionSession) and pull-style frame sources
//! (R3D / BRAW / ARRIRAW / X-OCN / Canon RAW Light).

use crate::Timebase;
use crate::Timestamp;
use crate::adapter::{AudioAdapter, SubtitleAdapter, VideoAdapter};
use crate::frame::{AudioFrame, SubtitleFrame, VideoFrame};
use crate::packet::{AudioPacket, SubtitlePacket, VideoPacket};

/// Push-style video decoder. Caller submits compressed packets and
/// drains decoded frames.
///
/// Backends: FFmpeg, WebCodecs, ProRes RAW (VideoToolbox).
pub trait VideoStreamDecoder {
    /// Backend-specific vocabulary.
    type Adapter: VideoAdapter;
    /// Buffer type held by the packets and frames this decoder
    /// produces or accepts.
    type Buffer: AsRef<[u8]>;
    /// Decoder-specific error type.
    type Error;

    /// Submits one compressed packet.
    fn send_packet(
        &mut self,
        packet: &VideoPacket<Self::Adapter, Self::Buffer>,
    ) -> Result<(), Self::Error>;

    /// Drains one decoded frame into `dst`. Backends signal "no
    /// frame ready" via a backend-specific `Error` variant.
    fn receive_frame(
        &mut self,
        dst: &mut VideoFrame<Self::Adapter, Self::Buffer>,
    ) -> Result<(), Self::Error>;

    /// Signals end-of-stream.
    fn send_eof(&mut self) -> Result<(), Self::Error>;

    /// Flushes internal state.
    fn flush(&mut self) -> Result<(), Self::Error>;
}

/// Pull-style video frame source. Caller requests frames by integer
/// index. Clip-level metadata accessible via `clip_meta()`.
///
/// Backends: R3D, BRAW, ARRIRAW, Sony X-OCN, Canon Cinema RAW Light.
pub trait VideoFrameSource {
    /// Backend-specific vocabulary.
    type Adapter: VideoAdapter;
    /// Buffer type for the produced frames.
    type Buffer: AsRef<[u8]>;
    /// Backend-specific clip-level metadata bag (e.g. `R3dClipMeta`,
    /// `ArriClipMeta`). Backends without clip metadata set this to `()`.
    type ClipMeta;
    /// Decoder-specific error type.
    type Error;

    /// Total frame count in the clip.
    fn frame_count(&self) -> u64;
    /// Video frame rate (frames per second as a `Timebase`).
    fn frame_rate(&self) -> Timebase;
    /// Total clip duration.
    fn duration(&self) -> Timestamp;
    /// Backend-specific clip-level metadata.
    fn clip_meta(&self) -> &Self::ClipMeta;

    /// Decodes one frame at `index` into `dst`.
    fn decode_frame(
        &mut self,
        index: u64,
        dst: &mut VideoFrame<Self::Adapter, Self::Buffer>,
    ) -> Result<(), Self::Error>;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Timebase;
    use core::num::NonZeroU32;

    pub(crate) struct VLoop;
    impl VideoAdapter for VLoop {
        type CodecId = u32;
        type PixelFormat = u32;
        type PacketExtra = ();
        type FrameExtra = ();
    }

    /// Trivial loopback impl — confirms the trait can be implemented.
    pub(crate) struct LoopVideoStream;

    #[derive(Debug)]
    pub(crate) struct LoopError;

    impl VideoStreamDecoder for LoopVideoStream {
        type Adapter = VLoop;
        type Buffer = &'static [u8];
        type Error = LoopError;

        fn send_packet(&mut self, _: &VideoPacket<VLoop, &'static [u8]>)
            -> Result<(), LoopError> { Ok(()) }
        fn receive_frame(&mut self, _: &mut VideoFrame<VLoop, &'static [u8]>)
            -> Result<(), LoopError> { Err(LoopError) }
        fn send_eof(&mut self) -> Result<(), LoopError> { Ok(()) }
        fn flush(&mut self) -> Result<(), LoopError> { Ok(()) }
    }

    pub(crate) struct LoopVideoSource;

    impl VideoFrameSource for LoopVideoSource {
        type Adapter = VLoop;
        type Buffer = &'static [u8];
        type ClipMeta = ();
        type Error = LoopError;

        fn frame_count(&self) -> u64 { 0 }
        fn frame_rate(&self) -> Timebase {
            Timebase::new(30, NonZeroU32::new(1).unwrap())
        }
        fn duration(&self) -> Timestamp {
            Timestamp::new(0, self.frame_rate())
        }
        fn clip_meta(&self) -> &() { &() }
        fn decode_frame(&mut self, _: u64, _: &mut VideoFrame<VLoop, &'static [u8]>)
            -> Result<(), LoopError> { Err(LoopError) }
    }

    #[test]
    fn video_traits_are_implementable() {
        fn _stream<D: VideoStreamDecoder>() {}
        fn _source<D: VideoFrameSource>() {}
        _stream::<LoopVideoStream>();
        _source::<LoopVideoSource>();
    }
}
