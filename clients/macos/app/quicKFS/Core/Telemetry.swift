// SPDX-License-Identifier: Apache-2.0
import Foundation
import Observation

/// Raw QUIC counters, as reported by `client-core`'s `LinkStats`.
struct LinkSnapshot: Decodable, Equatable, Sendable {
    let rttUs: UInt64
    let congestionWindow: UInt64
    let mtu: UInt16
    let bytesSent: UInt64
    let bytesReceived: UInt64
    let datagramsSent: UInt64
    let datagramsReceived: UInt64
    let packetsSent: UInt64
    let packetsLost: UInt64
    let bytesLost: UInt64
    let congestionEvents: UInt64
    let reconnects: UInt64
    /// Absent from helpers older than the protocol counters.
    let `protocol`: ProtocolSnapshot?
}

/// The quickfs protocol as negotiated and used on one connection.
struct ProtocolSnapshot: Decodable, Equatable, Sendable {
    let alpn: String
    let clientVersion: UInt16
    let serverVersion: UInt16
    let compression: Bool
    let streams: UInt64
    let framesSent: UInt64
    let framesReceived: UInt64
    let compressedFramesSent: UInt64
    let compressedFramesReceived: UInt64
    let frameBytesSent: UInt64
    let frameBytesReceived: UInt64
    let payloadBytesSent: UInt64
    let payloadBytesReceived: UInt64
    let dataBytesSent: UInt64
    let dataBytesReceived: UInt64

    /// "6.4" from a packed major/minor word.
    static func version(_ word: UInt16) -> String {
        word == 0 ? "—" : "\(word >> 8).\(word & 0xff)"
    }

    /// Control bytes saved by frame compression, as a fraction of the
    /// uncompressed size (both directions).
    var compressionSavings: Double {
        let payload = payloadBytesSent + payloadBytesReceived
        let wire = frameBytesSent + frameBytesReceived
        return payload == 0 || wire >= payload ? 0 : 1 - Double(wire) / Double(payload)
    }
}

/// One second of link activity, derived from two snapshots.
struct TelemetrySample: Identifiable, Sendable {
    let id: Int
    let date: Date
    let receiveRate: Double
    let sendRate: Double
    let rttMs: Double
    let lossFraction: Double
}

/// One link snapshot from the mount helper and when it arrived.
struct LinkReading: Equatable {
    let link: LinkSnapshot
    let at: Date
    let helperStarted: Date
}

/// A mounted volume's own connection, which telemetry adds to the
/// browsing session's. Only platforms that mount provide one.
@MainActor
protocol MountLinkSource: AnyObject {
    var isMounted: Bool { get }
    var linkReading: LinkReading? { get }
    func requestLinkStats()
}

/// Live link telemetry for one server.
///
/// Each second it samples both links the app may have open: its own
/// browsing session, and while mounted, the helper's connection (asked over
/// the helper's pipes, so nothing is written to disk). Throughput and loss
/// add the two together; round-trip time, window, and MTU follow whichever
/// link carried more traffic in the last second, so an idle link cannot
/// freeze the figures. Counters restart with each new connection, so totals
/// are accumulated from per-sample deltas and a counter that goes backwards
/// is treated as a reconnect rather than negative traffic.
@MainActor
@Observable
final class Telemetry {
    enum Source: Equatable { case none, session, mount, both }

    private(set) var source: Source = .none
    /// Cumulative counters of the link the figures currently follow.
    private(set) var latest: LinkSnapshot?
    private(set) var samples: [TelemetrySample] = []
    private(set) var totalReceived: UInt64 = 0
    private(set) var totalSent: UInt64 = 0

    @ObservationIgnored private var mixer = LinkMixer()
    @ObservationIgnored private var nextID = 0
    @ObservationIgnored private var watchers = 0
    @ObservationIgnored private var task: Task<Void, Never>?
    private let connection: ServerConnection
    /// The mount helper's link, where the platform mounts at all.
    private let mount: (any MountLinkSource)?
    private let log: ConsoleLog

    static let window = 120
    /// A mount reply older than this no longer counts as live.
    private static let mountReplyLifetime: TimeInterval = 3

    init(connection: ServerConnection, mount: (any MountLinkSource)?, log: ConsoleLog) {
        self.connection = connection
        self.mount = mount
        self.log = log
    }

    var current: TelemetrySample? { samples.last }
    var peakReceive: Double { samples.map(\.receiveRate).max() ?? 0 }
    var peakSend: Double { samples.map(\.sendRate).max() ?? 0 }
    /// When the running mount helper started, from its own report.
    var mountStarted: Date? {
        guard let mount, mount.isMounted else { return nil }
        return mount.linkReading?.helperStarted
    }

    /// Standard deviation of recent round-trip times, in milliseconds.
    var jitterMs: Double {
        let recent = samples.suffix(20).map(\.rttMs).filter { $0 > 0 }
        guard recent.count > 1 else { return 0 }
        let mean = recent.reduce(0, +) / Double(recent.count)
        let variance = recent.reduce(0) { $0 + ($1 - mean) * ($1 - mean) } / Double(recent.count - 1)
        return variance.squareRoot()
    }

    var lossPercent: Double {
        guard let latest, latest.packetsSent > 0 else { return 0 }
        return Double(latest.packetsLost) / Double(latest.packetsSent) * 100
    }

    /// Views call this while visible; sampling stops when none are watching.
    func watch() async {
        watchers += 1
        if task == nil {
            task = Task { [weak self] in
                while !Task.isCancelled {
                    await self?.sample()
                    try? await Task.sleep(for: .seconds(1))
                }
            }
        }
        defer {
            watchers -= 1
            if watchers == 0 {
                task?.cancel()
                task = nil
            }
        }
        // Park until the watching view disappears and cancels us.
        while !Task.isCancelled { try? await Task.sleep(for: .seconds(3600)) }
    }

    private func sample() async {
        // Ask the helper first: its reply usually arrives while the session
        // request is in flight, so both readings describe the same second.
        mount?.requestLinkStats()
        var readings: [LinkMixer.Link: LinkMixer.Reading] = [:]
        if connection.isOnline {
            struct Reply: Decodable, Sendable { let link: LinkSnapshot? }
            if let reply: Reply = try? await connection.request("link_stats"), let link = reply.link {
                readings[.session] = .init(snapshot: link, at: .now)
            }
        }
        if let mount, mount.isMounted, let reading = mount.linkReading,
           Date.now.timeIntervalSince(reading.at) < Self.mountReplyLifetime {
            readings[.mount] = .init(snapshot: reading.link, at: reading.at)
        }
        let mixed = mixer.mix(readings)
        for event in mixed.events { log.append(.link, event.warning ? .warning : .debug, event.message) }
        source = switch (readings[.session] != nil, readings[.mount] != nil) {
        case (true, true): .both
        case (true, false): .session
        case (false, true): .mount
        case (false, false): .none
        }
        latest = mixed.followed
        guard let tick = mixed.tick else { return }
        totalReceived &+= tick.received
        totalSent &+= tick.sent
        nextID += 1
        samples.append(TelemetrySample(
            id: nextID, date: .now, receiveRate: tick.receiveRate, sendRate: tick.sendRate,
            rttMs: tick.rttMs, lossFraction: tick.lossFraction))
        if samples.count > Self.window { samples.removeFirst(samples.count - Self.window) }
    }
}

/// Folds per-link counter readings into one figure per second.
///
/// Throughput and loss are summed across links. Round-trip time and the
/// cumulative counters follow the link that moved the most bytes, and stay
/// on it while every link is idle, so the figures never jump to a link that
/// is merely sitting open. A link missing from a tick is forgotten, so a new
/// connection (a new mount, a session that came back) starts from its own
/// baseline instead of being diffed against the old one.
struct LinkMixer {
    enum Link: Hashable { case session, mount }

    struct Reading {
        let snapshot: LinkSnapshot
        /// When the counters were read; a reading no newer than the last one
        /// for its link carries no new traffic.
        let at: Date
    }

    struct Tick {
        let received: UInt64
        let sent: UInt64
        let receiveRate: Double
        let sendRate: Double
        let rttMs: Double
        let lossFraction: Double
    }

    struct Event {
        let warning: Bool
        let message: String
    }

    struct Mixed {
        /// Counters of the link the figures follow.
        let followed: LinkSnapshot?
        /// Nil until some link has two readings to compare.
        let tick: Tick?
        let events: [Event]
    }

    private struct Delta {
        let received: UInt64
        let sent: UInt64
        let packets: UInt64
        let lost: UInt64
        let elapsed: TimeInterval
        var bytes: UInt64 { received &+ sent }
    }

    private var previous: [Link: Reading] = [:]
    private(set) var following: Link = .session

    mutating func mix(_ readings: [Link: Reading]) -> Mixed {
        for link in previous.keys where readings[link] == nil { previous[link] = nil }
        var events: [Event] = []
        var deltas: [Link: Delta] = [:]
        for (link, reading) in readings {
            if let last = previous[link] {
                guard reading.at > last.at else { continue }
                deltas[link] = Self.delta(link, from: last, to: reading, events: &events)
            }
            previous[link] = reading
        }

        if let busiest = deltas.max(by: { $0.value.bytes < $1.value.bytes }), busiest.value.bytes > 0 {
            following = busiest.key
        } else if readings[following] == nil {
            following = readings[.session] != nil ? .session : .mount
        }
        let followed = readings[following]?.snapshot

        guard !deltas.isEmpty, let followed else {
            return Mixed(followed: followed, tick: nil, events: events)
        }
        let packets = deltas.values.reduce(0) { $0 &+ $1.packets }
        let lost = deltas.values.reduce(0) { $0 &+ $1.lost }
        let tick = Tick(
            received: deltas.values.reduce(0) { $0 &+ $1.received },
            sent: deltas.values.reduce(0) { $0 &+ $1.sent },
            receiveRate: deltas.values.reduce(0) { $0 + Double($1.received) / $1.elapsed },
            sendRate: deltas.values.reduce(0) { $0 + Double($1.sent) / $1.elapsed },
            rttMs: Double(followed.rttUs) / 1000,
            lossFraction: packets == 0 ? 0 : min(1, Double(lost) / Double(packets)))
        return Mixed(followed: followed, tick: tick, events: events)
    }

    private static func delta(_ link: Link, from last: Reading, to reading: Reading,
                              events: inout [Event]) -> Delta {
        let old = last.snapshot
        let new = reading.snapshot
        let name = link == .mount ? "mount" : "session"
        if new.reconnects > old.reconnects {
            events.append(Event(warning: true, message: "\(name) transport replaced (reconnect #\(new.reconnects))"))
        }
        let restarted = new.bytesReceived < old.bytesReceived || new.bytesSent < old.bytesSent
        if !restarted, new.congestionEvents > old.congestionEvents {
            events.append(Event(warning: false,
                message: "\(name) congestion event; window \(Format.bytes(new.congestionWindow))"))
        }
        return Delta(
            received: restarted ? new.bytesReceived : new.bytesReceived - old.bytesReceived,
            sent: restarted ? new.bytesSent : new.bytesSent - old.bytesSent,
            packets: restarted ? new.packetsSent : new.packetsSent &- old.packetsSent,
            lost: restarted ? new.packetsLost : new.packetsLost &- old.packetsLost,
            elapsed: max(0.25, reading.at.timeIntervalSince(last.at)))
    }
}
