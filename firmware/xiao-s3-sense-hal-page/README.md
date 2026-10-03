# xiao-s3-sense-hal-page

Janus **J1's page on Track B** (`no_std` on esp-hal): the camera page and
its MJPEG stream served from a XIAO ESP32-S3 Sense with no ESP-IDF, no
esp32-camera and no lwIP under it. Row **X2** of the killing-C plan gave it
the network; row **X5** gave it the camera, and its kill test — `/stream`
at 320×240 faster than V1 on the same board — **passed 2026-09-30**.

What it is: esp-radio's radio and embassy-net's stack through
`rusty_esp_signal-esp::hal::netstack` (joining a network, or hosting one
with the DHCP server a laptop expects), the HTTP the page needs from
`rusty_esp_video_core::http` — the same protocol code the ESP-IDF server in
[`xiao-s3-sense-idf-mjpeg`](../xiao-s3-sense-idf-mjpeg) runs over
`std::net` — the multipart stream from `mjpeg_http::Multipart`, the sensor
driven by `rusty_esp_image_core::driver` and frames from
`rusty_esp_image_esp::hal::DvpCamera` (LCD_CAM and GDMA into a ring, JPEG
straight from the sensor). One viewer at a time, as a chip streams to one.

## Build

The network is compiled in, one of two ways, exactly as the ESP-IDF twin
has it. The passphrase is an environment variable, never a file in this
tree and never a commit.

```sh
# join a 2.4 GHz network:
export JANUS_WIFI_SSID=yournet JANUS_WIFI_PASS=yourpass
# or host one (WPA2, 192.168.71.1, leases from .50; the V1 arrangement):
export JANUS_AP_PASS=yourpass                # JANUS_AP_SSID defaults to janus-cam
export CARGO_TARGET_DIR=F:/jt-page           # any short directory
cargo build --release
```

Hosting wins when both are set. A build with neither fails at compile time
and says so.

## The run (needs the board)

```sh
espino flash   --board xiao-esp32s3-sense --port COM4 --app $CARGO_TARGET_DIR/xtensa-esp32s3-none-elf/release/xiao-s3-sense-hal-page
espino monitor --board xiao-esp32s3-sense --port COM4 --timeout 300 > page.txt
```

The first `PAGE` lines carry the sensor, the network (`hosting=` or
`joining=`) and the URL. Open it in a browser on that network: the page
loads and its `<img src="/stream">` plays. Or:

```sh
curl -i http://<ip>/            # 200, the page
ffplay http://<ip>/stream       # the stream
curl -i http://<ip>/x           # 404
curl -i -X POST http://<ip>/    # 405
```

Each connection prints `PAGE served=<what> …`; a stream prints `PAGE
streaming …` every hundred frames and, when the viewer leaves, `PAGE stream
frames=… fps_milli=… restarts=… timeouts=…`.

## The kill test (X5, second half) — passed

Measured unattended by the umbrella's `tools/x5-stream-offline.ps1` (the
laptop has one radio, so joining the board's network costs it the internet;
the runner saves the current network, joins `janus-cam` with the passphrase
read from V1's gitignored file into a profile it deletes afterwards,
measures, and comes back). On 2026-09-30 (`x5-results.txt` and
`x5-results.json` beside this file):

| | this firmware (Track B) | V1, ESP-IDF twin (2026-09-11) |
|---|---:|---:|
| ffmpeg decoded frames | 1,500 in 55.5 s | 1,500 in 108.6 s |
| **fps** | **27.04** | 13.81 (a 15 fps cap against a 27.5 fps sensor) |
| the board's own count | 1,504 at 27.60 fps | 1,502 |
| bytes over the link, 10 s | 1,107,291 (108 KiB/s) | 62.2 KiB/s |
| ping RTT min / mean / max | 1 / 9.1 / 60 ms | 2 / 3 / 7 ms |
| link | 802.11n ch 1, 95 %, 150 / 135 Mbps | 802.11n ch 1, 98 % |

The sensor makes 27.8 fps; this firmware delivers all of it, with no cap,
because nothing in the path stalls: the ring engine grabs, the stack sends,
and the one `restart` in 54 s was the ring filling behind a slow send.

## The kill test (X2)

`http://<ip>/` served from this firmware on a 2.4 GHz network the laptop is
also on — met by the run above (the page was fetched with status 200 before
the stream), with the board hosting the network rather than joining one.
