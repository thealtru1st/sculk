//! A small-footprint zlib compressor.
//!
//! `flate2` always configures zlib with its default memory level, which costs
//! about 256 KiB per compressor. Sculk compresses everything on one thread
//! with one reusable stream, configured with a 4 KiB window and memory level
//! 4 (about 30 KiB in total). Chunk data is highly local, so the ratio barely
//! changes.

use libz_sys as z;
use std::alloc::{alloc, dealloc, Layout};
use std::os::raw::{c_int, c_uint, c_void};
use std::ptr;

const WINDOW_BITS: c_int = 12;
const MEM_LEVEL: c_int = 4;
const HEADER: usize = 16;

pub struct Deflater {
    stream: Box<z::z_stream>,
}

impl Deflater {
    /// Creates a zlib-format compressor at `level` (0–9).
    pub fn new(level: c_int) -> Self {
        let mut stream = Box::new(z::z_stream {
            next_in: ptr::null_mut(),
            avail_in: 0,
            total_in: 0,
            next_out: ptr::null_mut(),
            avail_out: 0,
            total_out: 0,
            msg: ptr::null_mut(),
            state: ptr::null_mut(),
            zalloc,
            zfree,
            opaque: ptr::null_mut(),
            data_type: 0,
            adler: 0,
            reserved: 0,
        });
        // SAFETY: `stream` is a fully initialized z_stream with valid allocator
        // callbacks, and the version/size arguments describe the linked zlib.
        let status = unsafe {
            z::deflateInit2_(
                &mut *stream,
                level,
                z::Z_DEFLATED,
                WINDOW_BITS,
                MEM_LEVEL,
                z::Z_DEFAULT_STRATEGY,
                z::zlibVersion(),
                std::mem::size_of::<z::z_stream>() as c_int,
            )
        };
        assert_eq!(status, z::Z_OK, "zlib deflateInit2 failed");
        Self { stream }
    }

    /// Appends one complete zlib stream holding `input` to `out`.
    pub fn compress(&mut self, input: &[u8], out: &mut Vec<u8>) {
        let stream = &mut *self.stream;
        // SAFETY: the stream was initialized by deflateInit2 and is not in use.
        unsafe { z::deflateReset(stream) };
        stream.next_in = input.as_ptr() as *mut u8;
        stream.avail_in = input.len() as c_uint;
        loop {
            out.reserve((input.len() / 8).max(256));
            let spare = (out.capacity() - out.len()).min(c_uint::MAX as usize);
            // SAFETY: next_out points at `spare` writable bytes of `out`'s
            // spare capacity; zlib writes at most avail_out bytes there, and we
            // only extend the length by what it reports as written.
            let status = unsafe {
                stream.next_out = out.as_mut_ptr().add(out.len());
                stream.avail_out = spare as c_uint;
                let status = z::deflate(stream, z::Z_FINISH);
                let written = spare - stream.avail_out as usize;
                out.set_len(out.len() + written);
                status
            };
            match status {
                z::Z_STREAM_END => break,
                z::Z_OK | z::Z_BUF_ERROR => continue,
                other => panic!("zlib deflate failed with status {other}"),
            }
        }
        stream.next_in = ptr::null_mut();
        stream.next_out = ptr::null_mut();
    }
}

impl Drop for Deflater {
    fn drop(&mut self) {
        // SAFETY: the stream was initialized by deflateInit2 and is ended once.
        unsafe { z::deflateEnd(&mut *self.stream) };
    }
}

/// zlib allocation callback backed by the Rust global allocator, so zlib's
/// memory shows up in Sculk's allocation metrics.
unsafe extern "C" fn zalloc(_opaque: *mut c_void, items: c_uint, size: c_uint) -> *mut c_void {
    let Some(bytes) = (items as usize)
        .checked_mul(size as usize)
        .and_then(|bytes| bytes.checked_add(HEADER))
    else {
        return ptr::null_mut();
    };
    let Ok(layout) = Layout::from_size_align(bytes, HEADER) else {
        return ptr::null_mut();
    };
    // SAFETY: the layout has a non-zero size (at least HEADER bytes).
    let base = unsafe { alloc(layout) };
    if base.is_null() {
        return ptr::null_mut();
    }
    // SAFETY: `base` is valid for `bytes >= HEADER` bytes and 16-byte aligned.
    unsafe {
        (base as *mut usize).write(bytes);
        base.add(HEADER) as *mut c_void
    }
}

/// zlib free callback matching [`zalloc`].
unsafe extern "C" fn zfree(_opaque: *mut c_void, ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }
    // SAFETY: `ptr` came from `zalloc`, which stored the allocation size in the
    // HEADER bytes in front of it, using the same alignment.
    unsafe {
        let base = (ptr as *mut u8).sub(HEADER);
        let bytes = (base as *mut usize).read();
        dealloc(base, Layout::from_size_align_unchecked(bytes, HEADER));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::read::ZlibDecoder;
    use std::io::Read;

    #[test]
    fn output_is_standard_zlib_and_stream_is_reusable() {
        let mut deflater = Deflater::new(1);
        for round in 0..4u32 {
            let input: Vec<u8> = (0..50_000u32)
                .map(|i| ((i * (round + 1)) % 251) as u8 ^ (i >> 9) as u8)
                .collect();
            let mut out = vec![0xaa; 3];
            deflater.compress(&input, &mut out);
            assert_eq!(&out[..3], &[0xaa; 3]);
            let mut decoded = Vec::new();
            ZlibDecoder::new(&out[3..])
                .read_to_end(&mut decoded)
                .unwrap();
            assert_eq!(decoded, input);
        }
        let mut out = Vec::new();
        deflater.compress(&[], &mut out);
        let mut decoded = Vec::new();
        ZlibDecoder::new(&out[..])
            .read_to_end(&mut decoded)
            .unwrap();
        assert!(decoded.is_empty());
    }
}
