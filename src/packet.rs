//! Compressed `Packet` types and `PacketFlags`.
//!
//! The Packet types proper land in later tasks; this module starts
//! with `PacketFlags` so dependent types can use it.

use bitflags::bitflags;

bitflags! {
    /// Per-packet flags.
    ///
    /// Bit values are the public API:
    /// - `KEY = 0b001` — packet starts a keyframe (FFmpeg `AV_PKT_FLAG_KEY`,
    ///   WebCodecs `'key'`, ProRes RAW absence of
    ///   `kCMSampleAttachmentKey_NotSync`).
    /// - `CORRUPT = 0b010` — packet is known-corrupt (FFmpeg
    ///   `AV_PKT_FLAG_CORRUPT`).
    /// - `DISCARD = 0b100` — packet should be skipped during reconstruction
    ///   (FFmpeg `AV_PKT_FLAG_DISCARD`).
    #[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
    pub struct PacketFlags: u8 {
        /// Keyframe / sync sample.
        const KEY     = 0b001;
        /// Bitstream-level corruption known.
        const CORRUPT = 0b010;
        /// Demuxer hint: skip this packet.
        const DISCARD = 0b100;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flag_bits_are_stable() {
        assert_eq!(PacketFlags::KEY.bits(), 0b001);
        assert_eq!(PacketFlags::CORRUPT.bits(), 0b010);
        assert_eq!(PacketFlags::DISCARD.bits(), 0b100);
    }

    #[test]
    fn flags_combine() {
        let f = PacketFlags::KEY | PacketFlags::CORRUPT;
        assert!(f.contains(PacketFlags::KEY));
        assert!(f.contains(PacketFlags::CORRUPT));
        assert!(!f.contains(PacketFlags::DISCARD));
    }

    #[test]
    fn empty_default() {
        assert_eq!(PacketFlags::default(), PacketFlags::empty());
    }
}
