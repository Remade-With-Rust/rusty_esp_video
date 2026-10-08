//! Janus J1 on Track B: the camera page and its MJPEG stream served from a
//! XIAO ESP32-S3 Sense with no ESP-IDF, no esp32-camera and no lwIP under
//! it.
//!
//! The CameraWebServer remade on bare metal, whole: esp-radio's station,
//! embassy-net's DHCP and TCP through `rusty_esp_signal-esp::hal::netstack`
//! (X2), the HTTP the page needs from `rusty_esp_video_core::http` and the
//! multipart stream from `mjpeg_http::Multipart` — the same protocol code
//! the ESP-IDF server runs over `std::net` — and, since X5, the sensor
//! driven from Rust register tables (`rusty_esp_image_core::driver`) and
//! frames received by `rusty_esp_image_esp::hal::DvpCamera` over LCD_CAM
//! and GDMA, JPEG straight from the sensor. `GET /` is the page, `GET
//! /stream` the frames until the viewer leaves, everything else what the
//! `std` server would say. One connection at a time, as a chip streams to
//! one viewer.
//!
//! The network is compile-time, one of two ways, as the ESP-IDF twin has
//! it: `JANUS_WIFI_SSID` and `JANUS_WIFI_PASS` (empty for an open network)
//! to join a network; or `JANUS_AP_PASS` (with `JANUS_AP_SSID`, default
//! `janus-cam`) to host one, WPA2, at 192.168.71.1 with the DHCP server a
//! laptop expects — the arrangement V1 was measured in, with no router in
//! the line. Lines are prefixed `PAGE` so a monitor can parse them; the
//! first carries the network and the URL to open, and a stream's end
//! carries its frame count and rate.
#![no_std]
#![no_main]

extern crate alloc;

use core::alloc::Layout;
use core::net::Ipv4Addr;

use embassy_net::tcp::TcpSocket;
use embassy_net::{Ipv4Cidr, StackResources};
use embassy_time::{Duration, Timer};
use embedded_io_async::Write as _;
use esp_backtrace as _;
use esp_hal::delay::Delay;
use esp_hal::dma_rx_stream_buffer;
use esp_hal::i2c::master::{Config as I2cConfig, I2c};
use esp_hal::lcd_cam::cam::{Camera, Config as CamConfig, VsyncFilterThreshold};
use esp_hal::lcd_cam::LcdCam;
use esp_hal::psram::{PsramConfig, PsramMode};
use esp_hal::rng::Rng;
use esp_hal::time::{Instant, Rate};
use esp_hal::timer::timg::TimerGroup;
use esp_println::println;
use esp_radio::wifi::ap::EventInfo;
use esp_radio::wifi::{ControllerConfig, Interface, WifiController};
use rusty_esp_image_core::driver::{Ov2640, Ov3660};
use rusty_esp_image_core::esp_core::frame::Planes;
use rusty_esp_image_core::pool::FramePool;
use rusty_esp_image_core::sensor::{FrameSize, Mode};
use rusty_esp_image_core::source::ImageSource;
use rusty_esp_image_esp::hal::DvpCamera;
use rusty_esp_signal_core::wifi::PolicyConfig;
use rusty_esp_signal_esp::hal::netstack::{self, SOCKETS};
use rusty_esp_video_core::esp_core::Micros;
use rusty_esp_video_core::http::{self, Path, Request, Status, MAX_REQUEST_BYTES};
use rusty_esp_video_core::mjpeg_http::Multipart;
use rusty_esp_video_core::packet::{Codec, MediaPacket};
use rusty_esp_video_core::sink::SliceSink;
use static_cell::StaticCell;

esp_bootloader_esp_idf::esp_app_desc!();

/// The network, compiled in: join one, or host one.
const SSID: Option<&str> = option_env!("JANUS_WIFI_SSID");
const PASS: Option<&str> = option_env!("JANUS_WIFI_PASS");
const AP_SSID: Option<&str> = option_env!("JANUS_AP_SSID");
const AP_PASS: Option<&str> = option_env!("JANUS_AP_PASS");
const _: () = assert!(
    AP_PASS.is_some() || SSID.is_some(),
    "set JANUS_WIFI_SSID (and JANUS_WIFI_PASS) to join a network, or JANUS_AP_PASS (and JANUS_AP_SSID) to host one"
);
/// The hosted network's name when `JANUS_AP_SSID` is not set: the ESP-IDF
/// twin's, so the same bench runner joins either.
const AP_SSID_DEFAULT: &str = "janus-cam";
/// The hosted network's address: the board is `.1` and the gateway, as the
/// ESP-IDF twin was; leases run from `.50`.
const AP_ADDRESS: Ipv4Cidr = Ipv4Cidr::new(Ipv4Addr::new(192, 168, 71, 1), 24);
/// Clients the hosted network takes at once.
const AP_STATIONS: u16 = 4;
/// Where the page is served.
const PORT: u16 = 80;
/// The longest response this firmware writes outside the stream: the page
/// with its headers.
const RESPONSE_BYTES: usize = 1024;
/// The sensor's master clock; the C driver's.
const XCLK_HZ: u32 = 20_000_000;
/// Bytes of the DMA ring the camera streams into, and per PSRAM slot.
const RING_BYTES: usize = 64 * 1024;
const SLOT_BYTES: usize = 32 * 1024;
const POOL_SLOTS: usize = 2;
/// One multipart part: its headers and a frame.
const PART_BYTES: usize = SLOT_BYTES + 256;

/// The stack's sockets and buffers live for the firmware.
static RESOURCES: StaticCell<StackResources<SOCKETS>> = StaticCell::new();

/// A monotonic microsecond clock read from esp-hal's system timer.
fn now() -> Micros {
    Micros(Instant::now().duration_since_epoch().as_micros())
}

/// Which part answered on the bus, and how to drive it.
enum Sensor<I> {
    Ov3660(Ov3660<I>),
    Ov2640(Ov2640<I>),
}

impl<I: embedded_hal::i2c::I2c> Sensor<I> {
    fn name(&self) -> &'static str {
        match self {
            Sensor::Ov3660(_) => "OV3660",
            Sensor::Ov2640(_) => "OV2640",
        }
    }

    fn reset(
        &mut self,
        delay: &mut impl FnMut(u16),
    ) -> rusty_esp_image_core::esp_core::error::Result<()> {
        match self {
            Sensor::Ov3660(s) => s.reset(delay),
            Sensor::Ov2640(s) => s.reset(delay),
        }
    }

    fn configure(
        &mut self,
        mode: &Mode,
        delay: &mut impl FnMut(u16),
    ) -> rusty_esp_image_core::esp_core::error::Result<()> {
        match self {
            Sensor::Ov3660(s) => s.configure(mode),
            Sensor::Ov2640(s) => s.configure(mode, delay),
        }
    }
}

/// `n` bytes of PSRAM, from the external region esp-alloc holds.
fn psram_bytes(n: usize) -> &'static mut [u8] {
    let layout = Layout::from_size_align(n, 16).expect("layout");
    // SAFETY: never freed (the pool lives for the firmware), non-zero, and
    // the region is the PSRAM the allocator was given.
    unsafe {
        let ptr = esp_alloc::HEAP.alloc_caps(esp_alloc::MemoryCapability::External.into(), layout);
        assert!(!ptr.is_null(), "no PSRAM for the frame pool");
        core::ptr::write_bytes(ptr, 0, n);
        core::slice::from_raw_parts_mut(ptr, n)
    }
}

/// Keeps the hosted network's radio alive for the life of the firmware and
/// says who joins and leaves.
#[embassy_executor::task]
async fn access_point_task(controller: WifiController<'static>) -> ! {
    loop {
        match controller
            .wait_for_access_point_connected_event_async()
            .await
        {
            Ok(EventInfo::Connected(info)) => println!("PAGE station=joined aid={}", info.aid),
            Ok(EventInfo::Disconnected(info)) => println!("PAGE station=left aid={}", info.aid),
            Err(_) => Timer::after(Duration::from_secs(1)).await,
        }
    }
}

#[esp_rtos::main]
async fn main(spawner: embassy_executor::Spawner) {
    let peripherals =
        esp_hal::init(esp_hal::Config::default().with_cpu_clock(esp_hal::clock::CpuClock::max()));
    esp_alloc::heap_allocator!(size: 128 * 1024);
    esp_alloc::psram_allocator!(
        peripherals.PSRAM,
        esp_hal::psram,
        PsramConfig {
            mode: PsramMode::OctalSpi,
            ..PsramConfig::default()
        }
    );

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);

    println!("== JANUS PAGE xiao-s3 track=B ==");
    let boot = Instant::now();

    // ---- the camera, before the radio: its clock, its bus, its sensor ------
    let mode = Mode::jpeg(FrameSize::Qvga).expect("jpeg mode");
    let lcd_cam = LcdCam::new(peripherals.LCD_CAM);
    let cam_config = CamConfig::default()
        .with_frequency(Rate::from_hz(XCLK_HZ))
        .with_vsync_filter_threshold(VsyncFilterThreshold::Four);
    let camera = Camera::new(lcd_cam.cam, peripherals.DMA_CH0, cam_config)
        .expect("camera")
        .with_master_clock(peripherals.GPIO10)
        .with_pixel_clock(peripherals.GPIO13)
        .with_vsync(peripherals.GPIO38)
        .with_h_enable(peripherals.GPIO47)
        .with_data0(peripherals.GPIO15)
        .with_data1(peripherals.GPIO17)
        .with_data2(peripherals.GPIO18)
        .with_data3(peripherals.GPIO16)
        .with_data4(peripherals.GPIO14)
        .with_data5(peripherals.GPIO12)
        .with_data6(peripherals.GPIO11)
        .with_data7(peripherals.GPIO48);
    let delay = Delay::new();
    let mut wait = |ms: u16| delay.delay_millis(u32::from(ms));
    wait(10);
    let i2c = I2c::new(
        peripherals.I2C0,
        // The control bus at 400 kHz, SCCB's fast-mode rate (OV3660 and OV2640):
        // the OV3660 configures in 235 ms, not 305, every grab good (round 3, B11).
        I2cConfig::default().with_frequency(Rate::from_khz(400)),
    )
    .expect("i2c")
    .with_sda(peripherals.GPIO40)
    .with_scl(peripherals.GPIO39);
    let mut ov3660 = Ov3660::new(i2c, XCLK_HZ);
    let mut sensor = match ov3660.probe() {
        Ok(true) => Some(Sensor::Ov3660(ov3660)),
        _ => {
            let mut ov2640 = Ov2640::new(ov3660.release());
            match ov2640.probe() {
                Ok(true) => Some(Sensor::Ov2640(ov2640)),
                _ => None,
            }
        }
    };
    let mut cam = match sensor.as_mut() {
        Some(s) => {
            let r = s
                .reset(&mut wait)
                .and_then(|()| s.configure(&mode, &mut wait));
            println!("PAGE sensor={} configured={r:?}", s.name());
            let ring = dma_rx_stream_buffer!(RING_BYTES, 4092);
            Some(DvpCamera::new(camera, ring, mode.geometry).expect("engine"))
        }
        None => {
            println!("PAGE sensor=none");
            None
        }
    };
    let pool_bytes = psram_bytes(POOL_SLOTS * SLOT_BYTES);
    let mut pool: FramePool<'_, POOL_SLOTS> = FramePool::new(pool_bytes).expect("pool");

    // ---- the radio and the stack: joining a network, or hosting one --------
    let mut controller =
        WifiController::new(peripherals.WIFI, ControllerConfig::default()).expect("wifi");
    let rng = Rng::new();
    let seed = (u64::from(rng.random()) << 32) | u64::from(rng.random());
    let resources = RESOURCES.init(StackResources::new());
    let stack = match (AP_PASS, SSID) {
        (Some(pass), _) => {
            // hosting: the radio starts the access point inside set_config;
            // the stack sits at a fixed address and serves leases
            let ssid = AP_SSID.unwrap_or(AP_SSID_DEFAULT);
            controller
                .set_config(
                    &netstack::access_point_config(ssid, pass, AP_STATIONS)
                        .expect("access point credentials"),
                )
                .expect("access point config");
            let (stack, runner) =
                netstack::hosted_stack(Interface::access_point(), AP_ADDRESS, resources, seed);
            spawner.spawn(netstack::net_task(runner).expect("net task"));
            spawner.spawn(access_point_task(controller).expect("access point task"));
            spawner.spawn(netstack::dhcp_server_task(stack, AP_ADDRESS).expect("dhcp server task"));
            println!("PAGE hosting={ssid} auth=wpa2 stations={AP_STATIONS}");
            stack
        }
        (None, Some(ssid)) => {
            controller
                .set_config(
                    &netstack::station_config(ssid, PASS.unwrap_or("")).expect("credentials"),
                )
                .expect("station config");
            let (stack, runner) = netstack::stack(Interface::station(), resources, seed);
            spawner.spawn(netstack::net_task(runner).expect("net task"));
            spawner.spawn(
                netstack::station_task(controller, PolicyConfig::DEFAULT, now)
                    .expect("station task"),
            );
            println!("PAGE joining={ssid}");
            stack
        }
        (None, None) => unreachable!("the const assert above"),
    };

    stack.wait_link_up().await;
    let join_ms = boot.elapsed().as_millis();
    stack.wait_config_up().await;
    let dhcp_ms = boot.elapsed().as_millis();
    let ip = stack.config_v4().expect("ipv4 address").address.address();
    println!("PAGE ip={ip} url=http://{ip}/ stream=http://{ip}/stream join_ms={join_ms} dhcp_ms={dhcp_ms}");

    let mut rx_buf = [0u8; MAX_REQUEST_BYTES];
    let mut tx_buf = [0u8; 8 * 1024];
    let mut head = [0u8; MAX_REQUEST_BYTES];
    let mut response = [0u8; RESPONSE_BYTES];
    let part = psram_bytes(PART_BYTES);
    let mut connections: u64 = 0;
    loop {
        let mut socket = TcpSocket::new(stack, &mut rx_buf, &mut tx_buf);
        socket.set_timeout(Some(Duration::from_secs(5)));
        if socket.accept(PORT).await.is_err() {
            continue;
        }
        connections += 1;

        // The request head, then what the protocol says about it.
        let mut len = 0usize;
        // how much of the head has been searched for its blank line: each
        // read's new bytes are searched once (W8)
        let mut seen = 0usize;
        let complete = loop {
            if http::head_complete_from(&head[..len], seen) {
                break true;
            }
            seen = len;
            if len >= head.len() {
                break false;
            }
            match socket.read(&mut head[len..]).await {
                Ok(0) | Err(_) => break false,
                Ok(n) => len += n,
            }
        };
        let request = if complete {
            http::parse_request(&head[..len], None)
        } else {
            None
        };

        // The stream: the head, then frames until the viewer leaves.
        if let (
            Some(Request::Get {
                path: Path::Stream, ..
            }),
            Some(cam),
        ) = (&request, cam.as_mut())
        {
            let started = Instant::now();
            let mut frames: u64 = 0;
            let mut bytes: u64 = 0;
            let mut grab_errors: u64 = 0;
            let head_ok = {
                let mut mp = Multipart::new(SliceSink::new(part));
                let ok = mp.write_response_head().is_ok();
                let sink = mp.into_sink();
                ok && socket.write_all(sink.written()).await.is_ok()
            };
            if head_ok {
                loop {
                    // the frame, without spinning: the stack runs meanwhile
                    while !cam.frame_ready() {
                        Timer::after(Duration::from_millis(1)).await;
                    }
                    let slot = pool.acquire().expect("a free slot");
                    let grabbed = cam.grab(pool.slot_mut(slot).expect("slot"));
                    let sent = match grabbed {
                        Ok(frame) => {
                            let Planes::Packed(data) = frame.planes else {
                                pool.release(slot).expect("release");
                                continue;
                            };
                            let mut mp = Multipart::new(SliceSink::new(part));
                            let packet = MediaPacket::new(Codec::Jpeg, true, frame.timestamp, data);
                            let pushed = mp.push(&packet).is_ok();
                            let sink = mp.into_sink();
                            let n = sink.len();
                            let ok = pushed && socket.write_all(sink.written()).await.is_ok();
                            if ok {
                                frames += 1;
                                bytes += n as u64;
                            }
                            Some(ok)
                        }
                        Err(_) => {
                            grab_errors += 1;
                            None
                        }
                    };
                    pool.release(slot).expect("release");
                    if sent == Some(false) || grab_errors > 100 {
                        break;
                    }
                    if frames > 0 && frames.is_multiple_of(100) {
                        println!(
                            "PAGE streaming frames={frames} bytes={bytes} secs={}",
                            started.elapsed().as_secs()
                        );
                    }
                }
            }
            let us = started.elapsed().as_micros();
            let stats = cam.stats();
            println!(
                "PAGE stream frames={frames} bytes={bytes} us={us} fps_milli={} grab_errors={grab_errors} skipped={} timeouts={} restarts={} false_eoi={}",
                frames * 1_000_000_000 / us.max(1),
                stats.skipped,
                stats.timeouts,
                stats.restarts,
                stats.false_eoi
            );
            let _ = socket.flush().await;
            socket.close();
            Timer::after(Duration::from_millis(50)).await;
            continue;
        }

        // Everything else: formatted in full and sent as one write.
        let mut sink = SliceSink::new(&mut response);
        let what = match request {
            Some(Request::Get {
                path: Path::Index, ..
            }) => {
                let _ = http::write_index(&mut sink, None);
                "index"
            }
            Some(Request::Get {
                path: Path::Stream, ..
            }) => {
                let _ = http::write_status(&mut sink, Status::NoCamera);
                "no-camera"
            }
            Some(Request::Get { .. }) => {
                let _ = http::write_status(&mut sink, Status::NotFound);
                "not-found"
            }
            // this page takes no update and no form: a `PUT` (the generated
            // cells' `/update`, X7) and a `POST` are both methods it does not
            // serve
            Some(Request::Put { .. }) | Some(Request::Post { .. }) | Some(Request::Other) => {
                let _ = http::write_status(&mut sink, Status::MethodNotAllowed);
                "method"
            }
            None => {
                let _ = http::write_status(&mut sink, Status::BadRequest);
                "bad-request"
            }
        };
        let written = socket.write_all(sink.written()).await.is_ok();
        let _ = socket.flush().await;
        socket.close();
        // Let the close reach the peer before the socket is torn down.
        Timer::after(Duration::from_millis(50)).await;
        println!(
            "PAGE served={what} bytes={} written={written} connections={connections}",
            sink.len()
        );
    }
}
