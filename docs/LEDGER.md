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

## Sizes

| Date | Quantity | Value | Method |
|---|---|---|---|
| 2026-09-01 | 12 access units of 64×48 H.264 (house encoder, scalar) muxed to TS | **14 transport packets, 2 632 bytes**: one PAT + one PMT (the first key frame), 12 PES packets each fitting one 188-byte packet with PCR and stuffing | `tests/ts_oracle.rs`, `--nocapture` |

## Not yet measured

- Wi-Fi throughput and frames per second on a XIAO ESP32-S3 Sense (V1).
- `rff -i udp://` playback of the same stream: `rff` is not built on this machine yet; ffmpeg stands in as the external oracle until it is.
