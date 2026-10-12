//! Wire framing (`docs/DEV-PROTOCOL.md` § Transport): one UTF-8 JSON object
//! per line; a frame whose object has `"binary": n` is followed by exactly
//! `n` raw bytes after the newline.

use std::io::{self, Read, Write};

use serde_json::{Map, Value};

/// Longest request line accepted. Requests are small; a longer line means
/// the peer is not speaking the protocol.
pub const MAX_LINE: usize = 1 << 20;
/// Largest binary payload accepted on a frame (responses; requests never
/// carry one, see [`FrameReader::requests`]).
pub const MAX_BINARY: usize = 64 << 20;

/// One decoded frame.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    /// The JSON object.
    pub msg: Map<String, Value>,
    /// The payload announced by `"binary"`.
    pub binary: Option<Vec<u8>>,
}

/// Why a frame could not be read.
#[derive(Debug)]
pub enum FrameError {
    /// The socket failed.
    Io(io::Error),
    /// No newline within the line limit ([`MAX_LINE`] unless lowered with
    /// [`FrameReader::set_max_line`]): the stream is out of sync.
    LineTooLong,
    /// The stream ended inside a line or a payload.
    Truncated,
    /// `"binary"` is not a length within [`MAX_BINARY`]: out of sync.
    BadBinaryLength,
    /// A request announced a `"binary"` payload; no method takes one. The
    /// payload bytes would follow, so the stream is out of sync.
    BinaryOnRequest,
    /// The line is not JSON. The line was consumed; the next frame is
    /// readable.
    BadJson(String),
    /// The line is JSON but not an object. Consumed like `BadJson`.
    NotObject,
}

impl FrameError {
    /// Whether the stream is still in sync after this error (the bad line
    /// was skipped whole).
    pub fn recoverable(&self) -> bool {
        matches!(self, FrameError::BadJson(_) | FrameError::NotObject)
    }
}

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FrameError::Io(error) => write!(f, "i/o: {error}"),
            FrameError::LineTooLong => f.write_str("line longer than the line limit"),
            FrameError::Truncated => f.write_str("stream ended inside a frame"),
            FrameError::BadBinaryLength => {
                write!(f, "binary must be a byte count up to {MAX_BINARY}")
            }
            FrameError::BinaryOnRequest => f.write_str("requests carry no binary payload"),
            FrameError::BadJson(why) => write!(f, "invalid JSON line: {why}"),
            FrameError::NotObject => f.write_str("frame is not a JSON object"),
        }
    }
}

/// Incremental frame decoder. Chunk boundaries may fall anywhere.
#[derive(Debug)]
pub struct FrameReader<R> {
    inner: R,
    buf: Vec<u8>,
    eof: bool,
    max_line: usize,
    requests: bool,
}

impl<R: Read> FrameReader<R> {
    /// A reader over `inner` (responses: payloads up to [`MAX_BINARY`]).
    pub fn new(inner: R) -> FrameReader<R> {
        FrameReader {
            inner,
            buf: Vec::new(),
            eof: false,
            max_line: MAX_LINE,
            requests: false,
        }
    }

    /// A reader of requests: a frame announcing `"binary"` is refused with
    /// [`FrameError::BinaryOnRequest`] before any payload is buffered.
    pub fn requests(inner: R) -> FrameReader<R> {
        FrameReader {
            requests: true,
            ..FrameReader::new(inner)
        }
    }

    /// Accept lines of at most `max_line` bytes (capped at [`MAX_LINE`]).
    pub fn set_max_line(&mut self, max_line: usize) {
        self.max_line = max_line.min(MAX_LINE);
    }

    /// The underlying reader (e.g. to change its timeout).
    pub fn get_ref(&self) -> &R {
        &self.inner
    }

    /// The underlying reader, mutably.
    pub fn get_mut(&mut self) -> &mut R {
        &mut self.inner
    }

    fn fill(&mut self) -> Result<bool, FrameError> {
        if self.eof {
            return Ok(false);
        }
        let mut chunk = [0u8; 16 * 1024];
        loop {
            match self.inner.read(&mut chunk) {
                Ok(0) => {
                    self.eof = true;
                    return Ok(false);
                }
                Ok(n) => {
                    self.buf.extend_from_slice(&chunk[..n]);
                    return Ok(true);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(FrameError::Io(error)),
            }
        }
    }

    /// The next frame, or `None` at a clean end of stream.
    pub fn read_frame(&mut self) -> Result<Option<Frame>, FrameError> {
        let mut scanned = 0;
        let newline = loop {
            if let Some(at) = self.buf[scanned..].iter().position(|&b| b == b'\n') {
                break scanned + at;
            }
            scanned = self.buf.len();
            if scanned > self.max_line {
                return Err(FrameError::LineTooLong);
            }
            if !self.fill()? {
                return if self.buf.is_empty() {
                    Ok(None)
                } else {
                    Err(FrameError::Truncated)
                };
            }
        };
        if newline > self.max_line {
            return Err(FrameError::LineTooLong);
        }
        let line: Vec<u8> = self.buf.drain(..=newline).collect();
        let text = line.strip_suffix(b"\n").unwrap_or(&line);
        let text = text.strip_suffix(b"\r").unwrap_or(text);
        let msg = match serde_json::from_slice::<Value>(text) {
            Ok(Value::Object(msg)) => msg,
            Ok(_) => return Err(FrameError::NotObject),
            Err(error) => return Err(FrameError::BadJson(error.to_string())),
        };
        let size = match msg.get("binary") {
            None | Some(Value::Null) => return Ok(Some(Frame { msg, binary: None })),
            Some(_) if self.requests => return Err(FrameError::BinaryOnRequest),
            Some(value) => value
                .as_u64()
                .and_then(|n| usize::try_from(n).ok())
                .filter(|&n| n <= MAX_BINARY)
                .ok_or(FrameError::BadBinaryLength)?,
        };
        while self.buf.len() < size {
            if !self.fill()? {
                return Err(FrameError::Truncated);
            }
        }
        let binary: Vec<u8> = self.buf.drain(..size).collect();
        Ok(Some(Frame {
            msg,
            binary: Some(binary),
        }))
    }
}

/// The JSON line of a frame (with its newline). With `binary_len`, the
/// object gets `"binary": binary_len` and the caller sends that many bytes
/// after the line. Non-object messages are sent as they are.
pub fn encode_line(msg: &Value, binary_len: Option<usize>) -> Vec<u8> {
    let mut line = match (msg, binary_len) {
        (Value::Object(map), Some(len)) => {
            let mut map = map.clone();
            map.insert("binary".to_owned(), Value::from(len));
            serde_json::to_vec(&Value::Object(map))
        }
        _ => serde_json::to_vec(msg),
    }
    // Serialising a `Value` cannot fail (string keys only).
    .unwrap_or_default();
    line.push(b'\n');
    line
}

/// Write one frame: the line, then the payload.
pub fn write_frame(out: &mut impl Write, msg: &Value, binary: Option<&[u8]>) -> io::Result<()> {
    out.write_all(&encode_line(msg, binary.map(<[u8]>::len)))?;
    if let Some(bytes) = binary {
        out.write_all(bytes)?;
    }
    out.flush()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// Hands out `data` in chunks of `step` bytes.
    struct Chunked {
        data: Vec<u8>,
        at: usize,
        step: usize,
    }

    impl Read for Chunked {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            let n = self.step.min(out.len()).min(self.data.len() - self.at);
            out[..n].copy_from_slice(&self.data[self.at..self.at + n]);
            self.at += n;
            Ok(n)
        }
    }

    fn reader(data: &[u8], step: usize) -> FrameReader<Chunked> {
        FrameReader::new(Chunked {
            data: data.to_vec(),
            at: 0,
            step,
        })
    }

    fn stream() -> Vec<u8> {
        let mut out = Vec::new();
        write_frame(&mut out, &json!({"id": 1, "method": "state"}), None).unwrap();
        write_frame(&mut out, &json!({"id": 2}), Some(b"a\nb\0c")).unwrap();
        write_frame(&mut out, &json!({"event": "log"}), Some(b"")).unwrap();
        out
    }

    #[test]
    fn frames_survive_any_chunking() {
        for step in [1, 2, 3, 7, 64, 4096] {
            let mut r = reader(&stream(), step);
            let first = r.read_frame().unwrap().unwrap();
            assert_eq!(first.msg["method"], "state");
            assert_eq!(first.binary, None);
            let second = r.read_frame().unwrap().unwrap();
            assert_eq!(second.msg["binary"], 5);
            assert_eq!(second.binary.as_deref(), Some(&b"a\nb\0c"[..]));
            let third = r.read_frame().unwrap().unwrap();
            assert_eq!(third.binary.as_deref(), Some(&b""[..]));
            assert!(r.read_frame().unwrap().is_none(), "clean end, step {step}");
        }
    }

    #[test]
    fn crlf_lines_are_accepted() {
        let mut r = reader(b"{\"id\":3}\r\n", 5);
        assert_eq!(r.read_frame().unwrap().unwrap().msg["id"], 3);
    }

    #[test]
    fn a_bad_line_is_skipped_and_the_next_one_read() {
        let mut r = reader(b"{nope\n[1,2]\n{\"id\":4}\n", 3);
        let error = r.read_frame().unwrap_err();
        assert!(matches!(error, FrameError::BadJson(_)) && error.recoverable());
        let error = r.read_frame().unwrap_err();
        assert!(matches!(error, FrameError::NotObject) && error.recoverable());
        assert_eq!(r.read_frame().unwrap().unwrap().msg["id"], 4);
    }

    #[test]
    fn truncation_inside_a_line_or_payload_is_an_error() {
        let error = reader(b"{\"id\":1}", 4).read_frame().unwrap_err();
        assert!(matches!(error, FrameError::Truncated) && !error.recoverable());
        let error = reader(b"{\"binary\":4}\nab", 4).read_frame().unwrap_err();
        assert!(matches!(error, FrameError::Truncated));
    }

    #[test]
    fn bad_binary_lengths_desynchronise() {
        for bad in ["-1", "1.5", "\"4\"", "true", &(MAX_BINARY + 1).to_string()] {
            let line = format!("{{\"binary\":{bad}}}\n");
            let error = reader(line.as_bytes(), 64).read_frame().unwrap_err();
            assert!(matches!(error, FrameError::BadBinaryLength), "{bad}");
        }
        // `null` means no payload.
        let frame = reader(b"{\"binary\":null}\n", 64)
            .read_frame()
            .unwrap()
            .unwrap();
        assert_eq!(frame.binary, None);
    }

    #[test]
    fn an_endless_line_is_refused() {
        let data = vec![b' '; MAX_LINE + 2];
        let error = reader(&data, 1 << 16).read_frame().unwrap_err();
        assert!(matches!(error, FrameError::LineTooLong));
    }

    #[test]
    fn a_lowered_line_limit_is_enforced() {
        let mut data = vec![b' '; 5000];
        data.push(b'\n');
        let mut r = reader(&data, 1 << 16);
        r.set_max_line(4096);
        assert!(matches!(r.read_frame(), Err(FrameError::LineTooLong)));
        let mut r = reader(b"{\"id\":1}\n", 64);
        r.set_max_line(4096);
        assert_eq!(r.read_frame().unwrap().unwrap().msg["id"], 1);
    }

    #[test]
    fn requests_never_carry_a_payload() {
        let mut r = FrameReader::requests(Chunked {
            data: b"{\"binary\":0}\n{\"binary\":null,\"id\":2}\n".to_vec(),
            at: 0,
            step: 64,
        });
        let error = r.read_frame().unwrap_err();
        assert!(matches!(error, FrameError::BinaryOnRequest) && !error.recoverable());
    }

    #[test]
    fn encoding_sets_the_binary_length_and_never_embeds_newlines() {
        let line = encode_line(&json!({"m": "a\nb"}), Some(3));
        assert_eq!(line.iter().filter(|&&b| b == b'\n').count(), 1);
        let back: Value = serde_json::from_slice(&line).unwrap();
        assert_eq!(back, json!({"m": "a\nb", "binary": 3}));
        assert_eq!(encode_line(&json!({"id": 1}), None), b"{\"id\":1}\n");
    }
}
