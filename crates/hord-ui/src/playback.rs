//! Flight-recorder playback (spec §10.4): a recorded event log replayed on
//! the landing strip at any speed, with a scrubber.
//!
//! A [`Playback`] holds one recording ([`hord_api::recording`]). Its
//! position is an event index: [`Playback::strip_at`] folds the first
//! `index` events into a [`Strip`], so scrubbing to any point shows what the
//! live strip showed then. [`Playback::delay_before`] spaces events as they
//! were recorded, scaled by the speed, with long idle gaps shortened.

use std::ops::RangeInclusive;
use std::time::Duration;

use hord_api::proto;
use hord_api::recording;

use crate::strip::Strip;
use crate::{Error, Result};

/// Longest real-time pause between two events during playback, whatever
/// the recorded gap and speed: an idle minute in a recording should not
/// stall the demo.
pub const MAX_GAP: Duration = Duration::from_secs(2);

/// Fastest and slowest playback speeds accepted.
pub const SPEEDS: RangeInclusive<f64> = 0.1..=1000.0;

/// Strips are checkpointed every this many events, so scrubbing costs at
/// most this many event applications.
const CHECKPOINT_EVERY: usize = 256;

/// A recording, ready to play.
#[derive(Clone, Debug)]
pub struct Playback {
    header: proto::RecordingHeader,
    events: Vec<proto::EventEnvelope>,
    /// `checkpoints[k]` is the strip after `k * CHECKPOINT_EVERY` events.
    checkpoints: Vec<Strip>,
}

impl Playback {
    /// Parse a recording's bytes (JSON Lines, [`recording::FORMAT`]).
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let (header, events) = recording::parse(bytes).map_err(Error::Recording)?;
        Ok(Self::new(header, events))
    }

    /// A playback of already parsed events, in cursor order.
    #[must_use]
    pub fn new(header: proto::RecordingHeader, events: Vec<proto::EventEnvelope>) -> Self {
        let mut checkpoints = vec![Strip::new()];
        let mut strip = Strip::new();
        for (i, event) in events.iter().enumerate() {
            strip.apply(event);
            if (i + 1) % CHECKPOINT_EVERY == 0 {
                checkpoints.push(strip.clone());
            }
        }
        Self {
            header,
            events,
            checkpoints,
        }
    }

    /// The recording's header.
    #[must_use]
    pub fn header(&self) -> &proto::RecordingHeader {
        &self.header
    }

    /// Number of events: the scrubber runs from 0 to this.
    #[must_use]
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Whether the recording has no events.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// The recorded events.
    #[must_use]
    pub fn events(&self) -> &[proto::EventEnvelope] {
        &self.events
    }

    /// Recorded time from the first event to the last.
    #[must_use]
    pub fn duration(&self) -> Duration {
        match (self.events.first(), self.events.last()) {
            (Some(first), Some(last)) => {
                Duration::from_millis(last.at_ms.saturating_sub(first.at_ms))
            }
            _ => Duration::ZERO,
        }
    }

    /// The strip after the first `index` events (clamped to [`Self::len`]).
    #[must_use]
    pub fn strip_at(&self, index: usize) -> Strip {
        let index = index.min(self.events.len());
        let k = (index / CHECKPOINT_EVERY).min(self.checkpoints.len() - 1);
        let mut strip = self.checkpoints[k].clone();
        for event in &self.events[k * CHECKPOINT_EVERY..index] {
            strip.apply(event);
        }
        strip
    }

    /// Real time to wait before showing event `index` (0-based) at
    /// `speed`: the recorded gap since the previous event divided by the
    /// speed, at most [`MAX_GAP`]. Zero for the first event.
    pub fn delay_before(&self, index: usize, speed: f64) -> Result<Duration> {
        let speed = check_speed(speed)?;
        let (Some(prev), Some(next)) = (
            index.checked_sub(1).and_then(|i| self.events.get(i)),
            self.events.get(index),
        ) else {
            return Ok(Duration::ZERO);
        };
        let gap = Duration::from_millis(next.at_ms.saturating_sub(prev.at_ms));
        Ok(gap.div_f64(speed).min(MAX_GAP))
    }
}

/// `speed` if it is within [`SPEEDS`].
pub fn check_speed(speed: f64) -> Result<f64> {
    if SPEEDS.contains(&speed) {
        Ok(speed)
    } else {
        Err(Error::Speed(speed))
    }
}

#[cfg(test)]
mod tests {
    use hord_api::proto::event::Kind;
    use hord_api::recording::Recorder;
    use hord_api::wire;

    use super::*;
    use crate::strip::Stage;

    fn submitted(cursor: u64, at_ms: u64) -> proto::EventEnvelope {
        proto::EventEnvelope {
            cursor,
            at_ms,
            event: Some(wire::event(Kind::Submitted(proto::Submitted {
                submission: cursor,
                change: format!("c{cursor}"),
                actor: None,
                voucher: None,
            }))),
        }
    }

    fn landed(cursor: u64, at_ms: u64, of: u64) -> proto::EventEnvelope {
        proto::EventEnvelope {
            cursor,
            at_ms,
            event: Some(wire::event(Kind::Landed(proto::Landed {
                change: format!("c{of}"),
                position: of,
                ..Default::default()
            }))),
        }
    }

    #[test]
    fn scrubbing_matches_a_straight_fold() -> Result<()> {
        // Enough events to cross several checkpoints.
        let mut events = Vec::new();
        for n in 1..=600u64 {
            events.push(submitted(2 * n - 1, n * 100));
            events.push(landed(2 * n, n * 100 + 50, 2 * n - 1));
        }
        let mut bytes = Recorder::new(Vec::new(), proto::RecordingHeader::default())
            .map_err(Error::Recording)?;
        for e in &events {
            bytes.record(e).map_err(Error::Recording)?;
        }
        let playback = Playback::parse(&bytes.finish().map_err(Error::Recording)?)?;
        assert_eq!(playback.len(), 1200);
        for index in [0, 1, 255, 256, 257, 700, 1199, 1200, 5000] {
            let mut straight = Strip::new();
            for e in events.iter().take(index) {
                straight.apply(e);
            }
            assert_eq!(playback.strip_at(index), straight, "at {index}");
        }
        let end = playback.strip_at(playback.len());
        assert!(
            end.rows()
                .iter()
                .all(|r| matches!(r.stage, Stage::Landed { .. }))
        );
        Ok(())
    }

    #[test]
    fn delays_scale_with_speed_and_are_capped() -> Result<()> {
        let playback = Playback::new(
            proto::RecordingHeader::default(),
            vec![
                submitted(1, 1_000),
                submitted(2, 1_400),
                submitted(3, 600_000),
            ],
        );
        assert_eq!(playback.delay_before(0, 1.0)?, Duration::ZERO);
        assert_eq!(playback.delay_before(1, 1.0)?, Duration::from_millis(400));
        assert_eq!(playback.delay_before(1, 4.0)?, Duration::from_millis(100));
        assert_eq!(playback.delay_before(2, 1.0)?, MAX_GAP);
        assert_eq!(playback.delay_before(9, 1.0)?, Duration::ZERO);
        assert!(playback.delay_before(1, 0.0).is_err());
        assert!(playback.delay_before(1, f64::NAN).is_err());
        assert_eq!(playback.duration(), Duration::from_millis(599_000));
        Ok(())
    }
}
