#![cfg_attr(not(feature = "std"), no_std)]
#![deny(unsafe_code)]
//! `rusty_esp_video-esp` — chip backends for `rusty_esp_video`.
//!
//! This is the **wrap** crate of the package. Its first backend is the Track A
//! stream server in [`net`]: plain `std::net` sockets and a minimal HTTP/1.1
//! responder that serves the MJPEG multipart stream. ESP-IDF's `std` provides
//! exactly these sockets on the chip, so the server that runs on a laptop
//! today is the server the XIAO ESP32-S3 Sense runs in J1 — no port.
//!
//! `unsafe` is denied crate-wide; a backend that must use it at a DMA or FFI
//! boundary opts in per block with `#[allow(unsafe_code)]` and a `// SAFETY:`
//! comment stating the invariant.

#[cfg(feature = "alloc")]
extern crate alloc;

#[cfg(all(feature = "esp-hal", feature = "esp-idf"))]
compile_error!("enable exactly one track: `esp-hal` (no_std) or `esp-idf` (std)");

pub use rusty_esp_video_core as core;

#[cfg(feature = "std")]
pub mod net;

/// Which track this build of the backend crate was compiled for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Track {
    /// No chip backend compiled in: host build.
    Host,
    /// Track B — bare metal, esp-hal + Embassy.
    EspHal,
    /// Track A — std on ESP-IDF.
    EspIdf,
}

/// The track this crate was built with.
pub const TRACK: Track = if cfg!(feature = "esp-hal") {
    Track::EspHal
} else if cfg!(feature = "esp-idf") {
    Track::EspIdf
} else {
    Track::Host
};

#[cfg(feature = "esp-hal")]
pub mod hal {
    //! Track B backends. embassy-net sockets land here with their esp-hal pin.
}
