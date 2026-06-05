//! AWS `application/vnd.amazon.eventstream` incremental decoder
//! (WP-2 per `md/bedrock-converse-design.md`).
//!
//! Wire format (authoritative: smithy.io amazon-eventstream spec), all integers
//! big-endian:
//!
//! ```text
//! struct {
//!     uint32 total_length;     // whole message incl. both CRCs
//!     uint32 headers_length;   // byte length of headers section
//!     uint32 prelude_crc;      // CRC32 of the first 8 bytes (total+headers len)
//!     byte   headers[headers_length];
//!     byte   payload[total_length - headers_length - 12 - 4];
//!     uint32 message_crc;      // CRC32 of every byte from total_length .. payload end
//! } Message;
//! ```
//!
//! Header: `u8 name_len`, `name` (UTF-8), `u8 type`, value. Type indicators:
//! `0`=bool true (no bytes), `1`=bool false, `2`=byte, `3`=short, `4`=int,
//! `5`=long, `6`=byte_array (`u16` len prefix), `7`=string (`u16` len prefix +
//! UTF-8), `8`=timestamp (i64), `9`=uuid (16 bytes). We only retain string
//! (type 7) values for `:message-type` / `:event-type` / `:content-type` /
//! `:exception-type`; all other header types are length-skipped.
//!
//! The decoder is pure (no async, no I/O): feed wire chunks via [`EventStreamDecoder::feed`]
//! and pull complete, CRC-validated frames via [`EventStreamDecoder::next_frame`].
//! A single eventstream message may split across arbitrary chunk boundaries, so
//! `next_frame` returns `Ok(None)` until a whole message is buffered. CRC and
//! length violations are fatal for the stream (per spec the stream MUST terminate).

// Wired into the proxy in WP-8 (handlers re-framer); standalone here.
#![allow(dead_code)]

/// Smallest legal message: 12-byte prelude + 4-byte trailing CRC, zero headers,
/// zero payload.
const MIN_FRAME_LEN: u32 = 16;

/// Oversized-frame guard. Converse payloads are bounded (payload ≤ 24 MB per
/// spec), but for a privacy gateway we cap the *whole message* well below that
/// to bound buffer growth from a hostile or corrupt upstream. 16 MiB.
const MAX_FRAME_LEN: u32 = 16 * 1024 * 1024;

/// One decoded message: the routing headers we care about + the raw payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventFrame {
    /// `:message-type` header — `"event"` or `"exception"`.
    pub message_type: Option<String>,
    /// `:event-type` header — e.g. `"contentBlockDelta"`.
    pub event_type: Option<String>,
    /// `:content-type` header — usually `"application/json"`.
    pub content_type: Option<String>,
    /// `:exception-type` header — present on exception messages.
    pub exception_type: Option<String>,
    /// Raw message payload bytes (JSON for Converse events).
    pub payload: Vec<u8>,
}

/// Fatal decode failures. Any of these means the stream must terminate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    /// The 4-byte prelude CRC did not match CRC32 of the first 8 bytes.
    PreludeCrc,
    /// The 4-byte trailing message CRC did not match CRC32 of the message body.
    MessageCrc,
    /// `total_length` / `headers_length` are internally inconsistent or below
    /// the minimum legal frame size.
    BadLength,
    /// A header could not be parsed (bad bounds, non-UTF-8 string, unknown type).
    BadHeader,
    /// `total_length` exceeds [`MAX_FRAME_LEN`].
    Oversized,
    /// Not an error: more bytes are required before a frame can be produced.
    /// `next_frame` surfaces this condition as `Ok(None)`, so this variant is
    /// retained for API completeness but never returned in the `Err` channel.
    Truncated,
}

impl core::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let s = match self {
            DecodeError::PreludeCrc => "eventstream prelude CRC32 mismatch",
            DecodeError::MessageCrc => "eventstream message CRC32 mismatch",
            DecodeError::BadLength => "eventstream frame has inconsistent length fields",
            DecodeError::BadHeader => "eventstream header block is malformed",
            DecodeError::Oversized => "eventstream frame exceeds size cap",
            DecodeError::Truncated => "eventstream frame is incomplete",
        };
        f.write_str(s)
    }
}

impl std::error::Error for DecodeError {}

/// Incremental decoder. Feed `reqwest` chunks via [`feed`](Self::feed); pull
/// complete frames via [`next_frame`](Self::next_frame).
#[derive(Debug, Default)]
pub struct EventStreamDecoder {
    buf: Vec<u8>,
    /// Set once a fatal error is observed; every subsequent `next_frame` returns
    /// it so a poisoned stream stays poisoned.
    poisoned: Option<DecodeError>,
}

impl EventStreamDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Append bytes received from the wire.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Pull the next complete, CRC-valid frame.
    ///
    /// Returns `Ok(None)` when more bytes are needed (a partial frame is
    /// buffered), `Ok(Some(frame))` for each complete frame, and `Err` on a
    /// CRC, length, size, or header violation. After an `Err` the decoder is
    /// poisoned and keeps returning the same error.
    pub fn next_frame(&mut self) -> Result<Option<EventFrame>, DecodeError> {
        if let Some(err) = &self.poisoned {
            return Err(err.clone());
        }
        match self.try_next_frame() {
            Err(err) => {
                self.poisoned = Some(err.clone());
                Err(err)
            }
            ok => ok,
        }
    }

    fn try_next_frame(&mut self) -> Result<Option<EventFrame>, DecodeError> {
        // 1. Need ≥ 12 bytes for the prelude.
        if self.buf.len() < 12 {
            return Ok(None);
        }

        let total_length = read_u32(&self.buf, 0);
        let headers_length = read_u32(&self.buf, 4);

        // Oversized guard before we ever wait for the full frame to buffer:
        // refuse to grow the buffer toward an absurd total_length.
        if total_length > MAX_FRAME_LEN {
            return Err(DecodeError::Oversized);
        }
        // Structural length sanity: a frame is prelude(12) + headers + payload +
        // trailing CRC(4). headers must fit in total_length - 16.
        if total_length < MIN_FRAME_LEN || headers_length > total_length - MIN_FRAME_LEN {
            return Err(DecodeError::BadLength);
        }

        // 2. Validate prelude CRC (over the first 8 bytes) before waiting for the
        // rest — a corrupt prelude is fatal regardless of how much we have.
        let prelude_crc = read_u32(&self.buf, 8);
        if crc32fast::hash(&self.buf[0..8]) != prelude_crc {
            return Err(DecodeError::PreludeCrc);
        }

        // Wait for the whole message.
        let total = total_length as usize;
        if self.buf.len() < total {
            return Ok(None);
        }

        // 3. Validate message CRC over bytes [0 .. total-4].
        let message_crc = read_u32(&self.buf, total - 4);
        if crc32fast::hash(&self.buf[0..total - 4]) != message_crc {
            return Err(DecodeError::MessageCrc);
        }

        // 4. Parse headers.
        let headers_start = 12usize;
        let headers_end = headers_start + headers_length as usize;
        let frame = parse_frame(&self.buf, headers_start, headers_end, total - 4)?;

        // 6. Consume the frame from the buffer.
        self.buf.drain(0..total);
        Ok(Some(frame))
    }

    /// Drain every frame currently decodable, in order. Stops at the first
    /// `Ok(None)` (need more bytes) and propagates the first `Err`.
    pub fn drain(&mut self) -> Result<Vec<EventFrame>, DecodeError> {
        let mut out = Vec::new();
        while let Some(frame) = self.next_frame()? {
            out.push(frame);
        }
        Ok(out)
    }
}

/// Read a big-endian u32 at `off`. Caller guarantees `buf.len() >= off + 4`.
#[inline]
fn read_u32(buf: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([buf[off], buf[off + 1], buf[off + 2], buf[off + 3]])
}

/// Parse the headers block `buf[headers_start..headers_end]` and slice the
/// payload `buf[headers_end..payload_end]` into an [`EventFrame`].
fn parse_frame(
    buf: &[u8],
    headers_start: usize,
    headers_end: usize,
    payload_end: usize,
) -> Result<EventFrame, DecodeError> {
    let mut message_type = None;
    let mut event_type = None;
    let mut content_type = None;
    let mut exception_type = None;

    let mut pos = headers_start;
    while pos < headers_end {
        // u8 name_len
        let name_len = *buf.get(pos).ok_or(DecodeError::BadHeader)? as usize;
        pos += 1;
        let name_end = pos.checked_add(name_len).ok_or(DecodeError::BadHeader)?;
        if name_end > headers_end {
            return Err(DecodeError::BadHeader);
        }
        let name = &buf[pos..name_end];
        pos = name_end;

        // u8 value type
        let value_type = *buf.get(pos).ok_or(DecodeError::BadHeader)?;
        pos += 1;

        // Value, by type. We only retain string (type 7) values; everything
        // else is length-skipped after bounds checking.
        match value_type {
            0 | 1 => {} // bool true / false — no value bytes
            2 => pos = advance(pos, 1, headers_end)?, // byte
            3 => pos = advance(pos, 2, headers_end)?, // short
            4 => pos = advance(pos, 4, headers_end)?, // int
            5 => pos = advance(pos, 8, headers_end)?, // long
            6 | 7 => {
                // byte_array / string: u16 big-endian length prefix + bytes.
                let len_end = advance(pos, 2, headers_end)?;
                let val_len = u16::from_be_bytes([buf[pos], buf[pos + 1]]) as usize;
                let val_start = len_end;
                let val_end = val_start
                    .checked_add(val_len)
                    .ok_or(DecodeError::BadHeader)?;
                if val_end > headers_end {
                    return Err(DecodeError::BadHeader);
                }
                if value_type == 7 {
                    let value =
                        core::str::from_utf8(&buf[val_start..val_end]).map_err(|_| DecodeError::BadHeader)?;
                    match name {
                        b":message-type" => message_type = Some(value.to_owned()),
                        b":event-type" => event_type = Some(value.to_owned()),
                        b":content-type" => content_type = Some(value.to_owned()),
                        b":exception-type" => exception_type = Some(value.to_owned()),
                        _ => {}
                    }
                }
                pos = val_end;
            }
            8 => pos = advance(pos, 8, headers_end)?,  // timestamp (i64 millis)
            9 => pos = advance(pos, 16, headers_end)?, // uuid
            _ => return Err(DecodeError::BadHeader),
        }
    }

    // Exact consumption: a well-formed header block ends precisely on the boundary.
    if pos != headers_end {
        return Err(DecodeError::BadHeader);
    }

    let payload = buf[headers_end..payload_end].to_vec();
    Ok(EventFrame {
        message_type,
        event_type,
        content_type,
        exception_type,
        payload,
    })
}

/// Advance `pos` by `n`, failing if it would run past `limit`.
#[inline]
fn advance(pos: usize, n: usize, limit: usize) -> Result<usize, DecodeError> {
    let next = pos.checked_add(n).ok_or(DecodeError::BadHeader)?;
    if next > limit {
        return Err(DecodeError::BadHeader);
    }
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- fixture builders -------------------------------------------------

    /// Encode one header: `u8 name_len`, name, `u8 type=7`, `u16 len`, value.
    fn string_header(name: &str, value: &str) -> Vec<u8> {
        let mut h = Vec::new();
        h.push(name.len() as u8);
        h.extend_from_slice(name.as_bytes());
        h.push(7u8); // string type
        h.extend_from_slice(&(value.len() as u16).to_be_bytes());
        h.extend_from_slice(value.as_bytes());
        h
    }

    /// Encode a bool header (type 0 true / 1 false): `u8 name_len`, name, `u8 type`.
    fn bool_header(name: &str, value: bool) -> Vec<u8> {
        let mut h = Vec::new();
        h.push(name.len() as u8);
        h.extend_from_slice(name.as_bytes());
        h.push(if value { 0u8 } else { 1u8 });
        h
    }

    /// Encode an int header (type 4): `u8 name_len`, name, `u8 type=4`, i32 BE.
    fn int_header(name: &str, value: i32) -> Vec<u8> {
        let mut h = Vec::new();
        h.push(name.len() as u8);
        h.extend_from_slice(name.as_bytes());
        h.push(4u8);
        h.extend_from_slice(&value.to_be_bytes());
        h
    }

    /// Assemble a complete, CRC-correct eventstream message from a headers block
    /// and a payload. CRCs are computed with `crc32fast` so fixtures are
    /// self-consistent by construction.
    fn frame(headers: &[u8], payload: &[u8]) -> Vec<u8> {
        let headers_len = headers.len() as u32;
        let total_len = 12 + headers_len + payload.len() as u32 + 4;

        let mut msg = Vec::with_capacity(total_len as usize);
        msg.extend_from_slice(&total_len.to_be_bytes());
        msg.extend_from_slice(&headers_len.to_be_bytes());
        let prelude_crc = crc32fast::hash(&msg[0..8]);
        msg.extend_from_slice(&prelude_crc.to_be_bytes());
        msg.extend_from_slice(headers);
        msg.extend_from_slice(payload);
        let message_crc = crc32fast::hash(&msg);
        msg.extend_from_slice(&message_crc.to_be_bytes());

        debug_assert_eq!(msg.len() as u32, total_len);
        msg
    }

    /// Standard Converse-style event frame: :event-type + :content-type +
    /// :message-type=event, JSON payload.
    fn event_frame(event_type: &str, payload: &str) -> Vec<u8> {
        let mut headers = Vec::new();
        headers.extend_from_slice(&string_header(":event-type", event_type));
        headers.extend_from_slice(&string_header(":content-type", "application/json"));
        headers.extend_from_slice(&string_header(":message-type", "event"));
        frame(&headers, payload.as_bytes())
    }

    // ---- happy path -------------------------------------------------------

    #[test]
    fn single_frame_decodes() {
        let wire = event_frame("contentBlockDelta", r#"{"delta":{"text":"hi"}}"#);
        let mut dec = EventStreamDecoder::new();
        dec.feed(&wire);

        let f = dec.next_frame().unwrap().expect("a frame");
        assert_eq!(f.event_type.as_deref(), Some("contentBlockDelta"));
        assert_eq!(f.message_type.as_deref(), Some("event"));
        assert_eq!(f.content_type.as_deref(), Some("application/json"));
        assert_eq!(f.exception_type, None);
        assert_eq!(f.payload, br#"{"delta":{"text":"hi"}}"#);

        // Buffer fully consumed.
        assert_eq!(dec.next_frame().unwrap(), None);
    }

    #[test]
    fn empty_payload_and_no_headers() {
        let wire = frame(&[], &[]);
        let mut dec = EventStreamDecoder::new();
        dec.feed(&wire);
        let f = dec.next_frame().unwrap().expect("a frame");
        assert_eq!(f, EventFrame {
            message_type: None,
            event_type: None,
            content_type: None,
            exception_type: None,
            payload: Vec::new(),
        });
    }

    #[test]
    fn multi_event_happy_path() {
        let mut wire = Vec::new();
        wire.extend_from_slice(&event_frame("messageStart", r#"{"role":"assistant"}"#));
        wire.extend_from_slice(&event_frame("contentBlockDelta", r#"{"delta":{"text":"Hello"}}"#));
        wire.extend_from_slice(&event_frame("contentBlockDelta", r#"{"delta":{"text":", world"}}"#));
        wire.extend_from_slice(&event_frame("contentBlockStop", "{}"));
        wire.extend_from_slice(&event_frame("messageStop", r#"{"stopReason":"end_turn"}"#));
        wire.extend_from_slice(&event_frame(
            "metadata",
            r#"{"usage":{"inputTokens":10,"outputTokens":5,"totalTokens":15}}"#,
        ));

        let mut dec = EventStreamDecoder::new();
        dec.feed(&wire);
        let frames = dec.drain().unwrap();

        let types: Vec<&str> = frames
            .iter()
            .map(|f| f.event_type.as_deref().unwrap())
            .collect();
        assert_eq!(
            types,
            vec![
                "messageStart",
                "contentBlockDelta",
                "contentBlockDelta",
                "contentBlockStop",
                "messageStop",
                "metadata",
            ]
        );
        assert_eq!(frames[1].payload, br#"{"delta":{"text":"Hello"}}"#);
        assert_eq!(dec.next_frame().unwrap(), None);
    }

    // ---- byte-split invariance -------------------------------------------

    #[test]
    fn byte_split_invariance_one_byte_at_a_time() {
        let mut wire = Vec::new();
        wire.extend_from_slice(&event_frame("messageStart", r#"{"role":"assistant"}"#));
        wire.extend_from_slice(&event_frame("contentBlockDelta", r#"{"delta":{"text":"split me"}}"#));
        wire.extend_from_slice(&event_frame("messageStop", r#"{"stopReason":"end_turn"}"#));

        // Reference: all at once.
        let mut whole = EventStreamDecoder::new();
        whole.feed(&wire);
        let expected = whole.drain().unwrap();
        assert_eq!(expected.len(), 3);

        // Feed one byte at a time, draining after each push. Result must match.
        let mut dec = EventStreamDecoder::new();
        let mut got = Vec::new();
        for b in &wire {
            dec.feed(&[*b]);
            while let Some(f) = dec.next_frame().unwrap() {
                got.push(f);
            }
        }
        assert_eq!(got, expected);
    }

    #[test]
    fn byte_split_invariance_every_boundary() {
        let wire = event_frame("contentBlockDelta", r#"{"delta":{"text":"boundary"}}"#);

        let mut whole = EventStreamDecoder::new();
        whole.feed(&wire);
        let expected = whole.next_frame().unwrap().unwrap();

        // Split at every possible boundary; both halves fed separately must
        // still yield exactly the same single frame.
        for split in 0..=wire.len() {
            let mut dec = EventStreamDecoder::new();
            dec.feed(&wire[..split]);
            // Before the full frame is present, must be Ok(None).
            if split < wire.len() {
                assert_eq!(dec.next_frame().unwrap(), None, "premature frame at split {split}");
            }
            dec.feed(&wire[split..]);
            let f = dec.next_frame().unwrap().expect("frame after full feed");
            assert_eq!(f, expected, "mismatch at split {split}");
            assert_eq!(dec.next_frame().unwrap(), None);
        }
    }

    // ---- header type-skipping --------------------------------------------

    #[test]
    fn non_string_headers_are_skipped() {
        // Interleave bool / int headers with the string ones we care about.
        let mut headers = Vec::new();
        headers.extend_from_slice(&bool_header(":streaming", true));
        headers.extend_from_slice(&string_header(":event-type", "contentBlockDelta"));
        headers.extend_from_slice(&int_header(":seq", 42));
        headers.extend_from_slice(&string_header(":message-type", "event"));
        let wire = frame(&headers, br#"{"ok":true}"#);

        let mut dec = EventStreamDecoder::new();
        dec.feed(&wire);
        let f = dec.next_frame().unwrap().unwrap();
        assert_eq!(f.event_type.as_deref(), Some("contentBlockDelta"));
        assert_eq!(f.message_type.as_deref(), Some("event"));
        assert_eq!(f.payload, br#"{"ok":true}"#);
    }

    // ---- exception event --------------------------------------------------

    #[test]
    fn exception_event_surfaced() {
        let mut headers = Vec::new();
        headers.extend_from_slice(&string_header(":message-type", "exception"));
        headers.extend_from_slice(&string_header(":exception-type", "throttlingException"));
        headers.extend_from_slice(&string_header(":content-type", "application/json"));
        let wire = frame(&headers, br#"{"message":"Rate exceeded"}"#);

        let mut dec = EventStreamDecoder::new();
        dec.feed(&wire);
        let f = dec.next_frame().unwrap().unwrap();
        assert_eq!(f.message_type.as_deref(), Some("exception"));
        assert_eq!(f.exception_type.as_deref(), Some("throttlingException"));
        assert_eq!(f.event_type, None);
        assert_eq!(f.payload, br#"{"message":"Rate exceeded"}"#);
    }

    // ---- CRC corruption ---------------------------------------------------

    #[test]
    fn corrupted_prelude_crc_errors() {
        let mut wire = event_frame("contentBlockDelta", r#"{"x":1}"#);
        // Flip a bit in the prelude CRC (bytes 8..12).
        wire[8] ^= 0xFF;

        let mut dec = EventStreamDecoder::new();
        dec.feed(&wire);
        assert_eq!(dec.next_frame(), Err(DecodeError::PreludeCrc));
        // Stays poisoned.
        assert_eq!(dec.next_frame(), Err(DecodeError::PreludeCrc));
    }

    #[test]
    fn corrupted_message_crc_errors() {
        let mut wire = event_frame("contentBlockDelta", r#"{"x":1}"#);
        // Corrupt a payload byte; prelude CRC stays valid, message CRC fails.
        let payload_byte = wire.len() - 5; // last payload byte before trailing CRC
        wire[payload_byte] ^= 0xFF;

        let mut dec = EventStreamDecoder::new();
        dec.feed(&wire);
        assert_eq!(dec.next_frame(), Err(DecodeError::MessageCrc));
        assert_eq!(dec.next_frame(), Err(DecodeError::MessageCrc));
    }

    // ---- length / size guards --------------------------------------------

    #[test]
    fn oversized_frame_rejected() {
        // Hand-build a prelude whose total_length exceeds MAX_FRAME_LEN, with a
        // correct prelude CRC so the size guard (not the CRC) is what trips.
        let total_length = MAX_FRAME_LEN + 1;
        let headers_length = 0u32;
        let mut prelude = Vec::new();
        prelude.extend_from_slice(&total_length.to_be_bytes());
        prelude.extend_from_slice(&headers_length.to_be_bytes());
        let prelude_crc = crc32fast::hash(&prelude);
        prelude.extend_from_slice(&prelude_crc.to_be_bytes());

        let mut dec = EventStreamDecoder::new();
        dec.feed(&prelude);
        assert_eq!(dec.next_frame(), Err(DecodeError::Oversized));
    }

    #[test]
    fn bad_length_too_small_rejected() {
        // total_length below the 16-byte minimum.
        let mut prelude = Vec::new();
        prelude.extend_from_slice(&8u32.to_be_bytes()); // total_length = 8
        prelude.extend_from_slice(&0u32.to_be_bytes()); // headers_length = 0
        let prelude_crc = crc32fast::hash(&prelude);
        prelude.extend_from_slice(&prelude_crc.to_be_bytes());

        let mut dec = EventStreamDecoder::new();
        dec.feed(&prelude);
        assert_eq!(dec.next_frame(), Err(DecodeError::BadLength));
    }

    #[test]
    fn bad_length_headers_exceed_total_rejected() {
        // headers_length larger than total_length - 16.
        let total_length = 20u32; // room for only 4 header bytes
        let headers_length = 100u32;
        let mut prelude = Vec::new();
        prelude.extend_from_slice(&total_length.to_be_bytes());
        prelude.extend_from_slice(&headers_length.to_be_bytes());
        let prelude_crc = crc32fast::hash(&prelude);
        prelude.extend_from_slice(&prelude_crc.to_be_bytes());

        let mut dec = EventStreamDecoder::new();
        dec.feed(&prelude);
        assert_eq!(dec.next_frame(), Err(DecodeError::BadLength));
    }

    // ---- malformed headers ------------------------------------------------

    #[test]
    fn malformed_header_unknown_type_rejected() {
        // Header with an unknown value type (0x42) must error BadHeader.
        let mut headers = Vec::new();
        headers.push(3u8);
        headers.extend_from_slice(b"foo");
        headers.push(0x42u8); // unknown type
        let wire = frame(&headers, b"{}");

        let mut dec = EventStreamDecoder::new();
        dec.feed(&wire);
        assert_eq!(dec.next_frame(), Err(DecodeError::BadHeader));
    }

    #[test]
    fn malformed_header_value_overruns_block_rejected() {
        // A string header claiming more bytes than the headers block contains.
        // Build by hand so headers_length is honest but the inner value len lies.
        let mut headers = Vec::new();
        headers.push(1u8);
        headers.extend_from_slice(b"a");
        headers.push(7u8); // string
        headers.extend_from_slice(&9999u16.to_be_bytes()); // claims 9999 bytes
        headers.extend_from_slice(b"short"); // but only 5 present
        let wire = frame(&headers, b"{}");

        let mut dec = EventStreamDecoder::new();
        dec.feed(&wire);
        assert_eq!(dec.next_frame(), Err(DecodeError::BadHeader));
    }

    // ---- truncation -------------------------------------------------------

    #[test]
    fn truncated_buffer_yields_none() {
        let wire = event_frame("contentBlockDelta", r#"{"delta":{"text":"abc"}}"#);

        // Fewer than 12 bytes: not even a prelude.
        let mut dec = EventStreamDecoder::new();
        dec.feed(&wire[..5]);
        assert_eq!(dec.next_frame().unwrap(), None);

        // Full prelude but partial body.
        dec.feed(&wire[5..wire.len() - 1]);
        assert_eq!(dec.next_frame().unwrap(), None);

        // Final byte completes the frame.
        dec.feed(&wire[wire.len() - 1..]);
        assert!(dec.next_frame().unwrap().is_some());
    }

    // ---- no panic on arbitrary bytes -------------------------------------

    #[test]
    fn arbitrary_bytes_never_panic() {
        // Exhaustively feed a range of crafted prefixes/garbage; the decoder may
        // return Ok(None) / Ok(Some) / Err, but must never panic.
        for seed in 0u32..512 {
            let mut bytes = Vec::new();
            // Vary the prelude fields to drive different code paths.
            bytes.extend_from_slice(&seed.to_be_bytes());
            bytes.extend_from_slice(&(seed.wrapping_mul(7)).to_be_bytes());
            bytes.extend_from_slice(&(seed.wrapping_mul(13)).to_be_bytes());
            // Some pseudo-random tail.
            for i in 0..(seed % 64) {
                bytes.push((seed.wrapping_add(i) & 0xFF) as u8);
            }
            let mut dec = EventStreamDecoder::new();
            dec.feed(&bytes);
            let _ = dec.next_frame(); // must not panic
        }
    }
}
