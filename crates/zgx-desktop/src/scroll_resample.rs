//! Display-paced scrolling.
//!
//! Trackpads report motion on their own clock, out of phase with the display:
//! applying each event on arrival moves content 0, 1 or 2 events' worth per
//! refresh, which reads as judder even at full frame rate. Continuous (pixel)
//! input is instead resampled between buffered events. Irregular or sparse
//! input stays one interval behind; a stable stream at a multiple of the
//! display rate gradually moves to half an interval for less finger latency.
//! Discrete wheel notches have no motion curve of
//! their own; they ease towards their destination over a few refreshes.
use std::{collections::VecDeque, time::Duration, time::Instant};

/// A pause this long ends a stroke: whatever is still buffered is applied.
const STALL: Duration = Duration::from_millis(50);
/// Refreshes keep running this long after the last input, so a gesture never
/// stops and restarts the display link (a lost refresh each time) when the
/// applied motion momentarily catches up with the input.
const LINGER: Duration = Duration::from_millis(120);
/// Bounds for the sampling delay, which tracks the input interval.
const MIN_DELAY: Duration = Duration::from_millis(4);
const MAX_DELAY: Duration = Duration::from_millis(20);
/// Require a run of regular events before shortening the sampling delay.
const STABLE_INTERVALS: u8 = 8;
/// Wheel notches close this fraction of the remaining distance per second, in
/// the exponent: about 95% within 100 ms.
const WHEEL_RATE: f32 = 30.;

/// `ZGX_SCROLL_RESAMPLE=0` applies wheel input on arrival instead.
pub(crate) fn enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| zgx::env::var_os("ZGX_SCROLL_RESAMPLE").is_none_or(|v| v != "0"))
}

#[derive(Default)]
pub(crate) struct ScrollResampler {
    /// Arrival time and cumulative position of each buffered event.
    points: VecDeque<(Instant, (f32, f32))>,
    /// Cumulative continuous motion received and applied.
    received: (f32, f32),
    applied: (f32, f32),
    /// Smoothed spacing between continuous events.
    interval: Option<Duration>,
    stable_intervals: u8,
    sample_delay: Option<Duration>,
    /// Wheel distance not yet applied, and when it last advanced.
    wheel: (f32, f32),
    wheel_at: Option<Instant>,
    /// When the latest input arrived.
    last_input: Option<Instant>,
    /// Pointer position of the latest input; motion is delivered there.
    pub position: (f32, f32),
}

impl ScrollResampler {
    pub fn pending(&self) -> bool {
        self.received != self.applied || self.wheel != (0., 0.)
    }
    /// A gesture is under way: refreshes should keep coming at full rate.
    pub fn active(&self, now: Instant) -> bool {
        self.pending()
            || self
                .last_input
                .is_some_and(|at| now.duration_since(at) < LINGER)
    }
    /// Buffer pixel-precise motion that arrived at `now`.
    pub fn push_precise(&mut self, now: Instant, delta: (f32, f32)) {
        let last = self.last_input.replace(now);
        match last {
            Some(last) if now.duration_since(last) < STALL => {
                let gap = now.duration_since(last).max(Duration::from_micros(500));
                self.stable_intervals = match self.interval {
                    Some(interval)
                        if gap.abs_diff(interval)
                            <= (interval / 10).max(Duration::from_micros(500)) =>
                    {
                        self.stable_intervals
                            .saturating_add(1)
                            .min(STABLE_INTERVALS)
                    }
                    _ => 0,
                };
                self.interval = Some(match self.interval {
                    Some(interval) => interval.mul_f32(0.7) + gap.mul_f32(0.3),
                    None => gap,
                });
            }
            _ => {
                // A new stroke: the first event's motion spans one interval
                // before it, not the whole idle gap.
                let delay = self.delay();
                self.stable_intervals = 0;
                self.sample_delay = None;
                self.points.clear();
                // A stalled frame may not have delivered the previous event
                // yet. Keep that debt: the next sample (or flush) pays it.
                self.points
                    .push_back((now.checked_sub(delay).unwrap_or(now), self.received));
            }
        }
        self.received.0 += delta.0;
        self.received.1 += delta.1;
        self.points.push_back((now, self.received));
    }
    /// Queue a discrete wheel step to ease in over the next refreshes.
    pub fn push_wheel(&mut self, now: Instant, delta: (f32, f32)) {
        self.last_input = Some(now);
        if self.wheel == (0., 0.) {
            self.wheel_at = Some(now);
        }
        self.wheel.0 += delta.0;
        self.wheel.1 += delta.1;
    }
    /// Deliver everything at once (no display refreshes are coming).
    pub fn flush(&mut self) -> Option<(f32, f32)> {
        let delta = (
            self.received.0 - self.applied.0 + self.wheel.0,
            self.received.1 - self.applied.1 + self.wheel.1,
        );
        self.applied = self.received;
        self.points.clear();
        self.wheel = (0., 0.);
        self.wheel_at = None;
        self.stable_intervals = 0;
        self.sample_delay = None;
        (delta != (0., 0.)).then_some(delta)
    }
    /// The motion to apply at a display refresh processed at `now`.
    pub fn sample(&mut self, now: Instant, refresh_interval: Duration) -> Option<(f32, f32)> {
        let full_delay = self.delay();
        let aligned = self.interval.is_some_and(|interval| {
            let ratio = refresh_interval.as_secs_f64() / interval.as_secs_f64();
            let multiple = ratio.round().max(1.);
            (ratio - multiple).abs() <= multiple * 0.1
        });
        let desired = if self.stable_intervals == STABLE_INTERVALS && aligned {
            full_delay / 2
        } else {
            full_delay
        };
        // Approach a shorter delay over several refreshes, rather than
        // jumping the content ahead. Jitter restores the safe delay at once;
        // `advance` below prevents that older horizon moving content back.
        let delay = self
            .sample_delay
            .unwrap_or(full_delay)
            .saturating_sub(refresh_interval / 10)
            .max(desired)
            .min(full_delay);
        self.sample_delay = Some(delay);
        let target = match self.points.back() {
            Some((last, total)) if now.duration_since(*last) >= STALL => *total,
            Some(_) => self.position_at(now.checked_sub(delay).unwrap_or(now)),
            None => self.received,
        };
        // Never step back: a late sample must not undo motion already shown.
        let continuous = (
            advance(self.applied.0, target.0, self.received.0),
            advance(self.applied.1, target.1, self.received.1),
        );
        let mut delta = (continuous.0 - self.applied.0, continuous.1 - self.applied.1);
        self.applied = continuous;
        // Keep one point at or before the sample time for interpolation.
        let horizon = now.checked_sub(delay).unwrap_or(now);
        while self.points.len() > 1 && self.points[1].0 <= horizon {
            self.points.pop_front();
        }
        if let Some(at) = self.wheel_at.replace(now) {
            let elapsed = now.duration_since(at).as_secs_f32();
            let share = 1. - (-WHEEL_RATE * elapsed).exp();
            for (remaining, out) in [
                (&mut self.wheel.0, &mut delta.0),
                (&mut self.wheel.1, &mut delta.1),
            ] {
                // Whole pixels: content stays on the pixel grid, and the
                // offset sums exactly even tens of thousands of pixels down,
                // where an f32 holds fractions of a pixel only roughly.
                let step = if remaining.abs() <= 1. {
                    *remaining
                } else {
                    let step = (*remaining * share).round();
                    let step = if step == 0. { remaining.signum() } else { step };
                    step.clamp(-remaining.abs(), remaining.abs())
                };
                *remaining -= step;
                *out += step;
            }
            if self.wheel == (0., 0.) {
                self.wheel_at = None;
            }
        }
        (delta != (0., 0.)).then_some(delta)
    }
    fn delay(&self) -> Duration {
        self.interval
            .unwrap_or(Duration::from_millis(8))
            .clamp(MIN_DELAY, MAX_DELAY)
    }
    /// The margin chosen by the last refresh, for opt-in presentation logs.
    pub fn sampling_delay(&self) -> Duration {
        self.sample_delay.unwrap_or_else(|| self.delay())
    }
    /// Cumulative position at `time`, linear between the events around it.
    fn position_at(&self, time: Instant) -> (f32, f32) {
        let mut previous = match self.points.front() {
            Some((at, position)) if time <= *at => return *position,
            Some(point) => *point,
            None => return self.received,
        };
        for &(at, position) in self.points.iter().skip(1) {
            if time <= at {
                let span = at.duration_since(previous.0).as_secs_f32();
                let t = if span > 0. {
                    time.duration_since(previous.0).as_secs_f32() / span
                } else {
                    1.
                };
                return (
                    previous.1.0 + (position.0 - previous.1.0) * t,
                    previous.1.1 + (position.1 - previous.1.1) * t,
                );
            }
            previous = (at, position);
        }
        previous.1
    }
}

/// Move from `applied` towards `target` without passing `received` or
/// reversing the direction of travel.
fn advance(applied: f32, target: f32, received: f32) -> f32 {
    if received >= applied {
        target.clamp(applied, received)
    } else {
        target.clamp(received, applied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    const MS: Duration = Duration::from_millis(1);

    /// 120 Hz input sampled by a 120 Hz display half a period out of phase:
    /// applying events on arrival alternates 0/2-event frames; resampled
    /// frames each advance by one event's worth.
    #[test]
    fn out_of_phase_input_advances_evenly() {
        let start = Instant::now();
        let mut resampler = ScrollResampler::default();
        let step = Duration::from_micros(8333);
        let mut deltas = Vec::new();
        for i in 0..60u32 {
            // Two events land in some refresh intervals and none in others.
            let jitter = if i % 3 == 0 { 3 * MS } else { Duration::ZERO };
            resampler.push_precise(start + step * i + jitter, (0., 10.));
            let refresh = start + step * i + step / 2 + 2 * MS;
            if let Some((_, dy)) = resampler.sample(refresh, Duration::from_micros(8333)) {
                deltas.push(dy);
            }
        }
        let steady = &deltas[4..deltas.len() - 4];
        for dy in steady {
            assert!((dy - 10.).abs() < 4.5, "{deltas:?}");
        }
    }

    #[test]
    fn a_stalled_stroke_applies_everything() {
        let start = Instant::now();
        let mut resampler = ScrollResampler::default();
        resampler.push_precise(start, (0., 12.));
        resampler.push_precise(start + 8 * MS, (0., 12.));
        let first = resampler
            .sample(start + 9 * MS, Duration::from_micros(8333))
            .unwrap_or_default()
            .1;
        let rest = resampler
            .sample(start + 200 * MS, Duration::from_micros(8333))
            .unwrap_or_default()
            .1;
        assert!((first + rest - 24.).abs() < 1e-3);
        assert!(!resampler.pending());
    }

    #[test]
    fn restarting_after_a_stall_preserves_undelivered_motion() {
        for gap in [49, 50, 60, 100] {
            for delta in [(10., 20.), (-10., -20.), (-10., 20.)] {
                let start = Instant::now();
                let mut resampler = ScrollResampler::default();
                resampler.push_precise(start, delta);
                let first = resampler
                    .sample(start + 2 * MS, Duration::from_micros(8333))
                    .unwrap_or_default();
                resampler.push_precise(start + gap * MS, delta);
                let rest = resampler.flush().unwrap_or_default();
                assert!((first.0 + rest.0 - 2. * delta.0).abs() < 1e-3);
                assert!((first.1 + rest.1 - 2. * delta.1).abs() < 1e-3);
                assert!(!resampler.pending());
            }
        }
    }

    #[test]
    fn restarting_after_a_stall_delivers_the_debt_on_a_frame() {
        let start = Instant::now();
        let mut resampler = ScrollResampler::default();
        resampler.push_precise(start, (0., 10.));
        let first = resampler
            .sample(start + 2 * MS, Duration::from_micros(8333))
            .unwrap_or_default()
            .1;
        resampler.push_precise(start + 60 * MS, (0., 10.));
        let rest = resampler
            .sample(start + 100 * MS, Duration::from_micros(8333))
            .unwrap_or_default()
            .1;
        assert!((first + rest - 20.).abs() < 1e-3);
        assert!(!resampler.pending());
    }

    #[test]
    fn wheel_notches_ease_in_and_finish() {
        let start = Instant::now();
        let mut resampler = ScrollResampler::default();
        resampler.push_wheel(start, (0., 84.));
        let mut total = 0.;
        let mut steps = Vec::new();
        for i in 1..=30u32 {
            if let Some((_, dy)) = resampler.sample(
                start + Duration::from_micros(8333) * i,
                Duration::from_micros(8333),
            ) {
                total += dy;
                steps.push(dy);
            }
        }
        assert!((total - 84.).abs() < 1e-3);
        assert!(steps[0] > steps[3], "{steps:?}");
        assert!(steps.iter().all(|step| step.fract() == 0.), "{steps:?}");
        assert!(!resampler.pending());
    }

    /// Eased notches applied far down long content land exactly on their
    /// destination, as f32 sums of fractional steps would not.
    #[test]
    fn wheel_notches_land_exactly_far_down() {
        let start = Instant::now();
        let mut resampler = ScrollResampler::default();
        let mut offset = 79_952_f32;
        for notch in 0..5u32 {
            resampler.push_wheel(start + Duration::from_millis(25) * notch, (0., 56.));
        }
        for i in 1..=60u32 {
            if let Some((_, dy)) = resampler.sample(
                start + Duration::from_micros(8333) * i,
                Duration::from_micros(8333),
            ) {
                offset += dy;
            }
        }
        assert_eq!(offset, 79_952. + 280.);
        assert!(!resampler.pending());
    }

    #[test]
    fn flush_delivers_all_buffered_motion() {
        let start = Instant::now();
        let mut resampler = ScrollResampler::default();
        resampler.push_precise(start, (3., 5.));
        resampler.push_wheel(start, (0., 28.));
        assert_eq!(resampler.flush(), Some((3., 33.)));
        assert!(!resampler.pending());
    }

    /// Exercise the real sampler at several input/display phases. Distance
    /// behind the latest received event is expressed as equivalent input time.
    fn stroke(display_hz: u32, input_hz: u32, jitter: bool) -> (f64, f64) {
        let frame = Duration::from_secs_f64(1. / f64::from(display_hz));
        let input = Duration::from_secs_f64(1. / f64::from(input_hz));
        let velocity = 1200.;
        let delta = velocity / input_hz as f32;
        let mut lags = Vec::new();
        let mut steps = Vec::new();
        for phase in 0..16 {
            let start = Instant::now();
            let mut resampler = ScrollResampler::default();
            let mut next = 0;
            let mut received = 0.;
            let mut applied = 0.;
            for index in 0..display_hz * 3 {
                let at = frame * index + frame * phase / 16;
                while next < input_hz * 3 {
                    let event = input * next
                        + if jitter && next % 3 == 0 {
                            input.mul_f32(0.3)
                        } else {
                            Duration::ZERO
                        };
                    if event > at {
                        break;
                    }
                    resampler.push_precise(start + event, (0., delta));
                    received += delta;
                    next += 1;
                }
                let step = resampler.sample(start + at, frame).unwrap_or_default().1;
                applied += step;
                assert!(step >= 0., "forward input must never move backwards");
                assert!(
                    applied <= received + 0.01,
                    "never predict past received input"
                );
                if index > display_hz / 2 {
                    lags.push(f64::from(received - applied) / f64::from(velocity) * 1000.);
                    steps.push(f64::from(step));
                }
            }
            applied += resampler.flush().unwrap_or_default().1;
            assert!((applied - received).abs() < 0.01);
        }
        let mean = steps.iter().sum::<f64>() / steps.len() as f64;
        let cv = (steps.iter().map(|step| (step - mean).powi(2)).sum::<f64>() / steps.len() as f64)
            .sqrt()
            / mean;
        (lags.iter().sum::<f64>() / lags.len() as f64, cv)
    }

    #[test]
    fn regular_input_is_responsive_without_uneven_frames() {
        for (display, input, max_lag) in [(60, 60, 3.), (60, 120, 1.5), (120, 120, 1.5)] {
            let (lag, cv) = stroke(display, input, false);
            assert!(lag < max_lag, "{display}/{input}: {lag} ms behind input");
            assert!(cv < 0.001, "{display}/{input}: uneven motion {cv}");
        }
    }

    #[test]
    fn sparse_or_mismatched_input_keeps_even_frames() {
        for (display, input) in [(120, 60), (60, 100), (144, 120), (240, 60)] {
            let (_, cv) = stroke(display, input, false);
            assert!(cv < 0.001, "{display}/{input}: uneven motion {cv}");
        }
    }

    #[test]
    fn jittery_input_keeps_the_interpolation_margin() {
        let (lag, cv) = stroke(60, 60, true);
        assert!(lag > 7., "irregular input needs its interpolation margin");
        assert!(cv < 0.21, "jitter must stay smoothed, got {cv}");
    }

    #[test]
    fn jitter_after_a_regular_run_never_reverses_or_loses_distance() {
        let start = Instant::now();
        let frame = Duration::from_micros(8333);
        let mut resampler = ScrollResampler::default();
        let mut received = 0.;
        let mut applied = 0.;
        for index in 0..120 {
            let jitter = if index >= 60 && index % 3 == 0 {
                3 * MS
            } else {
                Duration::ZERO
            };
            resampler.push_precise(start + frame * index + jitter, (0., 10.));
            received += 10.;
            let step = resampler
                .sample(start + frame * index + 6 * MS, frame)
                .unwrap_or_default()
                .1;
            assert!(step >= 0.);
            applied += step;
            assert!(applied <= received + 0.001);
        }
        applied += resampler.flush().unwrap_or_default().1;
        assert!((received - applied).abs() < 0.001);
    }

    #[test]
    fn reversal_after_a_regular_run_preserves_the_exact_destination() {
        let start = Instant::now();
        let frame = Duration::from_micros(8333);
        let mut resampler = ScrollResampler::default();
        let mut applied = 0.;
        for index in 0..120 {
            resampler.push_precise(
                start + frame * index,
                (0., if index < 60 { 10. } else { -10. }),
            );
            applied += resampler
                .sample(start + frame * index + 4 * MS, frame)
                .unwrap_or_default()
                .1;
        }
        applied += resampler.flush().unwrap_or_default().1;
        assert!(applied.abs() < 0.001, "reversal stranded {applied} points");
    }
}
