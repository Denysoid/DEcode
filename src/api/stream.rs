use bytes::{Bytes, BytesMut};
use futures_util::{Stream, StreamExt, stream::BoxStream};
use serde_json::Value;

use super::types::StreamEvent;
use crate::error::ApiError;

pub const MAX_SSE_FRAME_BYTES: usize = 1024 * 1024;
pub const MAX_SSE_TURN_BYTES: usize = 8 * 1024 * 1024;

/// Decode a Responses SSE body without assuming HTTP chunk boundaries align
/// with UTF-8 code points or SSE frames.
pub fn parse_sse_stream<S>(byte_stream: S) -> BoxStream<'static, Result<StreamEvent, ApiError>>
where
    S: Stream<Item = Result<Bytes, ApiError>> + Send + 'static,
{
    Box::pin(parse_sse_data_stream(byte_stream).map(|item| {
        let data = item?;
        if data.trim() == "[DONE]" {
            return Ok(StreamEvent::Done);
        }
        decode_responses_event(&data).map_err(|error| {
            let preview: String = data.chars().take(512).collect();
            ApiError::Protocol(format!(
                "malformed Responses SSE event: {error}; data={preview:?}"
            ))
        })
    }))
}

pub(crate) fn decode_responses_event(data: &str) -> Result<StreamEvent, serde_json::Error> {
    let mut value: Value = serde_json::from_str(data)?;
    normalize_nested_error_event(&mut value);
    serde_json::from_value(value)
}

/// Azure currently wraps streaming error details in an `error` object, while
/// Responses implementations can also emit the fields at the event's top
/// level. Normalize both representations before the authoritative typed
/// decode so capacity errors are not misreported as malformed JSON.
fn normalize_nested_error_event(value: &mut Value) {
    let Value::Object(event) = value else {
        return;
    };
    if event.get("type").and_then(Value::as_str) != Some("error") {
        return;
    }
    let Some(nested) = event.get("error").and_then(Value::as_object) else {
        return;
    };
    let message = nested.get("message").cloned();
    let code = nested
        .get("code")
        .filter(|value| value.is_string())
        .cloned()
        .or_else(|| {
            nested
                .get("type")
                .filter(|value| value.is_string())
                .cloned()
        });
    let param = nested.get("param").cloned();

    if !event.get("message").is_some_and(Value::is_string)
        && let Some(message) = message
    {
        event.insert("message".to_owned(), message);
    }
    if !event.get("code").is_some_and(Value::is_string)
        && let Some(code) = code
    {
        event.insert("code".to_owned(), code);
    }
    if !event.contains_key("param")
        && let Some(param) = param
    {
        event.insert("param".to_owned(), param);
    }
}

pub(crate) fn parse_sse_data_stream<S>(
    byte_stream: S,
) -> BoxStream<'static, Result<String, ApiError>>
where
    S: Stream<Item = Result<Bytes, ApiError>> + Send + 'static,
{
    Box::pin(async_stream::stream! {
        let mut buffer = BytesMut::new();
        let mut scanner = FrameScanner::default();
        let mut total_bytes = 0usize;
        let mut first_frame = true;
        futures_util::pin_mut!(byte_stream);

        while let Some(item) = byte_stream.next().await {
            let chunk = match item {
                Ok(chunk) => chunk,
                Err(error) => {
                    yield Err(error);
                    return;
                }
            };
            total_bytes = match total_bytes.checked_add(chunk.len()) {
                Some(total) if total <= MAX_SSE_TURN_BYTES => total,
                _ => {
                    yield Err(ApiError::Protocol(format!(
                        "SSE turn exceeded {MAX_SSE_TURN_BYTES} bytes"
                    )));
                    return;
                }
            };
            buffer.extend_from_slice(&chunk);

            loop {
                let Some((frame_end, consumed)) = scanner.find_boundary(&buffer) else {
                    if buffer.len() > MAX_SSE_FRAME_BYTES {
                        yield Err(ApiError::Protocol(format!(
                            "SSE frame exceeded {MAX_SSE_FRAME_BYTES} bytes"
                        )));
                        return;
                    }
                    break;
                };
                if frame_end > MAX_SSE_FRAME_BYTES {
                    yield Err(ApiError::Protocol(format!(
                        "SSE frame exceeded {MAX_SSE_FRAME_BYTES} bytes"
                    )));
                    return;
                }

                // `BytesMut::split_to` advances the live buffer without
                // copying its entire remainder. A single HTTP chunk may hold
                // tens of thousands of small SSE frames, so copying the tail
                // once per frame would make decoding quadratic in chunk size.
                let frame = buffer.split_to(consumed);
                scanner = FrameScanner::default();
                let frame_bytes = &frame[..frame_end];
                let frame_bytes = if first_frame {
                    first_frame = false;
                    frame_bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(frame_bytes)
                } else {
                    frame_bytes
                };
                let parsed = parse_sse_frame(frame_bytes);
                drop(frame);
                match parsed {
                    Ok(Some(event)) => yield Ok(event),
                    Ok(None) => {}
                    Err(error) => {
                        yield Err(error);
                        return;
                    }
                }
            }
        }

        // A final event without a blank line is still decoded. The completed
        // gate in the client is what rejects an EOF without response.completed.
        if !buffer.is_empty() {
            if buffer.len() > MAX_SSE_FRAME_BYTES {
                yield Err(ApiError::Protocol(format!(
                    "SSE frame exceeded {MAX_SSE_FRAME_BYTES} bytes"
                )));
                return;
            }
            let frame = if first_frame {
                buffer
                    .strip_prefix(&[0xef, 0xbb, 0xbf])
                    .unwrap_or(&buffer)
            } else {
                &buffer
            };
            match parse_sse_frame(frame) {
                Ok(Some(event)) => yield Ok(event),
                Ok(None) => {}
                Err(error) => yield Err(error),
            }
        }
    })
}

#[derive(Debug, Default)]
struct FrameScanner {
    scan_index: usize,
    line_start: usize,
    frame_end: usize,
}

impl FrameScanner {
    /// Incrementally scan only bytes appended since the previous call. A CR
    /// at the end of a chunk is held until the next byte disambiguates CRLF
    /// from a bare CR, so one large frame delivered byte-by-byte remains O(n).
    fn find_boundary(&mut self, bytes: &[u8]) -> Option<(usize, usize)> {
        while self.scan_index < bytes.len() {
            let (line_end, consumed) = match bytes[self.scan_index] {
                b'\n' => (self.scan_index, self.scan_index + 1),
                b'\r' => {
                    let Some(next) = bytes.get(self.scan_index + 1) else {
                        // A trailing CR that starts an empty line already
                        // proves the frame boundary. Waiting for another byte
                        // here would stall a valid CR-only event until EOF (or
                        // the stream idle timeout). A CR after non-empty data
                        // is still held so a split CRLF is consumed together.
                        if self.scan_index == self.line_start {
                            return Some((self.frame_end, self.scan_index + 1));
                        }
                        return None;
                    };
                    if *next == b'\n' {
                        (self.scan_index, self.scan_index + 2)
                    } else {
                        (self.scan_index, self.scan_index + 1)
                    }
                }
                _ => {
                    self.scan_index += 1;
                    continue;
                }
            };
            if line_end == self.line_start {
                return Some((self.frame_end, consumed));
            }
            self.frame_end = line_end;
            self.line_start = consumed;
            self.scan_index = consumed;
        }
        None
    }
}

fn parse_sse_frame(frame: &[u8]) -> Result<Option<String>, ApiError> {
    let frame = std::str::from_utf8(frame)
        .map_err(|error| ApiError::Protocol(format!("SSE frame is not UTF-8: {error}")))?;
    let mut data_lines = Vec::new();
    for line in frame.split(['\r', '\n']) {
        if line.starts_with(':') {
            continue;
        }
        if let Some(data) = line.strip_prefix("data:") {
            data_lines.push(data.strip_prefix(' ').unwrap_or(data));
        }
    }
    if data_lines.is_empty() {
        return Ok(None);
    }

    Ok(Some(data_lines.join("\n")))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use bytes::Bytes;
    use futures_util::{StreamExt, stream};

    use super::*;

    async fn decode(chunks: Vec<Vec<u8>>) -> Vec<Result<StreamEvent, ApiError>> {
        parse_sse_stream(stream::iter(
            chunks.into_iter().map(|chunk| Ok(Bytes::from(chunk))),
        ))
        .collect()
        .await
    }

    #[tokio::test]
    async fn every_byte_split_preserves_utf8_and_frames() {
        let body = concat!(
            ": keepalive\r\n\r\n",
            "data:{\"type\":\"response.output_text.delta\",\r\n",
            "data: \"delta\":\"Привет 👋\"}\r\n\r\n",
            "data: [DONE]\n\n"
        )
        .as_bytes();

        for split in 0..=body.len() {
            let events = decode(vec![body[..split].to_vec(), body[split..].to_vec()]).await;
            assert!(
                matches!(
                    events.first(),
                    Some(Ok(StreamEvent::OutputTextDelta { delta })) if delta == "Привет 👋"
                ),
                "split={split}: {events:?}"
            );
            assert!(matches!(events.get(1), Some(Ok(StreamEvent::Done))));
        }
    }

    #[tokio::test]
    async fn leading_utf8_bom_is_ignored() {
        let body = concat!(
            "\u{feff}data:{\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\n\n",
            "data:[DONE]\n\n"
        );
        let events = decode(vec![body.as_bytes().to_vec()]).await;
        assert!(matches!(
            events.first(),
            Some(Ok(StreamEvent::OutputTextDelta { delta })) if delta == "ok"
        ));
        assert!(matches!(events.get(1), Some(Ok(StreamEvent::Done))));
    }

    #[tokio::test]
    async fn unknown_event_is_non_fatal() {
        let events = decode(vec![b"data:{\"type\":\"response.future\"}\n\n".to_vec()]).await;
        assert!(matches!(events.as_slice(), [Ok(StreamEvent::Ignored)]));
    }

    #[tokio::test]
    async fn azure_nested_error_event_is_normalized() {
        let body = concat!(
            "data:{\"type\":\"error\",\"error\":{",
            "\"type\":\"too_many_requests\",",
            "\"code\":\"no_capacity\",",
            "\"message\":\"peak demand\",\"param\":null},",
            "\"sequence_number\":2}\n\n"
        );
        let events = decode(vec![body.as_bytes().to_vec()]).await;
        assert!(matches!(
            events.as_slice(),
            [Ok(StreamEvent::Error {
                code: Some(code),
                message,
                param: None
            })] if code == "no_capacity" && message == "peak demand"
        ));
    }

    #[tokio::test]
    async fn top_level_error_event_remains_supported() {
        let body = concat!(
            "data:{\"type\":\"error\",\"code\":\"server_error\",",
            "\"message\":\"try later\",\"param\":null}\n\n"
        );
        let events = decode(vec![body.as_bytes().to_vec()]).await;
        assert!(matches!(
            events.as_slice(),
            [Ok(StreamEvent::Error {
                code: Some(code),
                message,
                param: None
            })] if code == "server_error" && message == "try later"
        ));
    }

    #[tokio::test]
    async fn cr_only_line_endings_and_boundaries_are_supported() {
        let body = concat!(
            "data:{\"type\":\"response.output_text.delta\",\r",
            "data:\"delta\":\"ok\"}\r\r",
            "data:[DONE]\r\r"
        )
        .as_bytes();
        for split in 0..=body.len() {
            let events = decode(vec![body[..split].to_vec(), body[split..].to_vec()]).await;
            assert!(
                matches!(events.first(), Some(Ok(StreamEvent::OutputTextDelta { delta })) if delta == "ok"),
                "split={split}: {events:?}"
            );
            assert!(matches!(events.get(1), Some(Ok(StreamEvent::Done))));
        }
    }

    #[tokio::test]
    async fn trailing_cr_only_boundary_emits_before_eof() {
        let source = stream::once(async {
            Ok(Bytes::from_static(
                b"data:{\"type\":\"response.output_text.delta\",\"delta\":\"ok\"}\r\r",
            ))
        })
        .chain(stream::pending::<Result<Bytes, ApiError>>());
        let mut decoded = parse_sse_stream(source);

        let event = tokio::time::timeout(Duration::from_millis(250), decoded.next()).await;
        assert!(
            matches!(event, Ok(Some(Ok(StreamEvent::OutputTextDelta { ref delta }))) if delta == "ok"),
            "CR-only boundary did not emit before EOF: {event:?}"
        );
    }

    #[tokio::test]
    async fn rejects_oversized_frame() {
        let events = decode(vec![vec![b'x'; MAX_SSE_FRAME_BYTES + 1]]).await;
        assert!(matches!(events.as_slice(), [Err(ApiError::Protocol(_))]));
    }

    #[tokio::test]
    async fn many_small_frames_in_one_chunk_are_processed_linearly() {
        const KEEPALIVE_FRAMES: usize = 100_000;
        let keepalive = b": keepalive\n\n";
        let mut body = Vec::with_capacity(
            KEEPALIVE_FRAMES
                .saturating_mul(keepalive.len())
                .saturating_add(16),
        );
        for _ in 0..KEEPALIVE_FRAMES {
            body.extend_from_slice(keepalive);
        }
        body.extend_from_slice(b"data:[DONE]\n\n");
        assert!(body.len() < MAX_SSE_TURN_BYTES);

        // Keepalive frames emit no item; the final marker proves that the
        // decoder traversed the entire single-chunk body. This shape guards
        // against copying the shrinking remainder for every frame.
        let events = decode(vec![body]).await;
        assert!(matches!(events.as_slice(), [Ok(StreamEvent::Done)]));
    }

    #[tokio::test]
    async fn one_large_frame_split_into_single_bytes_is_scanned_incrementally()
    -> Result<(), serde_json::Error> {
        let delta = "x".repeat(128 * 1024);
        let body = format!(
            "data:{{\"type\":\"response.output_text.delta\",\"delta\":{}}}\n\n",
            serde_json::to_string(&delta)?
        );
        assert!(body.len() < MAX_SSE_FRAME_BYTES);
        let chunks = body.bytes().map(|byte| vec![byte]).collect();

        let events = decode(chunks).await;
        assert!(
            matches!(events.as_slice(), [Ok(StreamEvent::OutputTextDelta { delta: decoded })] if decoded.len() == 128 * 1024)
        );
        Ok(())
    }
}
