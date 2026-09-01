#![cfg_attr(not(feature = "std"), no_std)]
#![forbid(unsafe_code)]
//! `rusty_esp_video` — esp_video / esp_h264 / CameraWebServer remade in Rust: MJPEG and H.264 (rusty_h264 embedded) encode, RTP + MJPEG-over-HTTP packetizers, P4 hardware encoder wrap. Memory safe, no_std core.
//!
//! This is the facade: it re-exports the `no_std` core and exposes the
//! chip backends under [`esp`]. Depend on this crate; reach into the
//! sub-crates only when you are building a backend.
//!
//! Part of Janus (Remade With Rust). Plan: `docs/plans/rusty_esp_video.md`.

pub use rusty_esp_video_core::*;

/// Chip backends (`esp-hal` for Track B, `esp-idf` for Track A).
pub mod esp {
    pub use rusty_esp_video_esp::*;
}

/// The names a sketch or firmware wants in scope.
pub mod prelude {
    pub use rusty_esp_video_core::prelude::*;
}
