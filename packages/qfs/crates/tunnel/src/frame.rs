//! The tunnel **wire frame** and its length-prefixed binary codec.
//!
//! Every byte that crosses the outbound connection is a [`TunnelFrame`]: a `(stream_id, kind, body)`
//! envelope. Multiplexing rides on `stream_id` — many in-flight requests interleave on the one
//! connection, each tagged with the stream it belongs to (see [`crate::mux`]). The [`FrameBody`]
//! carries the request/response as the **shared** [`qfs_http_core`] DTOs, so a frame INHERITS the
//! single redaction authority: deriving `Debug` here delegates to those DTOs' *redacting* `Debug`,
//! and a bearer token in an `Authorization` header is never rendered in cleartext in a log line.
//!
//! ## Wire layout (big-endian, length-prefixed)
//! ```text
//! ┌─────────┬──────┬───────────────┬───────────────┬──────────────────┐
//! │ version │ kind │   stream_id   │   body_len    │   body (body_len) │
//! │  u8 (1) │ u8(1)│   u64 (8)     │   u32 (4)     │   bytes           │
//! └─────────┴──────┴───────────────┴───────────────┴──────────────────┘
//! ```
//! The 14-byte fixed header lets a streaming reader learn a frame's total length before it has the
//! whole body — [`decode_one`] returns `Ok(None)` (need more) until `body_len` bytes have arrived,
//! so a partial read is never mistaken for a fault. The `body` bytes are an `Open`'s carried
//! [`HttpRequest`] or a `Data`'s carried [`HttpResponse`] (themselves length-prefixed field by
//! field); `Close`/`Ping` carry an empty body.

use qfs_http_core::{HttpMethod, HttpRequest, HttpResponse};

use crate::TunnelError;

/// The wire-format version this build speaks. Bumped only on a breaking frame-layout change; a
/// frame tagged with any other version is refused by [`decode_one`] (a forward-compat fence rather
/// than a silent misparse).
pub const FRAME_VERSION: u8 = 1;

/// The fixed frame-header length: `version(1) + kind(1) + stream_id(8) + body_len(4)`.
const HEADER_LEN: usize = 1 + 1 + 8 + 4;

/// The kind of a [`TunnelFrame`] — the four multiplexing verbs the tunnel needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum FrameKind {
    /// Opens a stream and carries the inbound [`HttpRequest`] the relay routed to this node. The
    /// resident node services it against its local qfs and replies on the same `stream_id`.
    Open,
    /// Carries a payload — for the resident node's reply, the serviced [`HttpResponse`].
    Data,
    /// Ends a stream: no more frames will arrive for this `stream_id` (the request is complete).
    Close,
    /// A keepalive with no body. Answered with a `Ping`, so a long-idle outbound connection (and
    /// any NAT mapping in front of it) is kept warm without opening an inbound port.
    Ping,
}

impl FrameKind {
    /// The single-byte wire tag.
    const fn tag(self) -> u8 {
        match self {
            FrameKind::Open => 1,
            FrameKind::Data => 2,
            FrameKind::Close => 3,
            FrameKind::Ping => 4,
        }
    }

    /// Parse a wire tag back into a kind (`None` for an unknown tag → a decode error).
    const fn from_tag(tag: u8) -> Option<Self> {
        match tag {
            1 => Some(FrameKind::Open),
            2 => Some(FrameKind::Data),
            3 => Some(FrameKind::Close),
            4 => Some(FrameKind::Ping),
            _ => None,
        }
    }
}

/// The payload a [`TunnelFrame`] carries. The request/response variants hold the **shared**
/// [`qfs_http_core`] DTOs verbatim — which is what makes redaction *inherited*: the derived `Debug`
/// on this enum delegates to those DTOs' redacting `Debug`, so no frame dump ever leaks a token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FrameBody {
    /// An `Open` frame's inbound request (relay → resident node).
    Request(HttpRequest),
    /// A `Data` frame's serviced response (resident node → relay → caller).
    Response(HttpResponse),
    /// A `Close`/`Ping` frame's (absent) payload.
    Empty,
}

/// One multiplexed tunnel frame. `Debug` is **derived** and therefore inherits the redacting `Debug`
/// of any carried [`HttpRequest`]/[`HttpResponse`] — dumping a frame never renders a sensitive
/// header value in cleartext (the redaction floor is inherited from [`qfs_http_core`], not
/// re-implemented here).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TunnelFrame {
    /// The multiplexing key: which logical request/response stream this frame belongs to.
    pub stream_id: u64,
    /// The frame verb.
    pub kind: FrameKind,
    /// The carried payload (consistent with `kind` via the constructors below).
    pub body: FrameBody,
}

impl TunnelFrame {
    /// An `Open` frame opening `stream_id` with the inbound request the relay routed down.
    #[must_use]
    pub fn open(stream_id: u64, request: HttpRequest) -> Self {
        Self {
            stream_id,
            kind: FrameKind::Open,
            body: FrameBody::Request(request),
        }
    }

    /// A `Data` frame carrying the serviced response for `stream_id`.
    #[must_use]
    pub fn data(stream_id: u64, response: HttpResponse) -> Self {
        Self {
            stream_id,
            kind: FrameKind::Data,
            body: FrameBody::Response(response),
        }
    }

    /// A `Close` frame ending `stream_id`.
    #[must_use]
    pub fn close(stream_id: u64) -> Self {
        Self {
            stream_id,
            kind: FrameKind::Close,
            body: FrameBody::Empty,
        }
    }

    /// A `Ping` keepalive (carried on the reserved stream `0`).
    #[must_use]
    pub fn ping() -> Self {
        Self {
            stream_id: 0,
            kind: FrameKind::Ping,
            body: FrameBody::Empty,
        }
    }

    /// Encode this frame to its length-prefixed wire bytes (see the module layout). Infallible: a
    /// length that does not fit a `u32` is not reachable from any qfs request, but is clamped via a
    /// saturating cast rather than panicking.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let mut body = Vec::new();
        match &self.body {
            FrameBody::Request(req) => encode_request(req, &mut body),
            FrameBody::Response(resp) => encode_response(resp, &mut body),
            FrameBody::Empty => {}
        }
        let mut out = Vec::with_capacity(HEADER_LEN + body.len());
        out.push(FRAME_VERSION);
        out.push(self.kind.tag());
        out.extend_from_slice(&self.stream_id.to_be_bytes());
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend_from_slice(&body);
        out
    }
}

/// Decode **exactly one** complete frame from `buf`, requiring `buf` to hold that frame and nothing
/// more. Convenience for round-trip tests and single-frame callers; streaming readers use
/// [`decode_one`].
///
/// # Errors
/// [`TunnelError::Incomplete`] if `buf` is shorter than one whole frame, [`TunnelError::Malformed`]
/// if there are trailing bytes after the frame, or any structural decode error.
pub fn decode(buf: &[u8]) -> Result<TunnelFrame, TunnelError> {
    match decode_one(buf)? {
        Some((frame, consumed)) if consumed == buf.len() => Ok(frame),
        Some(_) => Err(TunnelError::Malformed(
            "trailing bytes after a complete frame",
        )),
        None => Err(TunnelError::Incomplete),
    }
}

/// Try to decode one frame from the front of a streaming byte buffer.
///
/// Returns `Ok(Some((frame, consumed)))` with the number of bytes the frame occupied (so the caller
/// drains exactly those), `Ok(None)` if `buf` does not yet hold a whole frame (wait for more
/// bytes — **not** an error), or an `Err` if the bytes are structurally invalid.
///
/// # Errors
/// [`TunnelError::UnsupportedVersion`]/[`TunnelError::UnknownKind`]/[`TunnelError::Malformed`] on a
/// structurally invalid frame.
pub fn decode_one(buf: &[u8]) -> Result<Option<(TunnelFrame, usize)>, TunnelError> {
    if buf.len() < HEADER_LEN {
        return Ok(None);
    }
    let version = buf[0];
    if version != FRAME_VERSION {
        return Err(TunnelError::UnsupportedVersion(version));
    }
    let kind_tag = buf[1];
    let stream_id = u64::from_be_bytes(
        buf[2..10]
            .try_into()
            .map_err(|_| TunnelError::Malformed("stream_id slice"))?,
    );
    let body_len = u32::from_be_bytes(
        buf[10..14]
            .try_into()
            .map_err(|_| TunnelError::Malformed("body_len slice"))?,
    ) as usize;
    let total = HEADER_LEN + body_len;
    if buf.len() < total {
        return Ok(None);
    }
    let kind = FrameKind::from_tag(kind_tag).ok_or(TunnelError::UnknownKind(kind_tag))?;
    let body = decode_body(kind, &buf[HEADER_LEN..total])?;
    Ok(Some((
        TunnelFrame {
            stream_id,
            kind,
            body,
        },
        total,
    )))
}

fn decode_body(kind: FrameKind, bytes: &[u8]) -> Result<FrameBody, TunnelError> {
    match kind {
        FrameKind::Open => Ok(FrameBody::Request(decode_request(bytes)?)),
        FrameKind::Data => Ok(FrameBody::Response(decode_response(bytes)?)),
        FrameKind::Close | FrameKind::Ping => {
            if bytes.is_empty() {
                Ok(FrameBody::Empty)
            } else {
                Err(TunnelError::Malformed(
                    "Close/Ping frame must carry an empty body",
                ))
            }
        }
    }
}

// ---- HTTP DTO field codec (length-prefixed, big-endian) -------------------------------------

fn put_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    out.extend_from_slice(bytes);
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    put_bytes(out, s.as_bytes());
}

fn method_tag(method: HttpMethod) -> u8 {
    match method {
        HttpMethod::Get => 0,
        HttpMethod::Post => 1,
        HttpMethod::Put => 2,
        HttpMethod::Patch => 3,
        HttpMethod::Delete => 4,
        // `HttpMethod` is `#[non_exhaustive]`; an unknown future method round-trips to a decode
        // error rather than silently aliasing an existing one.
        _ => 255,
    }
}

fn method_from_tag(tag: u8) -> Result<HttpMethod, TunnelError> {
    match tag {
        0 => Ok(HttpMethod::Get),
        1 => Ok(HttpMethod::Post),
        2 => Ok(HttpMethod::Put),
        3 => Ok(HttpMethod::Patch),
        4 => Ok(HttpMethod::Delete),
        _ => Err(TunnelError::Malformed("unknown HTTP method tag")),
    }
}

fn encode_headers(out: &mut Vec<u8>, headers: &[(String, String)]) {
    out.extend_from_slice(&(headers.len() as u32).to_be_bytes());
    for (name, value) in headers {
        put_str(out, name);
        put_str(out, value);
    }
}

fn encode_request(req: &HttpRequest, out: &mut Vec<u8>) {
    out.push(method_tag(req.method));
    put_str(out, &req.url);
    encode_headers(out, &req.headers);
    match &req.body {
        Some(b) => {
            out.push(1);
            put_bytes(out, b);
        }
        None => out.push(0),
    }
}

fn encode_response(resp: &HttpResponse, out: &mut Vec<u8>) {
    out.extend_from_slice(&resp.status.to_be_bytes());
    encode_headers(out, &resp.headers);
    put_bytes(out, &resp.body);
}

/// A bounds-checked, sequential reader over a frame body's bytes.
struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    const fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], TunnelError> {
        let end = self
            .pos
            .checked_add(n)
            .ok_or(TunnelError::Malformed("length overflow"))?;
        let slice = self
            .buf
            .get(self.pos..end)
            .ok_or(TunnelError::Malformed("truncated frame body"))?;
        self.pos = end;
        Ok(slice)
    }

    fn u8(&mut self) -> Result<u8, TunnelError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, TunnelError> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes(
            b.try_into().map_err(|_| TunnelError::Malformed("u16"))?,
        ))
    }

    fn u32(&mut self) -> Result<u32, TunnelError> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes(
            b.try_into().map_err(|_| TunnelError::Malformed("u32"))?,
        ))
    }

    fn bytes(&mut self) -> Result<Vec<u8>, TunnelError> {
        let n = self.u32()? as usize;
        Ok(self.take(n)?.to_vec())
    }

    fn string(&mut self) -> Result<String, TunnelError> {
        String::from_utf8(self.bytes()?).map_err(|_| TunnelError::Malformed("non-UTF-8 string"))
    }

    fn finish(self) -> Result<(), TunnelError> {
        if self.pos == self.buf.len() {
            Ok(())
        } else {
            Err(TunnelError::Malformed("trailing bytes in frame body"))
        }
    }
}

fn decode_headers(r: &mut Reader<'_>) -> Result<Vec<(String, String)>, TunnelError> {
    let count = r.u32()? as usize;
    let mut headers = Vec::with_capacity(count.min(64));
    for _ in 0..count {
        let name = r.string()?;
        let value = r.string()?;
        headers.push((name, value));
    }
    Ok(headers)
}

fn decode_request(bytes: &[u8]) -> Result<HttpRequest, TunnelError> {
    let mut r = Reader::new(bytes);
    let method = method_from_tag(r.u8()?)?;
    let url = r.string()?;
    let headers = decode_headers(&mut r)?;
    let body = match r.u8()? {
        0 => None,
        1 => Some(r.bytes()?),
        _ => return Err(TunnelError::Malformed("invalid body presence flag")),
    };
    r.finish()?;
    Ok(HttpRequest {
        method,
        url,
        headers,
        body,
    })
}

fn decode_response(bytes: &[u8]) -> Result<HttpResponse, TunnelError> {
    let mut r = Reader::new(bytes);
    let status = r.u16()?;
    let headers = decode_headers(&mut r)?;
    let body = r.bytes()?;
    r.finish()?;
    Ok(HttpResponse {
        status,
        headers,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_request() -> HttpRequest {
        HttpRequest::new(
            HttpMethod::Post,
            "https://acme-ci.qfs.local/hosts/acme-ci/claude/sessions",
        )
        .header("Authorization", "Bearer super-secret-token")
        .header("Content-Type", "application/json")
        .with_body(b"{\"task\":\"rebase\"}".to_vec())
    }

    fn sample_response() -> HttpResponse {
        HttpResponse::new(200, b"{\"ok\":true}".to_vec())
            .header("Set-Cookie", "session=abc123-secret")
            .header("Content-Type", "application/json")
    }

    #[test]
    fn open_frame_round_trips_through_the_codec() {
        let frame = TunnelFrame::open(7, sample_request());
        let bytes = frame.encode();
        let decoded = decode(&bytes).expect("decode one Open frame");
        assert_eq!(decoded, frame);
        assert_eq!(decoded.stream_id, 7);
        assert_eq!(decoded.kind, FrameKind::Open);
    }

    #[test]
    fn data_frame_round_trips_with_status_headers_and_body() {
        let frame = TunnelFrame::data(7, sample_response());
        let decoded = decode(&frame.encode()).expect("decode one Data frame");
        assert_eq!(decoded, frame);
        match decoded.body {
            FrameBody::Response(resp) => {
                assert_eq!(resp.status, 200);
                assert_eq!(resp.body, b"{\"ok\":true}");
                assert_eq!(resp.header_value("content-type"), Some("application/json"));
            }
            other => panic!("expected a Response body, got {other:?}"),
        }
    }

    #[test]
    fn close_and_ping_frames_round_trip_with_empty_bodies() {
        for frame in [TunnelFrame::close(9), TunnelFrame::ping()] {
            let decoded = decode(&frame.encode()).expect("decode control frame");
            assert_eq!(decoded, frame);
            assert_eq!(decoded.body, FrameBody::Empty);
        }
    }

    #[test]
    fn every_http_method_survives_the_round_trip() {
        for method in [
            HttpMethod::Get,
            HttpMethod::Post,
            HttpMethod::Put,
            HttpMethod::Patch,
            HttpMethod::Delete,
        ] {
            let frame = TunnelFrame::open(1, HttpRequest::new(method, "https://x/y"));
            let decoded = decode(&frame.encode()).expect("decode method frame");
            assert_eq!(decoded, frame);
        }
    }

    #[test]
    fn decode_one_reports_need_more_until_the_whole_frame_arrives() {
        let bytes = TunnelFrame::open(3, sample_request()).encode();
        // Every strict prefix is "need more", not an error.
        for cut in 0..bytes.len() {
            assert_eq!(
                decode_one(&bytes[..cut]).expect("a prefix is not an error"),
                None,
                "prefix of len {cut} must report need-more"
            );
        }
        // The full buffer yields the frame and reports it consumed all the bytes.
        let (frame, consumed) = decode_one(&bytes).expect("decode").expect("a whole frame");
        assert_eq!(consumed, bytes.len());
        assert_eq!(frame.stream_id, 3);
    }

    #[test]
    fn two_concatenated_frames_decode_one_at_a_time() {
        let mut stream = TunnelFrame::open(1, sample_request()).encode();
        stream.extend(TunnelFrame::data(1, sample_response()).encode());
        let (first, n1) = decode_one(&stream).expect("ok").expect("first frame");
        assert_eq!(first.kind, FrameKind::Open);
        let (second, n2) = decode_one(&stream[n1..])
            .expect("ok")
            .expect("second frame");
        assert_eq!(second.kind, FrameKind::Data);
        assert_eq!(n1 + n2, stream.len());
    }

    #[test]
    fn an_unknown_version_is_refused() {
        let mut bytes = TunnelFrame::ping().encode();
        bytes[0] = 99;
        assert_eq!(decode_one(&bytes), Err(TunnelError::UnsupportedVersion(99)));
    }

    #[test]
    fn an_unknown_kind_tag_is_refused() {
        let mut bytes = TunnelFrame::ping().encode();
        bytes[1] = 200;
        assert_eq!(decode_one(&bytes), Err(TunnelError::UnknownKind(200)));
    }

    #[test]
    fn a_frame_debug_never_leaks_a_bearer_token_in_cleartext() {
        // The security property: the relay is untrusted transport, and no log line may render a
        // secret. A frame carrying an `Authorization: Bearer …` header must redact it in `Debug`
        // (inherited from the qfs-http-core redacting `Debug`), and a `Set-Cookie` response too.
        let open = TunnelFrame::open(1, sample_request());
        let dbg = format!("{open:?}");
        assert!(
            !dbg.contains("super-secret-token"),
            "a bearer token must never appear in a frame's Debug: {dbg}"
        );
        assert!(
            dbg.contains(qfs_secrets::REDACTED),
            "redaction marker present: {dbg}"
        );

        let data = TunnelFrame::data(1, sample_response());
        let dbg = format!("{data:?}");
        assert!(
            !dbg.contains("abc123-secret"),
            "a Set-Cookie value must never appear in a frame's Debug: {dbg}"
        );
        assert!(dbg.contains(qfs_secrets::REDACTED));
    }
}
