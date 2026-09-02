//! Where bytes go.
//!
//! A [`PacketSink`] is the one method every packetizer writes through: a TCP
//! socket, a UDP socket wrapper, a QUIC stream, a file on the host, or a
//! buffer in a test. Packetizers call it with MTU-sized pieces and never hold
//! an intermediate `Vec`.

use rusty_esp_core::error::{Error, Result};

/// Something that accepts bytes in order.
pub trait PacketSink {
    /// Write all of `bytes`.
    fn write(&mut self, bytes: &[u8]) -> Result<()>;
}

/// A sink over a caller-owned slice; refuses to overflow it.
#[derive(Debug)]
pub struct SliceSink<'a> {
    buf: &'a mut [u8],
    len: usize,
}

impl<'a> SliceSink<'a> {
    /// A sink writing into `buf` from the start.
    pub fn new(buf: &'a mut [u8]) -> Self {
        SliceSink { buf, len: 0 }
    }

    /// Bytes written so far.
    #[must_use]
    pub fn len(&self) -> usize {
        self.len
    }

    /// True when nothing has been written.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The bytes written so far.
    #[must_use]
    pub fn written(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    /// Forget everything written.
    pub fn clear(&mut self) {
        self.len = 0;
    }
}

impl PacketSink for SliceSink<'_> {
    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        let end = self.len + bytes.len();
        if end > self.buf.len() {
            return Err(Error::BufferTooSmall { needed: end });
        }
        self.buf[self.len..end].copy_from_slice(bytes);
        self.len = end;
        Ok(())
    }
}

/// A sink that only counts.
#[derive(Debug, Default, Clone, Copy)]
pub struct CountingSink {
    /// Bytes seen.
    pub bytes: u64,
    /// Calls seen.
    pub writes: u64,
}

impl PacketSink for CountingSink {
    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.bytes += bytes.len() as u64;
        self.writes += 1;
        Ok(())
    }
}

#[cfg(feature = "alloc")]
impl PacketSink for alloc::vec::Vec<u8> {
    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        self.extend_from_slice(bytes);
        Ok(())
    }
}

impl<S: PacketSink + ?Sized> PacketSink for &mut S {
    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        (**self).write(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slice_sink_refuses_overflow() {
        let mut buf = [0u8; 4];
        let mut s = SliceSink::new(&mut buf);
        s.write(&[1, 2]).unwrap();
        assert_eq!(
            s.write(&[3, 4, 5]),
            Err(Error::BufferTooSmall { needed: 5 })
        );
        s.write(&[3, 4]).unwrap();
        assert_eq!(s.written(), &[1, 2, 3, 4]);
        s.clear();
        assert!(s.is_empty());
    }

    #[test]
    fn counting_and_vec() {
        let mut c = CountingSink::default();
        c.write(&[0; 10]).unwrap();
        c.write(&[0; 5]).unwrap();
        assert_eq!((c.bytes, c.writes), (15, 2));
        let mut v = alloc::vec::Vec::new();
        (&mut v).write(b"ab").unwrap();
        v.write(b"c").unwrap();
        assert_eq!(v, b"abc");
    }
}
