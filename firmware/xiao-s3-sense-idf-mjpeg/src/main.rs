//! Janus J1 on the XIAO ESP32-S3 Sense: sensor JPEG → MJPEG over HTTP.
//!
//! The kill test: `http://<ip>/stream` opens in a browser at 320×240, and
//! this log prints the frame count and rate every connection.
//!
//! **Two ways onto a network.** Join one:
//!
//! ```sh
//! JANUS_WIFI_SSID=mynet JANUS_WIFI_PASS=secret cargo run --release
//! ```
//!
//! Or run one, which is what a board with no access point in reach has to
//! do, and what makes the V1 row measurable on a bench with no router:
//!
//! ```sh
//! JANUS_AP_PASS=... cargo run --release          # set it in your own shell
//! ```
//!
//! Hosting is the fallback, not the default: a device that can reach the
//! house should be on the house's network. The passphrase is read from the
//! environment at build time and **never logged** — the address is printed,
//! the key never is.

use std::time::Instant;

use anyhow::{Context, Result};
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::log::EspLogger;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::sys::link_patches;
use esp_idf_svc::wifi::{
    AccessPointConfiguration, AuthMethod, BlockingWifi, ClientConfiguration, Configuration, EspWifi,
};
use rusty_esp_image_core::sensor::{FrameSize, Mode};
use rusty_esp_image_esp::idf::IdfCamera;
use rusty_esp_image_esp::XIAO_ESP32S3_SENSE;
use rusty_esp_video_core::encoder::{EncoderConfig, Passthrough, VideoEncoder};
use rusty_esp_video_core::source::EncodedSource;
use rusty_esp_video_esp::net::{MjpegHttpServer, ServeStats};

/// Credentials for joining somebody else's network. Both must be set.
const SSID: Option<&str> = option_env!("JANUS_WIFI_SSID");
const PASS: Option<&str> = option_env!("JANUS_WIFI_PASS");
/// The network this board runs when it has none to join. WPA2 only, so the
/// passphrase is required and 8 to 63 bytes; an open access point would
/// serve the camera to the street.
const AP_SSID: Option<&str> = option_env!("JANUS_AP_SSID");
const AP_PASS: Option<&str> = option_env!("JANUS_AP_PASS");
/// Used when `JANUS_AP_SSID` is not set. A second board on the same bench
/// needs its own, or the two are indistinguishable to a client.
const AP_SSID_DEFAULT: &str = "janus-cam";

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

    // Join if there is something to join, otherwise run a network of our
    // own. Nothing below this line cares which happened: the server binds
    // 0.0.0.0 either way and the address is read from whichever interface
    // ended up holding it.
    let ip = match (SSID, PASS) {
        (Some(ssid), Some(pass)) => {
            wifi.set_configuration(&Configuration::Client(ClientConfiguration {
                ssid: ssid
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("SSID too long"))?,
                password: pass
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("password too long"))?,
                ..Default::default()
            }))?;
            wifi.start()?;
            wifi.connect()?;
            wifi.wait_netif_up()?;
            let ip = wifi.wifi().sta_netif().get_ip_info()?.ip;
            log::info!("janus j1: joined {ssid}");
            ip
        }
        _ => {
            let psk = AP_PASS.context(
                "no network to join and no JANUS_AP_PASS to host one: set either                  JANUS_WIFI_SSID and JANUS_WIFI_PASS, or JANUS_AP_PASS",
            )?;
            if !(8..=63).contains(&psk.len()) {
                anyhow::bail!("JANUS_AP_PASS must be 8 to 63 bytes for WPA2");
            }
            let ssid = AP_SSID.unwrap_or(AP_SSID_DEFAULT);
            wifi.set_configuration(&Configuration::AccessPoint(AccessPointConfiguration {
                ssid: ssid
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("SSID too long"))?,
                password: psk
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("password too long"))?,
                auth_method: AuthMethod::WPA2Personal,
                max_connections: 4,
                ..Default::default()
            }))?;
            wifi.start()?;
            wifi.wait_netif_up()?;
            let ip = wifi.wifi().ap_netif().get_ip_info()?.ip;
            // the network's name, never its key
            log::info!("janus j1: hosting {ssid} (WPA2), up to 4 clients");
            ip
        }
    };
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
