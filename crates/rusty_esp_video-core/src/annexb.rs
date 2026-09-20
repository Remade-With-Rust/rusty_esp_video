//! H.264 Annex-B byte-stream helpers: find NAL units between start codes and
//! split a stream into access units.

/// NAL unit types this crate looks at.
pub mod nal_type {
    /// Coded slice of a non-IDR picture.
    pub const SLICE: u8 = 1;
    /// Coded slice of an IDR picture.
    pub const IDR: u8 = 5;
    /// Supplemental enhancement information.
    pub const SEI: u8 = 6;
    /// Sequence parameter set.
    pub const SPS: u8 = 7;
    /// Picture parameter set.
    pub const PPS: u8 = 8;
    /// Access unit delimiter.
    pub const AUD: u8 = 9;
}

/// An access unit delimiter for a picture of any slice type, with a 4-byte start code.
pub const AUD_NAL: [u8; 6] = [0x00, 0x00, 0x00, 0x01, 0x09, 0xF0];

/// Index of the first `00 00 01` in `bytes`, by the definition this module
/// has always used: the lowest `i` with `i + 2 < bytes.len()` and
/// `bytes[i..i + 3] == [0, 0, 1]`.
///
/// **This is the oracle.** It stays in the tree, it is what every test
/// compares against, and it is the arm every target but an ESP32-S3 takes.
///
/// On an ESP32-S3 with `pie-s3` on, nothing calls it and the compiler says
/// so. That is correct and it stays anyway: the scalar version of a twinned
/// kernel is never deleted, because it is the definition the twin is gated
/// against, and a target where it happens to be unreachable is not a reason
/// to lose it.
#[cfg_attr(all(feature = "pie-s3", target_arch = "xtensa"), allow(dead_code))]
fn scan3(bytes: &[u8]) -> Option<usize> {
    if bytes.len() < 3 {
        return None;
    }
    (0..bytes.len() - 2).find(|&i| bytes[i] == 0 && bytes[i + 1] == 0 && bytes[i + 2] == 1)
}

/// The ESP32-S3 vector twin of [`scan3`], or [`scan3`] itself everywhere else.
///
/// A start code must BEGIN with a zero byte, so the twin tests sixteen bytes
/// at a time for "does this block contain any zero at all" and walks only the
/// blocks that do. On a real Annex-B stream almost no block does, which is
/// why this is the largest single win in the family: **−93.4%, 15.1x**.
///
/// Gated byte-identical against [`scan3`] on hardware, and `cfg`-switched on
/// the TARGET rather than the feature — a host build with `pie-s3` on still
/// runs the oracle, so the tests keep meaning what they say.
#[cfg(feature = "pie-s3")]
fn first_code3(bytes: &[u8]) -> Option<usize> {
    #[cfg(target_arch = "xtensa")]
    {
        rusty_esp_dsp_esp::pie_s3::find_start_code3(bytes)
    }
    #[cfg(not(target_arch = "xtensa"))]
    {
        scan3(bytes)
    }
}

/// The `pie-s3`-off twin of [`first_code3`]: the oracle, and nothing else.
#[cfg(not(feature = "pie-s3"))]
fn first_code3(bytes: &[u8]) -> Option<usize> {
    scan3(bytes)
}

/// Position of the next `00 00 01` in `bytes`: the index where the start code
/// (including any leading zero bytes) begins, and the index just after it.
///
/// The scan itself is [`first_code3`]; backing up over the leading zeros is
/// scalar either way, because it walks backwards a byte or two and a vector
/// load cannot help it.
fn find_start_code(bytes: &[u8]) -> Option<(usize, usize)> {
    let i = first_code3(bytes)?;
    let mut begin = i;
    while begin > 0 && bytes[begin - 1] == 0 {
        begin -= 1;
    }
    Some((begin, i + 3))
}

/// Iterate the NAL units of an Annex-B byte stream, start codes stripped.
pub fn nal_units(stream: &[u8]) -> impl Iterator<Item = &[u8]> {
    nal_spans(stream).map(move |s| &stream[s.nal_start..s.nal_end])
}

/// Where a NAL unit sits in its stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NalSpan {
    /// Offset of the first byte of the start code (leading zeros included).
    pub start_code: usize,
    /// Offset of the NAL header byte.
    pub nal_start: usize,
    /// Offset just past the NAL unit.
    pub nal_end: usize,
}

/// Iterate NAL unit positions.
#[must_use]
pub fn nal_spans(stream: &[u8]) -> NalSpans<'_> {
    NalSpans {
        stream,
        pos: 0,
        started: false,
    }
}

/// Iterator returned by [`nal_spans`].
#[derive(Debug, Clone)]
pub struct NalSpans<'a> {
    stream: &'a [u8],
    pos: usize,
    started: bool,
}

impl Iterator for NalSpans<'_> {
    type Item = NalSpan;

    fn next(&mut self) -> Option<NalSpan> {
        loop {
            if !self.started {
                let (_, after) = find_start_code(&self.stream[self.pos..])?;
                self.pos += after;
                self.started = true;
            }
            if self.pos >= self.stream.len() {
                return None;
            }
            let nal_start = self.pos;
            let (nal_end, next_pos) = match find_start_code(&self.stream[self.pos..]) {
                Some((begin, after)) => (self.pos + begin, self.pos + after),
                None => (self.stream.len(), self.stream.len()),
            };
            // The start code that introduced this NAL begins at the first of
            // the zero bytes before the `01`.
            let mut sc = nal_start - 1; // the 0x01
            while sc > 0 && self.stream[sc - 1] == 0 {
                sc -= 1;
            }
            self.pos = next_pos;
            if nal_end > nal_start {
                return Some(NalSpan {
                    start_code: sc,
                    nal_start,
                    nal_end,
                });
            }
        }
    }
}

/// The type of a NAL unit (its first byte's low five bits).
#[must_use]
pub fn nal_unit_type(nal: &[u8]) -> Option<u8> {
    nal.first().map(|b| b & 0x1F)
}

/// True for a coded slice (VCL) NAL unit.
#[must_use]
pub fn is_vcl(nal: &[u8]) -> bool {
    matches!(nal_unit_type(nal), Some(1..=5))
}

/// True for a coded slice whose `first_mb_in_slice` is 0 — the first slice of
/// a picture, and therefore the start of an access unit.
#[must_use]
pub fn is_first_slice(nal: &[u8]) -> bool {
    // first_mb_in_slice is ue(v) and the first syntax element; the value 0 is
    // coded as a single '1' bit.
    is_vcl(nal) && nal.get(1).is_some_and(|b| b & 0x80 != 0)
}

/// True when the access unit contains an IDR slice.
#[must_use]
pub fn contains_idr(stream: &[u8]) -> bool {
    nal_units(stream).any(|n| nal_unit_type(n) == Some(nal_type::IDR))
}

/// True when the access unit already begins with an access unit delimiter.
#[must_use]
pub fn starts_with_aud(stream: &[u8]) -> bool {
    nal_units(stream)
        .next()
        .and_then(nal_unit_type)
        .is_some_and(|t| t == nal_type::AUD)
}

/// Split an Annex-B stream holding several pictures into access units.
///
/// A new access unit begins at the first slice of a picture; parameter sets,
/// SEI and delimiters immediately before that slice belong to it. Each item
/// is a sub-slice of `stream` with its start codes intact, so it can be fed
/// to a packetizer or the mux as-is.
#[must_use]
pub fn access_units(stream: &[u8]) -> AccessUnits<'_> {
    AccessUnits {
        stream,
        spans: nal_spans(stream),
        au_start: None,
        au_has_vcl: false,
        boundary_candidate: None,
        done: false,
    }
}

/// Iterator returned by [`access_units`].
#[derive(Debug, Clone)]
pub struct AccessUnits<'a> {
    stream: &'a [u8],
    spans: NalSpans<'a>,
    au_start: Option<usize>,
    au_has_vcl: bool,
    boundary_candidate: Option<usize>,
    done: bool,
}

impl<'a> Iterator for AccessUnits<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        if self.done {
            return None;
        }
        for span in self.spans.by_ref() {
            let nal = &self.stream[span.nal_start..span.nal_end];
            let start = *self.au_start.get_or_insert(span.start_code);
            if is_vcl(nal) {
                if is_first_slice(nal) && self.au_has_vcl {
                    let boundary = self.boundary_candidate.unwrap_or(span.start_code);
                    let au = &self.stream[start..boundary];
                    self.au_start = Some(boundary);
                    self.au_has_vcl = true;
                    self.boundary_candidate = None;
                    return Some(au);
                }
                self.au_has_vcl = true;
                self.boundary_candidate = None;
            } else if self.au_has_vcl && self.boundary_candidate.is_none() {
                self.boundary_candidate = Some(span.start_code);
            }
        }
        self.done = true;
        let start = self.au_start?;
        let au = &self.stream[start..];
        if au.is_empty() { None } else { Some(au) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_on_three_and_four_byte_start_codes() {
        let s = [
            0, 0, 0, 1, 0x67, 0xAA, // SPS (4-byte start)
            0, 0, 1, 0x68, 0xBB, 0xCC, // PPS (3-byte start)
            0, 0, 0, 1, 0x65, 0x88, 2, 3, // IDR, first_mb_in_slice = 0
        ];
        let nals: std::vec::Vec<&[u8]> = nal_units(&s).collect();
        assert_eq!(nals.len(), 3);
        assert_eq!(nals[0], &[0x67, 0xAA]);
        assert_eq!(nals[1], &[0x68, 0xBB, 0xCC]);
        assert_eq!(nals[2], &[0x65, 0x88, 2, 3]);
        assert_eq!(nal_unit_type(nals[2]), Some(nal_type::IDR));
        assert!(contains_idr(&s));
        assert!(!starts_with_aud(&s));
        let spans: std::vec::Vec<NalSpan> = nal_spans(&s).collect();
        assert_eq!(spans[0].start_code, 0);
        assert_eq!(spans[1].start_code, 6);
        assert_eq!(spans[2].start_code, 12);
        let mut with_aud = AUD_NAL.to_vec();
        with_aud.extend_from_slice(&s);
        assert!(starts_with_aud(&with_aud));
        assert_eq!(nal_units(&with_aud).count(), 4);
        assert_eq!(nal_units(&[1, 2, 3]).count(), 0);
        assert_eq!(nal_units(&[]).count(), 0);
    }

    #[test]
    fn access_units_group_parameter_sets_with_their_picture() {
        let sps = [0u8, 0, 0, 1, 0x67, 0xAA];
        let pps = [0u8, 0, 0, 1, 0x68, 0xBB];
        let idr = [0u8, 0, 0, 1, 0x65, 0x88, 1, 2]; // first slice
        let p_first = [0u8, 0, 0, 1, 0x41, 0x9A, 3]; // first slice of a P picture
        let p_second = [0u8, 0, 0, 1, 0x41, 0x4A, 4]; // a second slice of the same picture (first_mb != 0)
        let sei = [0u8, 0, 0, 1, 0x06, 0x05, 0x00];
        let mut s = std::vec::Vec::new();
        for part in [&sps[..], &pps, &idr, &p_first, &p_second, &sei, &p_first] {
            s.extend_from_slice(part);
        }
        let aus: std::vec::Vec<&[u8]> = access_units(&s).collect();
        assert_eq!(aus.len(), 3);
        let mut au0 = sps.to_vec();
        au0.extend_from_slice(&pps);
        au0.extend_from_slice(&idr);
        assert_eq!(aus[0], au0);
        let mut au1 = p_first.to_vec();
        au1.extend_from_slice(&p_second);
        assert_eq!(aus[1], au1, "a non-first slice stays with its picture");
        let mut au2 = sei.to_vec();
        au2.extend_from_slice(&p_first);
        assert_eq!(
            aus[2], au2,
            "SEI before a first slice belongs to that picture"
        );
        assert_eq!(access_units(&[]).count(), 0);
        assert_eq!(access_units(&idr).count(), 1);
    }

    /// `find_start_code` exactly as it was written before the scan was split
    /// out into [`scan3`] and a vector twin. This is the definition the whole
    /// module was built and tested against, kept here so the split is gated
    /// by the thing it replaced rather than by a restatement of itself.
    fn find_start_code_as_written(bytes: &[u8]) -> Option<(usize, usize)> {
        let mut i = 0;
        while i + 2 < bytes.len() {
            if bytes[i] == 0 && bytes[i + 1] == 0 && bytes[i + 2] == 1 {
                let mut begin = i;
                while begin > 0 && bytes[begin - 1] == 0 {
                    begin -= 1;
                }
                return Some((begin, i + 3));
            }
            i += 1;
        }
        None
    }

    /// xorshift64*, so a failure is reproducible from its seed.
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            x.wrapping_mul(0x2545_f491_4f6c_dd1d)
        }
        /// A tiny alphabet, so start codes, four-byte start codes and
        /// near-misses occur by chance instead of being planted.
        fn byte(&mut self) -> u8 {
            match self.next() % 10 {
                0..=5 => 0,
                6 | 7 => 1,
                8 => 2,
                _ => (self.next() >> 33) as u8,
            }
        }
    }

    #[test]
    fn the_split_scan_matches_the_scan_it_replaced() {
        let mut rng = Rng(0x0B3_A17E_2026);
        let mut checked = 0usize;
        for len in 0..260usize {
            for _ in 0..12 {
                let buf: Vec<u8> = (0..len).map(|_| rng.byte()).collect();
                assert_eq!(
                    find_start_code(&buf),
                    find_start_code_as_written(&buf),
                    "len={len} buf={buf:?}"
                );
                // And at every offset into it, which is what `NalSpans` does:
                // `pos` lands wherever the previous NAL ended, so the slice
                // handed to the scan has an arbitrary base.
                for off in (0..len).step_by(7) {
                    assert_eq!(
                        find_start_code(&buf[off..]),
                        find_start_code_as_written(&buf[off..]),
                        "len={len} off={off}"
                    );
                    checked += 1;
                }
                checked += 1;
            }
        }
        assert!(checked > 50_000, "the sweep did not run: {checked}");
    }

    #[test]
    fn scan3_finds_the_three_byte_code_inside_a_four_byte_one() {
        // `00 00 00 01` carries its `00 00 01` at index 1; `find_start_code`
        // is what backs up over the leading zero to report the code's start.
        assert_eq!(scan3(&[0, 0, 0, 1, 9]), Some(1));
        assert_eq!(find_start_code(&[0, 0, 0, 1, 9]), Some((0, 4)));
        assert_eq!(find_start_code(&[0, 0, 1, 9]), Some((0, 3)));
        assert_eq!(find_start_code(&[0xFF, 0, 0, 0, 0, 1, 9]), Some((1, 6)));
        assert_eq!(scan3(&[]), None);
        assert_eq!(scan3(&[0, 0]), None);
        assert_eq!(scan3(&[0, 0, 2]), None);
    }
}
