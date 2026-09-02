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

**V0 shipped on the host (2026-09-01).** A 12-frame H.264 stream from the
house encoder, muxed by this crate into MPEG-TS, is read by `ffprobe` as
`h264,64,48,12` and decoded by `ffmpeg` without a single error, and it
round-trips byte-identical through the crate's own demuxer into the house
decoder. 18 unit tests and 3 oracle tests pass; clippy is clean; the core
compiles for riscv32 bare metal with and without `alloc`. See the ledger.

**J1 host half (2026-09-01):** the Track A stream server exists and is
proven — `rusty_esp_video-esp::net::MjpegHttpServer` over plain `std::net`
(what ESP-IDF's `std` gives the chip, so it runs on a laptop today and on the
board unchanged). ffmpeg's MJPEG demuxer reads 8 frames from it; a Rust
client checks every part is one JPEG of the right geometry. `EncodedSource`
joins any `ImageSource` to any `VideoEncoder` with a pacer. The XIAO ESP32-S3
Sense firmware project is written under `firmware/`. Try the host half:

```sh
cargo run -p rusty_esp_video-esp --features std --example mjpeg_server -- 127.0.0.1:8080
# open http://127.0.0.1:8080/  or:  ffmpeg -i http://127.0.0.1:8080/stream -frames:v 30 -f null -
```

The Pi record path exists too: `mjpeg_record` pulls a stream to disk and
`ffprobe -f mjpeg` counts its frames (in the ledger), so a Raspberry Pi needs
no `rff` to record a device.

Not yet: the board (flash, the 320×240 frame count on serial), the P4
hardware encoder, and `rusty_h264` on the chip (V3).

**J5 host half (2026-09-02):** `encoder::H264` puts the house H.264 encoder
behind the `VideoEncoder` seam in its chip configuration (Constrained
Baseline, CAVLC, no lookahead, fixed GOP); a QVGA stream muxes to TS and
`ffprobe` reads `h264,Constrained Baseline,320,240,30`. `policy::codec_for`
says which codec each job gets on each chip. Host encode time 439-475 us a
frame is the baseline the S3 number will be measured against, not a claim
about the chip. The upstream `no_std` pass is done
([rusty_h264 PR #7](https://github.com/Remade-With-Rust/rusty_h264/pull/7)),
so `h264` is `alloc` + `libm` and the crate **checks with the encoder on
`riscv32imac` and `riscv32imafc`**: the encoder is a `cargo build` from a
chip on the software side.

**V2 host half (2026-09-02):** the receiving halves of RTP/JPEG and the raw
datagram framing (`rtp::JpegDepayloader`, `udp_net` in `-esp`), with ffmpeg
as the oracle in both directions: ffmpeg reassembles and decodes what
`rtp_send` sends, and `rtp_recv` rebuilds what ffmpeg's RTP packetiser
sends. Ten minutes sender to receiver on the Wi-Fi adapter's address:
5 989 frames, 0 lost, 0 dropped. The drop policy is `pacer::Budget`: a
byte budget that drops a frame whole when it does not fit a bit-rate cap
and counts it, on both senders (`--kbps` on `rtp_send`).

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
| `source` | `PacketSource`; `EncodedSource` — an `ImageSource` joined to a `VideoEncoder`, paced, zero-copy for JPEG |

| `mjpeg_reader` | `Reader`: the receiving side of the multipart stream over a caller buffer — the Pi hub, the recorder and the bridge use it |

`rusty_esp_video-esp` (feature `std`): `net::{MjpegHttpServer, TcpSink}` — the
Track A stream server, one viewer at a time — and `client::pull_stream`, the
receiver. Examples: `mjpeg_server` (serve colour bars) and `mjpeg_record`
(record a device's stream to a `.mjpeg` file ffprobe reads).

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
crates/rusty_esp_video-esp      the WRAP crate: `net` (Track A server over std::net), `esp-idf`, `esp-hal`
firmware/xiao-s3-sense-idf-mjpeg the J1 firmware project (ESP-IDF, Xtensa): written, awaiting its first build and a board
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
