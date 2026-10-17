//! Probe 8: what decoding a compressed HTTP response costs, per decoder.
//!
//!     probe8_decompress <codec> <file> [runs]
//!
//! codec: `ruzstd`, `czstd` (zstd body), `lz4` (an LZ4-frame body, ClickHouse's
//! `Accept-Encoding: lz4`), `gzip` / `deflate` (zlib-wrapped, what ClickHouse calls deflate),
//! `arrow` (an Arrow IPC stream read batch by batch: run it on the uncompressed and on the
//! LZ4-bodied stream, and the difference is what arrow's LZ4 decode costs), `copy` (the file read
//! through the same buffers, decoded by nothing: the floor).
//!
//! Each decoder is read the way the ClickHouse source reads a body: through a 64 KiB `BufReader`,
//! pulled 64 KiB at a time, into nothing. Prints bytes in, bytes out, and the best wall time of
//! `runs`; count instructions with `perf stat -e instructions:u` around a single run.
use std::io::{BufReader, Read};
use std::time::Instant;

fn drain(mut r: impl Read) -> u64 {
    let mut buf = vec![0u8; 64 * 1024];
    let mut n = 0u64;
    loop {
        match r.read(&mut buf).expect("decode") {
            0 => return n,
            k => n += k as u64,
        }
    }
}

#[cfg(feature = "ruzstd")]
mod ruz {
    //! A copy of benchy's ruzstd reader as it was at round 1's start, before the public history began (frame after frame, window
    //! held to 32 MiB), so the probe measures what the loader ran.
    use std::io::{BufRead, Read};
    pub struct Zstd<R: BufRead> {
        src: R,
        dec: ruzstd::decoding::FrameDecoder,
        in_frame: bool,
    }
    impl<R: BufRead> Zstd<R> {
        pub fn new(src: R) -> Self {
            let mut dec = ruzstd::decoding::FrameDecoder::new();
            dec.set_max_window_size(32 * 1024 * 1024);
            Self {
                src,
                dec,
                in_frame: false,
            }
        }
    }
    impl<R: BufRead> Read for Zstd<R> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if buf.is_empty() {
                return Ok(0);
            }
            loop {
                if !self.in_frame {
                    if self.src.fill_buf()?.is_empty() {
                        return Ok(0);
                    }
                    self.dec
                        .reset(&mut self.src)
                        .map_err(std::io::Error::other)?;
                    self.in_frame = true;
                }
                while self.dec.can_collect() < buf.len() && !self.dec.is_finished() {
                    let want = buf.len() - self.dec.can_collect();
                    self.dec
                        .decode_blocks(
                            &mut self.src,
                            ruzstd::decoding::BlockDecodingStrategy::UptoBytes(want),
                        )
                        .map_err(std::io::Error::other)?;
                }
                let n = self.dec.read(buf)?;
                if n > 0 {
                    return Ok(n);
                }
                if self.dec.is_finished() {
                    self.in_frame = false;
                    continue;
                }
                return Ok(0);
            }
        }
    }
}

fn run(codec: &str, path: &str) -> u64 {
    let src = BufReader::with_capacity(64 * 1024, std::fs::File::open(path).expect("open"));
    match codec {
        "copy" => drain(src),
        #[cfg(feature = "ruzstd")]
        "ruzstd" => drain(ruz::Zstd::new(src)),
        #[cfg(feature = "czstd")]
        "czstd" => {
            let mut d = zstd::stream::read::Decoder::with_buffer(src).expect("zstd");
            d.window_log_max(25).expect("window");
            drain(d)
        }
        #[cfg(feature = "lz4")]
        "lz4" => drain(lz4_flex::frame::FrameDecoder::new(src)),
        #[cfg(any(feature = "gz-miniz", feature = "gz-zlibrs"))]
        "gzip" => drain(flate2::bufread::MultiGzDecoder::new(src)),
        #[cfg(any(feature = "gz-miniz", feature = "gz-zlibrs"))]
        "deflate" => drain(flate2::bufread::ZlibDecoder::new(src)),
        #[cfg(feature = "arrow")]
        "arrow" => {
            let reader = arrow::ipc::reader::StreamReader::try_new(src, None).expect("arrow");
            let mut n = 0u64;
            for b in reader {
                n += b.expect("batch").get_array_memory_size() as u64;
            }
            n
        }
        other => panic!("{other}: not built with that decoder"),
    }
}

fn main() {
    let a: Vec<String> = std::env::args().collect();
    let (codec, path) = (&a[1], &a[2]);
    let runs: usize = a.get(3).map(|s| s.parse().unwrap()).unwrap_or(1);
    let size = std::fs::metadata(path).expect("stat").len();
    let mut best = f64::MAX;
    let mut out = 0;
    for _ in 0..runs {
        let t = Instant::now();
        out = run(codec, path);
        best = best.min(t.elapsed().as_secs_f64());
    }
    println!(
        "{codec}: {size} bytes in, {out} out, best of {runs}: {best:.3} s, {:.0} MB/s out",
        out as f64 / best / 1e6
    );
}
