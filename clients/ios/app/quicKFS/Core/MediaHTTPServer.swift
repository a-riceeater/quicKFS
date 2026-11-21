// SPDX-License-Identifier: Apache-2.0
import Foundation
import Network

/// Serves one `RemoteStream` to AVPlayer over HTTP on the loopback interface.
///
/// AVPlayer's custom-URL-scheme loader proved unreliable for long videos: it
/// would ask for the head of a segment, never request the rest, and wait for
/// it forever, until the person seeked. Its HTTP stack has no such gap. It
/// schedules its own range requests, and closing a connection is how it
/// cancels one, which stops the reads here. So the player is handed
/// `http://127.0.0.1:<port>/<secret>/<name>`, and each request is answered
/// from the stream (and its read-ahead and cache).
///
/// Only loopback connections are accepted, and only for the random secret
/// path, so other apps on the device cannot read the file. One listener per
/// open preview; it stops when the preview closes.
final class MediaHTTPServer: @unchecked Sendable {
    private let stream: RemoteStream
    private let name: String
    private let contentType: String
    private let report: MediaReport
    private let secret = UUID().uuidString
    private let queue = DispatchQueue(label: "org.quickfs.media.http", qos: .userInitiated)
    private let reading = DispatchQueue(
        label: "org.quickfs.media.http.read", qos: .userInitiated, attributes: .concurrent)
    private var listener: NWListener?
    private var connections: [ObjectIdentifier: NWConnection] = [:]
    private var stopped = false
    private var stats = Stats()

    /// Bytes per body write; the next is read only once this one is sent.
    private static let chunk = 1 << 20

    struct Stats {
        var requests = 0
        var served: UInt64 = 0
        var waits = 0
        var longestWait = Duration.zero
        let opened = ContinuousClock.now
    }

    init(stream: RemoteStream, name: String, contentType: String, report: MediaReport) {
        self.stream = stream
        self.name = name
        self.contentType = contentType
        self.report = report
    }

    /// Start listening and return the URL to give the player.
    func start() async throws -> URL {
        let parameters = NWParameters.tcp
        // Bound to 127.0.0.1 itself, so nothing off the device can connect.
        // (Filtering by interface type instead rejected the player's own
        // loopback connections inside the app sandbox.)
        parameters.requiredLocalEndpoint = .hostPort(host: .ipv4(.loopback), port: .any)
        parameters.allowLocalEndpointReuse = true
        let listener = try NWListener(using: parameters)
        listener.newConnectionHandler = { [weak self] connection in self?.accept(connection) }
        let port: UInt16 = try await withCheckedThrowingContinuation { continuation in
            let once = Once()
            listener.stateUpdateHandler = { state in
                switch state {
                case .ready:
                    once.run { continuation.resume(returning: listener.port?.rawValue ?? 0) }
                case .failed(let error):
                    once.run { continuation.resume(throwing: error) }
                case .cancelled:
                    once.run { continuation.resume(throwing: CancellationError()) }
                default: break
                }
            }
            queue.sync { self.listener = listener }
            listener.start(queue: queue)
        }
        var components = URLComponents()
        components.scheme = "http"
        components.host = "127.0.0.1"
        components.port = Int(port)
        // Keep the name: AVFoundation reads the extension as a hint.
        components.path = "/\(secret)/\(name)"
        guard let url = components.url else { throw URLError(.badURL) }
        return url
    }

    func stop() {
        queue.async {
            self.stopped = true
            self.listener?.cancel()
            self.listener = nil
            for connection in self.connections.values { connection.cancel() }
            self.connections.removeAll()
        }
    }

    /// What the player asked for, for the console when it closes.
    var summary: String {
        queue.sync {
            let seconds = Int((ContinuousClock.now - stats.opened).components.seconds)
            return "closed after \(seconds) s: \(stats.requests) requests, \(Format.bytes(stats.served)) served, "
                + "\(stats.waits) reads waited over 100 ms, longest \(Self.milliseconds(stats.longestWait)) ms"
        }
    }

    private static func milliseconds(_ duration: Duration) -> Int {
        Int(duration.components.seconds * 1000 + duration.components.attoseconds / 1_000_000_000_000_000)
    }

    // MARK: Connections

    private func accept(_ connection: NWConnection) {
        guard !stopped else { return connection.cancel() }
        let id = ObjectIdentifier(connection)
        connections[id] = connection
        connection.stateUpdateHandler = { [weak self] state in
            switch state {
            case .failed, .cancelled: self?.connections[id] = nil
            default: break
            }
        }
        connection.start(queue: queue)
        receiveRequest(on: connection, buffered: Data())
    }

    /// Read one request head, answer it, then wait for the next on the same
    /// connection (HTTP/1.1 keep-alive).
    private func receiveRequest(on connection: NWConnection, buffered: Data) {
        if let end = buffered.range(of: Data("\r\n\r\n".utf8)) {
            let head = String(decoding: buffered[..<end.lowerBound], as: UTF8.self)
            let rest = Data(buffered[end.upperBound...])
            respond(to: head, on: connection) { [weak self] keepAlive in
                guard keepAlive else { return connection.cancel() }
                self?.receiveRequest(on: connection, buffered: rest)
            }
            return
        }
        guard buffered.count < 64 * 1024 else { return connection.cancel() }
        connection.receive(minimumIncompleteLength: 1, maximumLength: 16 * 1024) { [weak self] data, _, complete, error in
            guard let self, error == nil, let data, !data.isEmpty else { return connection.cancel() }
            var next = buffered
            next.append(data)
            if complete && next.range(of: Data("\r\n\r\n".utf8)) == nil { return connection.cancel() }
            self.receiveRequest(on: connection, buffered: next)
        }
    }

    private func respond(to head: String, on connection: NWConnection, then: @escaping @Sendable (Bool) -> Void) {
        let lines = head.components(separatedBy: "\r\n")
        let request = lines.first?.split(separator: " ") ?? []
        guard request.count >= 2, request[0] == "GET" || request[0] == "HEAD" else {
            return sendStatus(405, "Method Not Allowed", on: connection, then: then)
        }
        let path = String(request[1]).removingPercentEncoding ?? ""
        guard path.hasPrefix("/\(secret)/") else {
            return sendStatus(404, "Not Found", on: connection, then: then)
        }
        var headers: [String: String] = [:]
        for line in lines.dropFirst() {
            guard let colon = line.firstIndex(of: ":") else { continue }
            headers[line[..<colon].lowercased()] = line[line.index(after: colon)...]
                .trimmingCharacters(in: .whitespaces)
        }
        let size = stream.size
        var start: UInt64 = 0
        var end = size == 0 ? 0 : size - 1
        var partial = false
        if let range = headers["range"] {
            guard let parsed = Self.byteRange(range, size: size) else {
                let head = "HTTP/1.1 416 Range Not Satisfiable\r\nContent-Range: bytes */\(size)\r\n"
                    + "Content-Length: 0\r\n\r\n"
                return connection.send(content: Data(head.utf8), completion: .contentProcessed { error in
                    then(error == nil)
                })
            }
            (start, end) = parsed
            partial = true
        }
        let length = size == 0 ? 0 : end - start + 1
        stats.requests += 1
        mediaLog.info("http \(request[0], privacy: .public) range \(start, privacy: .public)..<\(start + length, privacy: .public)")
        var response = partial ? "HTTP/1.1 206 Partial Content\r\n" : "HTTP/1.1 200 OK\r\n"
        response += "Content-Type: \(contentType)\r\n"
        response += "Accept-Ranges: bytes\r\n"
        response += "Content-Length: \(length)\r\n"
        if partial { response += "Content-Range: bytes \(start)-\(end)/\(size)\r\n" }
        response += "Cache-Control: no-store\r\n\r\n"
        let body = request[0] == "GET" && length > 0
        let (first, last) = (start, start + length)
        connection.send(content: Data(response.utf8), completion: .contentProcessed { [weak self] error in
            guard let self, error == nil else { return then(false) }
            guard body else { return then(true) }
            self.sendBody(from: first, to: last, on: connection, then: then)
        })
    }

    /// Stream `start..<end`, reading the next chunk only after the previous
    /// one was handed to the connection, so a closed connection stops reads.
    private func sendBody(from start: UInt64, to end: UInt64, on connection: NWConnection,
                          then: @escaping @Sendable (Bool) -> Void) {
        guard !stopped else { return then(false) }
        guard start < end else { return then(true) }
        let count = Int(min(UInt64(Self.chunk), end - start))
        reading.async { [weak self] in
            guard let self else { return }
            let began = ContinuousClock.now
            let result = Result { try self.stream.read(at: start, count: count) }
            let took = ContinuousClock.now - began
            self.queue.async {
                guard case .success(let data) = result, !data.isEmpty else {
                    if case .failure(let error) = result {
                        self.report.note(.error, "read at \(start >> 20) MiB failed: \(error.localizedDescription)")
                    }
                    return then(false)
                }
                if took > .milliseconds(100) {
                    self.stats.waits += 1
                    self.stats.longestWait = max(self.stats.longestWait, took)
                    self.report.note(.notice, "waited \(Self.milliseconds(took)) ms for \(start >> 20) MiB")
                }
                connection.send(content: data, completion: .contentProcessed { error in
                    guard error == nil else { return then(false) }
                    self.stats.served += UInt64(data.count)
                    self.sendBody(from: start + UInt64(data.count), to: end, on: connection, then: then)
                })
            }
        }
    }

    private func sendStatus(_ code: Int, _ reason: String, on connection: NWConnection,
                            then: @escaping @Sendable (Bool) -> Void) {
        let head = "HTTP/1.1 \(code) \(reason)\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        connection.send(content: Data(head.utf8), completion: .contentProcessed { _ in then(false) })
    }

    /// `bytes=a-b`, `bytes=a-`, or `bytes=-n` (the last n bytes), clamped to
    /// the file. Nil when unsatisfiable or not a single byte range.
    static func byteRange(_ header: String, size: UInt64) -> (UInt64, UInt64)? {
        guard size > 0, header.hasPrefix("bytes="), !header.contains(",") else { return nil }
        let spec = header.dropFirst("bytes=".count)
        let parts = spec.split(separator: "-", maxSplits: 1, omittingEmptySubsequences: false)
        guard parts.count == 2 else { return nil }
        if parts[0].isEmpty {
            guard let suffix = UInt64(parts[1]), suffix > 0 else { return nil }
            return (size - min(suffix, size), size - 1)
        }
        guard let first = UInt64(parts[0]), first < size else { return nil }
        if parts[1].isEmpty { return (first, size - 1) }
        guard let last = UInt64(parts[1]), last >= first else { return nil }
        return (first, min(last, size - 1))
    }
}

/// Runs a closure at most once, from any thread.
private final class Once: @unchecked Sendable {
    private let lock = NSLock()
    private var done = false

    func run(_ body: () -> Void) {
        let first = lock.withLock { () -> Bool in
            defer { done = true }
            return !done
        }
        if first { body() }
    }
}
