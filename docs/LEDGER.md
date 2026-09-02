# rusty_esp_video — ledger

Every number this package quotes lives here with the run that produced it.

## Correctness gates (host, 2026-09-01, V0)

| Gate | Result |
|---|---|
| MPEG-TS mux of a **12-frame H.264 stream made by `rusty_h264`** (64×48, scalar build): `ffprobe 8.1.2` reports `codec_name,width,height,nb_read_frames` = `h264,64,48,12` | **pass** — the external oracle counts every access unit muxed |
| The same stream round-trips through the test-side demuxer: PMT declares stream type `0x1B` on PID `0x100`, zero continuity-counter errors on all three PIDs, one PCR per access unit, PTS = capture time × 90 kHz, every access unit byte-identical after AUD insertion, and the house decoder decodes the demuxed units | pass |
| Access-unit splitter on a concatenated encoder output: one unit per picture, parameter sets and SEI grouped with the picture they precede, a second slice stays with its picture | pass |
| `mjpeg_http::Multipart` output parsed by a browser-shaped parser: response head, boundary, `Content-Length` and `X-Timestamp` per part, bodies byte-identical | pass |
| RTP header write/parse, sequence wrap, 90 kHz timestamp | pass |
| RFC 6184: AUD and SPS as single-NAL packets, a 3001-byte IDR as three FU-A fragments with start/end bits, NRI preserved in the FU indicator, marker on the last packet of the unit, fragments reassemble to the NAL body | pass |
| RFC 2435 on a real `rusty_jpeg` baseline image: type from SOF sampling, offsets contiguous, quantization tables in the first packet only, payload equals the scan, marker last; progressive input refused | pass |
| UDP framer + reassembler: 1000-byte packet over a 300-byte MTU in 4 datagrams, byte-identical; a lost fragment is reported as a gap, not guessed | pass |
| `Pacer` caps at the configured rate and counts drops | pass |
| CRC-32/MPEG-2 check value for `123456789` = `0x0376E6E7`; PES PTS encoding decodes back | pass |
| Zero-copy `Passthrough`: the packet points at the frame's own bytes | pass |

Unit tests: **20 pass** (the two `source` tests added with the J1 host half).
Oracle tests: **5 pass** — the TS trio above, in which `ffprobe` reports
`h264,64,48,12` and `ffmpeg -v error -i fixture.ts -f null -` exits 0 with an
empty error log, plus the two HTTP-stream oracles below. Clippy `-D warnings`
on all targets is clean for both the default and the `std` feature set;
`riscv32imac-unknown-none-elf` compiles with `--no-default-features` and with
`--features alloc`.

## J1 stream path on the host (2026-09-01)

The Track A server (`rusty_esp_video-esp::net`, plain `std::net`, the same
code ESP-IDF runs) fed by colour bars encoded to JPEG:

| Gate | Result |
|---|---|
| **ffmpeg's MJPEG-over-HTTP demuxer** (what a browser does) opens `/stream`, reads **8 frames** (`-frames:v 8 -f framecrc`), exits 0 with an empty error log; the server had pushed 9 when the client hung up | **pass** |
| A Rust client reads the response head (`multipart/x-mixed-replace; boundary=janus-frame`) and five parts; every part is exactly one JPEG (`find_eoi` = `Content-Length`) whose header probes to 160×120 | pass |
| `GET /` returns the viewer page (`text/html`, `<img src="/stream">`); `GET /nope` returns 404; the server's counters agree (3 connections, 1 stream, ≥5 frames) | pass |
| `EncodedSource`: a 50 fps source capped at 10 fps admits every fifth frame, counts drops, and the JPEG passthrough packet borrows the frame half of scratch | pass |
| **The record path (pi-mission H3, without `rff`):** `mjpeg_reader::Reader` round-trips the writer's output in one push and under 1-, 3-, 7-, 64- and 1000-byte chunking, with and without the response head; `client::pull_stream` pulls 6 frames over TCP into a file; **`ffprobe -f mjpeg -count_frames` reads that file as `mjpeg,…,6`** | **pass** |

Unit tests after the record path: **23** in `rusty_esp_video-core`; oracle tests **6** (3 TS, 3 HTTP).

## First Track A firmware build (2026-09-01)

`firmware/xiao-s3-sense-idf-mjpeg` (`IdfCamera` → `EncodedSource` →
`MjpegHttpServer` on `:80`, Wi-Fi via esp-idf-svc 0.52) builds for
`xtensa-esp32s3-espidf` with ESP-IDF **v5.5.1** and esp32-camera **2.1.7**,
`cargo build --release` on this Windows box (esp toolchain from `espup`):

| quantity | value |
|---|---:|
| app image (`espflash save-image --chip esp32s3`) | **1,073,152 B** |
| factory partition, `partitions_singleapp_large.csv` (8 MB flash) | 1,536,000 B → **69.9 %** used |
| `.flash.text` / `.flash.rodata` | 795,488 B / 155,504 B |
| `.iram0.text` | 89,375 B |
| static DRAM (`.dram0.data` + `.dram0.bss`) | 31,356 B + 19,288 B |
| rebuild after the IDF is configured (Rust crates + link) | 1 m 50 s |

Fit: yes, with ~460 KB of factory partition to spare before OTA is a
question. Not measured: RAM at run time, frames per second, the browser
kill test — all need the board. The build also fixed three
write-then-discover mistakes recorded in the mission plan §8 (project
discovery with a short target dir, the `esp_core` re-export path, `ESP_OK`
being `u32` in the bindings).

## Sizes

| Date | Quantity | Value | Method |
|---|---|---|---|
| 2026-09-01 | 12 access units of 64×48 H.264 (house encoder, scalar) muxed to TS | **14 transport packets, 2 632 bytes**: one PAT + one PMT (the first key frame), 12 PES packets each fitting one 188-byte packet with PCR and stuffing | `tests/ts_oracle.rs`, `--nocapture` |

## V3 (J5) host half - the H.264 encoder behind the seam (2026-09-02)

`encoder::H264` (feature `h264`) wraps `rusty_h264` 0.12 (default features
off: scalar, `forbid(unsafe)`, no allocator hijack) behind `VideoEncoder`
with the chip configuration: **Constrained Baseline, CAVLC**, no 8x8
transform, no B-frames, one reference, `Preset::Fast`, **no lookahead, no
scene cut**, fixed GOP. `tests/h264_oracle.rs`, QVGA 320x240, 30 frames of a
moving square on a gradient, GOP 15, quality 80 (QP 21), Windows 11, Rust
1.98.0, `--release`, single thread:

| gate | result |
|---|---|
| one access unit per `encode` call (no buffering), `flush` returns 0 bytes | pass |
| IDR exactly on the GOP (frames 0 and 15), key flag == `contains_idr` of the bytes | pass |
| `request_keyframe` -> IDR on the next frame | pass (`[T F F T F F]`) |
| wrong pixel format / odd width / small output buffer refused | pass |
| TS mux -> `demux::parse`: 30 access units, stream type 0x1B, 0 CC errors; the house decoder decodes them | pass |
| **ffprobe** `codec_name,profile,width,height,nb_read_frames` | **`h264,Constrained Baseline,320,240,30`**; `ffmpeg -f null` exits 0 with an empty error log |
| bytes | 13 239 B for 30 frames, 441 B/frame (about 53 kbit/s at 15 fps for this synthetic content) |
| **host encode time per frame** (Instant around `encode`, min / median / max over 30) | **439 / 475 / 1 814 us** - a *host* number; the S3 row is in `hardware-verify.md` |

The host baseline says what the S3 must be compared against, not what it
will do: an ESP32-S3 at 240 MHz with no SIMD is one to two orders slower
than this laptop core, so the honest expectation is single-digit FPS at QVGA
in software, which is why the P4's hardware encoder sits behind the same
trait.

### The `no_std` pass, same day

`rusty_h264` 0.12 on crates.io is a host crate (`std::thread::scope` GOP
parallelism, ~100 `std::env` knobs, `Instant` profiling, file sinks). The
upstream pass is done on branch `no-std`
([PR #7](https://github.com/Remade-With-Rust/rusty_h264/pull/7), 3 commits,
+5 314 / -1 504): `rusty_h264-common`, `rusty_h264-encoder` and the facade
are `no_std` + `alloc` with a `libm` feature for the float math; the decoder
stays `std`-only behind the facade's `std` feature. Every upstream gate ran
green here: the full common + encoder suites with `std` and on the `no_std`
code paths (`--features libm`), the workspace with default features, the
scalar arm, and `cargo check` on `riscv32imac-unknown-none-elf`.

With that branch the `h264` feature here is **`alloc` + `rusty_h264/libm`**
(no `std`), and the wrapper is unchanged:

| gate | result |
|---|---|
| `cargo check -p rusty_esp_video-core --no-default-features --features alloc,h264 --target riscv32imac-unknown-none-elf` (ESP32-C6 class) | **clean** |
| same on `riscv32imafc-unknown-none-elf` (ESP32-P4 class) | **clean** |
| the QVGA oracle on the host with the branch (host `std` + `libm` math) | same 30 access units, IDR on the GOP, `ffprobe` `h264,Constrained Baseline,320,240,30`, **13 239 bytes** — byte-identical to the platform-libm build on this host; 452 / 534 µs per frame |

So the encoder is one `cargo build` away from a chip on the software
side; the S3 number is a row in `docs/plans/hardware-verify.md`.

Two facts worth a line. `rusty_h264` selects Baseline+CAVLC through an
environment variable (`RUSTY_H264_LEGACY_CAVLC`) by default; a chip has no
environment, so the wrapper sets `profile`, `cabac` and `transform_8x8`
explicitly - they are public fields, the env var only chooses the default.
And the encoder has no keyframe request: the wrapper recreates the encoder,
whose first picture is an IDR, which costs a full state reset per request
and is the right thing for a late joiner anyway.

## V2 host half - RTP/JPEG and raw datagrams to a laptop (2026-09-02)

V0 had the sending halves (`rtp::JpegPayloader`, `udp::Framer`, `Pacer`).
V2 adds the receiving halves and the two ends over real sockets:

- `rtp::JpegDepayloader`: RFC 2435 back to a JPEG in a caller buffer, the
  scan written in place and the headers (SOI, DQT, SOF0, the four Annex K
  DHTs, DRI, SOS) synthesised in front of it; a sequence gap or an offset
  that is not the next byte drops the frame and the next frame start
  resynchronises. `rtp::huffman` is the Annex K table set and
  `JpegScan::parse` now refuses a JPEG whose DHT is not that set (RFC 2435
  carries no Huffman tables, so an optimised-table encoder would produce a
  stream no receiver can decode). `default_quant_tables` is RFC 2435
  Appendix A for `Q < 128`, in the zigzag order ffmpeg uses.
- `rusty_esp_video-esp::udp_net`: `RtpJpegSender` / `RawUdpSender` over
  `std::net::UdpSocket` (what the chip runs under ESP-IDF), and
  `receive_rtp_jpeg` / `receive_raw` with `RxStats` (packets, frames, lost,
  dropped, bad, fps between first and last frame). Examples `rtp_send` and
  `rtp_recv`.

| Gate | Result |
|---|---|
| The Annex K tables the receiver writes equal the DHT segments `rusty_jpeg` (an independent transcription) emits; every table's code-length sum equals its symbol count | pass |
| A 64x32 `rusty_jpeg` image through payloader (300-byte MTU) and depayloader: same scan bytes, same quantization tables, same DHT bytes, and the house decoder returns identical pixels; a missing fragment drops that frame and the next frame decodes | pass |
| Headers built from a `Q` factor alone (no tables in-band) are a JPEG the house decoder reads; an optimised-Huffman JPEG is refused at the sender | pass |
| **ffmpeg's RFC 2435 receiver** (`-protocol_whitelist file,rtp,udp -i janus.sdp`) reassembles what our payloader sends and decodes 10 frames of it (`framecrc`), no complaint on stderr | **pass** |
| **Our depayloader rebuilds what ffmpeg's `rtpenc_jpeg` sends** (`testsrc` 320x240 10 fps, `-c:v mjpeg -huffman default -f rtp`): 20 frames, 0 lost / dropped / bad, every frame 320x240, the first one read by the house decoder and by `ffmpeg -f null` with empty stderr | **pass** |
| Our two ends over loopback, three frames at a 700-byte MTU: same packet count both sides, 90 kHz timestamps 0 / 9000 / 18000, every frame decodes to the same pixels as the original | pass |
| Raw datagram path over loopback (1200-byte MTU, four JPEGs): byte-identical, sequence / timestamp / key / codec tag preserved, 0 lost | pass |

Unit tests: **33** in the core (9 in `rtp`), 1 in `-esp`; oracle tests 4 in
`rtp_oracle` (the two ffmpeg ones serialised, they each bind RTP ports).
Clippy `-D warnings` clean on all targets with and without `std`;
`riscv32imac-unknown-none-elf` still compiles the core without `std`.

### The drop policy under a bit-rate cap

`pacer::Budget`: a token bucket in wire bytes refilled at `kbps`, holding
`burst_ms` of headroom; a frame that does not fit is dropped whole and
counted, never queued. Both senders take one (`with_budget`), counting
wire bytes with headers.

| Gate | Result |
|---|---|
| 100 frames of 6 000 B every 100 ms (480 kbit/s) against 200 kbit/s with 500 ms burst: 38 to 44 admitted, bytes admitted ≤ 10 s × 25 000 B/s + the burst; a frame larger than the burst never sends; `kbps = 0` admits everything; `reset` refills | pass |
| The raw sender over loopback with the same cap, a concurrent receiver: 40 packets in, at least 20 dropped whole, the receiver sees exactly the admitted sequence numbers in order with every payload intact, 0 lost, 0 bad | pass |

### The ten-minute run on the Wi-Fi address

`rtp_send` and `rtp_recv` as two processes on this machine, both bound to
the Wi-Fi adapter's address (192.168.0.224, not loopback), colour bars at
320x240, `rusty_jpeg` quality 80, 10 fps, 1200-byte MTU. The honest
substitute for the board-to-laptop row until there is a board.

| | RTP/JPEG, 600 s | raw datagrams, 60 s |
|---|---|---|
| sender: frames · packets · bytes | 6 000 · 30 000 · 33 468 000 | 600 · 3 600 · 3 683 009 |
| receiver: packets · frames | 29 946 · **5 989** | 3 535 · **589** |
| lost · dropped · bad | **0 · 0 · 0** | **0 · 0 · 0** |
| receiver fps (first to last frame) | 10.00 | 10.00 |
| frame bytes, smallest .. largest | 5 899 .. 5 993 | 5 921 .. 6 015 |
| sender packets outside the receiver's window | 54 | 65 |

The receiver's window closed a second before the sender's did (it was
started first), so each run's last frames arrived after it stopped; the
sequence counter saw no gap in either run. Three sampled frames of the RTP
run probe `mjpeg,320,240`. The decode gate over every file: in a second
60 s run writing frames the same way (590
frames, 0 lost), the 590 files on disk concatenated
into one MJPEG stream decode as **590 frames** in
`ffmpeg -f mjpeg`, every file. ("written" is the count of `std::fs::write`
calls that returned `Ok`; the ten-minute run's shortfall against frames
received is this Windows host's filesystem refusing some of the ten
creates a second, not the transport.)

## Not yet measured

- Wi-Fi throughput and frames per second on a XIAO ESP32-S3 Sense (V1).
- **J5 on the chip:** `H264` on the S3 - FPS at QVGA, cycle budget, PSRAM use; the P4 hardware encoder column. The host baseline above is the comparison, not the claim (`docs/plans/hardware-verify.md`).
- `rff -i udp://` playback of the same stream: `rff` is not built on this machine yet; ffmpeg stands in as the external oracle until it is.
