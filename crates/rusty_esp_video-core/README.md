# rusty_esp_video-core

[![Remade With Rust](https://img.shields.io/badge/Remade%20With-Rust-000?logo=rust&logoColor=fff)](https://github.com/remade-with-rust) [![By Mata Network](https://img.shields.io/badge/by-Mata%20Network-5b2be0)](https://www.mata.network) [![crates.io](https://img.shields.io/crates/v/rusty_esp_video-core.svg)](https://crates.io/crates/rusty_esp_video-core) [![docs.rs](https://docs.rs/rusty_esp_video-core/badge.svg)](https://docs.rs/rusty_esp_video-core) [![License: MIT OR Apache-2.0](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue)](https://github.com/Remade-With-Rust/rusty_esp_video/blob/main/LICENSE-MIT)

The pure half of the video package: the encoder seam with a zero-copy JPEG passthrough, MJPEG over HTTP, the RTP payloaders for H.264 and JPEG, a transport-stream muxer, and a frame-rate cap that drops honestly. `no_std`, `forbid(unsafe)`.

The payloaders are tested **against FFmpeg in both directions** — ours rebuilds what theirs sends, and theirs reads what ours emits, byte for byte.

## Where the evidence is

This crate is part of [`rusty_esp_video`](https://crates.io/crates/rusty_esp_video). The
hardware results, the method lines and the open defects live in that package's
[README](https://github.com/Remade-With-Rust/rusty_esp_video#readme) and in
[`docs/LEDGER.md`](https://github.com/Remade-With-Rust/rusty_esp_video/blob/main/docs/LEDGER.md), where no number
appears without the run that produced it.

## On an ESP32-S3

Turn on `pie-s3` and the Annex-B start-code scan runs the chip's 128-bit
vector twin. It is the largest single win in the family:

| | picoseconds per byte | |
|---|---:|---:|
| the scan, scalar | 119,820 | |
| the scan, vector | 8,089 | **−93.2%** |
| `nal_units` / `nal_spans`, scalar | 138,622 | |
| `nal_units` / `nal_spans`, vector | 18,861 | **−86.4%** |

```toml
rusty_esp_video-core = { version = "0.1", features = ["pie-s3"] }
```

A start code must BEGIN with a zero byte, so a sixteen-byte block with no
zero in it cannot contain one and is skipped whole — and on a real H.264
stream almost no block holds a zero. Only the blocks that do are walked byte
by byte, two bytes past the end so a code straddling a block edge is still
found.

The scan is gated byte-identical against `annexb::scan3`, which stays in the
tree as the oracle and is the arm every other target takes. The iterator is
measured too, not just the kernel: `nal_spans` costs 18,861 against the
twin's own 16,032, so what the wiring adds is the iterator's bookkeeping
rather than a scan that quietly fell back to scalar.

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
