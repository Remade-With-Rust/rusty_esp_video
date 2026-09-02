# rusty_esp_video — mission plan

**One sentence:** Espressif's video stack (esp_video, esp_h264, the
CameraWebServer stream, the ESP-WebRTC capture pipeline) remade in Rust — one
`VideoEncoder` seam over passthrough JPEG, `rusty_h264` and the P4 hardware
encoder, plus the packetizers (MJPEG-over-HTTP, RTP, MPEG-TS) that put those
frames on a wire a browser, `rff`, a Pi hub or the mesh can read.

Family plan: Janus `docs/plans/janus-mission.md`. Layer 1 · media. Depends on
`rusty_esp_core` and `rusty_esp_image` (`ImageSource`). Delivery over QUIC is
`rusty_esp_iroh`'s job; this crate stops at packets.

Written 2026-09-01. Status: **V0 shipped on the host** (`docs/LEDGER.md`); V1 needs a board.

---

## 1. Espressif map

| Espressif item | Job | Class | Janus |
|---|---|---|---|
| `esp_video` (V4L2-shaped device API on P4) | capture/encode devices | REMAKE the API; WRAP the P4 devices | `VideoSource`, `VideoEncoder` traits; `-esp` P4 backend |
| `esp_h264` (S3 SIMD software encoder, P4 hardware encoder/decoder) | H.264 | REMAKE via `rusty_h264`; WRAP the P4 HW | `encoder::H264` (feature `h264`), `-esp` `P4Encoder` |
| CameraWebServer example (`multipart/x-mixed-replace` MJPEG over HTTP) | browser stream | REMAKE | `mjpeg_http::Multipart` + a tiny HTTP responder in `-esp` |
| esp-webrtc-solution `esp_capture`, `esp_rtp` | capture graph, RTP | REMAKE | `rtp::{Rtp, H264Fua, JpegRfc2435}`, `Pacer` |
| ESP-WebRTC (ICE/DTLS/SRTP) | peer A/V | P2 | not in v1; `rusty_esp_iroh` is the peer path |
| MPEG-TS mux (Espressif has none on-device) | what `rff` ingests over `udp://` today | REMAKE | `mpegts::Mux` (PAT/PMT/PES; H.264 stream type 0x1B) |

## 2. Crate surface

### `rusty_esp_video-core` (`no_std`, `forbid(unsafe)`)

```rust
pub struct MediaPacket<'a> { pub codec: Codec, pub key: bool, pub timestamp: Micros, pub data: &'a [u8] }
pub enum Codec { Jpeg, H264 /* Annex-B */, Pcm(PcmFormat) }

pub trait VideoEncoder {
    fn configure(&mut self, geometry: Geometry, cfg: &EncoderConfig) -> Result<()>;
    /// Encode `frame` INTO `out`; the packet borrows `out`.
    fn encode<'b>(&mut self, frame: &Frame, out: &'b mut [u8]) -> Result<MediaPacket<'b>>;
    fn request_keyframe(&mut self);
}
pub struct Passthrough;            // JPEG in, JPEG out: the v1 encoder
pub struct EncoderConfig { pub bitrate_kbps: u32, pub fps: u8, pub gop: u16, pub quality: u8 }

pub trait PacketSink { fn write(&mut self, bytes: &[u8]) -> Result<()>; }   // a socket, a file, a QUIC stream

pub mod mjpeg_http { pub struct Multipart<S: PacketSink> { /* boundary, headers, one part per frame */ } }
pub mod rtp        { pub struct Rtp { ssrc, seq, clock_hz }  pub struct H264Fua;  pub struct JpegRfc2435; }
pub mod mpegts     { pub struct Mux<S: PacketSink> { /* PAT/PMT every N packets, PES, PCR from Micros */ } }
pub mod udp        { pub struct RawFramer; }  // tiny header: seq, ts, total, index — the Pi-hub UDP recipe
pub struct Pacer { /* fps cap + drop policy, driven by the Clock seam */ }
pub trait PacketSource { fn next_packet<'b>(&mut self, scratch: &'b mut [u8]) -> Result<MediaPacket<'b>>; }
pub struct EncodedSource<S: ImageSource, E: VideoEncoder> { /* the planned video→image edge, paced */ }
```

`rusty_esp_video-esp` (built 2026-09-01, feature `std`): `net::{TcpSink,
MjpegHttpServer, ServeStats, Served}` — the Track A server over `std::net`;
one viewer at a time; `GET /stream`, `GET /`, 404/405/400.

Rules: encoders write into caller buffers; packetizers write into a
`PacketSink` in MTU-sized pieces without an intermediate `Vec`; everything is
testable on the host against fixtures and against `rff`.

### `rusty_esp_video-esp`

| Feature | Backend |
|---|---|
| `esp-idf` | sockets via `std::net`; the MJPEG HTTP responder over `esp-idf-svc` HTTP server; P4 `esp_h264` hardware encoder behind `VideoEncoder`; P4 hardware JPEG re-encode |
| `esp-hal` | sockets via `embassy-net`; a minimal HTTP/1.1 responder for the MJPEG endpoint |

### `rusty_esp_video` (facade)

`prelude` = core prelude + `VideoEncoder`, `Passthrough`, `MediaPacket`, `PacketSink`, the packetizers.

## 3. House crates and the host side

| Need | Use | Status / work item |
|---|---|---|
| H.264 encode on-chip | `rusty_h264` 0.12 with `default-features = false` (scalar, `forbid(unsafe)`, no build script, **and no `rusty_alloc` hijack**) | needs a `no_std` pass and a borrowed-frame input path — **the `embedded` feature rusty-ESP-arduino §5.1 assumed does not exist yet**; it is milestone V3 here and an upstream PR in `rs_h264` |
| Host playback | `rff` (`remade_ffmpeg_rs`) | `udp://` MPEG-TS input **works today**; **MJPEG-over-HTTP input and `rtp://` do not exist** — filed upstream. So: browsers and the Pi record path verify MJPEG; H.264-in-TS-over-UDP is the first `rff`-native stream |
| Decode oracle | `rff` and `ffmpeg` | every on-chip bitstream must decode in both |
| Never | `esp_h264` C on S3, openh264 | |

## 4. Milestones and kill tests

| # | Deliverable | Kill test |
|---|---|---|
| **V0** ✅ 2026-09-01 | `MediaPacket`, `Passthrough` (zero-copy), `PacketSink`, `Multipart`, Annex-B scanner + access-unit splitter, `Rtp` + RFC 6184 / RFC 2435 payloaders, `Mux` (TS) + test demuxer, UDP framer/reassembler, `Pacer`; 18 unit + 3 oracle tests | **passed:** a TS muxed from a 12-frame `rusty_h264` stream (14 packets, 2 632 bytes) reads in `ffprobe` as `h264,64,48,12`, decodes in `ffmpeg` with an empty error log, and round-trips byte-identical through the demuxer into the house decoder; the multipart writer parses in a browser-shaped fixture; clippy clean; riscv32 with and without `alloc`. `rff` playback is recorded when an `rff` binary exists on the box (`docs/LEDGER.md`). |
| **V1** (J1) ◐ host half 2026-09-01 | MJPEG over HTTP from XIAO S3 Sense, Track A. **Done on the host:** `net::MjpegHttpServer` over `std::net` (the code the board runs), `EncodedSource`, `mjpeg_reader::Reader` + `client::pull_stream` + `mjpeg_record` (the Pi record path, ffprobe-verified), the firmware project `firmware/xiao-s3-sense-idf-mjpeg` **builds** for xtensa-esp32s3-espidf (ESP-IDF v5.5.1 + esp32-camera 2.1.7; 1,073,152 B image, 70 % of the 1.5 MiB factory partition), ffmpeg reads 8 MJPEG frames from the host server (`docs/LEDGER.md`) | `http://device/stream` opens in a browser at 320×240 at a recorded FPS; recorded to disk on a Pi (pi-mission H3) — **needs the board** |
| **V2** | RTP/JPEG (RFC 2435) and raw-UDP framing to a laptop; `Pacer` | a laptop receiver reassembles 10 minutes with the loss counter recorded |
| **V3** (J5) ◐ host half 2026-09-02 | `rusty_h264` `no_std` + Baseline I/P at QVGA; H.264 in TS over UDP. **Done on the host:** `encoder::H264` behind `VideoEncoder` (feature `h264`, Constrained Baseline CAVLC, no lookahead, fixed GOP), the QVGA oracle (`ffprobe` reads `h264,Constrained Baseline,320,240,30`, house decoder round-trip, IDR on the GOP, 439/475 us per frame on the host), the codec **policy** (`policy::{Job, codec_for}`); the upstream `no_std` pass started on branch `no-std` | `rff -i udp://@:1234` plays it; FPS, bitrate and cycle budget written in the ledger honestly (S3 software) - **needs the board** |
| **V4** | P4 hardware encoder behind the same trait; hardware JPEG | side-by-side table: S3 software vs P4 hardware at QVGA/VGA/720p |
| **V5** | PIE SAD/copy spike for the software path (with `rusty_esp_dsp`) | byte-identical bitstream vs scalar; ceiling probe first |

## 5. Measurement

- Frame counters and byte counters per stage before any clock; the
  `Pacer` exposes dropped-frame counts.
- The bitstream is the gate: `rff` and `ffmpeg` must decode it; the encoder's
  own reconstruction is a standing gate once `rusty_h264` lands
  (`codec-bringup-encoder` discipline).
- Cycle budget per frame vs frame interval is the on-chip speed number; it goes
  in `docs/LEDGER.md` with the chip, clock and build flags.

## 6. Risks

| Risk | Mitigation |
|---|---|
| `rusty_h264` frame memory on 512 KB SRAM | PSRAM boards first; QCIF/QVGA I-only as the floor; the borrowed-frame path upstream |
| Wi-Fi throughput on S3 at VGA MJPEG | measure; V2's pacer and drop policy are the answer, not a bigger buffer |
| `rff` MJPEG input never lands | the browser and the Pi record path are the M1 verifiers by design |

## 7. Decision log

| Date | Decision |
|---|---|
| 2026-09-01 | Packetizers live here (codec-aware); QUIC delivery lives in `rusty_esp_iroh`. |
| 2026-09-02 | **The job picks the codec** (`policy.rs`): Preview and Analytics are JPEG passthrough; Stream and Archive are H.264 where the chip can (software on S3/ESP32/S2, hardware on P4) and JPEG where it cannot (C-series); Archive is transcoded to AV1/AV2 on the home computer. There is no AV1/AV2 on a chip - the Remade AV1/AV2 crates are host-only and the cycle budget is not there. |
| 2026-09-02 | The chip H.264 configuration is Constrained Baseline + CAVLC, no 8x8, no B, one reference, `Fast`, lookahead 0, scene cut off, fixed GOP; set explicitly because `rusty_h264` chooses Baseline through an environment variable, which a chip does not have. |
| 2026-09-02 | `h264` implies `std` until the upstream `no_std` pass lands; the wrapper is written against the public API only so it moves down the ladder unchanged. |
| 2026-09-01 | The first `rff`-native stream is H.264 in MPEG-TS over UDP, because that is what `rff` reads today. |
| 2026-09-01 | `MediaPacket` starts here and is promoted to `rusty_esp_core` when audio needs the same type. |
| 2026-09-01 | Full WebRTC is not v1. |
