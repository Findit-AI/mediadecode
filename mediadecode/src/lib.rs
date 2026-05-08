//! Generic, no_std-friendly type-and-trait spine for media decoders.
//!
//! This crate provides a unified vocabulary of `Packet` / `Frame` types
//! and `Adapter` / `Decoder` traits that concrete decoder backends
//! (FFmpeg, WebCodecs, RED R3D, Blackmagic BRAW, ARRIRAW, Sony X-OCN,
//! Apple ProRes RAW, Canon Cinema RAW Light, …) implement. No decoder
//! implementation lives here; backend crates depend on this crate and
//! emit the unified types.
//!
//! See `docs/superpowers/specs/2026-05-08-mediadecode-design.md` for
//! the full design.

#![cfg_attr(not(feature = "std"), no_std)]
#![cfg_attr(docsrs, feature(doc_cfg))]
#![cfg_attr(docsrs, allow(unused_attributes))]
#![deny(missing_docs)]
#![forbid(unsafe_code)]
#![allow(clippy::type_complexity)]

// Workspace pattern (mirrors mediatime / colconv / scenesdetect) — alias
// `alloc` as `std` so `std::vec::Vec` etc. resolves in alloc-only builds.
// The `allow` is needed because `mediadecode`'s public API currently uses
// only `core::` paths, leaving the alias technically unused at this layer.
// `#[macro_use]` brings `vec!` / `format!` / `write!` etc. into scope so
// `#[cfg(test)]` modules under `--no-default-features --features alloc`
// still compile (the std prelude that normally provides them is gone).
#[cfg(all(not(feature = "std"), feature = "alloc"))]
#[allow(unused_extern_crates)]
#[macro_use]
extern crate alloc as std;

#[cfg(feature = "std")]
extern crate std;

pub mod adapter;
pub mod cfa;
pub mod channel;
pub mod color;
pub mod decoder;
pub mod frame;
pub mod packet;
pub mod pixel_format;
pub mod subtitle;

pub use pixel_format::PixelFormat;

// Re-export the time primitives so consumers don't have to add a
// separate `mediatime` dependency.
pub use mediatime::{TimeRange, Timebase, Timestamp};
