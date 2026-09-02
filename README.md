# rusty_esp_video

[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

Espressif's video stack (`esp_video`, `esp_h264`, the CameraWebServer stream,
the ESP-WebRTC capture pipeline) remade in Rust: one `VideoEncoder` seam over
passthrough JPEG, `rusty_h264` and the ESP32-P4 hardware encoders, plus the
packetizers (MJPEG-over-HTTP, RTP, MPEG-TS) that put those frames on a wire a
browser, `ffmpeg`, `rff`, a Pi hub or the mesh can read.

Part of **Janus**, the Remade-With-Rust programme that rebuilds the Espressif
ESP32 and Arduino application portfolio in memory-safe Rust for the MATA home
computer.

- This package's plan: [docs/plans/rusty_esp_video.md](docs/plans/rusty_esp_video.md)
- Numbers: [docs/LEDGER.md](docs/LEDGER.md)
- The family plan: Janus `docs/plans/janus-mission.md` (umbrella repo)

**Claims discipline:** every number in this README is in the ledger with the
run that produced it. Nothing here has run on a chip yet.

## Status

**V0 implemented on the host (2026-09-01); gate partly run.** A 12-frame
H.264 stream from the house encoder, muxed by this crate into MPEG-TS, is read
by `ffprobe` as `h264,64,48,12` — every access unit, and round-trips
byte-identical through the crate's own demuxer into the house decoder. 18
unit tests pass. Clippy and the riscv32 bare-metal checks for V0 have **not**
run yet: the development machine's disk filled during the run. See the ledger.

Not yet: the ESP backends (sockets, the MJPEG HTTP responder, the P4 hardware
encoder) — V1 onward, needing a board — and `rusty_h264` on the chip (V3).

## What is in the core

| Module | What |
|---|---|
| `packet` | `MediaPacket` (codec, key, timestamp, borrowed bytes), `Codec` |
| `encoder` | `VideoEncoder` seam, `EncoderConfig`, zero-copy JPEG `Passthrough` |
| `sink` | `PacketSink`; slice, counting and `Vec` sinks |
| `mjpeg_http` | `Multipart`: the `multipart/x-mixed-replace` stream a browser opens, no allocation |
| `annexb` | NAL unit scanning, IDR/AUD detection, an access-unit splitter |
| `rtp` | RTP header, RFC 6184 H.264 payloader (single NAL + FU-A), RFC 2435 JPEG payloader |
| `mpegts` | `Mux`: PAT, PMT, PES with PTS, PCR, AUD insertion, stuffing — 188-byte packets streamed to a sink; a test/host-side `demux` |
| `udp` | `Framer` and `Reassembler` for raw datagrams with a 28-byte header |
| `pacer` | `Pacer`: a frame-rate cap with drop counters |

```rust
use rusty_esp_video::prelude::*;

let mut enc = Passthrough::default();               // sensor JPEG is the v1 codec
enc.configure(geometry, &EncoderConfig::default())?;
let mut stream = Multipart::new(tcp);               // any PacketSink
stream.write_response_head()?;
loop {
    let frame = camera.grab(&mut buf)?;
    if pacer.admit(frame.timestamp) {
        stream.push(&enc.encode(&frame, &mut scratch)?)?;
    }
}
```

## Layout

```text
crates/rusty_esp_video          facade
crates/rusty_esp_video-core     no_std + alloc; forbid(unsafe); the core above
crates/rusty_esp_video-esp      the WRAP crate: `esp-hal` | `esp-idf` sockets and hardware encoders (V1+)
docs/plans/rusty_esp_video.md   the plan · docs/LEDGER.md the numbers
```

## Build

```sh
cargo test --workspace                 # includes the ffprobe oracle when ffprobe is on PATH
cargo check -p rusty_esp_video-core --no-default-features --target riscv32imac-unknown-none-elf
cargo check -p rusty_esp_video-core --no-default-features --features alloc --target riscv32imac-unknown-none-elf
```

## License

MIT OR Apache-2.0, at your option.
