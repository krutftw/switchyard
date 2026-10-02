//! Reading request bodies: the size limit and `Content-Encoding`.
//!
//! The limit (`server.body_limit_mb`) applies twice: to the bytes on the
//! wire, and to what they decompress to, so a small compressed body cannot
//! expand into gigabytes.

use axum::body::Body;
use bytes::{Bytes, BytesMut};
use futures::StreamExt;
use http::HeaderMap;
use http::header::{CONTENT_ENCODING, CONTENT_LENGTH, EXPECT};
use std::io::Read;
use std::time::Duration;
use switchyard_core::{ApiError, ErrorKind};

/// How many encodings a client may stack (`Content-Encoding: gzip, br`).
/// Real clients use one; more than a few is only ever an attack.
const MAX_ENCODINGS: usize = 4;

/// A request body compression this gateway can undo.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Encoding {
    Gzip,
    Deflate,
    Brotli,
    Zstd,
}

impl Encoding {
    fn parse(token: &str) -> Option<Encoding> {
        match token {
            "gzip" | "x-gzip" => Some(Encoding::Gzip),
            "deflate" => Some(Encoding::Deflate),
            "br" => Some(Encoding::Brotli),
            "zstd" => Some(Encoding::Zstd),
            _ => None,
        }
    }

    const fn name(self) -> &'static str {
        match self {
            Encoding::Gzip => "gzip",
            Encoding::Deflate => "deflate",
            Encoding::Brotli => "br",
            Encoding::Zstd => "zstd",
        }
    }
}

/// Why a request body was refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum BodyError {
    /// Larger than the limit, on the wire or once decompressed.
    TooLarge { limit: usize },
    /// A `Content-Encoding` this gateway does not decode.
    UnsupportedEncoding(String),
    /// The body is not valid data of the encoding it claims.
    Corrupt(&'static str),
    /// A zstd body that can only be decoded with a window (the history the
    /// decoder keeps in memory) larger than `max_window` bytes.
    ZstdWindow { max_window: u64 },
    /// The connection failed while the body was being read.
    Read,
    /// Nothing arrived for [`BODY_IDLE_TIMEOUT`].
    Stalled,
}

impl BodyError {
    /// The error the client is told.
    pub(crate) fn to_api_error(&self) -> ApiError {
        match self {
            BodyError::TooLarge { limit } => ApiError::new(
                ErrorKind::TooLarge,
                format!(
                    "the request body is larger than the {} MiB this gateway accepts",
                    limit / MIB
                ),
            )
            .with_code("request_too_large"),
            BodyError::UnsupportedEncoding(encoding) => ApiError::invalid_request(format!(
                "unsupported Content-Encoding `{encoding}`; this gateway decodes gzip, deflate, br and zstd"
            ))
            .with_status(415)
            .with_code("unsupported_content_encoding"),
            BodyError::Corrupt(encoding) => ApiError::invalid_request(format!(
                "the request body is not valid {encoding} data, as its Content-Encoding says"
            )),
            BodyError::ZstdWindow { max_window } => ApiError::invalid_request(format!(
                "the zstd request body needs a decoding window of more than the {} MiB this \
                 gateway allows; compress it with a smaller window (no long-distance mode, a \
                 level below 20)",
                max_window / MIB as u64
            ))
            .with_code("zstd_window_too_large"),
            BodyError::Read => {
                ApiError::invalid_request("the request body could not be read to its end")
            }
            BodyError::Stalled => ApiError::timeout(format!(
                "no request body data arrived for {} seconds",
                BODY_IDLE_TIMEOUT.as_secs()
            ))
            .with_status(408)
            .with_code("request_timeout"),
        }
    }
}

const MIB: usize = 1024 * 1024;

/// `server.body_limit_mb` in bytes.
pub(crate) fn limit_bytes(body_limit_mb: u64) -> usize {
    usize::try_from(body_limit_mb.max(1))
        .unwrap_or(usize::MAX)
        .saturating_mul(MIB)
}

/// The encodings named by the `Content-Encoding` headers, in the order they
/// were applied. `identity` and empty tokens are dropped.
fn encodings(headers: &HeaderMap) -> Result<Vec<Encoding>, BodyError> {
    let mut out = Vec::new();
    for value in headers.get_all(CONTENT_ENCODING) {
        let Ok(value) = value.to_str() else {
            return Err(BodyError::UnsupportedEncoding("(not text)".to_string()));
        };
        for token in value.split(',') {
            let token = token.trim().to_ascii_lowercase();
            if token.is_empty() || token == "identity" {
                continue;
            }
            match Encoding::parse(&token) {
                Some(encoding) => out.push(encoding),
                None => {
                    // The token is the client's own text; keep the echo short.
                    let shown: String = token.chars().take(32).collect();
                    return Err(BodyError::UnsupportedEncoding(shown));
                }
            }
            if out.len() > MAX_ENCODINGS {
                return Err(BodyError::UnsupportedEncoding(format!(
                    "more than {MAX_ENCODINGS} stacked encodings"
                )));
            }
        }
    }
    Ok(out)
}

/// How long a request body may make no progress at all before the request
/// is given up on. Not a limit on the upload as a whole: a large body on a
/// slow line is fine, a client that opens a request and then sends nothing
/// is not.
const BODY_IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a refused request body is read and thrown away before the
/// error is answered.
const DISCARD_TIME: Duration = Duration::from_secs(5);

/// The least that is read away of a refused body, whatever the body limit.
const DISCARD_FLOOR: usize = 8 * MIB;

/// A body refused while (or before) it was read.
struct Refused {
    error: BodyError,
    /// Whether reading had begun.
    started: bool,
}

/// Whether the client waits for `100 Continue` before sending its body.
/// Such a client is answered without the body ever being asked for.
fn expects_continue(headers: &HeaderMap) -> bool {
    headers
        .get_all(EXPECT)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .any(|value| value.trim().eq_ignore_ascii_case("100-continue"))
}

/// Reads and throws away what is left of a request body: at most
/// `max_bytes`, for at most [`DISCARD_TIME`].
///
/// A request that is answered before its body has been read leaves the
/// client still sending. Closing the connection on it then makes the
/// operating system reset the connection, and a client busy sending sees
/// "connection reset" instead of the error it was sent. Reading the body
/// away first lets the answer arrive.
async fn discard_stream(stream: &mut axum::body::BodyDataStream, max_bytes: usize) {
    // With a small limit, a body a little over it is the common case.
    let max_bytes = max_bytes.max(DISCARD_FLOOR);
    let _ = tokio::time::timeout(DISCARD_TIME, async {
        let mut seen = 0usize;
        while let Some(Ok(chunk)) = stream.next().await {
            seen = seen.saturating_add(chunk.len());
            if seen > max_bytes {
                break;
            }
        }
    })
    .await;
}

/// [`discard_stream`] for a request that is refused before anyone looked at
/// its body (unknown route, wrong method, failed authentication).
pub(crate) async fn discard(body: Body, headers: &HeaderMap, max_bytes: usize) {
    if expects_continue(headers) {
        return;
    }
    discard_stream(&mut body.into_data_stream(), max_bytes).await;
}

/// The bytes on the wire and the encodings to undo.
async fn read_raw(
    stream: &mut axum::body::BodyDataStream,
    headers: &HeaderMap,
    limit: usize,
) -> Result<(Bytes, Vec<Encoding>), Refused> {
    let encodings = encodings(headers).map_err(|error| Refused {
        error,
        started: false,
    })?;

    // A declared length over the limit is refused before a byte is read.
    let declared = headers
        .get(CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok());
    if declared.is_some_and(|length| length > limit as u64) {
        return Err(Refused {
            error: BodyError::TooLarge { limit },
            started: false,
        });
    }

    let mut raw = BytesMut::new();
    loop {
        let next = tokio::time::timeout(BODY_IDLE_TIMEOUT, stream.next())
            .await
            .map_err(|_| Refused {
                error: BodyError::Stalled,
                started: true,
            })?;
        let Some(chunk) = next else {
            break;
        };
        let chunk = chunk.map_err(|_| Refused {
            error: BodyError::Read,
            started: true,
        })?;
        if raw.len().saturating_add(chunk.len()) > limit {
            return Err(Refused {
                error: BodyError::TooLarge { limit },
                started: true,
            });
        }
        raw.extend_from_slice(&chunk);
    }
    Ok((raw.freeze(), encodings))
}

/// Reads a request body to its end, refusing more than `limit` bytes, and
/// undoes its `Content-Encoding`, refusing more than `limit` bytes of
/// output.
///
/// A body that is refused for its size or its encoding is still read (and
/// thrown away, within bounds) before the error is returned, so that the
/// client gets to see the answer.
pub(crate) async fn read(
    body: Body,
    headers: &HeaderMap,
    limit: usize,
) -> Result<Bytes, BodyError> {
    let mut stream = body.into_data_stream();
    let (raw, encodings) = match read_raw(&mut stream, headers, limit).await {
        Ok(read) => read,
        Err(Refused { error, started }) => {
            // The client may still be sending; see `discard_stream`. (Not
            // one whose connection broke or whose body stopped coming.)
            let still_sending = matches!(
                error,
                BodyError::TooLarge { .. } | BodyError::UnsupportedEncoding(_)
            );
            if still_sending && (started || !expects_continue(headers)) {
                discard_stream(&mut stream, limit).await;
            }
            return Err(error);
        }
    };
    if encodings.is_empty() || raw.is_empty() {
        return Ok(raw);
    }

    // Decompression is CPU work proportional to the limit; keep it off the
    // async workers.
    let input = raw.clone();
    let decoded = tokio::task::spawn_blocking(move || decode_all(&input, &encodings, limit))
        .await
        .map_err(|_| BodyError::Read)?;
    match decoded {
        Ok(bytes) => Ok(Bytes::from(bytes)),
        // Some clients set the header and then send the body as it is. JSON
        // cannot be mistaken for compressed data, so take it.
        Err(BodyError::Corrupt(_))
            if serde_json::from_slice::<serde::de::IgnoredAny>(&raw).is_ok() =>
        {
            Ok(raw)
        }
        Err(error) => Err(error),
    }
}

/// Undoes `encodings` (listed in the order they were applied, so the last
/// one comes off first).
fn decode_all(input: &[u8], encodings: &[Encoding], limit: usize) -> Result<Vec<u8>, BodyError> {
    let mut current: Option<Vec<u8>> = None;
    for encoding in encodings.iter().rev() {
        let source = current.as_deref().unwrap_or(input);
        current = Some(decode_one(source, *encoding, limit)?);
    }
    Ok(current.unwrap_or_else(|| input.to_vec()))
}

/// Reads a decoder to its end, stopping one byte past the limit.
fn drain(reader: impl Read, encoding: Encoding, limit: usize) -> Result<Vec<u8>, BodyError> {
    let mut out = Vec::new();
    let cap = u64::try_from(limit).unwrap_or(u64::MAX).saturating_add(1);
    reader
        .take(cap)
        .read_to_end(&mut out)
        .map_err(|_| BodyError::Corrupt(encoding.name()))?;
    if out.len() > limit {
        return Err(BodyError::TooLarge { limit });
    }
    Ok(out)
}

fn decode_one(input: &[u8], encoding: Encoding, limit: usize) -> Result<Vec<u8>, BodyError> {
    match encoding {
        Encoding::Gzip => drain(flate2::read::MultiGzDecoder::new(input), encoding, limit),
        Encoding::Deflate => {
            // HTTP's "deflate" is the zlib format, but raw deflate streams
            // are common enough in the wild to be worth a second look.
            match drain(flate2::read::ZlibDecoder::new(input), encoding, limit) {
                Err(BodyError::Corrupt(_)) => {
                    drain(flate2::read::DeflateDecoder::new(input), encoding, limit)
                }
                other => other,
            }
        }
        Encoding::Brotli => drain(brotli::Decompressor::new(input, 16 * 1024), encoding, limit),
        Encoding::Zstd => {
            // A frame that says how much it holds is taken at its word.
            if let Ok(Some(size)) = zstd::zstd_safe::get_frame_content_size(input)
                && size > u64::try_from(limit).unwrap_or(u64::MAX)
            {
                return Err(BodyError::TooLarge { limit });
            }
            let mut decoder =
                zstd::stream::read::Decoder::new(input).map_err(|_| BodyError::Corrupt("zstd"))?;
            let window_log = zstd_window_log(limit);
            decoder
                .window_log_max(window_log)
                .map_err(|_| BodyError::Corrupt("zstd"))?;
            let max_window = 1u64 << window_log;
            drain(decoder, encoding, limit).map_err(|error| match error {
                // The decoder refuses such a frame at its header, with an
                // error that reads like any other; say what is wrong.
                BodyError::Corrupt(_)
                    if zstd_declared_window(input).is_some_and(|window| window > max_window) =>
                {
                    BodyError::ZstdWindow { max_window }
                }
                other => other,
            })
        }
    }
}

/// The smallest window limit a zstd body is decoded with: 8 MiB, which is
/// what streaming encoders declare at every level up to 19 (and what Go's
/// common encoder declares by default) however small the body is.
const ZSTD_MIN_WINDOW_LOG: u32 = 23;

/// The largest: 128 MiB, the format's own default limit.
const ZSTD_MAX_WINDOW_LOG: u32 = 27;

/// The window limit (as a power of two) for decoding a zstd body under
/// `limit`.
///
/// The decoder reserves a frame's declared window up front, so the limit
/// follows the body limit rather than always being the largest: a few bytes
/// should not reserve 100+ MiB. But it never falls below what ordinary
/// encoders declare. A streaming encoder does not know how little input it
/// will get and declares the window of its compression level, so a window
/// larger than the body limit says nothing about the size of the body —
/// that is bounded separately, while decoding.
fn zstd_window_log(limit: usize) -> u32 {
    (usize::BITS - limit.leading_zeros()).clamp(ZSTD_MIN_WINDOW_LOG, ZSTD_MAX_WINDOW_LOG)
}

/// The window size the first frame of a zstd body declares, when it declares
/// one (a frame that states its content size instead does not).
fn zstd_declared_window(input: &[u8]) -> Option<u64> {
    let [0x28, 0xB5, 0x2F, 0xFD, descriptor, window, ..] = input else {
        return None;
    };
    // Single-segment frames have no window descriptor.
    if descriptor & 0x20 != 0 {
        return None;
    }
    let base = 1u64 << (10 + u32::from(window >> 3));
    Some(base + (base / 8) * u64::from(window & 7))
}

#[cfg(test)]
mod tests {
    use super::*;
    use http::HeaderValue;
    use pretty_assertions::assert_eq;
    use std::io::Write;

    const JSON: &[u8] =
        br#"{"model":"m","messages":[{"role":"user","content":"hello hello hello"}]}"#;

    fn gzip(data: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    fn zlib(data: &[u8]) -> Vec<u8> {
        let mut encoder =
            flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    fn raw_deflate(data: &[u8]) -> Vec<u8> {
        let mut encoder =
            flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    fn brotli(data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut writer = brotli::CompressorWriter::new(&mut out, 4096, 5, 20);
            writer.write_all(data).unwrap();
        }
        out
    }

    fn zstd(data: &[u8]) -> Vec<u8> {
        zstd::stream::encode_all(data, 3).unwrap()
    }

    /// What a streaming encoder writes: the frame header goes out before
    /// the encoder knows how little input there is, so it declares the
    /// window of its level.
    fn zstd_streamed(data: &[u8], level: i32, window_log: Option<u32>) -> Vec<u8> {
        let mut encoder = zstd::stream::write::Encoder::new(Vec::new(), level).unwrap();
        if let Some(window_log) = window_log {
            encoder.window_log(window_log).unwrap();
        }
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    /// One frame that states its content size (and so has no window of its
    /// own).
    fn zstd_sized(data: &[u8]) -> Vec<u8> {
        zstd::bulk::compress(data, 3).unwrap()
    }

    fn headers(encoding: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_ENCODING, HeaderValue::from_str(encoding).unwrap());
        headers
    }

    async fn read_with(data: Vec<u8>, encoding: &str, limit: usize) -> Result<Bytes, BodyError> {
        read(Body::from(data), &headers(encoding), limit).await
    }

    #[tokio::test]
    async fn plain_bodies_pass_through() {
        let body = read(Body::from(JSON), &HeaderMap::new(), MIB)
            .await
            .unwrap();
        assert_eq!(&body[..], JSON);
        let empty = read(Body::empty(), &headers("gzip"), MIB).await.unwrap();
        assert!(empty.is_empty());
        let identity = read_with(JSON.to_vec(), "identity", MIB).await.unwrap();
        assert_eq!(&identity[..], JSON);
    }

    #[tokio::test]
    async fn every_supported_encoding_round_trips() {
        for (encoding, data) in [
            ("gzip", gzip(JSON)),
            ("x-gzip", gzip(JSON)),
            ("GZIP", gzip(JSON)),
            ("deflate", zlib(JSON)),
            ("deflate", raw_deflate(JSON)),
            ("br", brotli(JSON)),
            ("zstd", zstd(JSON)),
        ] {
            let body = read_with(data, encoding, MIB).await;
            assert_eq!(body.as_deref(), Ok(JSON), "{encoding}");
        }
    }

    #[tokio::test]
    async fn stacked_encodings_come_off_last_first() {
        let data = brotli(&gzip(JSON));
        assert_eq!(read_with(data, "gzip, br", MIB).await.as_deref(), Ok(JSON));
        let data = zstd(&gzip(JSON));
        let mut headers = HeaderMap::new();
        headers.append(CONTENT_ENCODING, HeaderValue::from_static("gzip"));
        headers.append(CONTENT_ENCODING, HeaderValue::from_static("identity, zstd"));
        assert_eq!(
            read(Body::from(data), &headers, MIB).await.as_deref(),
            Ok(JSON)
        );
    }

    #[tokio::test]
    async fn unknown_encodings_are_refused() {
        assert_eq!(
            read_with(JSON.to_vec(), "compress", MIB).await,
            Err(BodyError::UnsupportedEncoding("compress".into()))
        );
        assert_eq!(
            read_with(JSON.to_vec(), "gzip, lz4", MIB).await,
            Err(BodyError::UnsupportedEncoding("lz4".into()))
        );
        let stacked = read_with(JSON.to_vec(), "gzip,gzip,gzip,gzip,gzip", MIB).await;
        assert!(matches!(stacked, Err(BodyError::UnsupportedEncoding(_))));
        let error = BodyError::UnsupportedEncoding("lz4".into()).to_api_error();
        assert_eq!(error.status, 415);
    }

    #[tokio::test]
    async fn oversized_bodies_are_refused() {
        let big = vec![b'a'; 2 * 1024];
        assert_eq!(
            read(Body::from(big.clone()), &HeaderMap::new(), 1024).await,
            Err(BodyError::TooLarge { limit: 1024 })
        );
        // Announced by Content-Length: refused without reading.
        let mut headers = HeaderMap::new();
        headers.insert(CONTENT_LENGTH, HeaderValue::from_static("999999"));
        assert_eq!(
            read(Body::empty(), &headers, 1024).await,
            Err(BodyError::TooLarge { limit: 1024 })
        );
        // Exactly at the limit is fine.
        let exact = vec![b'a'; 1024];
        assert_eq!(
            read(Body::from(exact.clone()), &HeaderMap::new(), 1024)
                .await
                .map(|body| body.len()),
            Ok(1024)
        );
        assert_eq!(
            BodyError::TooLarge { limit: MIB }.to_api_error().status,
            413
        );
    }

    #[tokio::test]
    async fn decompression_bombs_are_refused() {
        // 8 MiB of zeros compress to a few kilobytes in every format.
        let zeros = vec![0u8; 8 * MIB];
        for (encoding, data) in [
            ("gzip", gzip(&zeros)),
            ("deflate", zlib(&zeros)),
            ("br", brotli(&zeros)),
            ("zstd", zstd(&zeros)),
        ] {
            assert!(data.len() < MIB / 4, "{encoding}: {} bytes", data.len());
            assert_eq!(
                read_with(data, encoding, MIB).await,
                Err(BodyError::TooLarge { limit: MIB }),
                "{encoding}"
            );
        }
        // A bomb inside a bomb.
        let nested = gzip(&gzip(&zeros));
        assert_eq!(
            read_with(nested, "gzip, gzip", MIB).await,
            Err(BodyError::TooLarge { limit: MIB })
        );
    }

    #[tokio::test]
    async fn small_zstd_bodies_decode_whatever_window_their_encoder_declared() {
        // Every ordinary level, under the smallest body limit: the window a
        // frame declares (up to 8 MiB here) is not the size of its content.
        for level in [1, 3, 10, 15, 19] {
            let data = zstd_streamed(JSON, level, None);
            assert!(data.len() < 1024);
            assert_eq!(
                read_with(data, "zstd", MIB).await.as_deref(),
                Ok(JSON),
                "level {level}"
            );
        }
        let data = zstd_streamed(JSON, 3, Some(23));
        assert_eq!(zstd_declared_window(&data), Some(8 * MIB as u64));
        assert_eq!(read_with(data, "zstd", MIB).await.as_deref(), Ok(JSON));

        // A window beyond what is decoded is named as the problem, not
        // passed off as corrupt data …
        let data = zstd_streamed(JSON, 3, Some(24));
        assert_eq!(zstd_declared_window(&data), Some(16 * MIB as u64));
        let refused = read_with(data.clone(), "zstd", MIB).await;
        assert_eq!(
            refused,
            Err(BodyError::ZstdWindow {
                max_window: 8 * MIB as u64
            })
        );
        let error = refused.unwrap_err().to_api_error();
        assert_eq!(error.status, 400);
        assert!(error.message.contains("8 MiB"), "{}", error.message);
        // … and is fine where the body limit leaves room for it.
        assert_eq!(read_with(data, "zstd", 16 * MIB).await.as_deref(), Ok(JSON));
        // The largest window there is, under the largest limit.
        // (Written by hand — no encoder is asked to set up a 256 MiB window:
        // the frame header, then one empty last block.)
        let data = b"\x28\xB5\x2F\xFD\x00\x90\x01\x00\x00".to_vec();
        assert_eq!(zstd_declared_window(&data), Some(256 * MIB as u64));
        assert_eq!(
            read_with(data, "zstd", usize::MAX).await,
            Err(BodyError::ZstdWindow {
                max_window: 128 * MIB as u64
            })
        );

        assert_eq!(zstd_window_log(1), 23);
        assert_eq!(zstd_window_log(MIB), 23);
        assert_eq!(zstd_window_log(8 * MIB), 24);
        assert_eq!(zstd_window_log(64 * MIB), 27);
        assert_eq!(zstd_window_log(usize::MAX), 27);
        assert_eq!(zstd_declared_window(b"\x28\xB5\x2F\xFD"), None);
        assert_eq!(zstd_declared_window(b"{\"model\":\"m\"}"), None);
    }

    #[tokio::test]
    async fn a_zstd_frame_that_states_its_size_is_taken_at_its_word() {
        let data = zstd_sized(JSON);
        assert_eq!(zstd_declared_window(&data), None, "single segment");
        assert_eq!(read_with(data, "zstd", MIB).await.as_deref(), Ok(JSON));

        // 16 MiB declared: over the limit, and over the window limit too.
        let big = zstd_sized(&vec![0u8; 16 * MIB]);
        assert!(big.len() < MIB / 4);
        assert_eq!(
            read_with(big.clone(), "zstd", MIB).await,
            Err(BodyError::TooLarge { limit: MIB })
        );
        assert_eq!(
            read_with(big, "zstd", 16 * MIB)
                .await
                .map(|body| body.len()),
            Ok(16 * MIB)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_body_that_stops_arriving_is_given_up_on() {
        let head = futures::stream::iter([Ok::<_, std::io::Error>(Bytes::from_static(b"{\"mo"))]);
        let stalled = head.chain(futures::stream::pending());
        let result = read(Body::from_stream(stalled), &HeaderMap::new(), MIB).await;
        assert_eq!(result, Err(BodyError::Stalled));
        let error = BodyError::Stalled.to_api_error();
        assert_eq!(error.status, 408);

        // A slow body that keeps arriving is not a stalled one.
        let slow = futures::stream::unfold(0u8, |n| async move {
            if n == 5 {
                return None;
            }
            tokio::time::sleep(Duration::from_secs(40)).await;
            Some((Ok::<_, std::io::Error>(Bytes::from_static(b"x")), n + 1))
        });
        let result = read(Body::from_stream(slow), &HeaderMap::new(), MIB).await;
        assert_eq!(result.as_deref(), Ok(&b"xxxxx"[..]));
    }

    #[tokio::test]
    async fn mislabelled_json_is_accepted_and_garbage_is_not() {
        for encoding in ["gzip", "deflate", "br", "zstd"] {
            assert_eq!(
                read_with(JSON.to_vec(), encoding, MIB).await.as_deref(),
                Ok(JSON),
                "{encoding}"
            );
            let garbage = b"\x00\x01\x02 definitely not compressed".to_vec();
            let refused = read_with(garbage, encoding, MIB).await;
            assert!(
                matches!(refused, Err(BodyError::Corrupt(_))),
                "{encoding}: {refused:?}"
            );
        }
        assert_eq!(BodyError::Corrupt("gzip").to_api_error().status, 400);
    }

    #[test]
    fn the_limit_in_bytes() {
        assert_eq!(limit_bytes(1), MIB);
        assert_eq!(limit_bytes(64), 64 * MIB);
        // Zero is refused by config validation; never divide the world by it.
        assert_eq!(limit_bytes(0), MIB);
        assert_eq!(limit_bytes(u64::MAX), usize::MAX);
    }
}
