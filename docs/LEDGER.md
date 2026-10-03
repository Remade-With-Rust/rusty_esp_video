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

## The no-panic gate (host, 2026-09-02)

Every parser that takes bytes from a wire, a store or a bus must return an
error on bad input, never panic — the house rule made a test:
`tests/no_panic.rs` feeds each one random inputs from an LCG (the same corpus
on every machine) and mutations of a valid encoding (bit flips, overwrites,
truncation, extension, insertion, removal), under `catch_unwind` so a failure
names the parser and prints the input.

| covered | result |
|---|---|
| `rtp::Header::parse`, `JpegDepayloader::push`, `JpegScan::parse` (20 000 packets), `udp::Header::parse` + `Reassembler::push` (30 000 datagrams), the Annex-B iterators and `mpegts::demux::parse` (5 000 streams biased toward start codes and sync bytes) | **one finding, fixed:** `rtp::Header::parse` read the extension-header length before checking the packet reached it (index out of bounds on a short packet with the X bit set); the read is bounds-checked now |

## rusty_h264 0.14 and rusty_jpeg 0.4 from crates.io; rff as the second oracle (host, 2026-09-03)

The upstream work Janus asked for landed and was released (`rusty_h264`
0.13/0.14: `EncoderConfig::baseline`, borrowed `YuvPlanes`, `encode_into`,
`request_keyframe`, `no_std` + `alloc`; `rusty_jpeg` 0.4: `no_std` +
`alloc`, `SliceWriter`, packed YUYV input; rff: `rtp://` and `mpjpeg`
inputs). Every git pin became a crates.io version and the wrapper moved to
the chip API: the frame is borrowed, the access unit is written in place
into the packetizer's buffer, a key frame is requested rather than a fresh
encoder. `jpeg::SoftJpeg` (feature `jpeg`) is new: a raw frame into a JPEG
packet through the image package's `jpeg::encode`.

| gate | result |
|---|---|
| `cargo test --workspace` with `h264`, `jpeg` and the `-esp` `std` | **58 pass** — `h264_oracle` 4 (the QVGA stream reads in `ffprobe` as before, house decoder round-trip, IDR on the GOP), `ts_oracle` 3, `no_panic` 3, unit 39 (3 new: `SoftJpeg` colour bars → a baseline JPEG `JpegScan` accepts, YUYV coded as delivered, planar refused), `-esp` RTP and MJPEG-over-HTTP oracles |
| `cargo clippy --workspace --all-targets` with the same features, `-D warnings` | clean |
| `video-core --no-default-features --features alloc,h264` and `alloc,jpeg` | **riscv32imac, riscv32imafc and `xtensa-esp32s3-none-elf`** (esp toolchain, `build-std=core,alloc`): all six pass — the first time the H.264 and JPEG encoders compile for the S3's own bare-metal target |
| `cargo deny check` | clean; the `rusty_h264` git allow-list row is gone |

**rff (remade_ffmpeg_rs `e2c71cc`, built here) receiving the Janus streams**,
each counted by `ffprobe -count_frames` on what rff wrote with `-c:v copy`:

| stream | sender | rff | ffprobe |
|---|---|---|---|
| MJPEG over HTTP (`mpjpeg`) | `rusty_esp_arduino` sketch at `http://127.0.0.1:18080/stream`, colour bars 320×240 at 15 fps | `-i http://… -f mjpeg`, 12 s | `mjpeg, 320×240, 181 frames` |
| RTP/JPEG (RFC 2435) | the sketch's `stream::rtp_to`, ~6 s | `-i rtp://0.0.0.0:5004?pt=26&timeout=3 -f mjpeg` | `mjpeg, 320×240, 98 frames`, 98 packets written |
| RTP/H.264 (RFC 6184) | `rtp_send --h264`: the moving planar pattern → `encoder::H264` (0.14 chip API) → `H264Payloader`, 15 fps for 4 s: **60 frames, 4 IDRs, 68 datagrams, 17,595 B** | `-i rtp://0.0.0.0:5004?pt=96&timeout=3 -f mpegts` | **`h264, Constrained Baseline, 320×240, 60 frames`**; `ffmpeg -i out.ts -f null -` with an empty error log |

Finding, reported upstream ([remade_ffmpeg_rs#12](https://github.com/Remade-With-Rust/remade_ffmpeg_rs/issues/12)): at `e2c71cc` rff's documented `rtp://@:port` spelling
received nothing on Windows while `rtp://0.0.0.0:port` and
`rtp://127.0.0.1:port` received every packet, so the rows above use that
spelling. The cause the report gave (an IPv6-only bind) was wrong: nothing
bound `[::]`. The empty host reached `UdpSocket::bind` as `":5004"`, which
Windows resolves to the machine's own interface addresses, so the socket
bound the LAN address and never saw loopback (on Linux that bind fails
outright). Fixed upstream the same day in `de24a83` (PR #13): `udp_bind`
spells the wildcard `0.0.0.0` itself, and RTCP packet types 200..=204 are no
longer taken for RTP payload types 72..=76. The issue is closed.

**Re-run on the fixed rff (`ddc3355`, built here) with the documented
spelling, the same senders, counted the same way:**

| stream | sender | rff | ffprobe |
|---|---|---|---|
| RTP/JPEG (RFC 2435) | `rtp_send` at 15 fps for 4 s: 60 frames, 300 datagrams, 334,438 B | `-i "rtp://@:5004?pt=26&timeout=3" -f mjpeg` | `mjpeg, 320×240, 60 frames`, 60 packets written |
| RTP/JPEG (RFC 2435) | the sketch's `stream::rtp_to`, ~5 s | same | `mjpeg, 320×240, 82 frames`, 82 packets written |
| RTP/H.264 (RFC 6184) | `rtp_send --h264` at 15 fps for 4 s: 60 frames, 4 IDRs, 68 datagrams, 17,595 B | `-i "rtp://@:5004?pt=96&timeout=3" -f mpegts` | **`h264, Constrained Baseline, 320×240, 60 frames`**; `ffmpeg -i out.ts -f null -` with an empty error log |

Either spelling works from `de24a83` on; the example's doc comment names the
documented one again.

## V1 on the XIAO ESP32-S3: fps and throughput over the board's own access point (2026-09-11)

The first board row taken with the laptop on a network the board hosts, so
no router was in the line. `firmware/xiao-s3-sense-idf-mjpeg` (Track A,
ESP-IDF v5.5.1, esp-idf-svc 0.52) hosting `janus-cam`, WPA2, channel 1; the
laptop's Killer BE200 joined at 802.11n/40 MHz at 98 % signal and took DHCP
lease 192.168.71.2 from the board. Run unattended by `tools/v1-offline.ps1`
because joining the board costs the laptop its internet; the procedure is
`docs/plans/offline-runs.md`.

Method line: `board=xiao-esp32s3-sense radio=softap-wpa2-ch1 client=802.11n-98%
geometry=320x240 format=mjpeg fps_cap=15 oracle=ffmpeg-8.1.2-decode+ffprobe
metric=decoded-frames/stopwatch arms=2 stream_secs=60 self_metric=board-serial
rtt=2/3/7ms`.

| arm | ffmpeg frames | wall s | **fps** | board sent | board produced | board dropped at cap |
|---|---:|---:|---:|---:|---:|---:|
| 1 | 1,500 | 108.597 | **13.813** | 1,502 | 1,503 | 1,478 |
| 2 | 1,500 | 108.422 | **13.835** | 1,502 | 1,503 | 1,485 |

**The two arms agree to 0.16 %**, and the oracle and the self-metric agree to
two frames: the board sent 1,502 per arm, ffmpeg counted 1,500 because
`-t 60` is 60 s of *stream* time at MJPEG's nominal 25 fps and cut the last
two in flight. That is also why a "60-second" arm took 108 s of wall clock;
the fps above divides by the stopwatch, not by 60.

| throughput, own pass | |
|---|---:|
| bytes over the link | 6,915,677 |
| seconds | 108.497 |
| **KiB/s** | **62.2** |
| Mbit/s | 0.510 |
| bytes per frame, derived | ~4,610 |

### The finding: cap-limited, not link-limited

The board's counters give the whole pipeline. Per arm it produced 1,503 and
discarded 1,478 at the frame-rate cap, so the camera grabbed 2,981 frames in
108.4 s — **27.5 fps, matching I1's 27.8 fps off this sensor**. `FPS_CAP = 15`
against a 27.6 fps source is an integer frame-skip, and one-in-two gives
13.8, not 15. Wi-Fi then delivered 1,502 of the 1,503 offered. So the
delivered rate is set by the cap's granularity against the sensor rate, and
the link — 3 ms RTT, 0.51 Mbit/s used of an 802.11n channel at 98 % — had
an order of magnitude to spare. Raising the cap to the sensor rate, or making
the cap a ratio rather than a skip, is where the next fps comes from; the
radio is not.

### What this row does not close

The `rff` half of V1 stays open: there is no `rff` binary built on this
machine. ffprobe read the bitstream as `mjpeg, 320x240, yuvj422p`; that is
the external oracle this row rests on.

### What the runner had to learn first

Three trips. The first reported a join that never happened (a PowerShell
function's log line rode along in its return value, so a timed-out wait came
back truthy, and Windows had quietly fallen back to the home network). The
second joined for real — the antenna had not been on the board — and then
PowerShell 5.1 threw on ffmpeg's opening banner via `2>&1` and killed the
decode one frame in. Both are in `offline-runs.md` with their fixes, and both
were proven to fail on purpose before being called fixed.

## V2 on the XIAO: RTP/JPEG loss over the board's own access point (2026-09-11)

The porch-cam sketch's `stream::rtp_to` (the same RFC 2435 sender as
`rtp_send`, `TxStats` from `udp_net`) to the AP's **broadcast** address for
600 s of board time; the laptop counting from the packets themselves — a raw
UDP reader parsing the 12-byte RTP header for packets, marker-bit frames and
sequence-number gaps — with ffmpeg as a second oracle. Method line:
`sender=rtp_to(broadcast:5004) client=802.11n-95% listen=600s
metric=rtp-header-count loss=sequence-gaps second_oracle=ffmpeg-rtp
self_metric=board-TxStats window=546s-associated`.

| | laptop | board |
|---|---:|---:|
| packets/s | 35.04 | 36.02 |
| frames/s | 11.64 | 12.00 |
| packets per frame | 3.01 | 3.00 |
| dropped at the sender | — | **0** |
| **packet loss** | **2.66 % by sequence** (522 of 19,655) | **2.7 % by rate** |

Two independent methods agree: the sequence gaps say 2.66 %, and the
laptop's packet rate against the board's over the **546 s the laptop was
actually associated** says 2.7 %. (The runner's own rate-based figure read
11.5 % because it divided by the nominal 600 s; the laptop left the AP at
15:25:14 — the 600-s group-key rekey, espino ledger — and the sequence
method is immune to that.)

**Caveat that belongs in the number:** the sender is broadcast, and 802.11
delivers broadcast frames at the lowest basic rate with no acknowledgement
and no retry. 2.7 % is the broadcast figure at 95 % signal in a house; a
unicast run to the client's address, which the sketch can be built for by
setting `JANUS_RTP_DEST` to it, is the row's stricter form and is not taken
yet. ffmpeg's bare `rtp://` input did not open on the stream (0 frames in
15 s), consistent with the loopback self-test before the header counter was
written; an SDP-driven decode is the second oracle to add.

## M4's stream count, and a 60-second V2 point (2026-09-11)

**M4 — `HttpStats` against the laptop's count.** The sketch's `/stream` over
the board's own AP, token-gated: the board's `HttpStats { streams: 1,
frames: 503 }` for the connection against ffmpeg's **500 decoded** with
`-t 20` (stream time at a nominal 25 fps; 42 s of wall at the board's
12.5). Three frames apart, all three at the cut. Self-metric and oracle
agree.

**V2 at 60 s, same method as the 600-s row:** 2,171 packets, 708 frames,
**1.81 % lost by sequence, 1.17 % by rate** (36.18 vs the board's 36.61
packets/s), sender dropped 0, broadcast at 95 %. And the second oracle now
exists: ffmpeg's bare `rtp://` input opened on the live stream and decoded
170 frames in 15 s — 11.3 fps against the board's 12.0 — so the header
counter and a real decoder agree on the same packets. The full-ten-minute
unicast row is still the stricter form and still open.

## V2, strict form: RTP/JPEG unicast for ten minutes over the board's own AP (2026-09-11)

Same sender, same counter, same client as the broadcast row above; the
destination the laptop's lease (`192.168.71.2:5004`) instead of the
broadcast address, for 600 s. Method line: `sender=rtp_to(unicast:5004)
client=802.11n-95% listen=600s metric=rtp-header-count loss=sequence-gaps
self_metric=board-TxStats window=~550s-associated`.

| | laptop | board |
|---|---:|---:|
| packets | 20,119 | at 36.0/s |
| frames (marker bit) | 6,603 | at 12.0/s |
| **lost by sequence** | **1 of 20,120 — 0.005 %** | dropped 0 |

**Broadcast 2.66 %, unicast 0.005 %.** That is the whole difference between
the two rows and it is 802.11's, not ours: unicast frames are acknowledged
and retried, broadcast frames are sent once at the lowest basic rate. The
one lost packet in ten minutes is the unicast figure at 95 % signal in a
house.

The window is honest about its edge. The board's serial puts the rekey at
592 s of its uptime and the Killer driver's drop at 608 s; the runner
re-associated at 664 s — after this listen ended at 658 s — so the listen's
last ~50 s were dark. No later packet existed to expose that as a gap, which
is why the sequence method reads 1 lost while the rate method reads a 3.9 %
deficit: the deficit is the dark tail, and the single packet is the loss.
Both are reported; the loss figure is over the ~550 s associated.

## X0 of the killing-C plan: the C census — 2026-09-30

`python tools/c-census.py build && python tools/c-census.py report --ledger` from the umbrella, so sibling crates are the checkouts beside this one: each firmware is linked `--release` with a linker map and `--emit-relocs`, and the two are read together. Every input section the linker kept is charged to the archive the map names for it, one owner per address; every FUNC and OBJECT symbol in the ELF to the archive whose section holds its address; and a mask-ROM routine counts when a kept relocation names it (a linker script defines every ROM symbol whether or not anything calls it). `image B` is code + data as flashed; bss is RAM only. `tools/c-census.py verify` is the gate: the bytes charged equal the bytes the ELF loads, and every symbol charged to a C archive is one `llvm-nm` finds defined in that archive; on an ESP-IDF build the image bytes of every archive also equal what Espressif's own `esp_idf_size` reports from the same map. Two limits: a string table the linker merged is shared by everything that contributed to it, so it is charged where the map puts it (GNU ld) or to the linker row (lld, which names no contributor); and with LTO the Rust side is one object, so its crates are not told apart. Where a firmware reads its network at compile time the build is given placeholders for all of it (`census` / `census-pass`, stream destinations in 192.0.2.0/24): a firmware given no destination compiles its networking out, and the census would measure an image nobody ships.

**What it says.** 69.2% of the camera page's image is C:
452,689 B of ESP-IDF built from source, 229,737 B of radio blob,
62,369 B of the toolchain's C runtime. Leaving ESP-IDF removes 515,058 B and 5,120 symbols; the blob
(1,305 symbols) is what a Track B twin would still carry, in whatever
quantity `esp-radio` links on the S3. The rows of the killing-C plan, sized:
lwIP 82,599 B (X2), `esp32-camera` 60,236 B
and 578 symbols (X5), mbedTLS's crypto library
56,745 B — no TLS or X.509 code is kept, and it is absent from the one
image that starts no radio — `wpa_supplicant` 54,770 B, FreeRTOS 19,265 B,
NVS 13,058 B (X4).

### `xiao-s3-sense-idf-mjpeg` — S3, Track A, `main@86e8be9`

| origin | objects | symbols | code B | data B | bss B |
|---|---:|---:|---:|---:|---:|
| Rust | 1 | 743 | 218,841 | 103,181 | 118 |
| ESP-IDF, built from C source | 43 | 4,922 | 395,495 | 57,194 | 9,753 |
| precompiled Espressif archives (the blob) | 5 | 1,305 | 207,836 | 21,901 | 8,919 |
| toolchain C runtime (libc, libgcc) | 2 | 198 | 57,653 | 4,716 | 337 |
| linker (merged constants, padding, reservations) | 1 | 0 | 8,690 | 444 | 74,646 |

**C in this image: 6,425 symbols, 744,795 B of 1,075,951 B (69.2%). The blob floor is 1,305 symbols in 5 archives.** The 2nd-stage bootloader that starts it is espflash 4.6.0's bundled `esp32s3-bootloader.bin`: 21,072 B of C outside this image.

Mask-ROM routines called: 267 — 267 from C, 8 from Rust: `__divsf3`, `__floatundisf`, `__udivdi3`, `memcmp`, `memcpy`, `memmove`, `memset`, `strlen`.

| C archive | origin | symbols | image B | bss B |
|---|---|---:|---:|---:|
| `libnet80211.a` | blob | 633 | 133,339 | 7,590 |
| `lwip` | idf | 781 | 82,599 | 3,721 |
| `libpp.a` | blob | 487 | 62,626 | 1,234 |
| `libc.a` | toolchain | 143 | 60,881 | 320 |
| `espressif__esp32-camera` | idf | 578 | 60,236 | 2,161 |
| `mbedtls` | idf | 608 | 56,745 | 252 |
| `wpa_supplicant` | idf | 457 | 54,770 | 1,330 |
| `libphy.a` | blob | 179 | 33,486 | 86 |
| `esp_hw_support` | idf | 305 | 28,666 | 247 |
| `hal` | idf | 172 | 19,350 | 4 |
| `freertos` | idf | 210 | 19,265 | 757 |
| `spi_flash` | idf | 225 | 14,341 | 24 |
| `esp_system` | idf | 195 | 13,920 | 309 |
| `nvs_flash` | idf | 151 | 13,058 | 24 |
| `esp_driver_i2c` | idf | 51 | 10,829 | 28 |
| `libcore.a` | blob | 5 | 283 | 9 |
| `libespnow.a` | blob | 1 | 3 | 0 |
| … 33 smaller | | 1,244 | 80,398 | 913 |

## X2 of the killing-C plan: the page's HTTP with no socket in it, and the page on Track B — built, not yet joined (2026-09-30)

### The protocol, out of the transport

`rusty_esp_video-core::http` (5 host tests): the request line and the gate
(`parse_request`, `head_complete`), the page (`INDEX_HTML`, `write_index`
with the token cookie when gated) and every refusal (`write_status`: 400,
403, 404, 405, and a 503 for a chip with no camera to stream), all over
`PacketSink` and allocation-free. The `std` server in
`rusty_esp_video-esp::net` now keeps only its I/O — the listener, the read
loop, the TCP sink — and its oracle tests pass unchanged: index, stream,
404, 405, the gate's 403, the cookie the page sets. This is the video half of
the plan's transport seam; a Track B firmware formats a response into a
`SliceSink` and sends the slice over its async socket.

### The page firmware

`firmware/xiao-s3-sense-hal-page`: the camera page served over esp-radio's
station and embassy-net's TCP through `rusty_esp_signal-esp::hal::netstack`,
one connection at a time, with `GET /stream` an honest `503` until row X5
brings the camera driver to Track B. Census (`verify` closes, `llvm-nm`
agrees): **473,291 B image, 320,614 B of C (67.7 %) — the radio blob in 8
archives and 6 B of `crti.o`** — against 1,075,951 B and 744,795 B for the
ESP-IDF camera page (which has a camera).

### Not yet joined

Built with placeholder credentials; not run. No 2.4 GHz network was
reachable from the bench, and the passphrase is the operator's to type. The
kill test — `http://<ip>/` served from this firmware, the join time next to
Track A's — is the README's run.

## X5 of the killing-C plan: `/stream` on Track B — the kill test's second half passed on the XIAO (2026-09-30)

The page firmware with the camera in it, streaming to ffmpeg at the
sensor's own rate over a network the board hosts, with no ESP-IDF, no
esp32-camera and no lwIP in the image. The first half — the driver's
byte-identical colour bars against the C driver — is in `rusty_esp_image`'s
ledger.

### The firmware

`firmware/xiao-s3-sense-hal-page` now drives the sensor from
`rusty_esp_image_core::driver` and grabs frames from
`rusty_esp_image_esp::hal::DvpCamera` (LCD_CAM and GDMA into a 64 KB ring,
JPEG straight from the sensor, a two-slot PSRAM pool). `GET /stream` writes
the multipart head and then, per frame, `Multipart::push` into a PSRAM part
buffer and `write_all` to the socket; between frames it polls
`frame_ready()` and sleeps a millisecond, so the stack runs meanwhile. It
prints `PAGE streaming …` every hundred frames and, when the viewer leaves,
`PAGE stream frames=… fps_milli=… restarts=… timeouts=…`.

And it hosts: `JANUS_AP_PASS` (with `JANUS_AP_SSID`, default `janus-cam`)
compiles in the access point instead of the station — WPA2, 192.168.71.1,
leases from `.50`, the DHCP server from `rusty_esp_signal-esp::hal::netstack`
(its ledger has that half) — the arrangement V1 was measured in, so the two
rows are measured the same way. A build with neither set fails at compile
time and says so.

### The measurement

`tools/x5-stream-offline.ps1` (umbrella), the V1 runner's shape: the laptop
has one radio, so the run saves its network, adds a profile with the
passphrase from V1's gitignored file (deleted afterwards, never printed),
joins `janus-cam`, gates on port 80, pings ten times, fetches the page,
probes and decodes `/stream` for 60 s of stream time, copies 10 s of it for
the byte count, and comes back, while espino's monitor keeps the board's
serial. `x5-results.txt` / `x5-results.json` beside the firmware.

Method line: `board=xiao-esp32s3-sense radio=softap-wpa2-ch1 client=802.11n-95%
geometry=320x240 format=mjpeg fps_cap=none oracle=ffmpeg-8.1.2-decode+ffprobe
metric=decoded-frames/stopwatch arms=1 stream_secs=60 self_metric=board-serial
rtt=1/9.1/60ms`.

| | this firmware, Track B | V1, the ESP-IDF twin (2026-09-11) |
|---|---:|---:|
| ffmpeg decoded frames | 1,500 in 55.5 s | 1,500 in 108.6 s |
| **fps** | **27.04** | 13.81 (a 15 fps cap against a 27.5 fps sensor) |
| the board's own count | 1,504 sent at 27.60 fps | 1,502 sent, 1,478 dropped at the cap |
| bytes over the link, 10 s | 1,107,291 (108.1 KiB/s, 0.89 Mbit/s) | 62.2 KiB/s |
| ping RTT min / mean / max | 1 / 9.1 / 60 ms | 2 / 3 / 7 ms |
| link | 802.11n ch 1, 95 %, 150 / 135 Mbps | 802.11n ch 1, 98 % |
| lease | 192.168.71.50 from the board | 192.168.71.2 from the board |

**The stream runs at the sensor's rate.** ffmpeg's 27.04 fps against the
board's 27.60 is the stopwatch's share of the join and the first frame; the
sensor makes 27.8 (I1, and X5's first half). Nothing in the path caps or
stalls: the ring engine grabs, the stack sends, and the one `restart` in
54 s was the ring filling behind a slow send — the counter the engine keeps
for exactly that. V1's finding was that its rate was cap-limited, not
link-limited, and that the next fps would come from the cap; this firmware
has no cap and delivers twice V1's frames per second at 1.7 × its bytes per
second, on a link with an order of magnitude left. The plan's row named
V1's number as 9.2–10; the V1 ledger row is 13.8, and that is the bar this
clears.

X2's page half is met by the same run — `GET /` answered 200 from this
firmware before the stream — with the board hosting the network rather
than joining one; `join_ms=949` is the access point up, not a join.

### The census, re-run on this firmware

### `xiao-s3-sense-hal-page` — S3, Track B, `main@86e8be9`

| origin | objects | symbols | code B | data B | bss B |
|---|---:|---:|---:|---:|---:|
| Rust | 2 | 910 | 150,227 | 50,753 | 213,424 |
| precompiled Espressif archives (the blob) | 8 | 1,710 | 276,044 | 44,540 | 10,920 |
| toolchain C runtime (libc, libgcc) | 1 | 2 | 6 | 0 | 0 |
| linker (merged constants, padding, reservations) | 1 | 0 | 3,998 | 211 | 101,040 |

**C in this image: 1,712 symbols, 320,590 B of 525,779 B (61.0%). The blob floor is 1,710 symbols in 8 archives.** The 2nd-stage bootloader that starts it is espflash 4.6.0's bundled `esp32s3-bootloader.bin`: 21,072 B of C outside this image.

Mask-ROM routines called: 196 — 173 from C, 29 from Rust: `Cache_Invalidate_Addr`, `Cache_Resume_DCache`, `Cache_Resume_DCache_Autoload`, `Cache_Suspend_DCache`, `Cache_Suspend_DCache_Autoload`, `__divdi3`, `cache_dbus_mmu_set`, `esp_rom_efuse_get_flash_gpio_info`, `esp_rom_efuse_get_flash_wp_gpio`, `esp_rom_opiflash_exec_cmd`, `esp_rom_opiflash_pin_config`, `esp_rom_regi2c_read`, `esp_rom_spi_cmd_config`, `esp_rom_spi_cmd_start`, `esp_rom_spi_set_dtr_swap_mode`, `esp_rom_spi_set_op_mode`, `esp_rom_spiflash_select_qio_pins`, `ets_delay_us`, `ets_update_cpu_frequency`, `intr_matrix_set`, `memcmp`, `memcpy`, `memmove`, `memset`, `rom_Cache_WriteBack_Addr`, `rom_config_data_cache_mode`, `rom_config_instruction_cache_mode`, `rom_i2c_writeReg`, `rtc_get_reset_reason`.

| C archive | origin | symbols | image B | bss B |
|---|---|---:|---:|---:|
| `libnet80211.a` | blob | 687 | 169,861 | 8,027 |
| `libpp.a` | blob | 509 | 66,954 | 1,364 |
| `libwpa_supplicant.a` | blob | 301 | 42,754 | 1,483 |
| `libphy.a` | blob | 179 | 32,840 | 46 |
| `libprintf.a` | blob | 17 | 4,782 | 0 |
| `libbtbb.a` | blob | 14 | 2,638 | 0 |
| `libregulatory.a` | blob | 2 | 752 | 0 |
| `crti.o` | toolchain | 2 | 6 | 0 |
| `libespnow.a` | blob | 1 | 3 | 0 |

The ESP-IDF camera page is 1,075,951 B with 744,795 B of C (69.2 %) in
6,425 symbols; this one, with the same camera and the same page, is
525,779 B with 320,590 B of C in 1,712 symbols — the radio blob's 1,710 and
`crti.o`'s 2. `esp32-camera` (60,284 B / 578), lwIP, `esp_netif`, NVS,
FreeRTOS and the rest of ESP-IDF are gone from the table. The census
builds with the station placeholders (`census` / `census-pass`), so the
access-point branch and the DHCP server are compiled out of the measured
image; the hosted build the stream was measured on is 946,860 B on disk as
an ELF and carries them.

### X7 addendum: `PUT /update` in the page protocol (2026-09-30, night)

`http` learnt one more request: `PUT /update`, `Request::Put` with the
body's announced length, gated like a `GET` (the device's own token on the
query string or the cookie); `body_offset` finds where a body begins in a
head buffer that already holds some of it, and `write_plain` answers in
one line (`committed <sha256>`, or the refusal's name). Host-tested with
the rest. The page firmware under X5 does not take it; the generated Track
B cell with a maker does, and that is where the update's bytes are read,
verified and written (iroh and espino ledgers).

## X11 note: the station page firmware compiles again (2026-10-01)

`firmware/xiao-s3-sense-hal-page` stopped compiling when X7 gave
`http::Request` a `Put` variant (the generated cells' `/update`): its
request `match` did not cover it. The page takes no update, so a `PUT` is
answered `405` like any other method it does not serve. Found by X11's
census build; on the XIAO it boots to `PAGE sensor=` with the census's
placeholder network and no C of ours in the image.

## The optimization campaign after X11: the request head 2.6× cheaper (2026-10-01)

- W7: the blank line is found by its `\n` and the three bytes before it;
  the window search it replaced (`windows(4)` against `\r\n\r\n`) called
  `memcmp` once per byte position, about 1,350 times a request. Oracle:
  the window search on 3,200 random heads.
- W8: `http::head_complete_from(head, seen)` searches only what a read
  added (and the three bytes before it); the station page firmware's loop
  and espino's Track B template use it.
- W9: a header is split and trimmed only when its first byte could begin
  the name after trimming (the name's first letter in either case, or the
  first byte of a Unicode `White_Space` character). Oracle: the full scan
  on 4,000 heads with names padded by every kind of space.
- W11: newlines are found eight bytes at a time (`b ^ 0x0A` is zero
  exactly for `\n`).
- Refuted: an ASCII split of the request line (1.4 % of a request).

On the XIAO's probe at 80 MHz, a 400-byte GET with a cookie in three
reads, checked and parsed with the gate: 409.0 → 158.5 µs. Numbers and
method: rusty_esp_dsp's ledger.

## Round 2: the request head (2026-10-01)

- **H1**: the `Cookie` header is read only when the query string carries no
  token (`Gate::admits_with`), and an ASCII request line is split on ASCII
  whitespace (the same tokens; a Unicode space outside ASCII is never in an
  ASCII line). Pinned by a test against the old parser on 5,400 heads.
- **R12**: with `pie-s3`, newlines are found sixteen bytes a test
  (`find_byte`).
- **R13**: with `pie-s3`, the blank line is found as the one `\n\r` pair
  in the head (`find_pair`), not by stopping at every newline; fuzzed on
  the chip against the portable search, 3,000 of 3,000.

| XIAO at 80 MHz, `http_head` (three reads, then the parse with the gate) | us |
|---|---:|
| round 1's end | 158.4 |
| H1 | 105.4 |
| R12 | 92.0 |
| R13 | **70.5** |

dsp's ledger, "Round 2", has the method, every run and the refuted shapes.

## Round 3: the page token read on bytes (2026-10-01)

**B3.** `mjpeg_http::query_param` walks the query string's bytes for
`name=` at a parameter start and stops at `&`, with no `split` iterators;
`query_param_split` (the old code) is its oracle in a host test over more
than 50,000 generated query strings (Unicode, empty pieces, repeated and
look-alike names).

| XIAO, 80 MHz | before | after |
|---|---:|---:|
| `http_gate` (a request's token check) | 14.2 us | **11.0 us** |

**B11.** The page firmware's SCCB bus at 400 kHz (the camera's configure
305 -> 235 ms on the capture firmware; image's ledger).

**The Track A server answers 405 to `/update` (2026-10-02, enc-ble M7).**
killing-C's signed updates gave `http::Request` a `Put` and `Path` an
`Update`; `rusty_esp_video-esp`'s std server (`net.rs`) matched neither and no
Track A cell compiled. It serves only GETs, and a Track A cell takes its
updates elsewhere, so both are `405 Method Not Allowed`. Found building C6.
