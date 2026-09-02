# xiao-s3-sense-idf-mjpeg — the Janus J1 firmware

XIAO ESP32-S3 Sense, Track A (`std` on ESP-IDF): the OV2640 in JPEG mode →
`rusty_esp_image` (`IdfCamera` over esp32-camera) → `rusty_esp_video`
(`Passthrough`, `EncodedSource`, `Multipart`) → `MjpegHttpServer` on port 80.

**Status: written, not yet compiled or flashed.** The host half of this
exact pipeline runs today: `cargo run -p rusty_esp_video-esp --features std
--example mjpeg_server` serves colour bars at `http://127.0.0.1:8080/stream`,
and `tests/mjpeg_http_oracle.rs` reads it back with ffmpeg. What this project
adds is the camera and Wi-Fi on the board.

## Prerequisites (one-time, this machine)

- `espup install` (done here; the `esp` toolchain is in rustup). Source
  `C:\Users\talmo\export-esp.ps1` in a new shell before building.
- `cargo install ldproxy` — the linker shim ESP-IDF projects need.
- `cargo install espflash` (done here).
- ESP-IDF v5.5.1 is downloaded by `esp-idf-sys` on the first build into
  `.embuild/` (about 1.5 GB, plus a Python environment). Have the disk room.

## Build, flash, watch

On Windows the ESP-IDF build needs a very short target directory (esp-idf-sys
refuses long output paths), so set `CARGO_TARGET_DIR` to something like
`C:\janus-t`. The IDF tools install globally under `~/.espressif`.

```sh
export CARGO_TARGET_DIR=C:/janus-t                 # Windows only
JANUS_WIFI_SSID=yournet JANUS_WIFI_PASS=yourpass cargo build --release
JANUS_WIFI_SSID=yournet JANUS_WIFI_PASS=yourpass cargo run --release   # espflash flash --monitor
```

The log prints `stream at http://<ip>/stream`. Open it in a browser, or:

```sh
ffmpeg -i http://<ip>/stream -frames:v 150 -f null -      # 10 s at 15 fps
```

## The kill test (from the plan, I1 + V1)

- `http://<ip>/stream` opens in a browser at 320×240.
- Ten minutes of streaming with the frame count and drop count from the log;
  every frame passed `Frame::packed` (starts `FF D8`).
- Optional (pi-mission H3): the same URL recorded to disk on a Raspberry Pi.

Record the numbers in `rusty_esp_video/docs/LEDGER.md` and
`rusty_esp_image/docs/LEDGER.md`.

## Notes

- One viewer at a time, by design for v1.
- The frame is copied once from the driver's PSRAM buffer into the pool slot;
  a zero-copy variant is a later optimisation, measured first.
- Wi-Fi credentials are compile-time here. `espino adopt` replaces this with
  the signed adoption record in `data/config.json`.
