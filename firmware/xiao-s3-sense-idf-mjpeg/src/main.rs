//! Janus J1 on the XIAO ESP32-S3 Sense: sensor JPEG → MJPEG over HTTP.
//!
//! The kill test: `http://<ip>/stream` opens in a browser at 320×240, and
//! this log prints the frame count and rate every connection. Set the Wi-Fi
//! credentials at build time:
//!
//! ```sh
//! JANUS_WIFI_SSID=mynet JANUS_WIFI_PASS=secret cargo run --release
//! ```

use std::time::Instant;

use anyhow::{Context, Result};
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::log::EspLogger;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::sys::link_patches;
use esp_idf_svc::wifi::{BlockingWifi, ClientConfiguration, Configuration, EspWifi};
use rusty_esp_image_core::sensor::{FrameSize, Mode};
use rusty_esp_image_esp::XIAO_ESP32S3_SENSE;
use rusty_esp_image_esp::idf::IdfCamera;
use rusty_esp_video_core::encoder::{EncoderConfig, Passthrough, VideoEncoder};
use rusty_esp_video_core::source::EncodedSource;
use rusty_esp_video_esp::net::{MjpegHttpServer, ServeStats};

const SSID: &str = env!("JANUS_WIFI_SSID");
const PASS: &str = env!("JANUS_WIFI_PASS");

/// Largest JPEG the pool must hold; QVGA from an OV2640 is ~10–25 KB.
const FRAME_BYTES: usize = 64 * 1024;
/// Frame-rate cap for the stream.
const FPS_CAP: u32 = 15;

fn main() -> Result<()> {
    link_patches();
    EspLogger::initialize_default();

    let peripherals = Peripherals::take()?;
    let sysloop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;

    let mut wifi = BlockingWifi::wrap(
        EspWifi::new(peripherals.modem, sysloop.clone(), Some(nvs))?,
        sysloop,
    )?;
    wifi.set_configuration(&Configuration::Client(ClientConfiguration {
        ssid: SSID.try_into().map_err(|_| anyhow::anyhow!("SSID too long"))?,
        password: PASS.try_into().map_err(|_| anyhow::anyhow!("password too long"))?,
        ..Default::default()
    }))?;
    wifi.start()?;
    wifi.connect()?;
    wifi.wait_netif_up()?;
    let ip = wifi.wifi().sta_netif().get_ip_info()?.ip;
    log::info!("janus j1: stream at http://{ip}/stream  (page at http://{ip}/)");

    let mode = Mode::jpeg(FrameSize::Qvga).context("mode")?;
    let camera = IdfCamera::init(&XIAO_ESP32S3_SENSE, &mode, 2).context("camera init")?;
    let mut encoder = Passthrough::default();
    encoder
        .configure(mode.geometry, &EncoderConfig::default())
        .context("encoder")?;
    let mut source = EncodedSource::new(camera, encoder, FRAME_BYTES, FPS_CAP).context("source")?;
    let mut scratch = vec![0u8; FRAME_BYTES + 1];

    let server = MjpegHttpServer::bind("0.0.0.0:80").context("bind :80")?;
    let mut stats = ServeStats::default();
    let started = Instant::now();
    loop {
        let served = server
            .serve_one(&mut source, &mut scratch, &mut stats)
            .context("serve")?;
        let secs = started.elapsed().as_secs_f32().max(0.001);
        log::info!(
            "{served:?}  frames={} dropped={} produced={} avg_fps={:.1}  totals={stats:?}",
            stats.frames,
            source.dropped,
            source.produced,
            stats.frames as f32 / secs
        );
    }
}
