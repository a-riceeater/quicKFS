// SPDX-License-Identifier: Apache-2.0
import Foundation
import Observation

/// Fetches one remote file for Quick Look.
///
/// The whole file comes down through the core's download engine (pipelined,
/// retried, striped like any download) into a private temporary folder that
/// is removed when the preview closes, so the preview always sees the real
/// file rather than a truncated head. The first bytes are read separately
/// and at once for the hex view. Preview downloads never appear in the
/// transfer list.
@MainActor
@Observable
final class PreviewLoader {
    enum Phase: Equatable {
        case loading
        /// Bigger than `automaticLimit`; waits for the person to ask.
        case confirm
        case ready(URL)
        case failed(String)
    }

    /// Files up to this size load as soon as the preview opens.
    static let automaticLimit: UInt64 = 256 << 20
    /// Bytes shown in the hex view.
    static let headLength: UInt64 = 16 << 10
    /// Free space to leave on the startup disk after a preview download.
    private static let spareSpace: UInt64 = 1 << 30
    private static let root = FileManager.default.temporaryDirectory
        .appendingPathComponent("quicKFS-preview", isDirectory: true)

    let node: RemoteNode
    private(set) var phase: Phase = .loading
    private(set) var status: TransferStatus?
    /// Recent throughput in bytes/s, from the last couple of seconds.
    private(set) var rate: Double = 0
    /// The first `headLength` bytes as a hex dump, formatted once.
    private(set) var hexDump: String?
    private(set) var headFailed = false
    private(set) var started: Date?
    private(set) var elapsed: TimeInterval?

    @ObservationIgnored private let connection: ServerConnection
    @ObservationIgnored private var transfer: UInt64?
    @ObservationIgnored private var folder: URL?
    @ObservationIgnored private var history: [(Date, UInt64)] = []
    @ObservationIgnored private var closed = false

    init(node: RemoteNode, connection: ServerConnection) {
        self.node = node
        self.connection = connection
    }

    var fraction: Double? {
        guard let status, status.total > 0 else { return nil }
        return status.fraction
    }

    /// Seconds left at the recent rate, once there is one.
    var remaining: TimeInterval? {
        guard let status, rate > 0, status.total > status.bytes else { return nil }
        return Double(status.total - status.bytes) / rate
    }

    /// Start loading: the hex head always, the whole file if it is small
    /// enough or once confirmed.
    func open() async {
        Self.sweepStale()
        async let head: Void = loadHead()
        if node.size <= Self.automaticLimit {
            await download()
        } else {
            phase = .confirm
        }
        await head
    }

    /// Load a file above `automaticLimit` after the person asks.
    func loadAnyway() async {
        guard phase == .confirm || isRetryable else { return }
        await download()
    }

    var isRetryable: Bool {
        if case .failed = phase { return true }
        return false
    }

    /// Free space check for `loadAnyway`, so a big preview cannot fill the disk.
    var fitsOnDisk: Bool {
        let values = try? FileManager.default.temporaryDirectory
            .resourceValues(forKeys: [.volumeAvailableCapacityForImportantUsageKey])
        guard let free = values?.volumeAvailableCapacityForImportantUsage, free > 0 else { return true }
        return UInt64(free) > node.size &+ Self.spareSpace
    }

    /// Stop any download and delete the local copy.
    func close() {
        closed = true
        let transfer = transfer
        let folder = folder
        self.transfer = nil
        self.folder = nil
        Task.detached { await Self.retire(transfer, folder: folder) }
    }

    /// Cancel a preview download and clean up after it. Cancelling only asks
    /// the transfer to stop, and the core keeps a running transfer's record,
    /// so wait (bounded) for it to stop before forgetting it and deleting
    /// the folder it was writing into.
    private nonisolated static func retire(_ transfer: UInt64?, folder: URL?) async {
        if let transfer {
            _ = try? await Core.call("transfer_cancel", ["transfer": .uint(transfer)], as: EmptyReply.self)
            for _ in 0..<100 {
                guard let status: TransferStatus = try? await Core.call(
                    "transfer_status", ["transfer": .uint(transfer)]),
                    status.state == .running
                else { break }
                try? await Task.sleep(for: .milliseconds(100))
            }
            _ = try? await Core.call("transfer_forget", ["transfer": .uint(transfer)], as: EmptyReply.self)
        }
        if let folder { try? FileManager.default.removeItem(at: folder) }
    }

    private func loadHead() async {
        struct Reply: Decodable, Sendable { let data: String }
        do {
            let reply: Reply = try await connection.request(
                "read", ["node": .uuid(node.node), "offset": 0, "length": .uint(Self.headLength)])
            hexDump = Self.hexDump(Data(hex: reply.data))
        } catch {
            headFailed = true
        }
    }

    nonisolated static func hexDump(_ data: Data) -> String {
        var lines: [String] = []
        let bytes = [UInt8](data)
        lines.reserveCapacity(bytes.count / 16 + 1)
        for start in stride(from: 0, to: bytes.count, by: 16) {
            let chunk = bytes[start..<min(start + 16, bytes.count)]
            let hex = chunk.map { String(format: "%02x", $0) }.joined(separator: " ")
            let ascii = String(chunk.map { (0x20..<0x7f).contains($0) ? Character(UnicodeScalar($0)) : "." })
            lines.append(String(format: "%08x  ", start) + hex.padding(toLength: 48, withPad: " ", startingAt: 0) + " " + ascii)
        }
        return lines.joined(separator: "\n")
    }

    private func download() async {
        removeFolder()
        phase = .loading
        status = nil
        rate = 0
        history.removeAll()
        elapsed = nil
        let folder = Self.root.appendingPathComponent(UUID().uuidString, isDirectory: true)
        do {
            try FileManager.default.createDirectory(
                at: folder, withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700])
        } catch {
            phase = .failed(error.localizedDescription)
            return
        }
        self.folder = folder
        // Keep the name, and so the extension Quick Look picks a viewer by.
        let file = folder.appendingPathComponent(Paths.safeFolderName(node.displayName))
        let begun = Date.now
        started = begun
        struct Reply: Decodable, Sendable { let transfer: UInt64 }
        let reply: Reply
        do {
            reply = try await connection.request(
                "download", ["node": .uuid(node.node), "destination": .string(file.path)])
        } catch let failure as CoreError {
            phase = .failed(failure.message)
            return
        } catch {
            phase = .failed(error.localizedDescription)
            return
        }
        guard !closed else {
            // Closed while the download was starting: `close` could not see
            // this transfer or its folder yet.
            let folder = self.folder
            self.folder = nil
            Task.detached { await Self.retire(reply.transfer, folder: folder) }
            return
        }
        transfer = reply.transfer
        await poll(reply.transfer, file: file, begun: begun)
    }

    private func poll(_ id: UInt64, file: URL, begun: Date) async {
        while !closed, transfer == id {
            guard let next: TransferStatus = try? await Core.call(
                "transfer_status", ["transfer": .uint(id)])
            else {
                phase = .failed("Lost track of the download.")
                return
            }
            guard !closed, transfer == id else { return }
            status = next
            measure(next.bytes)
            switch next.state {
            case .running:
                try? await Task.sleep(for: .milliseconds(150))
            case .finished:
                elapsed = Date.now.timeIntervalSince(begun)
                transfer = nil
                _ = try? await Core.call("transfer_forget", ["transfer": .uint(id)], as: EmptyReply.self)
                phase = .ready(file)
                return
            case .failed, .cancelled:
                transfer = nil
                _ = try? await Core.call("transfer_forget", ["transfer": .uint(id)], as: EmptyReply.self)
                phase = .failed(next.reason ?? next.error?.message ?? "The file could not be read.")
                removeFolder()
                return
            }
        }
    }

    private func measure(_ bytes: UInt64) {
        let now = Date.now
        history.append((now, bytes))
        history.removeAll { now.timeIntervalSince($0.0) > 2 }
        guard let first = history.first, let last = history.last, last.0 > first.0 else { return }
        rate = Double(last.1 &- first.1) / last.0.timeIntervalSince(first.0)
    }

    private func removeFolder() {
        if let folder { try? FileManager.default.removeItem(at: folder) }
        folder = nil
    }

    /// Previews left behind by a crash or forced quit: anything over a day
    /// old cannot belong to an open preview.
    private static func sweepStale() {
        let manager = FileManager.default
        guard let entries = try? manager.contentsOfDirectory(
            at: root, includingPropertiesForKeys: [.contentModificationDateKey])
        else { return }
        for entry in entries {
            let modified = (try? entry.resourceValues(forKeys: [.contentModificationDateKey]))?
                .contentModificationDate ?? .distantPast
            if Date.now.timeIntervalSince(modified) > 86_400 { try? manager.removeItem(at: entry) }
        }
    }
}
