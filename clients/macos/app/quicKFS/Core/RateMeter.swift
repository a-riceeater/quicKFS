// SPDX-License-Identifier: Apache-2.0
import Foundation

/// A transfer's speed, measured between the moments chunks commit.
///
/// The core counts bytes only when a whole chunk lands (up to 4 MiB), so on a
/// 2 MB/s link progress jumps once every two seconds while the wire is busy
/// the whole time. Bytes over a fixed one-second window then alternate
/// between twice the real rate and zero. Measuring from one commit to another
/// counts only whole chunks over the time they took, which is the real rate.
///
/// When no chunk has landed for longer than the longest recent gap, the
/// reading falls as `rate × gap / idle`: roughly what the rate since the last
/// commit would be if a chunk landed right now. A genuine stall therefore
/// still shows as falling speed instead of being smoothed away.
struct RateMeter {
    /// Commit moments, in seconds on the caller's clock, with total bytes then.
    private var points: [(at: Double, bytes: UInt64)] = []

    /// Measure over at least this many seconds of commits.
    static let window = 3.0
    /// Below this span a burst of commits would read as an absurd rate.
    static let shortest = 0.5
    /// Commits older than this no longer describe the link.
    static let horizon = 12.0

    /// Record a status poll. `sinceCommit` is how long ago the core last
    /// committed, which dates a new total exactly instead of by the poll.
    mutating func observe(bytes: UInt64, at now: Double, sinceCommit: Double?) {
        guard let last = points.last else {
            points.append((now, bytes))
            return
        }
        if bytes < last.bytes {
            points = [(now, bytes)]
            return
        }
        guard bytes > last.bytes else { return }
        let at = min(now, max(last.at, now - max(0, sinceCommit ?? 0)))
        points.append((at, bytes))
        // Keep one point older than the horizon as the window's anchor.
        while points.count > 2, points[1].at < now - Self.horizon {
            points.removeFirst()
        }
    }

    /// Bytes per second, or nil until enough commits give a span.
    func rate(at now: Double) -> Double? {
        guard let last = points.last, points.count >= 2 else { return nil }
        let anchorIndex = points.lastIndex { $0.at <= last.at - Self.window } ?? 0
        let anchor = points[anchorIndex]
        let span = last.at - anchor.at
        guard span >= Self.shortest else { return nil }
        let rate = Double(last.bytes - anchor.bytes) / span
        // Chunks can land in bursts (striped lanes, a pipeline's first wave),
        // so a pause is judged against the longest gap, not the average.
        let gap = zip(points[anchorIndex...], points[(anchorIndex + 1)...])
            .map { $1.at - $0.at }.max() ?? span
        let idle = now - last.at
        return idle > gap ? rate * gap / idle : rate
    }

    mutating func reset() { points.removeAll() }
}
