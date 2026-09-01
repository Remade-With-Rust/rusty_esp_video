# rusty_esp_video — mission plan

**One sentence:** Espressif's video stack (esp_video, esp_h264, the
CameraWebServer stream, the ESP-WebRTC capture pipeline) remade in Rust — one
`VideoEncoder` seam over passthrough JPEG, `rusty_h264` and the P4 hardware
encoder, plus the packetizers (MJPEG-over-HTTP, RTP, MPEG-TS) that put those
frames on a wire a browser, `rff`, a Pi hub or the mesh can read.

Family plan: Janus `docs/plans/janus-mission.md`. Layer 1 · media. Depends on
`rusty_esp_core` and `rusty_esp_image` (`ImageSource`). Delivery over QUIC is
`rusty_esp_iroh`'s job; this crate stops at packets.

Written 2026-09-01. Status: **scaffold.**

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
```

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
| **V0** | `MediaPacket`, `Passthrough`, `Multipart`, `Mux` (TS), `Rtp` + payloaders, `Pacer`, host tests | a TS file muxed on the host from an Annex-B fixture decodes in `rff` and `ffmpeg` frame-count-identical; the multipart writer's output parses in a browser-equivalent parser fixture |
| **V1** (J1) | MJPEG over HTTP from XIAO S3 Sense, Track A | `http://device/stream` opens in a browser at 320×240 at a recorded FPS; recorded to disk on a Pi (pi-mission H3) |
| **V2** | RTP/JPEG (RFC 2435) and raw-UDP framing to a laptop; `Pacer` | a laptop receiver reassembles 10 minutes with the loss counter recorded |
| **V3** (J5) | `rusty_h264` `no_std` + Baseline I/P at QVGA; H.264 in TS over UDP | `rff -i udp://@:1234` plays it; FPS, bitrate and cycle budget written in the ledger honestly (S3 software) |
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
| 2026-09-01 | The first `rff`-native stream is H.264 in MPEG-TS over UDP, because that is what `rff` reads today. |
| 2026-09-01 | `MediaPacket` starts here and is promoted to `rusty_esp_core` when audio needs the same type. |
| 2026-09-01 | Full WebRTC is not v1. |
