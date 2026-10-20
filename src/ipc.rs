//! Reading an Arrow IPC stream one message at a time, bounded by what it decodes to.
//!
//! A stream's size bounds what it holds only while its bodies are uncompressed. ClickHouse's
//! `FORMAT ArrowStream` compresses them with LZ4 by default, and a compressed buffer states the
//! size it decompresses to — a claim, made by the sender, which arrow trusts: it allocates what
//! the claim says, and LZ4's decoder reads on past it, checking only once it has finished.
//! Measured on Int64, compression was 31.6×, and LZ4 allows about 255×. So each message is read
//! here first, whole, and what it decodes to is counted against the bound before arrow sees it:
//! an uncompressed body by its length, a compressed buffer by its claim — after checking the
//! claim, by decompressing the buffer and stopping one byte past it.
//!
//! A compressed message is decoded here, once, and goes to arrow's push decoder as one whose
//! buffers are stored rather than compressed — each prefixed `-1`, which the format allows and
//! which arrow takes as it is, without a copy. The buffers' places in the body and the body's
//! length are rewritten in the message's metadata, where they are fixed-size values; nothing else
//! of it changes. The check used to decompress into nothing and leave the decoding to arrow:
//! LZ4 ran twice, 1.7 G instructions more for 10,000,000 rows than the same rows uncompressed
//! (docs/history/PERFORMANCE-rounds.md, R2-6). Decoded into memory, a lie costs what it decoded
//! before it was caught, never more than its claim, which the bound has already admitted.
//!
//! An uncompressed message goes to arrow as it was read: one copy of each body, the one arrow's
//! own reader would have made.
//!
//! Only LZ4 is decoded. A zstd body is refused, naming the setting that turns compression off or
//! to LZ4: a zstd buffer's claim would need the same check, and LZ4 already halves what a fetch
//! moves. (libzstd is linked now, for HTTP responses, in src/decompress.rs; arrow's zstd feature
//! is still off.)

use crate::exec::Fault;
use std::io::Read;

/// The most a message's metadata may be. A schema of 512 columns with long names is a few
/// hundred kilobytes; anything near this is not a schema.
const MAX_METADATA: usize = 16 * 1024 * 1024;

/// One message, as read: its bytes, ready for arrow; whether it was the end-of-stream marker; and
/// the schema it carries, if it is one — read here, because arrow's push decoder holds a message
/// with no body until the next one arrives, and a stream may end without one.
pub struct Message {
    pub bytes: Vec<u8>,
    pub end: bool,
    pub schema: Option<arrow::datatypes::Schema>,
}

/// What a stream has decoded to so far, against the most it may.
pub struct Decoded {
    pub bytes: u64,
    pub max: u64,
}

impl Decoded {
    pub fn new(max: u64) -> Self {
        Self { bytes: 0, max }
    }

    fn add(&mut self, n: u64) -> Result<(), Fault> {
        self.bytes = self.bytes.saturating_add(n);
        if self.bytes > self.max {
            return Err(Fault::host(
                "E_HOST_OVERSIZE",
                format!(
                    "the stream decodes to more than {} bytes; its compressed size is not what \
                     is bounded, what it decompresses to is",
                    self.max
                ),
            ));
        }
        Ok(())
    }

    /// What a batch's dictionary-encoded and view columns unpack to (`crate::exec`'s
    /// `unpacked_size`), counted against the same bound as the bodies, which already hold the
    /// keys, the views and the dictionary itself. One total, not two: each counted against the
    /// bound on its own, a stream of some of each was let through at up to twice it — 1,000 rows
    /// of an 800-byte text beside a dictionary column picking one 800-byte value, 818 KB of
    /// bodies and 804 KB unpacked, attached under 1 MiB, measured (#14; REQ-13
    /// `an_attachments_plain_and_unpacked_bytes_share_one_bound`). A dictionary's keys are so
    /// counted twice, as the body that sent them and as the offsets they unpack to: at most
    /// eight bytes a row more than the table holds, and the bound errs toward refusing.
    pub fn add_unpacked(&mut self, n: u64) -> Result<(), Fault> {
        self.bytes = self.bytes.saturating_add(n);
        if self.bytes > self.max {
            return Err(Fault::host(
                "E_HOST_OVERSIZE",
                format!(
                    "the stream decodes to more than {} bytes, its bodies decompressed and its \
                     dictionary-encoded and view columns unpacked",
                    self.max
                ),
            ));
        }
        Ok(())
    }
}

/// An I/O error from below, as the reader that raised it meant it: a cap, a cancellation, or the
/// stream simply not being an Arrow stream.
pub fn io_fault(e: std::io::Error) -> Fault {
    let msg = e.to_string();
    if msg.contains("cap reached") {
        Fault::host(
            "E_HOST_OVERSIZE",
            "the stream is longer than its `max_bytes`",
        )
    } else if msg.contains("over the remaining budget") {
        Fault::host("E_TOO_LARGE", msg)
    } else if e.kind() == std::io::ErrorKind::UnexpectedEof {
        Fault::host(
            "E_HOST_PROTOCOL",
            "bad Arrow stream: it ended in the middle of a message",
        )
    } else {
        Fault::host("E_HOST_PROTOCOL", format!("bad Arrow stream: {msg}"))
    }
}

/// A schema message as arrow's schema. arrow's conversion panics on a type it cannot build — an
/// integer 7 bits wide, say — rather than returning an error, and a panic took `exec-test` down
/// with it; in a loader it would have been a crash where a refusal is due. So a panic here is
/// the stream's fault, and refused as one (`a_hostile_schema_is_refused_not_a_panic`). The
/// conversion only reads the message, so nothing is left half-changed by the unwind.
fn schema_of(fb: arrow::ipc::Schema) -> Result<arrow::datatypes::Schema, Fault> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        arrow::ipc::convert::fb_to_schema(fb)
    }))
    .map_err(|_| {
        Fault::host(
            "E_HOST_PROTOCOL",
            "bad Arrow stream: its schema names a type that cannot be built",
        )
    })
}

/// Read exactly `buf.len()` bytes, or `Ok(false)` at a clean end before the first one.
fn read_all_or_nothing<R: Read>(r: &mut R, buf: &mut [u8]) -> Result<bool, Fault> {
    let mut got = 0;
    while got < buf.len() {
        match r.read(&mut buf[got..]) {
            Ok(0) if got == 0 => return Ok(false),
            Ok(0) => {
                return Err(io_fault(std::io::Error::from(
                    std::io::ErrorKind::UnexpectedEof,
                )))
            }
            Ok(n) => got += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(io_fault(e)),
        }
    }
    Ok(true)
}

/// The next message, or `None` where the stream stops without its end-of-stream marker — which
/// the caller decides about: the marker is what tells finished from cut short.
pub fn next<R: Read>(r: &mut R, decoded: &mut Decoded) -> Result<Option<Message>, Fault> {
    let mut word = [0u8; 4];
    if !read_all_or_nothing(r, &mut word)? {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&word);
    // A continuation marker, then the length; or, from writers before Arrow 0.15, the length.
    let len = if word == [0xFF; 4] {
        if !read_all_or_nothing(r, &mut word)? {
            return Err(io_fault(std::io::ErrorKind::UnexpectedEof.into()));
        }
        bytes.extend_from_slice(&word);
        i32::from_le_bytes(word)
    } else {
        i32::from_le_bytes(word)
    };
    if len == 0 {
        return Ok(Some(Message {
            bytes,
            end: true,
            schema: None,
        }));
    }
    if len < 0 || len as usize > MAX_METADATA {
        return Err(Fault::host(
            "E_HOST_PROTOCOL",
            format!("bad Arrow stream: a message's metadata claims {len} bytes"),
        ));
    }
    let meta_at = bytes.len();
    bytes.resize(meta_at + len as usize, 0);
    if !read_all_or_nothing(r, &mut bytes[meta_at..])? {
        return Err(io_fault(std::io::ErrorKind::UnexpectedEof.into()));
    }
    let meta = arrow::ipc::root_as_message(&bytes[meta_at..]).map_err(|e| {
        Fault::host(
            "E_HOST_PROTOCOL",
            format!("bad Arrow stream: unreadable message metadata: {e}"),
        )
    })?;
    let body_len = meta.bodyLength();
    if body_len < 0 || body_len as u64 > decoded.max {
        // Before anything is allocated for it: arrow's reader allocates what this says.
        return Err(Fault::host(
            "E_HOST_OVERSIZE",
            format!(
                "a message claims a body of {body_len} bytes, more than the {} the stream may be",
                decoded.max
            ),
        ));
    }
    let schema = match meta.header_as_schema() {
        Some(fb) => Some(schema_of(fb)?),
        None => None,
    };
    let batch = meta
        .header_as_record_batch()
        .or_else(|| meta.header_as_dictionary_batch().and_then(|d| d.data()));
    let compressed = batch
        .as_ref()
        .and_then(|b| b.compression())
        .map(|c| c.codec());
    let buffers: Vec<(i64, i64)> = batch
        .as_ref()
        .and_then(|b| b.buffers())
        .map(|bs| bs.iter().map(|b| (b.offset(), b.length())).collect())
        .unwrap_or_default();
    // Where, in the metadata, the buffers' places and the body's length are kept, to be
    // rewritten once a compressed body is decoded: a vector of fixed-size structs, and a scalar
    // of the message's table.
    let metadata = &bytes[meta_at..];
    let buffers_at = batch
        .as_ref()
        .and_then(|b| b.buffers())
        .map(|bs| bs.bytes().as_ptr() as usize - metadata.as_ptr() as usize);
    let body_len_at = match meta._tab.vtable().get(arrow::ipc::Message::VT_BODYLENGTH) {
        0 => None,
        o => Some(meta._tab.loc() + o as usize),
    };
    if let Some(codec) = compressed {
        if codec != arrow::ipc::CompressionType::LZ4_FRAME {
            return Err(Fault::host(
                "E_HOST_COMPRESSED",
                format!(
                    "the stream's bodies are compressed with {:?}, and only LZ4 is decoded here. \
                     Send it uncompressed or with LZ4 — from ClickHouse, \
                     `SETTINGS output_format_arrow_compression_method = 'lz4_frame'` or 'none'",
                    codec
                ),
            ));
        }
    }
    let at = bytes.len();
    bytes.resize(at + body_len as usize, 0);
    if body_len > 0 && !read_all_or_nothing(r, &mut bytes[at..])? {
        return Err(io_fault(std::io::ErrorKind::UnexpectedEof.into()));
    }
    if compressed.is_none() {
        decoded.add(body_len as u64)?;
        return Ok(Some(Message {
            bytes,
            end: false,
            schema,
        }));
    }
    let bad = |why: &str| Fault::host("E_HOST_PROTOCOL", format!("bad Arrow stream: {why}"));
    let body = &bytes[at..];
    // What the buffers claim, for the memory they will take: reserved at once when the bound
    // admits it all, so the decoded body is not moved as it grows.
    let claimed: u64 = buffers
        .iter()
        .filter_map(|(o, l)| {
            body.get(usize::try_from(*o).ok()?..)?
                .get(..8.min(*l as usize))
        })
        .filter_map(|b| b.try_into().ok().map(i64::from_le_bytes))
        .map(|n| n.max(0) as u64 + 16)
        .sum();
    let left = decoded.max.saturating_sub(decoded.bytes);
    let mut out = Vec::with_capacity(at + if claimed <= left { claimed as usize } else { 0 });
    out.extend_from_slice(&bytes[..at]);
    let mut placed = Vec::with_capacity(buffers.len());
    for (offset, length) in buffers {
        // Each buffer from an 8-byte boundary of the body, as arrow's writer places them.
        out.resize(at + (out.len() - at).next_multiple_of(8), 0);
        let start = out.len() - at;
        if length != 0 {
            let buffer = usize::try_from(offset)
                .ok()
                .zip(usize::try_from(length).ok())
                .and_then(|(o, l)| body.get(o..o.checked_add(l)?))
                .ok_or_else(|| bad("a buffer lies outside its message's body"))?;
            count_compressed(buffer, decoded, &mut out)?;
        }
        placed.push((start, out.len() - at - start));
    }
    out.resize(at + (out.len() - at).next_multiple_of(8), 0);
    let new_len = out.len() - at;
    let put = |out: &mut Vec<u8>, p: usize, v: usize| {
        out[meta_at + p..meta_at + p + 8].copy_from_slice(&(v as i64).to_le_bytes())
    };
    if let Some(p) = buffers_at {
        for (k, (start, len)) in placed.into_iter().enumerate() {
            put(&mut out, p + 16 * k, start);
            put(&mut out, p + 16 * k + 8, len);
        }
    }
    match body_len_at {
        Some(p) => put(&mut out, p, new_len),
        None if new_len == 0 => {}
        None => return Err(bad("a message with buffers states no body")),
    }
    Ok(Some(Message {
        bytes: out,
        end: false,
        schema,
    }))
}

/// Count one compressed buffer against what the stream has left, and append it to `out` decoded,
/// stored as the format stores an uncompressed buffer: prefixed `-1`. A claim past what is left
/// is refused by the bound whatever the buffer holds, so it is refused on the claim: checking it
/// would decompress up to 255 times the buffer, for a stream already over
/// (`a_claim_past_what_is_left_is_refused_before_it_is_decoded`).
fn count_compressed(buffer: &[u8], decoded: &mut Decoded, out: &mut Vec<u8>) -> Result<(), Fault> {
    let claim = buffer
        .get(..8)
        .map(|b| i64::from_le_bytes(b.try_into().unwrap()));
    if let Some(n) = claim.filter(|n| *n > 0) {
        if n as u64 > decoded.max.saturating_sub(decoded.bytes) {
            return decoded.add(n as u64);
        }
    }
    decoded.add(decode_into(buffer, out)?)
}

/// Decode one compressed buffer onto the end of `out`, prefixed `-1`, and say what it decoded to:
/// its 8-byte claim, checked. A buffer stored rather than compressed is what follows its prefix,
/// and an empty one appends nothing. A compressed one must decompress to exactly its claim, which
/// is found out by decompressing it and stopping one byte past the claim — so a lie costs what it
/// decoded until then, and no more than its claim.
fn decode_into(buffer: &[u8], out: &mut Vec<u8>) -> Result<u64, Fault> {
    let bad = |why: &str| Fault::host("E_HOST_PROTOCOL", format!("bad Arrow stream: {why}"));
    let claim = buffer
        .get(..8)
        .map(|b| i64::from_le_bytes(b.try_into().unwrap()))
        .ok_or_else(|| bad("a compressed buffer is shorter than its length prefix"))?;
    match claim {
        -1 => {
            out.extend_from_slice(buffer);
            Ok(buffer.len() as u64 - 8)
        }
        0 => Ok(0),
        n if n < 0 => Err(bad("a compressed buffer claims a negative length")),
        n => {
            let n = n as u64;
            out.extend_from_slice(&(-1i64).to_le_bytes());
            let mut frame = lz4_flex::frame::FrameDecoder::new(&buffer[8..]).take(n + 1);
            let got = frame
                .read_to_end(out)
                .map_err(|e| bad(&format!("an LZ4 frame does not decode: {e}")))?
                as u64;
            if got != n {
                return Err(bad(&format!(
                    "a compressed buffer claims {n} bytes and decodes to {}",
                    if got > n {
                        "more".to_string()
                    } else {
                        got.to_string()
                    }
                )));
            }
            Ok(n)
        }
    }
}

/// One stream from a channel that carries many back to back: a host's stream channel
/// (docs/HOST-PROTOCOL.md, "Through a stream channel"). Reads pass through untouched, but never
/// past the message being read, and once the stream's end-of-stream marker has passed, this reads
/// as the end of its input. The next stream stays in the channel for the next attach.
///
/// A pipe cannot be read back, so nothing may be read ahead: the messages are followed as their
/// bytes pass — each one's length, its metadata, the body length the metadata states — and a read
/// is held to what is left of the part being read. [`next`] reads exactly a message at a time
/// already, so this costs no read of its own: one copy of each message's metadata, which is a few
/// hundred bytes, and one parse of it, where `next` makes the same.
///
/// When a stream is refused part-way, [`Framed::drain`] reads it on to its marker and drops it,
/// so the channel stays at a boundary between streams. Clones share one position: the loader
/// keeps one to drain with after the reader it handed out has been dropped.
pub struct Framed<R>(std::rc::Rc<std::cell::RefCell<Frame<R>>>);

impl<R> Clone for Framed<R> {
    fn clone(&self) -> Self {
        Framed(self.0.clone())
    }
}

pub struct Frame<R> {
    inner: R,
    at: At,
    /// The message's prefix: a continuation marker and a length, or, before Arrow 0.15, a length.
    head: [u8; 8],
    have: usize,
    meta: Vec<u8>,
    meta_len: usize,
    body_left: u64,
    /// The input ended.
    eof: bool,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum At {
    Prefix,
    Metadata,
    Body,
    /// The end-of-stream marker has passed.
    Ended,
    /// What passed was no message: the reader above refuses it too, and nothing can find the end.
    Lost,
}

impl<R: Read> Framed<R> {
    pub fn new(inner: R) -> Self {
        Framed(std::rc::Rc::new(std::cell::RefCell::new(Frame {
            inner,
            at: At::Prefix,
            head: [0; 8],
            have: 0,
            meta: Vec::new(),
            meta_len: 0,
            body_left: 0,
            eof: false,
        })))
    }

    /// Whether the stream's end-of-stream marker has been read, and nothing after it.
    pub fn ended(&self) -> bool {
        self.0.borrow().at == At::Ended
    }

    /// Read the rest of the stream, to its end-of-stream marker, and drop it: at most `limit`
    /// bytes. `Err` says why its end could not be found — the input ended first, what came was no
    /// message, or more than `limit` bytes were left — and then where the next stream starts is
    /// lost. Nothing is decoded: each message's body is read past as it comes.
    pub fn drain(&self, limit: u64) -> Result<u64, String> {
        let mut f = self.0.borrow_mut();
        // A pipe hands over at most 64 KiB a read.
        let mut scratch = vec![0u8; 64 * 1024];
        let mut drained = 0u64;
        loop {
            match f.at {
                At::Ended => return Ok(drained),
                At::Lost => {
                    return Err(
                        "what came is not a sequence of Arrow messages, so its end-of-stream \
                         marker cannot be found"
                            .into(),
                    )
                }
                _ => {}
            }
            // One byte past the limit is how a rest larger than the limit is told.
            let room = (limit - drained.min(limit) + 1).min(scratch.len() as u64) as usize;
            match f.read(&mut scratch[..room]) {
                Ok(0) => {
                    return Err(
                        "the channel ended before the stream's end-of-stream marker; a host that \
                         stops part-way through a stream closes the channel with it"
                            .into(),
                    )
                }
                Ok(n) => {
                    drained += n as u64;
                    if drained > limit {
                        return Err(format!(
                            "more than {limit} bytes of the refused stream were left to read past"
                        ));
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(e) => return Err(format!("the channel could not be read: {}", e.kind())),
            }
        }
    }
}

impl<R: Read> Read for Framed<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().read(buf)
    }
}

impl<R: Read> Frame<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let left = match self.at {
            At::Ended => return Ok(0),
            At::Lost => {
                return Err(std::io::Error::other(
                    "the stream is not a sequence of Arrow messages",
                ))
            }
            // Four bytes, then four more where the first four are a continuation marker.
            At::Prefix if self.have < 4 => 4 - self.have,
            At::Prefix => 8 - self.have,
            At::Metadata => self.meta_len - self.meta.len(),
            At::Body => usize::try_from(self.body_left).unwrap_or(usize::MAX),
        };
        let want = left.min(buf.len());
        if want == 0 {
            return Ok(0);
        }
        let n = self.inner.read(&mut buf[..want])?;
        if n == 0 {
            self.eof = true;
            return Ok(0);
        }
        self.passed(&buf[..n]);
        Ok(n)
    }

    /// Follow the stream's messages through the bytes just read.
    fn passed(&mut self, got: &[u8]) {
        match self.at {
            At::Prefix => {
                self.head[self.have..self.have + got.len()].copy_from_slice(got);
                self.have += got.len();
                let len = match self.have {
                    4 if self.head[..4] != [0xFF; 4] => &self.head[..4],
                    8 => &self.head[4..8],
                    _ => return,
                };
                let len = i32::from_le_bytes(len.try_into().expect("four bytes"));
                self.at = if len == 0 {
                    At::Ended
                } else if len < 0 || len as usize > MAX_METADATA {
                    At::Lost
                } else {
                    self.meta.clear();
                    self.meta_len = len as usize;
                    At::Metadata
                };
            }
            At::Metadata => {
                self.meta.extend_from_slice(got);
                if self.meta.len() < self.meta_len {
                    return;
                }
                let body = arrow::ipc::root_as_message(&self.meta)
                    .map(|m| m.bodyLength())
                    .unwrap_or(-1);
                self.have = 0;
                self.at = match body {
                    b if b < 0 => At::Lost,
                    0 => At::Prefix,
                    b => {
                        self.body_left = b as u64;
                        At::Body
                    }
                };
            }
            At::Body => {
                self.body_left -= got.len() as u64;
                if self.body_left == 0 {
                    self.have = 0;
                    self.at = At::Prefix;
                }
            }
            At::Ended | At::Lost => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `rows` rows of one int64 column as an Arrow stream, in batches of `batch` rows.
    fn ints(rows: i64, batch: i64) -> Vec<u8> {
        use arrow::array::{ArrayRef, Int64Array, RecordBatch};
        let schema = std::sync::Arc::new(arrow::datatypes::Schema::new(vec![
            arrow::datatypes::Field::new("v", arrow::datatypes::DataType::Int64, false),
        ]));
        let mut out = Vec::new();
        let mut w = arrow::ipc::writer::StreamWriter::try_new(&mut out, &schema).unwrap();
        let mut at = 0;
        while at < rows {
            let n = batch.min(rows - at);
            let col = std::sync::Arc::new(Int64Array::from((at..at + n).collect::<Vec<_>>()));
            w.write(&RecordBatch::try_new(schema.clone(), vec![col as ArrayRef]).unwrap())
                .unwrap();
            at += n;
        }
        w.finish().unwrap();
        drop(w);
        out
    }

    #[test]
    fn a_channel_reader_stops_at_each_streams_end_marker() {
        // Two streams back to back, read in reads of every size: each reader gives exactly its
        // stream's bytes and then the end, and the next one starts where it stopped.
        let (a, b) = (ints(1000, 100), ints(5, 5));
        let mut both = a.clone();
        both.extend(&b);
        for size in [1, 3, 7, 8, 64, 4096, 1 << 20] {
            let mut input = std::io::Cursor::new(both.clone());
            for want in [&a, &b] {
                let framed = Framed::new(&mut input);
                let mut got = Vec::new();
                let mut buf = vec![0u8; size];
                let mut r = framed.clone();
                loop {
                    let n = r.read(&mut buf).unwrap();
                    if n == 0 {
                        break;
                    }
                    got.extend_from_slice(&buf[..n]);
                }
                assert!(framed.ended(), "{size}");
                assert_eq!(&got, want, "reads of {size}");
            }
            assert_eq!(input.position() as usize, both.len());
        }
    }

    #[test]
    fn a_refused_stream_is_drained_to_its_marker_within_a_bound() {
        let (a, b) = (ints(10_000, 1000), ints(5, 5));
        let mut both = a.clone();
        both.extend(&b);
        // Stopped part-way through a body: the rest is read past, and the next stream is next.
        let mut input = std::io::Cursor::new(both.clone());
        let framed = Framed::new(&mut input);
        let mut part = vec![0u8; 1000];
        framed.clone().read_exact(&mut part).unwrap();
        assert_eq!(framed.drain(a.len() as u64), Ok(a.len() as u64 - 1000));
        assert_eq!(input.position() as usize, a.len());
        // Exactly the bound fits; one byte less does not.
        let mut input = std::io::Cursor::new(both.clone());
        let rest = a.len() as u64;
        assert_eq!(Framed::new(&mut input).drain(rest), Ok(rest));
        let mut input = std::io::Cursor::new(both.clone());
        assert!(Framed::new(&mut input).drain(rest - 1).is_err());
        // The input ends before the marker; what came is no message.
        let mut cut = std::io::Cursor::new(a[..a.len() - 8].to_vec());
        assert!(Framed::new(&mut cut).drain(u64::MAX / 2).is_err());
        let mut csv = std::io::Cursor::new(b"id,meter\n1,lab\n".to_vec());
        assert!(Framed::new(&mut csv).drain(u64::MAX / 2).is_err());
    }

    fn lz4(data: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut enc = lz4_flex::frame::FrameEncoder::new(Vec::new());
        enc.write_all(data).unwrap();
        enc.finish().unwrap()
    }

    fn claiming(n: i64, frame: &[u8]) -> Vec<u8> {
        let mut b = n.to_le_bytes().to_vec();
        b.extend_from_slice(frame);
        b
    }

    #[test]
    fn a_compressed_buffer_counts_what_it_decodes_to_and_may_not_lie() {
        let data = vec![7u8; 300 * 1024];
        let frame = lz4(&data);
        assert!(frame.len() < 4096, "it compresses: {}", frame.len());
        let decoded_size = |b: &[u8]| {
            let mut out = Vec::new();
            let n = decode_into(b, &mut out)?;
            // What is appended is the buffer as arrow takes it uncompressed: prefixed -1.
            assert_eq!(out.get(..8), (n > 0).then_some(&(-1i64).to_le_bytes()[..]));
            assert_eq!(out.len() as u64, if n > 0 { n + 8 } else { 0 });
            Ok::<_, Fault>(n)
        };
        assert_eq!(
            decoded_size(&claiming(data.len() as i64, &frame)).unwrap(),
            data.len() as u64
        );
        // A claim below what the frame holds: arrow would allocate the claim and decode past it.
        assert!(decoded_size(&claiming(1024, &frame)).is_err());
        // And it is decoded only one byte past its claim: a bomb costs what it claimed.
        let mut out = Vec::new();
        assert!(decode_into(&claiming(1024, &frame), &mut out).is_err());
        assert!(out.len() <= 8 + 1025, "{}", out.len());
        // Above it: arrow would allocate the claim, which counts against the bound for nothing.
        assert!(decoded_size(&claiming(1 << 40, &frame)).is_err());
        assert!(decoded_size(&claiming(data.len() as i64, &frame[..frame.len() / 2])).is_err());
        assert_eq!(decoded_size(&claiming(-1, b"stored")).unwrap(), 6);
        assert_eq!(decoded_size(&claiming(0, b"")).unwrap(), 0);
    }

    #[test]
    fn a_claim_past_what_is_left_is_refused_before_it_is_decoded() {
        // Checking a claim decompresses the buffer, which costs CPU in proportion to the claim —
        // up to 255 times the buffer. A claim past what the stream has left is refused by the
        // bound whatever the buffer holds, so it is refused before anything is decompressed:
        // here the frame does not decode at all, and the bound is still what answers.
        let mut decoded = Decoded::new(1024);
        decoded.bytes = 1000;
        let mut out = Vec::new();
        let f = count_compressed(&claiming(25, b"not an lz4 frame"), &mut decoded, &mut out)
            .unwrap_err();
        assert_eq!(f.code, "E_HOST_OVERSIZE", "{}", f.detail);
        assert!(out.is_empty(), "nothing was decoded");
        // Within it, the frame is decompressed and the lie found.
        decoded.bytes = 1000;
        let f = count_compressed(&claiming(24, b"not an lz4 frame"), &mut decoded, &mut out)
            .unwrap_err();
        assert_eq!(f.code, "E_HOST_PROTOCOL", "{}", f.detail);
    }

    #[test]
    fn a_compressed_message_reaches_arrow_decoded_once() {
        // LZ4 bodies are decoded here, to check their claims, and handed to arrow as stored
        // buffers, which arrow takes as they are: decoded once, not here and again in arrow.
        use arrow::array::{Array, ArrayRef, Float64Array, Int64Array, RecordBatch, StringArray};
        use std::sync::Arc;
        let n = 10_000;
        let batch = RecordBatch::try_from_iter([
            (
                "id",
                Arc::new(Int64Array::from_iter_values(0..n)) as ArrayRef,
            ),
            (
                "meter",
                Arc::new(StringArray::from_iter(
                    (0..n).map(|i| (i % 7 != 3).then(|| format!("m{}", i % 50))),
                )) as ArrayRef,
            ),
            (
                "kw",
                Arc::new(Float64Array::from_iter(
                    (0..n).map(|i| (i % 11 != 0).then_some(i as f64 * 0.5)),
                )) as ArrayRef,
            ),
        ])
        .unwrap();
        let options = arrow::ipc::writer::IpcWriteOptions::default()
            .try_with_compression(Some(arrow::ipc::CompressionType::LZ4_FRAME))
            .unwrap();
        let mut raw = Vec::new();
        {
            let mut w = arrow::ipc::writer::StreamWriter::try_new_with_options(
                &mut raw,
                &batch.schema(),
                options,
            )
            .unwrap();
            w.write(&batch).unwrap();
            w.write(&batch.slice(100, 5_000)).unwrap();
            w.finish().unwrap();
        }
        let mut r = raw.as_slice();
        let mut decoded = Decoded::new(1 << 30);
        let mut decoder = arrow::ipc::reader::StreamDecoder::new();
        let (mut got, mut stored) = (Vec::new(), 0);
        while let Some(m) = next(&mut r, &mut decoded).unwrap() {
            let meta_len = i32::from_le_bytes(m.bytes[4..8].try_into().unwrap()) as usize;
            if !m.end {
                let meta = arrow::ipc::root_as_message(&m.bytes[8..8 + meta_len]).unwrap();
                let body = &m.bytes[8 + meta_len..];
                assert_eq!(meta.bodyLength() as usize, body.len());
                if let Some(b) = meta.header_as_record_batch() {
                    for buf in b.buffers().unwrap() {
                        let (o, l) = (buf.offset() as usize, buf.length() as usize);
                        if l > 0 {
                            assert_eq!(o % 8, 0, "aligned");
                            assert_eq!(body[o..o + 8], (-1i64).to_le_bytes(), "stored");
                            stored += 1;
                        }
                    }
                }
            }
            let mut buffer = arrow::buffer::Buffer::from_vec(m.bytes);
            while !buffer.is_empty() {
                if let Some(b) = decoder.decode(&mut buffer).unwrap() {
                    got.push(b);
                }
            }
        }
        decoder.finish().unwrap();
        assert!(stored >= 10, "{stored}");
        assert_eq!(got, vec![batch.clone(), batch.slice(100, 5_000)]);
        assert!(got[0].column(1).null_count() > 0);
        // What it decodes to is what was counted.
        let plain: u64 = got.iter().map(|b| b.get_array_memory_size() as u64).sum();
        assert!(
            decoded.bytes > 0 && decoded.bytes <= plain,
            "{} {plain}",
            decoded.bytes
        );
    }

    #[test]
    fn a_hostile_schema_is_refused_not_a_panic() {
        // arrow's schema conversion panics on a type it cannot build — an integer 7 bits wide,
        // say — and a panic took `exec-test` down with it. Every 32-bit 64 in a schema's
        // metadata is set to 7 in turn; one of them is the Int64 column's bit width.
        use arrow::array::{ArrayRef, Int64Array, RecordBatch};
        let batch = RecordBatch::try_from_iter([(
            "id",
            std::sync::Arc::new(Int64Array::from(vec![1])) as ArrayRef,
        )])
        .unwrap();
        let mut raw = Vec::new();
        {
            let mut w =
                arrow::ipc::writer::StreamWriter::try_new(&mut raw, &batch.schema()).unwrap();
            w.write(&batch).unwrap();
            w.finish().unwrap();
        }
        let len = u32::from_le_bytes(raw[4..8].try_into().unwrap()) as usize;
        let mut refused = 0;
        for at in 8..8 + len - 3 {
            if raw[at..at + 4] != 64u32.to_le_bytes() {
                continue;
            }
            let mut hostile = raw.clone();
            hostile[at..at + 4].copy_from_slice(&7u32.to_le_bytes());
            if let Err(f) = next(&mut hostile.as_slice(), &mut Decoded::new(1 << 20)) {
                assert_eq!(f.code, "E_HOST_PROTOCOL", "{}", f.detail);
                refused += 1;
            }
        }
        assert!(refused > 0, "the bit width was found and refused");
    }
}
