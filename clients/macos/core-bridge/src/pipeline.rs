// SPDX-License-Identifier: Apache-2.0
//! Sizing for pipelined transfers.
//!
//! Every request in flight shares one QUIC connection, so a chunk's latency is
//! roughly `bytes in flight / link rate`. A fixed 8 x 4 MiB pipeline is ideal
//! on a LAN but on a 30 KB/s relayed path each chunk needs about 18 minutes,
//! far past any timeout. [`Pipeline`] measures the rate that chunks actually
//! complete at and keeps only about [`Tuning::target_latency`] worth of data in
//! flight, so each request finishes in seconds on any link. It grows quickly on
//! a fast link (in-flight can double per completion) and cuts hard after a
//! failure.

use quickfs_client_core::CongestionControl;
use serde::Deserialize;
use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

/// Smallest chunk the controller shrinks to. Each request costs well under
/// 1 KiB of control traffic, so 128 KiB keeps overhead below 1%.
pub const MIN_CHUNK_BYTES: u64 = 128 * 1024;
pub const DEFAULT_MAX_CHUNK_BYTES: u64 = 4 * 1024 * 1024;
pub const DEFAULT_MAX_DEPTH: usize = 8;
pub const MAX_DEPTH_LIMIT: usize = 32;
pub const DEFAULT_RETRY_WINDOW: Duration = Duration::from_secs(300);
/// Most connections one download stripes across. Each costs the server a
/// login (one password hash) and counts against its per-IP login rate.
pub const MAX_CONNECTIONS: usize = 4;
/// Completions the rate estimate looks back over.
const RATE_SAMPLES: usize = 8;
/// Shortest span of completions a rate is measured over.
const MIN_RATE_SPAN: f64 = 0.25;
/// Completions older than this no longer describe the link.
const RATE_HORIZON: Duration = Duration::from_secs(20);

/// Per-session transfer settings, chosen in the app's server settings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Tuning {
    /// Resize the pipeline to the measured link. Off keeps it fixed at the
    /// maxima, which is the pre-adaptive behaviour.
    pub adaptive: bool,
    pub max_depth: usize,
    pub max_chunk: u64,
    /// How long a transfer keeps retrying while no data is committed. Any
    /// committed chunk restarts the window.
    pub retry_window: Duration,
    /// The connection's idle limit, used to pick a safe per-chunk latency.
    pub stall_timeout: Duration,
    /// Separate QUIC connections a download stripes chunks across. Each has
    /// its own congestion window, so on a lossy path where one connection's
    /// rate collapses, several together move proportionally more.
    pub connections: usize,
    /// How this client paces what it sends. Uploads follow it; downloads
    /// follow the server's own setting.
    pub congestion: CongestionControl,
}

impl Default for Tuning {
    fn default() -> Self {
        Self {
            adaptive: true,
            max_depth: DEFAULT_MAX_DEPTH,
            max_chunk: DEFAULT_MAX_CHUNK_BYTES,
            retry_window: DEFAULT_RETRY_WINDOW,
            stall_timeout: Duration::from_secs(30),
            connections: 1,
            congestion: CongestionControl::default(),
        }
    }
}

/// The optional `transfer` object of a `connect` request. Missing fields keep
/// their defaults, and out-of-range values are clamped rather than rejected.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct TuningRequest {
    pub adaptive: Option<bool>,
    pub max_depth: Option<usize>,
    pub max_chunk_bytes: Option<u64>,
    pub retry_window_ms: Option<u64>,
    pub connections: Option<usize>,
    /// `cubic`, `newreno`, or `bbr`; anything else keeps the default.
    pub congestion_control: Option<String>,
}

impl Tuning {
    pub fn from_request(request: Option<TuningRequest>, stall_timeout: Duration) -> Self {
        let request = request.unwrap_or_default();
        let defaults = Self::default();
        Self {
            adaptive: request.adaptive.unwrap_or(defaults.adaptive),
            max_depth: request
                .max_depth
                .unwrap_or(defaults.max_depth)
                .clamp(1, MAX_DEPTH_LIMIT),
            max_chunk: request
                .max_chunk_bytes
                .unwrap_or(defaults.max_chunk)
                .clamp(MIN_CHUNK_BYTES, 16 * 1024 * 1024),
            retry_window: request
                .retry_window_ms
                .map_or(defaults.retry_window, Duration::from_millis),
            stall_timeout,
            connections: request
                .connections
                .unwrap_or(defaults.connections)
                .clamp(1, MAX_CONNECTIONS),
            congestion: request
                .congestion_control
                .and_then(|name| name.parse().ok())
                .unwrap_or(defaults.congestion),
        }
    }

    /// The latency each chunk is sized for: a quarter of the idle limit, so a
    /// chunk that runs long still has room, within 2 to 10 seconds.
    pub fn target_latency(&self) -> Duration {
        (self.stall_timeout / 4).clamp(Duration::from_secs(2), Duration::from_secs(10))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Shape {
    pub depth: usize,
    pub chunk: u64,
}

impl Shape {
    pub fn in_flight(self) -> u64 {
        self.chunk * self.depth as u64
    }
}

pub struct Pipeline {
    tuning: Tuning,
    max_chunk: u64,
    shape: Shape,
    completions: VecDeque<(Instant, u64)>,
}

impl Pipeline {
    /// `limit` is the largest request the server and client both accept.
    pub fn new(tuning: Tuning, limit: u64) -> Self {
        let max_chunk = tuning.max_chunk.min(limit).max(1);
        let shape = if tuning.adaptive {
            // Start at 2 x 256 KiB: at 60 KB/s the first chunk still lands in
            // about 8 s, and a fast link doubles to full size within a few
            // milliseconds of completions.
            Shape {
                depth: 2.min(tuning.max_depth),
                chunk: (256 * 1024).min(max_chunk),
            }
        } else {
            Shape {
                depth: tuning.max_depth,
                chunk: max_chunk,
            }
        };
        Self {
            tuning,
            max_chunk,
            shape,
            completions: VecDeque::new(),
        }
    }

    pub fn shape(&self) -> Shape {
        self.shape
    }

    /// The link rate chunks are completing at, in bytes per second.
    pub fn rate(&self) -> Option<f64> {
        let (first, _) = self.completions.front()?;
        let (last, _) = self.completions.back()?;
        let span = last.duration_since(*first).as_secs_f64();
        // A burst of chunks that arrived while the reader was busy would
        // read as an absurd rate over a few milliseconds.
        if self.completions.len() < 2 || span < MIN_RATE_SPAN {
            return None;
        }
        let bytes: u64 = self
            .completions
            .iter()
            .skip(1)
            .map(|(_, bytes)| bytes)
            .sum();
        Some(bytes as f64 / span)
    }

    /// Record a finished chunk and resize. Returns the new shape when it
    /// changed.
    pub fn complete(&mut self, bytes: u64, latency: Duration, now: Instant) -> Option<Shape> {
        self.completions.push_back((now, bytes));
        while self.completions.len() > RATE_SAMPLES
            || self
                .completions
                .front()
                .is_some_and(|(at, _)| now.duration_since(*at) > RATE_HORIZON)
        {
            self.completions.pop_front();
        }
        if !self.tuning.adaptive {
            return None;
        }
        // Until two completions give a span, one chunk's own latency is the
        // only measure: it took `latency` to move `bytes` while sharing the
        // link with the rest of the pipeline.
        let rate = self
            .rate()
            .unwrap_or_else(|| self.shape.in_flight() as f64 / latency.as_secs_f64().max(0.001));
        let wanted = rate * self.tuning.target_latency().as_secs_f64();
        // Grow at most 2x per completion so one lucky sample cannot flood a
        // link; shrink immediately.
        let wanted = wanted.min(self.shape.in_flight() as f64 * 2.0);
        self.resize(wanted as u64)
    }

    /// A chunk failed. Cut in-flight to a quarter and forget the rate, since
    /// it described the link before whatever went wrong.
    pub fn fail(&mut self) -> Option<Shape> {
        self.completions.clear();
        if !self.tuning.adaptive {
            return None;
        }
        self.resize(self.shape.in_flight() / 4)
    }

    fn resize(&mut self, wanted: u64) -> Option<Shape> {
        let max_depth = self.tuning.max_depth;
        let ceiling = self.max_chunk * max_depth as u64;
        let floor = MIN_CHUNK_BYTES.min(self.max_chunk);
        let wanted = wanted.clamp(floor, ceiling);
        let shape = if max_depth >= 2 && wanted >= 2 * self.max_chunk {
            Shape {
                depth: ((wanted / self.max_chunk) as usize).clamp(2, max_depth),
                chunk: self.max_chunk,
            }
        } else {
            // Two requests keep the link busy across a round trip; below
            // that, make the chunks smaller rather than the pipeline shorter.
            let depth = 2.min(max_depth);
            let per = (wanted / depth as u64).max(1);
            let chunk = (1_u64 << (63 - per.leading_zeros())).clamp(floor, self.max_chunk);
            Shape { depth, chunk }
        };
        (shape != self.shape).then(|| {
            self.shape = shape;
            shape
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1024 * 1024;

    fn tuning() -> Tuning {
        Tuning::default()
    }

    /// Feed completions as a link of `rate` bytes/s would deliver them and
    /// return where the pipeline settles.
    fn settle(pipeline: &mut Pipeline, rate: f64, rounds: usize) -> Shape {
        let mut now = Instant::now();
        for _ in 0..rounds {
            let shape = pipeline.shape();
            // Chunks share the link, so each takes in-flight / rate, and they
            // complete one chunk-time apart.
            let latency = Duration::from_secs_f64(shape.in_flight() as f64 / rate);
            now += Duration::from_secs_f64(shape.chunk as f64 / rate);
            pipeline.complete(shape.chunk, latency, now);
        }
        pipeline.shape()
    }

    #[test]
    fn fast_link_reaches_the_full_pipeline() {
        let mut pipeline = Pipeline::new(tuning(), 16 * MIB);
        assert_eq!(
            pipeline.shape(),
            Shape {
                depth: 2,
                chunk: MIB / 4
            }
        );
        let shape = settle(&mut pipeline, 25e6, 12);
        assert_eq!(
            shape,
            Shape {
                depth: 8,
                chunk: 4 * MIB
            }
        );
    }

    #[test]
    fn slow_link_keeps_each_chunk_within_the_target_latency() {
        let mut pipeline = Pipeline::new(tuning(), 16 * MIB);
        let rate = 30e3;
        let shape = settle(&mut pipeline, rate, 12);
        assert_eq!(shape.depth, 2);
        assert_eq!(shape.chunk, MIN_CHUNK_BYTES);
        let latency = shape.in_flight() as f64 / rate;
        assert!(latency < 10.0, "each chunk takes {latency:.1} s");
    }

    #[test]
    fn medium_link_sizes_in_flight_to_rate_times_target() {
        let mut pipeline = Pipeline::new(tuning(), 16 * MIB);
        // 2 MB/s with a 7.5 s target wants about 15 MB in flight.
        let shape = settle(&mut pipeline, 2e6, 20);
        assert_eq!(shape.chunk, 4 * MIB);
        assert_eq!(shape.depth, 3);
    }

    #[test]
    fn failure_cuts_in_flight_to_a_quarter() {
        let mut pipeline = Pipeline::new(tuning(), 16 * MIB);
        settle(&mut pipeline, 25e6, 12);
        assert_eq!(
            pipeline.fail(),
            Some(Shape {
                depth: 2,
                chunk: 4 * MIB
            })
        );
        assert_eq!(
            pipeline.fail(),
            Some(Shape {
                depth: 2,
                chunk: MIB
            })
        );
        assert_eq!(pipeline.rate(), None);
    }

    #[test]
    fn fixed_mode_never_resizes() {
        let tuning = Tuning {
            adaptive: false,
            ..tuning()
        };
        let mut pipeline = Pipeline::new(tuning, 16 * MIB);
        let shape = settle(&mut pipeline, 30e3, 12);
        assert_eq!(
            shape,
            Shape {
                depth: 8,
                chunk: 4 * MIB
            }
        );
        assert_eq!(pipeline.fail(), None);
    }

    #[test]
    fn server_limit_and_settings_bound_the_shape() {
        let tuning = Tuning {
            max_depth: 3,
            max_chunk: 8 * MIB,
            ..tuning()
        };
        let mut pipeline = Pipeline::new(tuning, 2 * MIB);
        let shape = settle(&mut pipeline, 100e6, 12);
        assert_eq!(
            shape,
            Shape {
                depth: 3,
                chunk: 2 * MIB
            }
        );
    }

    #[test]
    fn requests_are_clamped_not_rejected() {
        let tuning = Tuning::from_request(
            Some(TuningRequest {
                adaptive: None,
                max_depth: Some(0),
                max_chunk_bytes: Some(1),
                retry_window_ms: Some(5_000),
                connections: Some(9),
                congestion_control: Some("warp".into()),
            }),
            Duration::from_secs(60),
        );
        assert!(tuning.adaptive);
        assert_eq!(tuning.max_depth, 1);
        assert_eq!(tuning.max_chunk, MIN_CHUNK_BYTES);
        assert_eq!(tuning.retry_window, Duration::from_secs(5));
        assert_eq!(tuning.connections, MAX_CONNECTIONS);
        assert_eq!(tuning.target_latency(), Duration::from_secs(10));
        assert_eq!(tuning.congestion, CongestionControl::Cubic);
    }

    #[test]
    fn a_named_congestion_controller_is_used() {
        let request: TuningRequest =
            serde_json::from_str(r#"{"congestion_control": "bbr"}"#).unwrap();
        let tuning = Tuning::from_request(Some(request), Duration::from_secs(30));
        assert_eq!(tuning.congestion, CongestionControl::Bbr);
        assert_eq!(
            Tuning::from_request(None, Duration::from_secs(30)).congestion,
            CongestionControl::Cubic
        );
    }
}
