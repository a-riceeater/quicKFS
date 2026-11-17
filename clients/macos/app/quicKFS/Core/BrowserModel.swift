// SPDX-License-Identifier: Apache-2.0
import Foundation
import Observation

/// One point on a transfer's speed chart.
struct TransferRateSample: Identifiable, Sendable {
    let id: Int
    let elapsed: Double
    let rate: Double
}

/// A long-running upload or download tracked by polling the core, with the
/// link and protocol counters sampled while it runs.
@MainActor
@Observable
final class TransferItem: Identifiable {
    enum Direction { case upload, download, write }

    let id: UInt64
    let title: String
    let direction: Direction
    let destination: URL?
    private(set) var status: TransferStatus?
    private(set) var startedAt = Date.now
    private(set) var finishedAt: Date?
    private(set) var rates: [TransferRateSample] = []
    private(set) var peakRate: Double = 0
    /// Link counters when the transfer began and most recently.
    private(set) var linkStart: LinkSnapshot?
    private(set) var link: LinkSnapshot?
    private var windowStart: (Date, UInt64)?

    init(id: UInt64, title: String, direction: Direction, destination: URL?) {
        self.id = id
        self.title = title
        self.direction = direction
        self.destination = destination
    }

    var isRunning: Bool { status == nil || status?.state == .running }

    var elapsed: TimeInterval { (finishedAt ?? .now).timeIntervalSince(startedAt) }

    /// Average bytes per second over the whole transfer.
    var rate: Double {
        guard let status else { return 0 }
        return elapsed > 0.5 ? Double(status.bytes) / elapsed : 0
    }

    /// The most recent one-second rate, or the average before there is one.
    var currentRate: Double { isRunning ? (rates.last?.rate ?? rate) : 0 }

    var remaining: TimeInterval? {
        guard let status, isRunning, status.total > status.bytes else { return nil }
        let recent = rates.suffix(5).map(\.rate)
        let speed = recent.isEmpty ? rate : recent.reduce(0, +) / Double(recent.count)
        return speed > 0 ? Double(status.total - status.bytes) / speed : nil
    }

    func update(_ status: TransferStatus) {
        self.status = status
        let now = Date.now
        if let (since, bytes) = windowStart {
            let span = now.timeIntervalSince(since)
            if span >= 1 {
                let rate = Double(status.bytes &- bytes) / span
                rates.append(TransferRateSample(id: rates.count, elapsed: now.timeIntervalSince(startedAt), rate: rate))
                peakRate = max(peakRate, rate)
                windowStart = (now, status.bytes)
            }
        } else {
            windowStart = (now, status.bytes)
        }
        if status.state != .running, finishedAt == nil { finishedAt = now }
    }

    func updateLink(_ snapshot: LinkSnapshot) {
        if linkStart == nil || (link.map { snapshot.reconnects != $0.reconnects } ?? false) {
            linkStart = snapshot
        }
        link = snapshot
    }

    /// Counter change since the transfer began, robust to a reconnect.
    func delta(_ value: (LinkSnapshot) -> UInt64) -> UInt64 {
        guard let link, let linkStart else { return 0 }
        let now = value(link), then = value(linkStart)
        return now >= then ? now - then : now
    }

    func protocolDelta(_ value: (ProtocolSnapshot) -> UInt64) -> UInt64 {
        guard let now = link?.protocol, let then = linkStart?.protocol else { return 0 }
        let a = value(now), b = value(then)
        return a >= b ? a - b : a
    }

    func cancel() {
        Task { _ = try? await Core.call("transfer_cancel", ["transfer": .uint(id)], as: EmptyReply.self) }
    }

    /// A plain-text report for pasting into an issue.
    var summary: String {
        var lines = ["quicKFS \(direction == .download ? "download" : direction == .upload ? "upload" : "write"): \(title)"]
        if let status {
            lines.append("state \(status.state.rawValue), \(Format.bytes(status.bytes)) of \(Format.bytes(status.total)), \(status.files)/\(status.filesTotal) files")
        }
        lines.append(String(format: "elapsed %.1f s, average %@/s, peak %@/s", elapsed, Format.bytes(UInt64(rate)), Format.bytes(UInt64(peakRate))))
        if let link {
            lines.append(String(format: "rtt %.2f ms, mtu %d, cwnd %@", Double(link.rttUs) / 1000, Int(link.mtu), Format.bytes(link.congestionWindow)))
            lines.append("packets \(delta(\.packetsSent)) sent, \(delta(\.packetsLost)) lost, \(delta(\.congestionEvents)) congestion events")
            if let proto = link.protocol {
                lines.append("protocol \(proto.alpn) client \(ProtocolSnapshot.version(proto.clientVersion)) server \(ProtocolSnapshot.version(proto.serverVersion)) compression \(proto.compression ? "on" : "off")")
                lines.append("streams \(protocolDelta(\.streams)), frames \(protocolDelta(\.framesSent))/\(protocolDelta(\.framesReceived)), data \(Format.bytes(protocolDelta(\.dataBytesReceived))) in / \(Format.bytes(protocolDelta(\.dataBytesSent))) out")
            }
        }
        return lines.joined(separator: "\n")
    }
}

@MainActor
@Observable
final class BrowserModel {
    struct Crumb: Hashable {
        let node: UUID
        let name: String
    }

    let connection: ServerConnection
    private let log: ConsoleLog
    private(set) var path: [Crumb] = [Crumb(node: RemoteNode.rootID, name: "/")]
    private(set) var forwardStack: [[Crumb]] = []
    private(set) var backStack: [[Crumb]] = []
    private(set) var entries: [RemoteNode] = []
    private(set) var loading = false
    private(set) var error: CoreError?
    private(set) var transfers: [TransferItem] = []
    var selection: Set<UUID> = []
    var sortOrder = [KeyPathComparator(\RemoteNode.displayName, comparator: .localizedStandard)]
    var showHidden = false

    init(connection: ServerConnection, log: ConsoleLog) {
        self.connection = connection
        self.log = log
    }

    var directory: UUID { path.last?.node ?? RemoteNode.rootID }
    var writable: Bool { connection.capabilities?.writable ?? false }
    var selected: [RemoteNode] { entries.filter { selection.contains($0.id) } }

    var visibleEntries: [RemoteNode] {
        let filtered = showHidden ? entries : entries.filter { !$0.displayName.hasPrefix(".") }
        let sorted = filtered.sorted(using: sortOrder)
        // Folders first, as Finder does when sorting by name.
        return sorted.filter(\.isDirectory) + sorted.filter { !$0.isDirectory }
    }

    var pathString: String {
        "/" + path.dropFirst().map(\.name).joined(separator: "/")
    }

    // MARK: Navigation

    func reload() async {
        loading = true
        defer { loading = false }
        do {
            let listing: DirectoryListing = try await connection.request(
                "list", ["node": .uuid(directory)])
            entries = listing.entries
            error = nil
            selection = selection.filter { id in entries.contains { $0.id == id } }
        } catch let failure as CoreError {
            error = failure
            entries = []
        } catch {
            self.error = CoreError(code: "client", message: error.localizedDescription)
        }
    }

    func open(_ node: RemoteNode) async {
        guard node.isDirectory else { return }
        go(to: path + [Crumb(node: node.node, name: node.displayName)])
        await reload()
    }

    func jump(to crumb: Crumb) async {
        guard let index = path.firstIndex(of: crumb), index < path.count - 1 else { return }
        go(to: Array(path[...index]))
        await reload()
    }

    func goUp() async {
        guard path.count > 1 else { return }
        go(to: Array(path.dropLast()))
        await reload()
    }

    func goBack() async {
        guard let previous = backStack.popLast() else { return }
        forwardStack.append(path)
        path = previous
        selection = []
        await reload()
    }

    func goForward() async {
        guard let next = forwardStack.popLast() else { return }
        backStack.append(path)
        path = next
        selection = []
        await reload()
    }

    /// Navigate to a typed absolute path, the equivalent of the CLI's paths.
    func goTo(path text: String) async {
        let components = text.split(separator: "/").map(String.init)
        var crumbs = [Crumb(node: RemoteNode.rootID, name: "/")]
        var node = RemoteNode.rootID
        do {
            for name in components {
                let listing: DirectoryListing = try await connection.request("list", ["node": .uuid(node)])
                guard let next = listing.entries.first(where: { $0.displayName == name && $0.isDirectory })
                else {
                    throw CoreError(code: "NotFound", message: "No folder named \(name).")
                }
                node = next.node
                crumbs.append(Crumb(node: node, name: name))
            }
            go(to: crumbs)
            await reload()
        } catch let failure as CoreError {
            error = failure
        } catch {}
    }

    private func go(to crumbs: [Crumb]) {
        backStack.append(path)
        forwardStack.removeAll()
        path = crumbs
        selection = []
    }

    // MARK: Mutations

    /// Run one mutation, then refresh the listing. Returns the failure, if any.
    @discardableResult
    func mutate(_ op: String, _ fields: [String: JSONValue]) async -> CoreError? {
        do {
            _ = try await connection.request(op, fields, as: EmptyReply.self)
            await reload()
            return nil
        } catch let failure as CoreError {
            error = failure
            await reload()
            return failure
        } catch {
            return CoreError(code: "client", message: error.localizedDescription)
        }
    }

    func createFolder(named name: String) async -> CoreError? {
        await mutate("create_directory", ["parent": .uuid(directory), "name": .string(name)])
    }

    func createFile(named name: String) async -> CoreError? {
        await mutate("create_file", ["parent": .uuid(directory), "name": .string(name)])
    }

    func rename(_ node: RemoteNode, to name: String, replace: Bool = false) async -> CoreError? {
        var fields = node.nameFields
        fields["parent"] = .uuid(directory)
        fields["new_parent"] = .uuid(directory)
        fields["new_name"] = .string(name)
        fields["no_replace"] = .bool(!replace)
        return await mutate("rename", fields)
    }

    /// Move entries into a sibling folder.
    func move(_ nodes: [RemoteNode], into folder: RemoteNode) async {
        for node in nodes where node.id != folder.id {
            var fields = node.nameFields
            fields["parent"] = .uuid(directory)
            fields["new_parent"] = .uuid(folder.node)
            fields["new_name"] = .string(node.displayName)
            fields["no_replace"] = true
            _ = try? await connection.request("rename", fields, as: EmptyReply.self)
        }
        await reload()
    }

    func delete(_ nodes: [RemoteNode]) async {
        for node in nodes {
            var fields = node.nameFields
            fields["parent"] = .uuid(directory)
            fields["directory"] = .bool(node.isDirectory)
            fields["recursive"] = .bool(node.isDirectory)
            do {
                _ = try await connection.request("remove", fields, as: EmptyReply.self)
            } catch let failure as CoreError {
                error = failure
                break
            } catch { break }
        }
        selection.removeAll()
        await reload()
    }

    func truncate(_ node: RemoteNode, to size: UInt64) async -> CoreError? {
        await mutate("truncate", ["node": .uuid(node.node), "size": .uint(size)])
    }

    func setMode(_ node: RemoteNode, _ mode: UInt32) async -> CoreError? {
        await mutate("set_mode", ["node": .uuid(node.node), "mode": .uint(UInt64(mode))])
    }

    func clearError() { error = nil }

    // MARK: Transfers

    func upload(_ urls: [URL], replace: Bool = false) {
        for url in urls {
            start("upload", [
                "parent": .uuid(directory), "source": .string(url.path), "replace": .bool(replace),
            ], title: url.lastPathComponent, direction: .upload, destination: nil)
        }
    }

    func download(_ nodes: [RemoteNode], into folder: URL) {
        for node in nodes {
            let destination = Self.uniqueURL(in: folder, name: node.displayName)
            start("download", ["node": .uuid(node.node), "destination": .string(destination.path)],
                  title: node.displayName, direction: .download, destination: destination)
        }
    }

    /// The CLI `write` command: overwrite part of a remote file from a local file.
    func write(_ source: URL, into node: RemoteNode, offset: UInt64, sync: Bool) {
        start("write_at", [
            "node": .uuid(node.node), "source": .string(source.path),
            "offset": .uint(offset), "sync": .bool(sync),
        ], title: node.displayName, direction: .write, destination: nil)
    }

    func dismissFinishedTransfers() {
        for item in transfers where !item.isRunning {
            Task { _ = try? await Core.call("transfer_forget", ["transfer": .uint(item.id)], as: EmptyReply.self) }
        }
        transfers.removeAll { !$0.isRunning }
    }

    private func start(
        _ op: String, _ fields: [String: JSONValue], title: String,
        direction: TransferItem.Direction, destination: URL?
    ) {
        Task {
            struct Reply: Decodable, Sendable { let transfer: UInt64 }
            do {
                let reply: Reply = try await connection.request(op, fields)
                let item = TransferItem(id: reply.transfer, title: title, direction: direction, destination: destination)
                transfers.append(item)
                log.append(.app, .info, "\(op) started: \(title)")
                await poll(item)
            } catch let failure as CoreError {
                error = failure
            } catch {}
        }
    }

    private func sampleLink(into item: TransferItem) async {
        struct Reply: Decodable, Sendable { let link: LinkSnapshot? }
        if let reply: Reply = try? await connection.request("link_stats"), let link = reply.link {
            item.updateLink(link)
        }
    }

    private func poll(_ item: TransferItem) async {
        await sampleLink(into: item)
        var tick = 0
        while true {
            tick += 1
            if tick % 5 == 0 { await sampleLink(into: item) }
            guard let status: TransferStatus = try? await Core.call(
                "transfer_status", ["transfer": .uint(item.id)])
            else { return }
            item.update(status)
            if status.state != .running {
                await sampleLink(into: item)
                let seconds = Date.now.timeIntervalSince(item.startedAt)
                switch status.state {
                case .finished:
                    let rate = seconds > 0 ? Double(status.bytes) / seconds : 0
                    log.append(.app, .notice, String(format: "%@ finished: %@ · %@ in %.1f s · %@/s",
                        item.direction == .download ? "download" : "upload", item.title,
                        Format.bytes(status.bytes), seconds, Format.bytes(UInt64(rate))))
                case .failed:
                    log.append(.app, .error, "\(item.title) failed: \(status.error?.message ?? "unknown error")")
                case .cancelled:
                    log.append(.app, .warning, "\(item.title) cancelled")
                case .running: break
                }
                if status.state == .failed, let failure = status.error { error = failure }
                if item.direction != .download { await reload() }
                return
            }
            try? await Task.sleep(for: .milliseconds(200))
        }
    }

    static func uniqueURL(in folder: URL, name: String) -> URL {
        let manager = FileManager.default
        var candidate = folder.appendingPathComponent(name)
        let base = (name as NSString).deletingPathExtension
        let ext = (name as NSString).pathExtension
        var counter = 2
        while manager.fileExists(atPath: candidate.path) {
            let numbered = ext.isEmpty ? "\(base) \(counter)" : "\(base) \(counter).\(ext)"
            candidate = folder.appendingPathComponent(numbered)
            counter += 1
        }
        return candidate
    }
}
