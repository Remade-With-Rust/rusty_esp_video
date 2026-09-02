//! The one packet type the media packages share.
//!
//! [`MediaPacket`] and [`Codec`] were made here and now live in
//! `rusty_esp_core::media` (C1), so audio, video and the mesh frame the same
//! shape without a conversion. This module re-exports them: every path that
//! said `rusty_esp_video_core::packet::MediaPacket` still does.

pub use rusty_esp_core::media::{Codec, MediaPacket};
