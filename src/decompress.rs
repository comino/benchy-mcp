//! A compressed HTTP body, decoded as it is read: the one place a network source turns a
//! `Content-Encoding` into bytes.
//!
//! - **Decoded only as far as it is read.** [`decoder`] returns a reader that produces output a
//!   block at a time, as the caller pulls. A caller that stops at its byte cap has decoded the cap
//!   and at most one block more, whatever the body would decode to: a bomb is stopped at the cap,
//!   never decoded whole (`a_bomb_is_decoded_only_as_far_as_it_is_read`). The cap itself is the
//!   caller's: the ClickHouse source's reader counts the decoded Arrow (REQ-16).
//! - **Memory is bounded too.** A zstd frame names the window it needs, and the frame header is
//!   the server's to write; the decoder refuses one over 32 MiB
//!   (`a_zstd_window_past_32_mib_is_refused`). zstd's level 3, which ClickHouse uses, asks for a
//!   few MiB.
//! - **A body cut short is an error**, never a shorter body that looks complete
//!   (`a_multi_frame_zstd_body_is_read_to_its_end`).
//! - **No file is read.** The loader that runs a network source is Landlock-confined before its
//!   first request (REQ-3); libzstd decodes from memory and opens nothing.
//!
//! Why these decoders, measured on 10,000,000 rows of `bench/gen_data.py` as one ClickHouse 26.8
//! `ArrowStream` response, 240 MB decoded, aarch64 Neoverse-N1, user-space instructions
//! (spike/decompress, docs/history/PERFORMANCE-rounds.md, round 1 item 3):
//!
//! | body | decoder | on the wire | instructions | CPU |
//! |---|---|---:|---:|---:|
//! | zstd | ruzstd 0.9 (pure Rust, what benchy used) | 41.5 MB | 5.99 G | 0.75 s |
//! | zstd | libzstd 1.5.7 (the `zstd` crate, C) | 41.5 MB | 1.64 G | 0.29 s |
//! | gzip | flate2 + miniz_oxide | 72.2 MB | 4.61 G | 0.66 s |
//! | gzip | flate2 + zlib-rs | 72.2 MB | 4.53 G | 0.53 s |
//!
//! libzstd decodes in 27% of ruzstd's instructions; it is compiled from source by `cc`, which
//! `ring` already needs, in about 75 s of a cold build. zlib-rs takes 20% less CPU than
//! miniz_oxide for the same instructions, so gzip, which the HTTP source reads (REQ-17), is
//! flate2 on zlib-rs, every member of a multi-member body read.
use std::io::{BufRead, Read};

/// zstd's window, as a power of two: 32 MiB. See the module's comment.
const ZSTD_WINDOW_LOG_MAX: u32 = 25;

/// How a body crossed the network: a `Content-Encoding` benchy can undo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    /// Not encoded: absent, empty or `identity`.
    Identity,
    /// `zstd`: one frame or several, back to back.
    Zstd,
    /// `gzip` (or `x-gzip`): one member or several, back to back, as `cat a.gz b.gz` makes.
    Gzip,
}

impl Encoding {
    /// The encoding a `Content-Encoding` header names, or `None` for one benchy does not decode
    /// (including a list of several, which no source asks for).
    pub fn from_header(value: Option<&str>) -> Option<Self> {
        match value.unwrap_or("").trim().to_ascii_lowercase().as_str() {
            "" | "identity" => Some(Encoding::Identity),
            "zstd" => Some(Encoding::Zstd),
            "gzip" | "x-gzip" => Some(Encoding::Gzip),
            _ => None,
        }
    }

    /// Its token, as `Accept-Encoding` and `Content-Encoding` write it.
    pub fn token(self) -> &'static str {
        match self {
            Encoding::Identity => "identity",
            Encoding::Zstd => "zstd",
            Encoding::Gzip => "gzip",
        }
    }
}

/// `src` decoded as it is read. Read it to a cap; it decodes no further than it is read. `Send`,
/// so a body can be decoded on another thread than the one that asked for it.
pub fn decoder<'a, R: BufRead + Send + 'a>(
    encoding: Encoding,
    src: R,
) -> std::io::Result<Box<dyn Read + Send + 'a>> {
    Ok(match encoding {
        Encoding::Identity => Box::new(src),
        Encoding::Zstd => {
            // Frame after frame (a stream may hold several), and an error if the input ends
            // inside one.
            let mut d = zstd::stream::read::Decoder::with_buffer(src)?;
            d.window_log_max(ZSTD_WINDOW_LOG_MAX)?;
            Box::new(d)
        }
        // Every member, and an error if the input ends inside one.
        Encoding::Gzip => Box::new(flate2::bufread::MultiGzDecoder::new(src)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
    use std::sync::Arc;

    fn zstd(data: &[u8]) -> Vec<u8> {
        zstd::bulk::compress(data, 3).unwrap()
    }

    fn decode(enc: Encoding, body: &[u8]) -> std::io::Result<Vec<u8>> {
        let mut out = Vec::new();
        decoder(enc, body)?.read_to_end(&mut out)?;
        Ok(out)
    }

    #[test]
    fn a_multi_frame_zstd_body_is_read_to_its_end() {
        let mut both = zstd(b"first frame ");
        both.extend(zstd(b"and the second"));
        assert_eq!(
            decode(Encoding::Zstd, &both).unwrap(),
            b"first frame and the second"
        );
        // Cut short, it fails rather than ending as if complete: inside the second frame, and
        // inside the first.
        assert!(decode(Encoding::Zstd, &both[..both.len() - 3]).is_err());
        assert!(decode(Encoding::Zstd, &both[..6]).is_err());
    }

    /// Counts what the decoder has taken from the body.
    struct Counted<'a> {
        inner: &'a [u8],
        taken: Arc<AtomicUsize>,
    }

    impl Read for Counted<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.inner.read(buf)?;
            self.taken.fetch_add(n, Relaxed);
            Ok(n)
        }
    }

    /// Read to a 1 MiB cap, a body that would decode to 1 GiB gives the cap and has taken less
    /// than one frame's few KiB off the wire: the decoder works as the reader pulls, so a bomb is
    /// stopped at the cap, never decoded whole.
    #[test]
    fn a_bomb_is_decoded_only_as_far_as_it_is_read() {
        // 1 GiB of zeros, as 16 frames of 64 MiB: a few KiB on the wire.
        let frame = zstd(&vec![0u8; 64 << 20]);
        let bomb: Vec<u8> = frame
            .iter()
            .copied()
            .cycle()
            .take(frame.len() * 16)
            .collect();
        assert!(bomb.len() < 64 << 10, "{}", bomb.len());

        let taken = Arc::new(AtomicUsize::new(0));
        let body = std::io::BufReader::with_capacity(
            64,
            Counted {
                inner: &bomb,
                taken: taken.clone(),
            },
        );
        let cap = 1 << 20;
        let mut out = Vec::new();
        decoder(Encoding::Zstd, body)
            .unwrap()
            .take(cap)
            .read_to_end(&mut out)
            .unwrap();
        assert_eq!(out.len(), cap as usize);
        // Stopping at the cap took a sliver of the first frame's few KiB: decoding the body whole
        // first would have taken all of it.
        assert!(
            taken.load(Relaxed) < frame.len(),
            "took {} of a {}-byte frame to give {cap} bytes",
            taken.load(Relaxed),
            frame.len()
        );
    }

    /// A frame header is the server's to write: one that asks for a 64 MiB window is refused, and
    /// 32 MiB is within the bound.
    #[test]
    fn a_zstd_window_past_32_mib_is_refused() {
        // A frame that asks for a 64 MiB window. libzstd's own default limit is 128 MiB, so it
        // is the limit set here that refuses it.
        let mut enc = zstd::stream::write::Encoder::new(Vec::new(), 3).unwrap();
        enc.include_contentsize(false).unwrap();
        enc.window_log(26).unwrap();
        std::io::Write::write_all(&mut enc, b"tiny, but the header asks for 64 MiB").unwrap();
        let big = enc.finish().unwrap();
        let e = decode(Encoding::Zstd, &big).unwrap_err();
        assert!(e.to_string().contains("memory"), "{e}");
        // 32 MiB is within it.
        let mut enc = zstd::stream::write::Encoder::new(Vec::new(), 3).unwrap();
        enc.include_contentsize(false).unwrap();
        enc.window_log(25).unwrap();
        std::io::Write::write_all(&mut enc, b"within").unwrap();
        assert_eq!(
            decode(Encoding::Zstd, &enc.finish().unwrap()).unwrap(),
            b"within"
        );
    }

    #[test]
    fn a_multi_member_gzip_body_is_read_to_its_end() {
        use std::io::Write;
        let gz = |data: &[u8]| {
            let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
            e.write_all(data).unwrap();
            e.finish().unwrap()
        };
        let mut both = gz(b"first member ");
        both.extend(gz(b"and the second"));
        assert_eq!(
            decode(Encoding::Gzip, &both).unwrap(),
            b"first member and the second"
        );
        assert!(decode(Encoding::Gzip, &both[..both.len() - 3]).is_err());
        assert_eq!(Encoding::from_header(Some("x-gzip")), Some(Encoding::Gzip));
    }

    #[test]
    fn a_content_encoding_is_read_as_a_token() {
        assert_eq!(Encoding::from_header(None), Some(Encoding::Identity));
        assert_eq!(Encoding::from_header(Some(" ")), Some(Encoding::Identity));
        assert_eq!(
            Encoding::from_header(Some("Identity")),
            Some(Encoding::Identity)
        );
        assert_eq!(Encoding::from_header(Some(" ZSTD ")), Some(Encoding::Zstd));
        assert_eq!(Encoding::from_header(Some("br")), None);
        assert_eq!(Encoding::from_header(Some("zstd, gzip")), None);
        assert_eq!(
            Encoding::from_header(Some(Encoding::Zstd.token())),
            Some(Encoding::Zstd)
        );
    }
}
