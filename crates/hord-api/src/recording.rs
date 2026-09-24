//! Flight-recorder logs: the event stream written down, replayable.
//!
//! A recording is JSON Lines: a [`proto::RecordingHeader`], then one
//! [`proto::EventEnvelope`] per line in cursor order, each in the canonical
//! protobuf JSON mapping. It is what M5's `GET /api/v1/recordings/{id}`
//! serves; stored as a `Blob` object, its object id names it.

use std::io::Write;

use crate::{ApiError, ApiResult, proto};

/// [`proto::RecordingHeader::format`] of this layout.
pub const FORMAT: &str = "hord.recording.v1";

/// Writes a recording line by line.
#[derive(Debug)]
pub struct Recorder<W: Write> {
    out: W,
    last: u64,
}

impl<W: Write> Recorder<W> {
    /// Start a recording on `out` with `header` (its format is set).
    pub fn new(mut out: W, mut header: proto::RecordingHeader) -> ApiResult<Self> {
        header.format = FORMAT.into();
        let last = header.from_cursor;
        line(&mut out, &header)?;
        Ok(Self { out, last })
    }

    /// Append one event. Cursors must increase.
    pub fn record(&mut self, event: &proto::EventEnvelope) -> ApiResult<()> {
        if event.cursor <= self.last {
            return Err(ApiError::InvalidArgument(format!(
                "recording: cursor {} after {}",
                event.cursor, self.last
            )));
        }
        self.last = event.cursor;
        line(&mut self.out, event)
    }

    /// Flush and return the writer.
    pub fn finish(mut self) -> ApiResult<W> {
        self.out.flush().map_err(io)?;
        Ok(self.out)
    }
}

fn io(err: std::io::Error) -> ApiError {
    ApiError::Internal(format!("recording: {err}"))
}

fn line<W: Write, T: serde::Serialize>(out: &mut W, value: &T) -> ApiResult<()> {
    serde_json::to_writer(&mut *out, value)
        .map_err(|err| ApiError::Internal(format!("recording: {err}")))?;
    out.write_all(b"\n").map_err(io)
}

/// Read a recording back: its header and events, checking the format and
/// that cursors increase.
pub fn parse(bytes: &[u8]) -> ApiResult<(proto::RecordingHeader, Vec<proto::EventEnvelope>)> {
    let bad = |what: String| ApiError::InvalidArgument(format!("recording: {what}"));
    let text = std::str::from_utf8(bytes).map_err(|e| bad(e.to_string()))?;
    let mut lines = text.lines().filter(|l| !l.trim().is_empty());
    let header: proto::RecordingHeader =
        serde_json::from_str(lines.next().ok_or_else(|| bad("empty".into()))?)
            .map_err(|e| bad(format!("header: {e}")))?;
    if header.format != FORMAT {
        return Err(bad(format!("unknown format {:?}", header.format)));
    }
    let mut last = header.from_cursor;
    let mut events = Vec::new();
    for (i, text) in lines.enumerate() {
        let event: proto::EventEnvelope =
            serde_json::from_str(text).map_err(|e| bad(format!("line {}: {e}", i + 2)))?;
        if event.cursor <= last {
            return Err(bad(format!(
                "line {}: cursor {} after {last}",
                i + 2,
                event.cursor
            )));
        }
        last = event.cursor;
        events.push(event);
    }
    Ok((header, events))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(cursor: u64) -> proto::EventEnvelope {
        proto::EventEnvelope {
            cursor,
            at_ms: 5,
            event: Some(crate::wire::event(proto::event::Kind::HeadMoved(
                proto::HeadMoved {
                    from: None,
                    to: "ab".into(),
                },
            ))),
        }
    }

    #[test]
    fn a_recording_replays_what_was_recorded() {
        let header = proto::RecordingHeader {
            repo: "r".into(),
            from_cursor: 2,
            ..Default::default()
        };
        let mut rec = Recorder::new(Vec::new(), header).unwrap();
        rec.record(&event(3)).unwrap();
        rec.record(&event(7)).unwrap();
        assert!(rec.record(&event(7)).is_err(), "cursors must increase");
        let bytes = rec.finish().unwrap();
        let (header, events) = parse(&bytes).unwrap();
        assert_eq!(header.format, FORMAT);
        assert_eq!(header.repo, "r");
        assert_eq!(events, vec![event(3), event(7)]);
        assert!(parse(b"{\"format\":\"other\"}\n").is_err());
    }
}
