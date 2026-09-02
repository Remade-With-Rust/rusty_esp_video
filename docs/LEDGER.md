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

Unit tests: **18 pass**. Oracle tests: the two pure-Rust ones pass; the
ffprobe test reached and printed its verdict (`h264,64,48,12`) and then hit a
parsing bug in the test itself, since fixed. **Not yet re-run, and clippy
and the riscv32 checks not yet run for V0: the development machine's disk
filled to zero during the run** (a 291 GB build directory of another project
plus a process writing continuously). They run next session; the code is
unchanged by them.

## Sizes

| Date | Quantity | Value | Method |
|---|---|---|---|
| 2026-09-01 | 12 access units of 64×48 H.264 muxed to TS | 188-byte packets, one PAT+PMT pair at start (first key frame), PCR on every access unit | `tests/ts_oracle.rs`; exact packet count is printed by the ffprobe test and lands here after the re-run |

## Not yet measured

- Wi-Fi throughput and frames per second on a XIAO ESP32-S3 Sense (V1).
- `rff -i udp://` playback of the same stream: `rff` is not built on this machine yet; ffmpeg stands in as the external oracle until it is.
