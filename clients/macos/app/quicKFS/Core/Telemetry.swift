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

/// Live link telemetry for one server.
///
/// While the server is mounted it reads the helper's `--stats-file`, which
/// covers all Finder and application traffic; otherwise it samples the
/// app's own browsing session. Counters restart with each new connection, so
/// totals are accumulated from per-sample deltas and a counter that goes
/// backwards is treated as a reconnect rather than negative traffic.
@MainActor
@Observable
final class Telemetry {
    enum Source: Equatable { case none, session, mount }

    private(set) var source: Source = .none
    private(set) var latest: LinkSnapshot?
    private(set) var samples: [TelemetrySample] = []
    private(set) var totalReceived: UInt64 = 0
    private(set) var totalSent: UInt64 = 0
    private(set) var mountPID: Int32?
    private(set) var mountStarted: Date?

    private var previous: (LinkSnapshot, Date, Source)?
    private var nextID = 0
    private var watchers = 0
    private var task: Task<Void, Never>?
    private let connection: ServerConnection
    private let mount: MountController
    private let log: ConsoleLog

    static let window = 120

    init(connection: ServerConnection, mount: MountController, log: ConsoleLog) {
        self.connection = connection
        self.mount = mount
        self.log = log
    }

    var current: TelemetrySample? { samples.last }
    var peakReceive: Double { samples.map(\.receiveRate).max() ?? 0 }
    var peakSend: Double { samples.map(\.sendRate).max() ?? 0 }

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
        var snapshot: LinkSnapshot?
        var source = Source.none
        if mount.isMounted, let url = mount.statsFile {
            struct StatsFile: Decodable { let pid: Int32; let startedMs: UInt64; let link: LinkSnapshot? }
            if let data = try? Data(contentsOf: url) {
                let decoder = JSONDecoder()
                decoder.keyDecodingStrategy = .convertFromSnakeCase
                if let file = try? decoder.decode(StatsFile.self, from: data) {
                    snapshot = file.link
                    mountPID = file.pid
                    mountStarted = Date(timeIntervalSince1970: Double(file.startedMs) / 1000)
                    source = .mount
                }
            }
        } else if connection.isOnline {
            struct Reply: Decodable, Sendable { let link: LinkSnapshot? }
            if let reply: Reply = try? await connection.request("link_stats") {
                snapshot = reply.link
                source = .session
            }
        }
        if !mount.isMounted { mountPID = nil; mountStarted = nil }
        guard let snapshot else {
            self.source = .none
            return
        }
        record(snapshot, source: source)
    }

    private func record(_ snapshot: LinkSnapshot, source: Source) {
        let now = Date.now
        defer {
            previous = (snapshot, now, source)
            latest = snapshot
            self.source = source
        }
        guard let (last, lastDate, lastSource) = previous, lastSource == source else { return }
        if snapshot.reconnects > last.reconnects {
            log.append(.link, .warning, "transport replaced (reconnect #\(snapshot.reconnects))")
        }
        let restarted = snapshot.bytesReceived < last.bytesReceived || snapshot.bytesSent < last.bytesSent
        let received = restarted ? snapshot.bytesReceived : snapshot.bytesReceived - last.bytesReceived
        let sent = restarted ? snapshot.bytesSent : snapshot.bytesSent - last.bytesSent
        let packets = restarted ? snapshot.packetsSent : snapshot.packetsSent - last.packetsSent
        let lost = restarted ? snapshot.packetsLost : snapshot.packetsLost &- last.packetsLost
        if !restarted, snapshot.congestionEvents > last.congestionEvents {
            log.append(.link, .debug, "congestion event; window \(Format.bytes(snapshot.congestionWindow))")
        }
        totalReceived &+= received
        totalSent &+= sent
        let elapsed = max(0.25, now.timeIntervalSince(lastDate))
        nextID += 1
        samples.append(TelemetrySample(
            id: nextID, date: now,
            receiveRate: Double(received) / elapsed,
            sendRate: Double(sent) / elapsed,
            rttMs: Double(snapshot.rttUs) / 1000,
            lossFraction: packets == 0 ? 0 : min(1, Double(lost) / Double(packets))))
        if samples.count > Self.window { samples.removeFirst(samples.count - Self.window) }
    }
}
