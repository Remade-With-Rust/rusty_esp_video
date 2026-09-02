//! Which encoder for which job — the codec policy, as data and one function.
//!
//! A device does not have one encoder. It has a sensor that already emits
//! JPEG, a CPU that can run H.264 Baseline at a small size, on some chips a
//! hardware H.264 block, and a home computer at the other end of the link
//! that can transcode to anything. **The job picks the codec**, not the other
//! way round, and the pick is a property of the whole path: what the chip can
//! afford, what the link can carry, and what the consumer will do with the
//! bytes.
//!
//! | job | on the chip | on the home computer | why |
//! |---|---|---|---|
//! | [`Job::Preview`] — a browser tab, a dashboard tile | **JPEG** (sensor passthrough) | none | zero encode cost, any chip, one frame is one packet, a browser renders it natively |
//! | [`Job::Stream`] — live view over Wi-Fi / the mesh | **H.264** Baseline at QVGA–VGA (software on S3/C6, hardware on P4) | none, or a relay | 5–10× fewer bytes than MJPEG at the same quality; the link is the scarce resource |
//! | [`Job::Archive`] — hours on a disk | **H.264** on the chip | **AV1 / AV2** transcode on the host (`rff`) | the chip cannot afford AV1 (no `no_std` encoder, 50× the cycles); the host can, and archive bytes are paid for forever |
//! | [`Job::Analytics`] — FFAI models on the host | **JPEG** key frames at a low rate, or H.264 I-frames | decode to raw | a detector wants whole frames, not a GOP; a lower frame rate beats a lower quality |
//!
//! Two consequences worth stating. First, **there is no AV1 or AV2 on a
//! chip** in this family: the Remade AV1/AV2 crates are host-only (they carry
//! C and assembler), and the cycle budget of an ESP32-S3 is a fraction of what
//! AV1 needs; a product that wants AV1 archive gets H.264 from the device and
//! transcodes where the power is. Second, **JPEG is not the "cheap" choice,
//! it is the *right* choice for two of the four jobs**, so `Passthrough` stays
//! a first-class encoder and not a fallback.
//!
//! Audio has the same shape, decided in the audio package's plan: PCM on the
//! LAN (lowest latency), IMA-ADPCM when a link is tight (4:1, trivial), FLAC
//! for lossless archive, and Opus only as a host-side transcode until an
//! Opus encoder exists that a chip can run.
//!
//! [`codec_for`] is the table above as a function, so a firmware, a manifest
//! builder and the home computer's fleet code all pick the same way and a
//! test pins the policy.

use rusty_esp_core::Chip;

use crate::packet::Codec;

/// What the consumer will do with the frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum Job {
    /// A human glancing at it: a browser tab, a dashboard tile.
    Preview,
    /// A human watching it live across a link that is the scarce resource.
    Stream,
    /// Hours on a disk, to be kept.
    Archive,
    /// A model on the home computer consuming whole frames.
    Analytics,
}

impl Job {
    /// Every job, for tables and manifests.
    pub const ALL: [Job; 4] = [Job::Preview, Job::Stream, Job::Archive, Job::Analytics];

    /// Short stable tag for logs and manifests.
    #[must_use]
    pub const fn tag(self) -> &'static str {
        match self {
            Job::Preview => "preview",
            Job::Stream => "stream",
            Job::Archive => "archive",
            Job::Analytics => "analytics",
        }
    }
}

/// Where the H.264 bytes come from on a given chip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum H264Path {
    /// `rusty_h264` Baseline, software, small sizes only.
    Software,
    /// The chip's hardware encoder (ESP32-P4) behind the same trait.
    Hardware,
}

/// The codec a chip should emit for a job, and what the host does after.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Choice {
    /// What the device encodes.
    pub on_chip: Codec,
    /// For H.264, which encoder does it.
    pub h264: Option<H264Path>,
    /// Whether the home computer transcodes the archive to AV1/AV2.
    pub host_transcodes: bool,
}

/// The policy, as a function of the job and the chip.
///
/// Chips without Wi-Fi (H2) or without the memory for a software H.264
/// pipeline (C6 at QVGA is the floor) fall back to JPEG for `Stream` and
/// `Archive`; the host still gets bytes it can transcode.
#[must_use]
pub const fn codec_for(job: Job, chip: Chip) -> Choice {
    let h264 = h264_path(chip);
    match job {
        Job::Preview | Job::Analytics => Choice {
            on_chip: Codec::Jpeg,
            h264: None,
            host_transcodes: false,
        },
        Job::Stream => match h264 {
            Some(path) => Choice {
                on_chip: Codec::H264,
                h264: Some(path),
                host_transcodes: false,
            },
            None => Choice {
                on_chip: Codec::Jpeg,
                h264: None,
                host_transcodes: false,
            },
        },
        Job::Archive => match h264 {
            Some(path) => Choice {
                on_chip: Codec::H264,
                h264: Some(path),
                host_transcodes: true,
            },
            None => Choice {
                on_chip: Codec::Jpeg,
                h264: None,
                host_transcodes: true,
            },
        },
    }
}

/// Which H.264 encoder a chip can run, if any.
#[must_use]
pub const fn h264_path(chip: Chip) -> Option<H264Path> {
    match chip {
        Chip::Esp32P4 => Some(H264Path::Hardware),
        Chip::Esp32S3 | Chip::Esp32 | Chip::Esp32S2 => Some(H264Path::Software),
        // 400 KB of SRAM and no PSRAM on the devkit: QVGA I-only would fit,
        // but the plan keeps the C6 for signal and the camera boards for
        // video, so it streams JPEG.
        Chip::Esp32C6 | Chip::Esp32C3 | Chip::Esp32C5 | Chip::Esp32C61 | Chip::Esp32H2 => None,
        // `Chip` is non-exhaustive: a part this policy has not met streams JPEG.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_and_analytics_are_jpeg_everywhere() {
        for chip in [Chip::Esp32P4, Chip::Esp32S3, Chip::Esp32C6, Chip::Esp32H2] {
            assert_eq!(codec_for(Job::Preview, chip).on_chip, Codec::Jpeg);
            assert_eq!(codec_for(Job::Analytics, chip).on_chip, Codec::Jpeg);
            assert!(!codec_for(Job::Preview, chip).host_transcodes);
        }
    }

    #[test]
    fn stream_is_h264_where_the_chip_can_and_jpeg_where_it_cannot() {
        assert_eq!(
            codec_for(Job::Stream, Chip::Esp32P4),
            Choice {
                on_chip: Codec::H264,
                h264: Some(H264Path::Hardware),
                host_transcodes: false
            }
        );
        assert_eq!(
            codec_for(Job::Stream, Chip::Esp32S3).h264,
            Some(H264Path::Software)
        );
        assert_eq!(codec_for(Job::Stream, Chip::Esp32C6).on_chip, Codec::Jpeg);
    }

    #[test]
    fn archive_always_transcodes_on_the_host_and_never_av1_on_chip() {
        for chip in [Chip::Esp32P4, Chip::Esp32S3, Chip::Esp32C6] {
            let c = codec_for(Job::Archive, chip);
            assert!(c.host_transcodes, "{chip:?}");
            assert!(matches!(c.on_chip, Codec::Jpeg | Codec::H264), "{chip:?}");
        }
    }

    #[test]
    fn tags_are_stable() {
        assert_eq!(
            Job::ALL.map(Job::tag),
            ["preview", "stream", "archive", "analytics"]
        );
    }
}
