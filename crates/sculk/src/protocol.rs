//! Wire primitives: VarInts, packet framing, compression, and a bounds-checked
//! reader for inbound packets.

use crate::deflate::Deflater;
use crate::Result;
use flate2::{Decompress, FlushDecompress, Status};
use std::cell::RefCell;
use std::io;
use std::ops::Deref;

pub const PROTOCOL_VERSION: i32 = 775;
pub const CLIENT_VERSION: &str = "26.1.2";
pub const COMPRESSION_THRESHOLD: usize = 2048;

/// Largest frame accepted from a client. Every serverbound packet Sculk
/// handles is far smaller, so this bounds per-connection buffering tightly.
pub const MAX_INBOUND_FRAME: usize = 32 * 1024;
/// Largest decompressed inbound payload accepted from a client.
pub const MAX_INBOUND_PAYLOAD: usize = 32 * 1024;

/// Space reserved in front of every payload: frame length (3 bytes max for
/// frames below 2 MiB) plus the uncompressed-length prefix (3 bytes max).
const HEADER_RESERVE: usize = 6;

thread_local! {
    static DEFLATE: RefCell<Deflater> = RefCell::new(Deflater::new(1));
    static INFLATE: RefCell<Decompress> = RefCell::new(Decompress::new(true));
}

/// A finished, framed packet. The header is written in place in front of the
/// payload, so building a frame never shifts the payload bytes.
pub struct Frame {
    buf: Vec<u8>,
    start: usize,
}

impl Frame {
    /// Wraps bytes that are already framed (for example several frames
    /// concatenated into one buffer).
    pub fn from_vec(buf: Vec<u8>) -> Self {
        Self { buf, start: 0 }
    }

    pub fn into_vec(mut self) -> Vec<u8> {
        if self.start > 0 {
            self.buf.drain(..self.start);
        }
        self.buf
    }
}

impl Deref for Frame {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        &self.buf[self.start..]
    }
}

/// Builds one packet payload (id + fields).
pub struct PacketWriter {
    data: Vec<u8>,
}

impl PacketWriter {
    pub fn new(id: i32) -> Self {
        Self::with_capacity(id, 48)
    }

    pub fn with_capacity(id: i32, capacity: usize) -> Self {
        let mut data = Vec::with_capacity(HEADER_RESERVE + capacity);
        data.resize(HEADER_RESERVE, 0);
        write_var_i32(&mut data, id);
        Self { data }
    }

    /// Raw access to the payload buffer, for encoders that append directly.
    pub fn buffer(&mut self) -> &mut Vec<u8> {
        &mut self.data
    }

    pub fn finish(self, compression: bool) -> Frame {
        let payload_len = self.data.len() - HEADER_RESERVE;
        if compression && payload_len >= COMPRESSION_THRESHOLD {
            return compress_frame(&self.data[HEADER_RESERVE..]);
        }
        let mut buf = self.data;
        let mut header = [0u8; 10];
        let mut len = 0;
        if compression {
            header[len] = 0; // below threshold: data length 0 = uncompressed
            len += 1;
        }
        let body_len = payload_len + len;
        let mut frame_len = [0u8; 5];
        let frame_len_size = encode_var_i32(&mut frame_len, body_len as i32);
        let start = HEADER_RESERVE - len - frame_len_size;
        buf[start..start + frame_len_size].copy_from_slice(&frame_len[..frame_len_size]);
        buf[start + frame_len_size..HEADER_RESERVE].copy_from_slice(&header[..len]);
        Frame { buf, start }
    }

    pub fn into_packet(self, compression: bool) -> Frame {
        self.finish(compression)
    }

    pub fn u8(&mut self, v: u8) {
        self.data.push(v);
    }

    pub fn bool(&mut self, v: bool) {
        self.u8(u8::from(v));
    }

    pub fn i16(&mut self, v: i16) {
        self.data.extend_from_slice(&v.to_be_bytes());
    }

    pub fn i32(&mut self, v: i32) {
        self.data.extend_from_slice(&v.to_be_bytes());
    }

    pub fn i64(&mut self, v: i64) {
        self.data.extend_from_slice(&v.to_be_bytes());
    }

    pub fn u64(&mut self, v: u64) {
        self.data.extend_from_slice(&v.to_be_bytes());
    }

    pub fn f32(&mut self, v: f32) {
        self.data.extend_from_slice(&v.to_bits().to_be_bytes());
    }

    pub fn f64(&mut self, v: f64) {
        self.data.extend_from_slice(&v.to_bits().to_be_bytes());
    }

    pub fn var_i32(&mut self, v: i32) {
        write_var_i32(&mut self.data, v);
    }

    pub fn string(&mut self, s: &str) {
        self.var_i32(s.len() as i32);
        self.data.extend_from_slice(s.as_bytes());
    }

    pub fn bytes(&mut self, bytes: &[u8]) {
        self.data.extend_from_slice(bytes);
    }

    /// JSON text component `{"text": s}` used by the login-phase disconnect.
    pub fn json_text(&mut self, s: &str) {
        let mut json = String::with_capacity(s.len() + 12);
        json.push_str("{\"text\":\"");
        for ch in s.chars() {
            match ch {
                '"' => json.push_str("\\\""),
                '\\' => json.push_str("\\\\"),
                c if (c as u32) < 0x20 => json.push_str(&format!("\\u{:04x}", c as u32)),
                c => json.push(c),
            }
        }
        json.push_str("\"}");
        self.string(&json);
    }

    /// Anonymous NBT compound `{text: s}`, the play-phase text component form.
    /// Strings are written as Java modified UTF-8.
    pub fn nbt_text(&mut self, s: &str) {
        self.u8(0x0a);
        self.u8(0x08);
        self.i16(4);
        self.bytes(b"text");
        let len_at = self.data.len();
        self.i16(0);
        let start = self.data.len();
        write_modified_utf8(&mut self.data, s);
        let len = (self.data.len() - start).min(u16::MAX as usize) as u16;
        self.data[len_at..len_at + 2].copy_from_slice(&len.to_be_bytes());
        self.u8(0);
    }

    pub fn uuid(&mut self, uuid: u128) {
        self.data.extend_from_slice(&uuid.to_be_bytes());
    }

    pub fn position(&mut self, x: i32, y: i32, z: i32) {
        let packed =
            ((x as i64 & 0x3ffffff) << 38) | ((z as i64 & 0x3ffffff) << 12) | (y as i64 & 0xfff);
        self.i64(packed);
    }
}

fn compress_frame(payload: &[u8]) -> Frame {
    DEFLATE.with(|cell| {
        let mut buf = Vec::with_capacity(HEADER_RESERVE + payload.len() / 16 + 256);
        buf.resize(HEADER_RESERVE, 0);
        cell.borrow_mut().compress(payload, &mut buf);
        let mut data_len = [0u8; 5];
        let data_len_size = encode_var_i32(&mut data_len, payload.len() as i32);
        let body_len = buf.len() - HEADER_RESERVE + data_len_size;
        let mut frame_len = [0u8; 5];
        let frame_len_size = encode_var_i32(&mut frame_len, body_len as i32);
        // Compressed frames are often cached; don't keep the oversized buffer.
        buf.shrink_to_fit();
        let start = HEADER_RESERVE - data_len_size - frame_len_size;
        buf[start..start + frame_len_size].copy_from_slice(&frame_len[..frame_len_size]);
        buf[start + frame_len_size..HEADER_RESERVE].copy_from_slice(&data_len[..data_len_size]);
        Frame { buf, start }
    })
}

/// Locates the next complete frame in `buf` starting at `*pos`. Returns the
/// frame body range and advances `*pos` past it, or `None` if incomplete.
pub fn next_frame(buf: &[u8], pos: &mut usize) -> Result<Option<(usize, usize)>> {
    let mut cursor = *pos;
    let len = match read_var_i32_from_slice(buf, &mut cursor) {
        Ok(len) => len,
        Err(_) if buf.len() - *pos < 3 => return Ok(None),
        Err(err) => return Err(err),
    };
    if len < 0 || len as usize > MAX_INBOUND_FRAME {
        return Err(invalid_data("frame length out of range").into());
    }
    let end = cursor + len as usize;
    if buf.len() < end {
        return Ok(None);
    }
    *pos = end;
    Ok(Some((cursor, end)))
}

/// Inflates a compressed inbound payload into `out` with strict size limits.
pub fn inflate_payload(compressed: &[u8], expected_len: usize, out: &mut Vec<u8>) -> Result<()> {
    if !(COMPRESSION_THRESHOLD..=MAX_INBOUND_PAYLOAD).contains(&expected_len) {
        return Err(invalid_data("compressed payload length out of range").into());
    }
    out.clear();
    out.reserve(expected_len);
    INFLATE.with(|cell| {
        let mut inflate = cell.borrow_mut();
        inflate.reset(true);
        let status = inflate
            .decompress_vec(compressed, out, FlushDecompress::Finish)
            .map_err(|_| invalid_data("corrupt compressed payload"))?;
        if status != Status::StreamEnd || out.len() != expected_len {
            return Err(invalid_data("compressed payload length mismatch").into());
        }
        Ok(())
    })
}

fn write_modified_utf8(out: &mut Vec<u8>, s: &str) {
    for ch in s.chars() {
        let c = ch as u32;
        match c {
            0x01..=0x7f => out.push(c as u8),
            0 | 0x80..=0x7ff => {
                out.push(0xc0 | (c >> 6) as u8);
                out.push(0x80 | (c & 0x3f) as u8);
            }
            0x800..=0xffff => {
                out.push(0xe0 | (c >> 12) as u8);
                out.push(0x80 | ((c >> 6) & 0x3f) as u8);
                out.push(0x80 | (c & 0x3f) as u8);
            }
            _ => {
                let v = c - 0x10000;
                for unit in [0xd800 | (v >> 10), 0xdc00 | (v & 0x3ff)] {
                    out.push(0xe0 | (unit >> 12) as u8);
                    out.push(0x80 | ((unit >> 6) & 0x3f) as u8);
                    out.push(0x80 | (unit & 0x3f) as u8);
                }
            }
        }
    }
}

/// Bounds-checked reader over one inbound packet payload.
pub struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub fn bool(&mut self) -> Result<bool> {
        Ok(self.u8()? != 0)
    }

    pub fn i16(&mut self) -> Result<i16> {
        Ok(i16::from_be_bytes(self.array()?))
    }

    pub fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_be_bytes(self.array()?))
    }

    pub fn f32(&mut self) -> Result<f32> {
        Ok(f32::from_bits(u32::from_be_bytes(self.array()?)))
    }

    pub fn f64(&mut self) -> Result<f64> {
        Ok(f64::from_bits(u64::from_be_bytes(self.array()?)))
    }

    pub fn var_i32(&mut self) -> Result<i32> {
        read_var_i32_from_slice(self.data, &mut self.pos)
    }

    /// Reads a string of at most `max_chars` characters, borrowing the packet.
    pub fn str(&mut self, max_chars: usize) -> Result<&'a str> {
        let len = self.var_i32()?;
        if len < 0 || len as usize > max_chars * 3 {
            return Err(invalid_data("string length out of range").into());
        }
        let text = std::str::from_utf8(self.take(len as usize)?)?;
        if text.chars().count() > max_chars {
            return Err(invalid_data("string too long").into());
        }
        Ok(text)
    }

    pub fn position(&mut self) -> Result<(i32, i32, i32)> {
        let val = self.i64()?;
        let x = (val >> 38) as i32;
        let y = ((val << 52) >> 52) as i32;
        let z = ((val << 26) >> 38) as i32;
        Ok((x, y, z))
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        Ok(self.take(N)?.try_into().expect("take returns N bytes"))
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8]> {
        let end = self
            .pos
            .checked_add(len)
            .filter(|end| *end <= self.data.len())
            .ok_or_else(|| invalid_data("packet underflow"))?;
        let out = &self.data[self.pos..end];
        self.pos = end;
        Ok(out)
    }
}

fn encode_var_i32(out: &mut [u8; 5], value: i32) -> usize {
    let mut value = value as u32;
    let mut len = 0;
    loop {
        if value < 0x80 {
            out[len] = value as u8;
            return len + 1;
        }
        out[len] = (value as u8 & 0x7f) | 0x80;
        len += 1;
        value >>= 7;
    }
}

pub fn read_var_i32_from_slice(data: &[u8], pos: &mut usize) -> Result<i32> {
    let mut result = 0u32;
    for shift in (0..35).step_by(7) {
        let byte = *data
            .get(*pos)
            .ok_or_else(|| invalid_data("varint underflow"))?;
        *pos += 1;
        result |= u32::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(result as i32);
        }
    }
    Err(invalid_data("varint too long").into())
}

pub fn write_var_i32(out: &mut Vec<u8>, value: i32) {
    let mut bytes = [0u8; 5];
    let len = encode_var_i32(&mut bytes, value);
    out.extend_from_slice(&bytes[..len]);
}

pub fn var_i32_len(value: i32) -> usize {
    let mut bytes = [0u8; 5];
    encode_var_i32(&mut bytes, value)
}

pub fn valid_name(name: &str) -> bool {
    (1..=16).contains(&name.len()) && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn invalid_data(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::read::ZlibDecoder;
    use std::io::Read;

    fn framed_body(packet: &[u8]) -> &[u8] {
        let mut pos = 0;
        let len = read_var_i32_from_slice(packet, &mut pos).unwrap() as usize;
        assert_eq!(packet.len(), pos + len);
        &packet[pos..]
    }

    #[test]
    fn small_packet_in_compressed_session_is_sent_uncompressed() {
        let mut writer = PacketWriter::new(0x2a);
        writer.string("hello");
        let packet = writer.finish(true);
        let body = framed_body(&packet);
        assert_eq!(body, b"\x00\x2a\x05hello");
    }

    #[test]
    fn small_packet_without_compression_has_no_data_length() {
        let mut writer = PacketWriter::new(0x01);
        writer.u8(9);
        assert_eq!(&*writer.finish(false), b"\x02\x01\x09");
    }

    #[test]
    fn large_packets_compress_and_reuse_the_compressor() {
        for round in 0..3u8 {
            let mut writer = PacketWriter::new(0x2a);
            writer.bytes(&vec![round; COMPRESSION_THRESHOLD + 4096]);
            let packet = writer.finish(true);
            let body = framed_body(&packet);
            let mut pos = 0;
            let uncompressed_len = read_var_i32_from_slice(body, &mut pos).unwrap() as usize;
            let mut decoded = Vec::new();
            ZlibDecoder::new(&body[pos..])
                .read_to_end(&mut decoded)
                .unwrap();
            assert_eq!(decoded.len(), uncompressed_len);
            assert_eq!(decoded[0], 0x2a);
            assert!(decoded[1..].iter().all(|byte| *byte == round));
            let mut inflated = Vec::new();
            inflate_payload(&body[pos..], uncompressed_len, &mut inflated).unwrap();
            assert_eq!(inflated, decoded);
        }
    }

    #[test]
    fn inflate_rejects_oversized_or_lying_lengths() {
        let mut writer = PacketWriter::new(1);
        writer.bytes(&[0; COMPRESSION_THRESHOLD * 2]);
        let packet = writer.finish(true);
        let body = framed_body(&packet);
        let mut pos = 0;
        let len = read_var_i32_from_slice(body, &mut pos).unwrap() as usize;
        let mut out = Vec::new();
        assert!(inflate_payload(&body[pos..], len + 1, &mut out).is_err());
        assert!(inflate_payload(&body[pos..], MAX_INBOUND_PAYLOAD + 1, &mut out).is_err());
        assert!(inflate_payload(&body[pos..], 10, &mut out).is_err());
    }

    #[test]
    fn varint_roundtrips_boundaries_and_random_values() {
        let mut values = vec![i32::MIN, -1, 0, 1, 127, 128, 255, 16_384, i32::MAX];
        let mut state = 0x243f_6a88_85a3_08d3u64;
        for _ in 0..100_000 {
            state = state.wrapping_mul(0x9e37_79b9_7f4a_7c15).rotate_left(29) ^ 0x5851_f42d;
            values.push(state as i32);
        }
        for expected in values {
            let mut encoded = Vec::new();
            write_var_i32(&mut encoded, expected);
            assert_eq!(encoded.len(), var_i32_len(expected));
            let mut pos = 0;
            assert_eq!(
                read_var_i32_from_slice(&encoded, &mut pos).unwrap(),
                expected
            );
            assert_eq!(pos, encoded.len());
        }
    }

    #[test]
    fn frame_scanner_handles_arbitrary_input_without_panicking() {
        let mut state = 0x1357_9bdf_2468_ace0u64;
        for len in 0..=96 {
            for _ in 0..256 {
                let input: Vec<u8> = (0..len)
                    .map(|_| {
                        state = state.wrapping_mul(0x2545_f491_4f6c_dd1d).wrapping_add(7);
                        (state >> 40) as u8
                    })
                    .collect();
                let mut pos = 0;
                while let Ok(Some((start, end))) = next_frame(&input, &mut pos) {
                    assert!(start <= end && end <= input.len());
                }
            }
        }
    }

    #[test]
    fn frame_scanner_rejects_huge_lengths() {
        let mut buf = Vec::new();
        write_var_i32(&mut buf, MAX_INBOUND_FRAME as i32 + 1);
        assert!(next_frame(&buf, &mut 0).is_err());
    }

    #[test]
    fn packed_positions_roundtrip() {
        for (x, y, z) in [
            (0, 0, 0),
            (-1, -64, -1),
            (33_554_431, 2047, -33_554_432),
            (12, -2048, 99),
        ] {
            let mut writer = PacketWriter::new(0);
            writer.position(x, y, z);
            let packet = writer.finish(false);
            let mut cursor = Cursor::new(framed_body(&packet));
            assert_eq!(cursor.var_i32().unwrap(), 0);
            assert_eq!(cursor.position().unwrap(), (x, y, z));
        }
    }

    #[test]
    fn nbt_text_uses_modified_utf8() {
        let mut writer = PacketWriter::new(7);
        writer.nbt_text("a\u{0}é😀");
        let packet = writer.finish(false);
        let body = framed_body(&packet);
        // id, compound tag, string tag, name length, "text", value length
        assert_eq!(&body[..9], b"\x07\x0a\x08\x00\x04text");
        let text_len = u16::from_be_bytes([body[9], body[10]]) as usize;
        assert_eq!(
            &body[11..11 + text_len],
            b"a\xc0\x80\xc3\xa9\xed\xa0\xbd\xed\xb8\x80"
        );
        assert_eq!(body[11 + text_len], 0);
    }
}
