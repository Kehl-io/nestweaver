//! Zstandard compression bound to the copy of libzstd that lbug already links.
//!
//! # Why this exists instead of the `zstd` crate
//!
//! Every NestWeaver binary statically links `liblbug.a` with
//! `static:+whole-archive` (lbug's own `build.rs`), and that archive *contains*
//! zstd 1.5.7: lbug's CMake rolls `third_party/zstd` into the merged archive
//! (`LBUG_STATIC_ARCHIVE_LIBRARIES` in `lbug-src/CMakeLists.txt`) and its
//! `third_party/zstd/CMakeLists.txt` defines `ZSTDLIB_VISIBILITY` to *nothing*,
//! so every `ZSTD_*` entry point keeps default visibility and is exported.
//! `+whole-archive` then forces all of it into the final binary whether or not
//! anything references it.
//!
//! Depending on the `zstd` crate pulled in `zstd-sys`, which compiles a *second*
//! complete copy of the same zstd 1.5.7. `rust-lld` — the default linker on
//! x86_64 Linux — rejects that with duplicate-symbol errors, which is why the
//! tree carried `-Wl,--allow-multiple-definition`. That flag never merged the
//! two copies; it told the linker to pick one definition silently and left the
//! other in the binary.
//!
//! This module removes the second copy. It declares the handful of stable
//! public zstd entry points it needs and lets them resolve against the copy
//! already present in the binary. There is no `#[link]` attribute on purpose:
//! the symbols come from `liblbug.a` in the source-build path, and from the
//! `zstd` static library that `build.rs` compiles from lbug's vendored sources
//! in the prebuilt-lbug path. Both paths already emit the link directives.
//!
//! What this requires of lbug is narrow but real: it must keep *defining* the
//! `ZSTD_*` entry points with *global* binding. Note that hiding them — the
//! stated intent of the `ZSTDLIB_VISIBILITY` line above, which does not
//! currently do so — would NOT break this: ELF visibility governs dynamic
//! export, not static resolution, so a hidden-but-global symbol still links.
//! Only removing the symbols, or localizing them to `STB_LOCAL`, produces a
//! link error. There is no configuration in which this silently binds to a
//! different zstd.
//!
//! # Format
//!
//! Wire-compatible in both directions, with the same libzstd driven through the
//! same public API the `zstd` crate used: `.nwsnap.zst` backups and `NWRC`
//! response-cache sidecars written by earlier versions still read, and what is
//! written now is readable by any standard zstd implementation.
//!
//! The exact bytes are *not* always identical, so do not content-address a
//! compressed blob across this change. [`encode_all`] uses `ZSTD_compress`,
//! which records the frame content size in the header; the `zstd` crate's bulk
//! path did not, so every non-empty one-shot frame differs (and is one byte
//! longer for small inputs). [`Encoder`] output — the `.nwsnap.zst` path — is
//! byte-identical.

use std::ffi::{CStr, c_char, c_int, c_uint, c_void};
use std::io::{self, Read, Write};
use std::ptr::NonNull;

// --------------------------------------------------------------------------
// Raw bindings.
//
// Only the stable, non-experimental surface of `zstd.h` is declared here: every
// one of these has been part of libzstd's committed API since 1.4 and none of
// them requires `ZSTD_STATIC_LINKING_ONLY`.
// --------------------------------------------------------------------------

/// Opaque `ZSTD_CStream` / `ZSTD_DStream`.
#[repr(C)]
struct ZstdStream {
    _private: [u8; 0],
}

/// `ZSTD_inBuffer`. `src`/`size` are written by Rust and read by libzstd, never
/// read back on this side, hence the `dead_code` exemption — the layout is the
/// contract, not the Rust-side accesses.
#[repr(C)]
#[allow(dead_code)]
struct ZstdInBuffer {
    src: *const c_void,
    size: usize,
    pos: usize,
}

/// `ZSTD_outBuffer`. See [`ZstdInBuffer`] for why `dead_code` is exempted.
#[repr(C)]
#[allow(dead_code)]
struct ZstdOutBuffer {
    dst: *mut c_void,
    size: usize,
    pos: usize,
}

/// `ZSTD_EndDirective`.
const ZSTD_E_CONTINUE: c_int = 0;
const ZSTD_E_FLUSH: c_int = 1;
const ZSTD_E_END: c_int = 2;

// The declarations keep libzstd's own names so they match `zstd.h` exactly.
#[allow(non_snake_case)]
unsafe extern "C" {
    fn ZSTD_versionNumber() -> c_uint;
    fn ZSTD_isError(code: usize) -> c_uint;
    fn ZSTD_getErrorName(code: usize) -> *const c_char;

    fn ZSTD_compressBound(src_size: usize) -> usize;
    fn ZSTD_compress(
        dst: *mut c_void,
        dst_capacity: usize,
        src: *const c_void,
        src_size: usize,
        compression_level: c_int,
    ) -> usize;

    fn ZSTD_createCStream() -> *mut ZstdStream;
    fn ZSTD_freeCStream(zcs: *mut ZstdStream) -> usize;
    fn ZSTD_initCStream(zcs: *mut ZstdStream, compression_level: c_int) -> usize;
    fn ZSTD_compressStream2(
        zcs: *mut ZstdStream,
        output: *mut ZstdOutBuffer,
        input: *mut ZstdInBuffer,
        end_op: c_int,
    ) -> usize;
    fn ZSTD_CStreamOutSize() -> usize;

    fn ZSTD_createDStream() -> *mut ZstdStream;
    fn ZSTD_freeDStream(zds: *mut ZstdStream) -> usize;
    fn ZSTD_initDStream(zds: *mut ZstdStream) -> usize;
    fn ZSTD_decompressStream(
        zds: *mut ZstdStream,
        output: *mut ZstdOutBuffer,
        input: *mut ZstdInBuffer,
    ) -> usize;
    fn ZSTD_DStreamInSize() -> usize;
}

/// The libzstd version actually linked into this binary, as `MAJOR*10000 +
/// MINOR*100 + RELEASE`.
///
/// Exposed so a test can assert that the symbols really did resolve against a
/// real libzstd rather than something that merely linked.
pub fn version_number() -> u32 {
    // SAFETY: no arguments, no state, returns a plain integer.
    unsafe { ZSTD_versionNumber() }
}

/// Translate a zstd return code into an `io::Error`, or pass it through.
fn check(code: usize) -> io::Result<usize> {
    // SAFETY: `ZSTD_isError` reads only the integer it is handed.
    if unsafe { ZSTD_isError(code) } == 0 {
        return Ok(code);
    }
    // SAFETY: for an error code `ZSTD_getErrorName` returns a pointer to a
    // static NUL-terminated string with 'static lifetime.
    let name = unsafe { CStr::from_ptr(ZSTD_getErrorName(code)) };
    Err(io::Error::other(format!(
        "zstd: {}",
        name.to_string_lossy()
    )))
}

// --------------------------------------------------------------------------
// One-shot helpers
// --------------------------------------------------------------------------

/// Compress `data` into a single zstd frame at `level`.
pub fn encode_all(data: &[u8], level: i32) -> io::Result<Vec<u8>> {
    // SAFETY: pure arithmetic on the input length.
    let capacity = unsafe { ZSTD_compressBound(data.len()) };
    let mut out = vec![0u8; capacity];
    // SAFETY: `out` has `capacity` writable bytes and `data` has `data.len()`
    // readable bytes; both outlive the call.
    let written = check(unsafe {
        ZSTD_compress(
            out.as_mut_ptr().cast::<c_void>(),
            capacity,
            data.as_ptr().cast::<c_void>(),
            data.len(),
            level as c_int,
        )
    })?;
    out.truncate(written);
    Ok(out)
}

/// Decompress every zstd frame in `data`.
///
/// Uses the streaming API rather than `ZSTD_decompress` so that frames without
/// a recorded content size — anything written by [`Encoder`] — decode too.
pub fn decode_all(data: &[u8]) -> io::Result<Vec<u8>> {
    let mut out = Vec::new();
    Decoder::new(data)?.read_to_end(&mut out)?;
    Ok(out)
}

// --------------------------------------------------------------------------
// Streaming compression
// --------------------------------------------------------------------------

/// A `Write` adapter that zstd-compresses into an inner writer.
///
/// Call [`finish`](Self::finish) to terminate the frame; dropping without
/// finishing leaves a truncated, unreadable frame, exactly as with the `zstd`
/// crate's encoder.
pub struct Encoder<W: Write> {
    stream: NonNull<ZstdStream>,
    writer: Option<W>,
    buffer: Vec<u8>,
    /// Set after a zstd error. `zstd.h` documents the stream as undefined once
    /// `ZSTD_compressStream2` fails, and `io::Write` permits a caller to keep
    /// writing after an error, so refuse instead of touching it.
    poisoned: bool,
}

// SAFETY: a `ZSTD_CStream` is owned exclusively by its `Encoder` and carries no
// thread-affine state; libzstd only requires that it not be used concurrently,
// which `&mut self` already guarantees.
unsafe impl<W: Write + Send> Send for Encoder<W> {}

impl<W: Write> Encoder<W> {
    /// Create an encoder writing a zstd frame at `level` into `writer`.
    pub fn new(writer: W, level: i32) -> io::Result<Self> {
        // SAFETY: allocation only; returns null on failure.
        let stream = NonNull::new(unsafe { ZSTD_createCStream() })
            .ok_or_else(|| io::Error::other("zstd: could not allocate a compression stream"))?;
        // SAFETY: `stream` is a freshly allocated, non-null `ZSTD_CStream`.
        if let Err(error) = check(unsafe { ZSTD_initCStream(stream.as_ptr(), level as c_int) }) {
            // SAFETY: `stream` is still owned here and not yet handed out.
            unsafe { ZSTD_freeCStream(stream.as_ptr()) };
            return Err(error);
        }
        // SAFETY: no arguments; returns the recommended output buffer size.
        let buffer = vec![0u8; unsafe { ZSTD_CStreamOutSize() }];
        Ok(Self {
            stream,
            writer: Some(writer),
            buffer,
            poisoned: false,
        })
    }

    /// Drive `ZSTD_compressStream2` over `input` until zstd is done with the
    /// directive, writing every produced byte to the inner writer.
    fn run(&mut self, input: &[u8], end_op: c_int) -> io::Result<()> {
        if self.poisoned {
            return Err(io::Error::other(
                "zstd: compression stream used after an error",
            ));
        }
        let mut in_buffer = ZstdInBuffer {
            src: input.as_ptr().cast::<c_void>(),
            size: input.len(),
            pos: 0,
        };
        loop {
            let mut out_buffer = ZstdOutBuffer {
                dst: self.buffer.as_mut_ptr().cast::<c_void>(),
                size: self.buffer.len(),
                pos: 0,
            };
            // SAFETY: both buffers describe live allocations for the duration
            // of the call, and `self.stream` is a live `ZSTD_CStream`.
            let remaining = match check(unsafe {
                ZSTD_compressStream2(
                    self.stream.as_ptr(),
                    &mut out_buffer,
                    &mut in_buffer,
                    end_op,
                )
            }) {
                Ok(remaining) => remaining,
                Err(error) => {
                    self.poisoned = true;
                    return Err(error);
                }
            };
            if out_buffer.pos > 0 {
                let writer = self
                    .writer
                    .as_mut()
                    .ok_or_else(|| io::Error::other("zstd: encoder already finished"))?;
                writer.write_all(&self.buffer[..out_buffer.pos])?;
            }
            let done = match end_op {
                // Feeding input is complete once zstd has consumed all of it.
                ZSTD_E_CONTINUE => in_buffer.pos == in_buffer.size,
                // A flush or an end is complete once zstd reports nothing left.
                _ => remaining == 0,
            };
            if done {
                return Ok(());
            }
        }
    }

    /// Terminate the frame and return the inner writer.
    pub fn finish(mut self) -> io::Result<W> {
        self.run(&[], ZSTD_E_END)?;
        let mut writer = self
            .writer
            .take()
            .ok_or_else(|| io::Error::other("zstd: encoder already finished"))?;
        writer.flush()?;
        Ok(writer)
    }
}

impl<W: Write> Write for Encoder<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.run(buf, ZSTD_E_CONTINUE)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.run(&[], ZSTD_E_FLUSH)?;
        match self.writer.as_mut() {
            Some(writer) => writer.flush(),
            None => Err(io::Error::other("zstd: encoder already finished")),
        }
    }
}

impl<W: Write> Drop for Encoder<W> {
    fn drop(&mut self) {
        // SAFETY: `self.stream` is live and owned solely by this encoder; it is
        // never freed anywhere else.
        unsafe { ZSTD_freeCStream(self.stream.as_ptr()) };
    }
}

// --------------------------------------------------------------------------
// Streaming decompression
// --------------------------------------------------------------------------

/// A `Read` adapter that zstd-decompresses from an inner reader.
///
/// Handles frames with or without a recorded content size, and consumes
/// consecutive frames as one stream.
pub struct Decoder<R: Read> {
    stream: NonNull<ZstdStream>,
    reader: R,
    input: Vec<u8>,
    /// Bytes of `input` that hold data read from `reader`.
    filled: usize,
    /// Bytes of `input[..filled]` already consumed by zstd.
    consumed: usize,
    eof: bool,
    /// True when the last `ZSTD_decompressStream` returned 0 — "frame
    /// completely decoded and fully flushed" (`zstd.h`). Starts `false`: input
    /// that ends before any frame completes is truncated, and must be reported
    /// as an error rather than as a short-but-successful read.
    frame_complete: bool,
    /// Set after a zstd error. `zstd.h` documents the stream as undefined once
    /// `ZSTD_decompressStream` fails, and `io::Read` permits a caller to call
    /// `read` again after an error, so refuse instead of touching it.
    poisoned: bool,
}

// SAFETY: as for `Encoder` — the `ZSTD_DStream` is exclusively owned and only
// ever touched behind `&mut self`.
unsafe impl<R: Read + Send> Send for Decoder<R> {}

impl<R: Read> Decoder<R> {
    /// Create a decoder reading compressed bytes from `reader`.
    pub fn new(reader: R) -> io::Result<Self> {
        // SAFETY: allocation only; returns null on failure.
        let stream = NonNull::new(unsafe { ZSTD_createDStream() })
            .ok_or_else(|| io::Error::other("zstd: could not allocate a decompression stream"))?;
        // SAFETY: `stream` is a freshly allocated, non-null `ZSTD_DStream`.
        if let Err(error) = check(unsafe { ZSTD_initDStream(stream.as_ptr()) }) {
            // SAFETY: `stream` is still owned here and not yet handed out.
            unsafe { ZSTD_freeDStream(stream.as_ptr()) };
            return Err(error);
        }
        // SAFETY: no arguments; returns the recommended input buffer size.
        let input = vec![0u8; unsafe { ZSTD_DStreamInSize() }];
        Ok(Self {
            stream,
            reader,
            input,
            filled: 0,
            consumed: 0,
            eof: false,
            frame_complete: false,
            poisoned: false,
        })
    }

    /// The inner reader is exhausted. Distinguish a clean end-of-stream from a
    /// truncated one.
    fn end_of_input(&self) -> io::Result<usize> {
        if self.frame_complete {
            Ok(0)
        } else {
            Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "zstd: incomplete frame — the compressed input ends mid-frame",
            ))
        }
    }
}

impl<R: Read> Read for Decoder<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.poisoned {
            return Err(io::Error::other(
                "zstd: decompression stream used after an error",
            ));
        }
        if buf.is_empty() {
            return Ok(0);
        }
        loop {
            if self.consumed == self.filled {
                if self.eof {
                    return self.end_of_input();
                }
                self.filled = self.reader.read(&mut self.input)?;
                self.consumed = 0;
                if self.filled == 0 {
                    self.eof = true;
                    return self.end_of_input();
                }
            }

            let mut out_buffer = ZstdOutBuffer {
                dst: buf.as_mut_ptr().cast::<c_void>(),
                size: buf.len(),
                pos: 0,
            };
            let mut in_buffer = ZstdInBuffer {
                src: self.input.as_ptr().cast::<c_void>(),
                size: self.filled,
                pos: self.consumed,
            };
            // SAFETY: both buffers describe live allocations for the duration
            // of the call, and `self.stream` is a live `ZSTD_DStream`.
            let remaining = match check(unsafe {
                ZSTD_decompressStream(self.stream.as_ptr(), &mut out_buffer, &mut in_buffer)
            }) {
                Ok(remaining) => remaining,
                Err(error) => {
                    self.poisoned = true;
                    return Err(error);
                }
            };
            self.consumed = in_buffer.pos;
            // 0 means the frame is decoded and flushed; anything else means
            // zstd is mid-frame and still wants input. Latching it is what lets
            // `end_of_input` tell a complete stream from a truncated one.
            self.frame_complete = remaining == 0;
            if out_buffer.pos > 0 {
                return Ok(out_buffer.pos);
            }
            // zstd produced nothing: it needs more input. Loop and refill.
        }
    }
}

impl<R: Read> Drop for Decoder<R> {
    fn drop(&mut self) {
        // SAFETY: `self.stream` is live and owned solely by this decoder; it is
        // never freed anywhere else.
        unsafe { ZSTD_freeDStream(self.stream.as_ptr()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic, effectively incompressible bytes.
    ///
    /// Truncation tests need a frame whose compressed form is larger than the
    /// tail being cut off. Repetitive filler compresses to a few dozen bytes
    /// and makes such a test silently vacuous, so generate from an LCG instead.
    fn incompressible(len: usize) -> Vec<u8> {
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        (0..len)
            .map(|_| {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (state >> 33) as u8
            })
            .collect()
    }

    /// The symbols must resolve against the libzstd lbug vendors, not merely
    /// link against something. Pinned exactly rather than as a floor: a loose
    /// bound would also pass against a stray system libzstd, and the point of
    /// this test is to prove *which* copy answered.
    ///
    /// lbug 0.19.1 vendors 1.5.7 (`lbug-src/third_party/versions.txt`). If an
    /// lbug bump changes it, update this constant deliberately after checking
    /// that the entry points this module declares are unchanged.
    #[test]
    fn links_against_the_libzstd_lbug_vendors() {
        assert_eq!(
            version_number(),
            10507,
            "expected lbug's vendored libzstd 1.5.7"
        );
    }

    #[test]
    fn one_shot_round_trips() {
        let data = b"nestweaver".repeat(500);
        let compressed = encode_all(&data, 3).unwrap();
        assert!(compressed.len() < data.len());
        assert_eq!(decode_all(&compressed).unwrap(), data);
    }

    #[test]
    fn one_shot_round_trips_empty_input() {
        let compressed = encode_all(b"", 3).unwrap();
        assert_eq!(decode_all(&compressed).unwrap(), Vec::<u8>::new());
    }

    /// A truncated frame must be an error, never a short success.
    ///
    /// This is the failure mode that matters: a `.nwsnap.zst` cut short by an
    /// interrupted write or a partial copy. Returning `Ok` with fewer bytes
    /// would let `backup_inspect` report a manifest from a damaged archive, and
    /// would hand `cache.rs` a silently truncated payload. `tar` and the
    /// manifest checksums catch *some* of that downstream, but the compression
    /// layer must signal it in its own right.
    #[test]
    fn truncated_frames_are_an_error_not_a_short_read() {
        // Big enough that dropping a large tail still leaves whole blocks that
        // decode fine — the case a length-only check would miss.
        let data = incompressible(2_000_000);
        let compressed = encode_all(&data, 3).unwrap();

        for cut in [1usize, 50, 5000] {
            assert!(
                cut < compressed.len(),
                "fixture too compressible: {} bytes cannot lose {cut}",
                compressed.len()
            );
            let truncated = &compressed[..compressed.len() - cut];
            let error = match decode_all(truncated) {
                Ok(bytes) => panic!(
                    "truncation by {cut} bytes was accepted, yielding {} bytes",
                    bytes.len()
                ),
                Err(error) => error,
            };
            assert_eq!(
                error.kind(),
                io::ErrorKind::UnexpectedEof,
                "truncation by {cut}: {error}"
            );
        }
    }

    /// Empty input is a truncated frame, not an empty stream: zero bytes cannot
    /// contain a frame header.
    #[test]
    fn empty_input_is_an_incomplete_frame() {
        let error = decode_all(b"").unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof, "{error}");
    }

    /// `io::Read` allows another `read` after an error. The decoder must keep
    /// reporting the problem rather than reporting success.
    #[test]
    fn a_truncated_decoder_keeps_failing_on_reread() {
        let compressed = encode_all(&incompressible(100_000), 3).unwrap();
        let truncated = &compressed[..compressed.len() - 20];
        let mut decoder = Decoder::new(truncated).unwrap();
        let mut sink = Vec::new();
        assert!(decoder.read_to_end(&mut sink).is_err());
        let mut buf = [0u8; 64];
        assert!(
            decoder.read(&mut buf).is_err(),
            "a second read reported success after a truncated stream"
        );
    }

    /// A stream cut mid-frame after a complete earlier frame is still
    /// truncated: the latch must reset when the next frame starts.
    #[test]
    fn truncation_after_a_complete_frame_is_still_an_error() {
        let mut compressed = encode_all(b"first frame", 3).unwrap();
        let second = encode_all(&incompressible(100_000), 3).unwrap();
        assert!(second.len() > 30, "fixture too compressible");
        compressed.extend_from_slice(&second[..second.len() - 30]);
        let error = decode_all(&compressed).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof, "{error}");
    }

    #[test]
    fn streaming_round_trips_across_many_writes() {
        // More than one `ZSTD_CStreamOutSize` worth, written in small pieces,
        // so both the encoder's flush loop and the decoder's refill loop run.
        let chunk = b"the quick brown fox jumps over the lazy dog\n";
        let mut expected = Vec::new();
        let mut encoder = Encoder::new(Vec::new(), 3).unwrap();
        for _ in 0..20_000 {
            encoder.write_all(chunk).unwrap();
            expected.extend_from_slice(chunk);
        }
        let compressed = encoder.finish().unwrap();
        assert!(compressed.len() < expected.len());

        let mut decoded = Vec::new();
        Decoder::new(compressed.as_slice())
            .unwrap()
            .read_to_end(&mut decoded)
            .unwrap();
        assert_eq!(decoded, expected);
    }

    /// A streamed frame records no content size, so a decoder that relied on
    /// `ZSTD_getFrameContentSize` would fail here.
    #[test]
    fn streamed_frames_decode_through_the_one_shot_helper() {
        let mut encoder = Encoder::new(Vec::new(), 3).unwrap();
        encoder.write_all(b"streamed payload").unwrap();
        let compressed = encoder.finish().unwrap();
        assert_eq!(decode_all(&compressed).unwrap(), b"streamed payload");
    }

    /// The one-shot and streaming paths must interoperate in both directions,
    /// because `.nwsnap.zst` archives and `NWRC` sidecars written by earlier
    /// releases used the `zstd` crate's equivalents of each.
    #[test]
    fn one_shot_output_decodes_through_the_streaming_decoder() {
        let data = b"cross-path payload".repeat(200);
        let compressed = encode_all(&data, 3).unwrap();
        let mut decoded = Vec::new();
        Decoder::new(compressed.as_slice())
            .unwrap()
            .read_to_end(&mut decoded)
            .unwrap();
        assert_eq!(decoded, data);
    }

    #[test]
    fn consecutive_frames_decode_as_one_stream() {
        let mut compressed = encode_all(b"first", 3).unwrap();
        compressed.extend_from_slice(&encode_all(b"second", 3).unwrap());
        assert_eq!(decode_all(&compressed).unwrap(), b"firstsecond");
    }

    #[test]
    fn corrupt_input_is_an_error_not_a_panic() {
        let error = decode_all(b"not a zstd frame at all").unwrap_err();
        assert!(error.to_string().contains("zstd:"), "{error}");
    }

    #[test]
    fn flush_does_not_end_the_frame() {
        let mut encoder = Encoder::new(Vec::new(), 3).unwrap();
        encoder.write_all(b"before flush ").unwrap();
        encoder.flush().unwrap();
        encoder.write_all(b"after flush").unwrap();
        let compressed = encoder.finish().unwrap();
        assert_eq!(
            decode_all(&compressed).unwrap(),
            b"before flush after flush"
        );
    }
}
