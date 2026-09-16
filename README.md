# rusty_esp_video

[![Remade With Rust](https://img.shields.io/badge/Remade%20With-Rust-000?logo=rust&logoColor=fff)](https://github.com/remade-with-rust) [![By Mata Network](https://img.shields.io/badge/by-Mata%20Network-5b2be0)](https://www.mata.network) [![crates.io](https://img.shields.io/crates/v/rusty_esp_video.svg)](https://crates.io/crates/rusty_esp_video) [![docs.rs](https://docs.rs/rusty_esp_video/badge.svg)](https://docs.rs/rusty_esp_video) [![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](https://github.com/Remade-With-Rust/rusty_esp_video/blob/main/LICENSE-MIT)

Video out of an ESP32: an MJPEG page a browser can open, RTP payloaders for
JPEG and H.264 that interoperate with FFmpeg, a transport-stream muxer, and a
rate cap that drops frames honestly rather than falling behind. Pure Rust, no C,
no FFI.

* **It interoperates, and that is tested both ways.** Our depayloader rebuilds
  what FFmpeg's own RTP JPEG sender emits — 20 frames, **nothing lost, dropped
  or reordered** — and our payloader's output is read back byte-identically,
  scan bytes, quantization tables and Huffman tables alike.
* **Fragmentation that reports gaps instead of guessing.** A 1000-byte packet
  over a 300-byte path arrives in four datagrams and reassembles byte-identical;
  a lost fragment is reported as a gap, never interpolated.
* **Zero-copy where it matters.** The passthrough packet points at the frame's
  own bytes: a JPEG the camera produced reaches the network without being
  copied.
* **H.264 in a transport stream**, from the house encoder, with the key frame
  landing exactly on the group boundary and the flag agreeing with the bytes.

## What has run on hardware

Measured on a Seeed XIAO ESP32-S3 Sense, over a Wi-Fi network **the board
hosts itself**, with the laptop counting from the wire and the board counting
its own sends at the same time.

| row | laptop | board |
|---|---|---|
| a camera page in a browser | **500 frames decoded** in 20 s of stream, 14.3 fps under a 15 fps cap | 503 served — three apart, at the decoder's cut |
| RTP over ten minutes, unicast | **20,119 packets, one lost** — 0.005% | the same sender's own count |
| the same sender, broadcast | 2.66% lost | — |
| throughput at the cap | 13.8 fps, 0.51 Mbit/s — limited by the cap's granularity against a 27.5 fps sensor, **not by the link** | — |

The five-hundred-fold difference between unicast and broadcast is the radio
standard rather than anything here: broadcast frames go out once at the lowest
rate with no acknowledgement, unicast frames are retried.

Every number, with the run that produced it:
[`docs/LEDGER.md`](https://github.com/Remade-With-Rust/rusty_esp_video/blob/main/docs/LEDGER.md).

## Using it

```rust
use rusty_esp_video::prelude::*;

// A 50 fps source capped at 10: every fifth frame is admitted, the rest are
// counted as drops rather than queued.
let mut source = EncodedSource::passthrough(Cap::fps(10));
if let Some(packet) = source.admit(&frame)? {
    // The packet borrows the frame's own bytes -- no copy.
    rtp.send(&packet)?;
}
```

## Two tracks

| track | what it is | this crate |
|---|---|---|
| **A** | `std` on ESP-IDF — the page, the RTP and PCM senders, the sockets | `rusty_esp_video-esp --features esp-idf` |
| **B** | `no_std` on `esp-hal` — the payloaders and the muxer | `rusty_esp_video-core`, default |

## Part of Janus

**Janus** rebuilds the Espressif ESP32 and Arduino application portfolio as
independent, memory-safe Rust packages — so a hardware maker can ship a device
that the [MATA](https://www.mata.network) home computer discovers, catalogs honestly, adopts
under its own identity, and pays for. Ten packages, three layers, and the
dependency direction never reverses.

| layer | packages |
|---|---|
| **0 — the vocabulary** | [`rusty_esp_core`](https://crates.io/crates/rusty_esp_core) · [`rusty_esp_dsp`](https://crates.io/crates/rusty_esp_dsp) |
| **1 — the functions** | [`rusty_esp_image`](https://crates.io/crates/rusty_esp_image) · [`rusty_esp_video`](https://crates.io/crates/rusty_esp_video) · [`rusty_esp_audio`](https://crates.io/crates/rusty_esp_audio) · [`rusty_esp_signal`](https://crates.io/crates/rusty_esp_signal) · [`rusty_esp_mid`](https://crates.io/crates/rusty_esp_mid) · [`rusty_esp_iroh`](https://crates.io/crates/rusty_esp_iroh) |
| **2 — the surfaces** | [`rusty_esp_arduino`](https://crates.io/crates/rusty_esp_arduino) — the sketch facade · `espino` — the maker's CLI (not published) |

Every package is host-verified against an external oracle and keeps a ledger
in which no number appears without the run that produced it. **Five of seven
device profiles have now run their kill tests on real silicon**, three of them
over a Wi-Fi network the board hosts itself.

Also check out the rest of [Remade With Rust](https://github.com/remade-with-rust) — including
[`rusty_alloc`](https://crates.io/crates/rusty_alloc), the pure-Rust rebuild of
mimalloc that these firmwares run on, and
[`rusty_jpeg`](https://crates.io/crates/rusty_jpeg), the JPEG engine behind the
camera path — and our sister project
[remade_ffmpeg_rs](https://github.com/Remade-With-Rust/remade_ffmpeg_rs), a ground-up Rust rebuild of FFmpeg.

## About Mata Network

[Mata Network](https://www.mata.network) builds sovereign, self-hostable infrastructure.
**Remade With Rust** is our open-source home for the permissively-licensed
building blocks that work depends on.

## License

MIT OR Apache-2.0, at your option. See [LICENSE-MIT](https://github.com/Remade-With-Rust/rusty_esp_video/blob/main/LICENSE-MIT)
and [LICENSE-APACHE](https://github.com/Remade-With-Rust/rusty_esp_video/blob/main/LICENSE-APACHE).
