//! Wire protocol: length-prefixed binary frames over Unix stream sockets.
//!
//! Frame: `[u32 LE len][u8 type][payload]` where `len` counts type + payload.
//! Decoders never panic on malformed input; they return `Err(Bad)`.

use std::io::{self, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::os::unix::net::UnixStream;

pub const VERSION: u32 = 1;
pub const MAX_FRAME: usize = 64 << 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Bad;

impl std::fmt::Display for Bad {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("malformed message")
    }
}
impl std::error::Error for Bad {}

#[derive(Default)]
pub struct Enc {
    pub buf: Vec<u8>,
}

impl Enc {
    pub fn new() -> Enc {
        Enc { buf: Vec::with_capacity(64) }
    }
    pub fn u8(&mut self, v: u8) -> &mut Self {
        self.buf.push(v);
        self
    }
    pub fn u16(&mut self, v: u16) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn u32(&mut self, v: u32) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn i32(&mut self, v: i32) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn u64(&mut self, v: u64) -> &mut Self {
        self.buf.extend_from_slice(&v.to_le_bytes());
        self
    }
    pub fn var(&mut self, mut v: u64) -> &mut Self {
        while v >= 0x80 {
            self.buf.push((v as u8) | 0x80);
            v >>= 7;
        }
        self.buf.push(v as u8);
        self
    }
    pub fn bytes(&mut self, b: &[u8]) -> &mut Self {
        self.var(b.len() as u64);
        self.buf.extend_from_slice(b);
        self
    }
    pub fn str(&mut self, s: &str) -> &mut Self {
        self.bytes(s.as_bytes())
    }
    pub fn strs(&mut self, v: &[String]) -> &mut Self {
        self.var(v.len() as u64);
        for s in v {
            self.str(s);
        }
        self
    }
}

pub struct Dec<'a> {
    b: &'a [u8],
    pos: usize,
}

#[allow(clippy::len_without_is_empty)]
impl<'a> Dec<'a> {
    pub fn new(b: &'a [u8]) -> Dec<'a> {
        Dec { b, pos: 0 }
    }
    fn take(&mut self, n: usize) -> Result<&'a [u8], Bad> {
        let end = self.pos.checked_add(n).ok_or(Bad)?;
        let s = self.b.get(self.pos..end).ok_or(Bad)?;
        self.pos = end;
        Ok(s)
    }
    pub fn u8(&mut self) -> Result<u8, Bad> {
        Ok(self.take(1)?[0])
    }
    pub fn u16(&mut self) -> Result<u16, Bad> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().map_err(|_| Bad)?))
    }
    pub fn u32(&mut self) -> Result<u32, Bad> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().map_err(|_| Bad)?))
    }
    pub fn i32(&mut self) -> Result<i32, Bad> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().map_err(|_| Bad)?))
    }
    pub fn u64(&mut self) -> Result<u64, Bad> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().map_err(|_| Bad)?))
    }
    pub fn var(&mut self) -> Result<u64, Bad> {
        let mut v = 0u64;
        let mut shift = 0;
        loop {
            let b = self.u8()?;
            v |= ((b & 0x7f) as u64) << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
            shift += 7;
            if shift > 63 {
                return Err(Bad);
            }
        }
    }
    pub fn len(&mut self) -> Result<usize, Bad> {
        let n = self.var()? as usize;
        if n > MAX_FRAME {
            return Err(Bad);
        }
        Ok(n)
    }
    pub fn bytes(&mut self) -> Result<&'a [u8], Bad> {
        let n = self.len()?;
        self.take(n)
    }
    pub fn str(&mut self) -> Result<&'a str, Bad> {
        std::str::from_utf8(self.bytes()?).map_err(|_| Bad)
    }
    pub fn string(&mut self) -> Result<String, Bad> {
        Ok(self.str()?.to_string())
    }
    pub fn strs(&mut self) -> Result<Vec<String>, Bad> {
        let n = self.len()?;
        let mut v = Vec::with_capacity(n.min(1024));
        for _ in 0..n {
            v.push(self.string()?);
        }
        Ok(v)
    }
    pub fn rest(&mut self) -> &'a [u8] {
        let s = &self.b[self.pos.min(self.b.len())..];
        self.pos = self.b.len();
        s
    }
    pub fn remaining(&self) -> usize {
        self.b.len().saturating_sub(self.pos)
    }
}

/// Non-blocking framed connection with read/write buffers.
pub struct Conn {
    pub stream: UnixStream,
    rbuf: Vec<u8>,
    rpos: usize,
    wbuf: Vec<u8>,
    wpos: usize,
    pub eof: bool,
}

#[allow(clippy::should_implement_trait)]
impl Conn {
    /// The process at the other end of the socket.
    pub fn peer_pid(&self) -> Option<u32> {
        rustix::net::sockopt::socket_peercred(&self.stream).ok().map(|c| c.pid.as_raw_nonzero().get() as u32)
    }

    pub fn new(stream: UnixStream) -> io::Result<Conn> {
        stream.set_nonblocking(true)?;
        Ok(Conn { stream, rbuf: Vec::new(), rpos: 0, wbuf: Vec::new(), wpos: 0, eof: false })
    }

    pub fn fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }

    /// Read what is available (bounded per call). Sets `eof` on hangup.
    pub fn fill(&mut self) -> io::Result<usize> {
        if self.rpos > 0 && self.rpos == self.rbuf.len() {
            self.rbuf.clear();
            self.rpos = 0;
        } else if self.rpos > 1 << 20 {
            self.rbuf.drain(..self.rpos);
            self.rpos = 0;
        }
        let mut total = 0;
        loop {
            // Read straight into the buffer's spare capacity (no bounce
            // buffer, nothing to zero).
            self.rbuf.reserve(16 * 1024);
            match rustix::io::read(&self.stream, rustix::buffer::spare_capacity(&mut self.rbuf)) {
                Ok(0) => {
                    self.eof = true;
                    return Ok(total);
                }
                Ok(n) => {
                    total += n;
                    if total >= 4 << 20 {
                        return Ok(total);
                    }
                }
                Err(rustix::io::Errno::AGAIN) => return Ok(total),
                Err(rustix::io::Errno::INTR) => continue,
                Err(e) => {
                    self.eof = true;
                    return Err(e.into());
                }
            }
        }
    }

    /// Next complete frame, if any.
    pub fn next(&mut self) -> Result<Option<(u8, Vec<u8>)>, Bad> {
        let avail = &self.rbuf[self.rpos..];
        if avail.len() < 5 {
            return Ok(None);
        }
        let len = u32::from_le_bytes(avail[..4].try_into().map_err(|_| Bad)?) as usize;
        if len == 0 || len > MAX_FRAME {
            return Err(Bad);
        }
        if avail.len() < 4 + len {
            return Ok(None);
        }
        let ty = avail[4];
        let payload = avail[5..4 + len].to_vec();
        self.rpos += 4 + len;
        Ok(Some((ty, payload)))
    }

    pub fn send(&mut self, ty: u8, payload: &[u8]) {
        let len = (payload.len() + 1) as u32;
        self.wbuf.extend_from_slice(&len.to_le_bytes());
        self.wbuf.push(ty);
        self.wbuf.extend_from_slice(payload);
    }

    pub fn send_enc(&mut self, ty: u8, e: &Enc) {
        self.send(ty, &e.buf)
    }

    pub fn pending_out(&self) -> usize {
        self.wbuf.len() - self.wpos
    }

    pub fn flush(&mut self) -> io::Result<()> {
        while self.wpos < self.wbuf.len() {
            match self.stream.write(&self.wbuf[self.wpos..]) {
                Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "write zero")),
                Ok(n) => self.wpos += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        if self.wpos == self.wbuf.len() {
            self.wbuf.clear();
            self.wpos = 0;
        } else if self.wpos > 1 << 20 {
            self.wbuf.drain(..self.wpos);
            self.wpos = 0;
        }
        Ok(())
    }

    /// Blocking flush (used by short-lived CLI clients).
    pub fn flush_blocking(&mut self) -> io::Result<()> {
        self.stream.set_nonblocking(false)?;
        let r = self.stream.write_all(&self.wbuf[self.wpos..]);
        self.wbuf.clear();
        self.wpos = 0;
        self.stream.set_nonblocking(true)?;
        r
    }
}

// Client <-> server message types.
pub mod c2s {
    pub const HELLO: u8 = 1;
    pub const CMD: u8 = 2;
    pub const INPUT: u8 = 3;
    pub const RESIZE: u8 = 4;
    pub const DETACH: u8 = 5;
    pub const COLORS: u8 = 6;
}
pub mod s2c {
    pub const OUTPUT: u8 = 64;
    pub const STDOUT: u8 = 65;
    pub const STDERR: u8 = 66;
    pub const EXIT: u8 = 67;
    pub const ATTACHED: u8 = 68;
    pub const SUSPEND: u8 = 69;
}
// Server <-> keeper message types.
pub mod s2k {
    pub const HELLO: u8 = 1;
    pub const INPUT: u8 = 2;
    pub const RESIZE: u8 = 3;
    pub const ACK: u8 = 4;
    pub const REDRAW: u8 = 5;
    pub const CAPTURE: u8 = 6;
    pub const COPY: u8 = 7;
    pub const SIGNAL: u8 = 8;
    pub const CLOSE: u8 = 9;
    pub const SNAPSHOT: u8 = 10;
    pub const SETOPT: u8 = 11;
    pub const CLEAR_HISTORY: u8 = 12;
    pub const PANIC_TEST: u8 = 13;
    pub const ECHO: u8 = 14;
}
pub mod k2s {
    pub const INFO: u8 = 64;
    pub const FRAME: u8 = 65;
    pub const EVENT: u8 = 66;
    pub const EXITED: u8 = 67;
    pub const CAPTURED: u8 = 68;
    pub const COPIED: u8 = 69;
    pub const PROC: u8 = 70;
    pub const MODE: u8 = 71;
    pub const IMAGES: u8 = 72;
}

/// Event kinds inside `k2s::EVENT`.
pub mod ev {
    pub const BELL: u8 = 1;
    pub const TITLE: u8 = 2;
    pub const CWD: u8 = 3;
    pub const NOTIFY: u8 = 4;
    pub const PROGRESS: u8 = 5;
    pub const CLIPBOARD: u8 = 6;
    pub const PROMPT: u8 = 7;
    pub const RECOVERED: u8 = 8;
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roundtrip() {
        let mut e = Enc::new();
        e.u8(7).u16(300).u32(70000).i32(-5).u64(1 << 40).var(123456).str("héllo").strs(&["a".into(), "bc".into()]);
        let mut d = Dec::new(&e.buf);
        assert_eq!(d.u8().unwrap(), 7);
        assert_eq!(d.u16().unwrap(), 300);
        assert_eq!(d.u32().unwrap(), 70000);
        assert_eq!(d.i32().unwrap(), -5);
        assert_eq!(d.u64().unwrap(), 1 << 40);
        assert_eq!(d.var().unwrap(), 123456);
        assert_eq!(d.str().unwrap(), "héllo");
        assert_eq!(d.strs().unwrap(), vec!["a".to_string(), "bc".to_string()]);
        assert!(d.u8().is_err());
    }
    #[test]
    fn malformed_never_panics() {
        let mut d = Dec::new(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01]);
        assert!(d.bytes().is_err());
        let mut d = Dec::new(&[5, b'a']);
        assert!(d.str().is_err());
    }
    #[test]
    fn framing() {
        let (a, b) = UnixStream::pair().unwrap();
        let mut ca = Conn::new(a).unwrap();
        let mut cb = Conn::new(b).unwrap();
        ca.send(3, b"hello");
        ca.send(4, b"");
        ca.flush().unwrap();
        cb.fill().unwrap();
        assert_eq!(cb.next().unwrap(), Some((3, b"hello".to_vec())));
        assert_eq!(cb.next().unwrap(), Some((4, vec![])));
        assert_eq!(cb.next().unwrap(), None);
    }
}
