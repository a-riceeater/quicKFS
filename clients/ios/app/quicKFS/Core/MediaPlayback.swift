// SPDX-License-Identifier: Apache-2.0
import AVFoundation
import Observation
import os
import UniformTypeIdentifiers

/// Plays a remote video or audio file while it streams.
///
/// The player is handed a loopback HTTP URL served by `MediaHTTPServer`, so
/// AVFoundation asks this app, not the network, for every byte range it
/// wants: the header, the index (often at the end of the file), then the
/// stretch after the playhead, and a new stretch on each seek. Each range is
/// read from the core's cached view of the file, whose read-ahead keeps the
/// next blocks in flight. Nothing is downloaded up front and nothing is
/// written outside the bounded streaming cache.
@MainActor
@Observable
final class MediaPlayback {
    enum Phase {
        case opening
        case ready(AVPlayer)
        case failed(String)
    }

    let node: RemoteNode
    private(set) var phase: Phase = .opening

    @ObservationIgnored private let connection: ServerConnection
    @ObservationIgnored private let cacheBytes: UInt64
    @ObservationIgnored private var stream: RemoteStream?
    @ObservationIgnored private var server: MediaHTTPServer?
    @ObservationIgnored private var player: AVPlayer?
    @ObservationIgnored private var statusObservation: NSKeyValueObservation?
    @ObservationIgnored private var closed = false

    @ObservationIgnored private let report: MediaReport

    init(node: RemoteNode, connection: ServerConnection, cacheBytes: UInt64, log: ConsoleLog) {
        self.node = node
        self.connection = connection
        self.cacheBytes = cacheBytes
        report = MediaReport(log: log)
    }

    /// Whether AVFoundation can play files of this type at all.
    nonisolated static func canPlay(_ node: RemoteNode) -> Bool {
        guard let type = node.contentType else { return false }
        let playable = AVURLAsset.audiovisualTypes().compactMap { UTType($0.rawValue) }
        return playable.contains { type.conforms(to: $0) }
    }

    func open() async {
        guard stream == nil, !closed else { return }
        let opened: RemoteStream
        do {
            opened = try await RemoteStream.open(node, connection: connection, cacheBytes: cacheBytes)
        } catch {
            phase = .failed(error.localizedDescription)
            return
        }
        guard !closed else {
            opened.close()
            return
        }
        stream = opened
        let mime = node.contentType?.preferredMIMEType ?? "application/octet-stream"
        report.note(.info, "playing \(node.displayName), \(Format.bytes(opened.size)), \(mime)")
        let server = MediaHTTPServer(stream: opened, name: node.displayName, contentType: mime, report: report)
        self.server = server
        let url: URL
        do {
            url = try await server.start()
        } catch {
            phase = .failed(error.localizedDescription)
            return
        }
        guard !closed else { return server.stop() }
        let asset = AVURLAsset(url: url)
        let item = AVPlayerItem(asset: asset)
        let player = AVPlayer(playerItem: item)
        self.player = player
        observeStalls(player: player, item: item)
        statusObservation = item.observe(\.status) { [weak self] item, _ in
            guard item.status == .failed else { return }
            let message = item.error?.localizedDescription ?? "This file cannot be played."
            Task { @MainActor in self?.phase = .failed(message) }
        }
        phase = .ready(player)
        player.play()
    }

    @ObservationIgnored private var stallObservers: [any NSObjectProtocol] = []
    @ObservationIgnored private var waitObservation: NSKeyValueObservation?

    /// Log what the player itself reports when it runs dry.
    private func observeStalls(player: AVPlayer, item: AVPlayerItem) {
        let center = NotificationCenter.default
        stallObservers.append(center.addObserver(
            forName: AVPlayerItem.playbackStalledNotification, object: item, queue: nil
        ) { [report] _ in
            mediaLog.notice("player stalled")
            report.note(.warning, "player ran out of data")
        })
        stallObservers.append(center.addObserver(
            forName: AVPlayerItem.newErrorLogEntryNotification, object: item, queue: nil
        ) { [report] notification in
            let entry = (notification.object as? AVPlayerItem)?.errorLog()?.events.last
            report.note(.error, "player error \(entry?.errorStatusCode ?? 0): \(entry?.errorComment ?? "")")
            mediaLog.error("player error \(entry?.errorStatusCode ?? 0, privacy: .public): \(entry?.errorComment ?? "", privacy: .public)")
        })
        waitObservation = player.observe(\.timeControlStatus) { [report] player, _ in
            let reason = player.reasonForWaitingToPlay?.rawValue ?? "none"
            if player.timeControlStatus == .waitingToPlayAtSpecifiedRate {
                let seconds = player.currentTime().seconds
                report.note(.notice, String(format: "player waiting at %.1f s: %@", seconds.isFinite ? seconds : 0,
                                            reason.replacingOccurrences(of: "AVPlayerWaiting", with: "")))
            }
            mediaLog.info("time control \(player.timeControlStatus.rawValue, privacy: .public), waiting: \(reason, privacy: .public)")
        }
    }

    /// Stop playback and release the remote file.
    func close() {
        closed = true
        statusObservation = nil
        waitObservation = nil
        for observer in stallObservers { NotificationCenter.default.removeObserver(observer) }
        stallObservers.removeAll()
        player?.pause()
        player?.replaceCurrentItem(with: nil)
        player = nil
        if let server {
            server.stop()
            report.note(.info, server.summary)
        }
        server = nil
        stream?.close()
        stream = nil
    }
}

/// Streaming diagnostics: `log stream --predicate 'subsystem == "org.quickfs.client" AND category == "media"'`.
let mediaLog = Logger(subsystem: "org.quickfs.client", category: "media")

/// Streaming events for the server's console, so a stall on a phone can be
/// copied from the app itself.
struct MediaReport: Sendable {
    let log: ConsoleLog

    func note(_ level: ConsoleLine.Level, _ text: String) {
        Task { @MainActor in log.append(.app, level, "media: \(text)") }
    }
}
