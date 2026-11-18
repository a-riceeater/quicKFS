// SPDX-License-Identifier: Apache-2.0
import Foundation
import Observation

/// An authenticated browsing session with one server, independent of any
/// mount. Requests transparently reconnect once when the session was lost.
@MainActor
@Observable
final class ServerConnection {
    enum Phase: Equatable {
        case offline
        case connecting
        case online
        case failed(CoreError)
    }

    private(set) var phase: Phase = .offline
    private(set) var capabilities: Capabilities?
    private(set) var stats: FilesystemStats?
    /// Recent round-trip samples in milliseconds, oldest first.
    private(set) var latency: [Double] = []

    private var session: UInt64?
    private var connectTask: Task<UInt64, Error>?
    private var sampler: Task<Void, Never>?
    private let profile: @MainActor () -> ServerProfile
    private let log: ConsoleLog

    private static let latencySamples = 48

    init(profile: @escaping @MainActor () -> ServerProfile, log: ConsoleLog) {
        self.profile = profile
        self.log = log
    }

    var isOnline: Bool { phase == .online }

    /// Connect (or join an in-flight connect) and return the session id.
    @discardableResult
    func connect() async throws -> UInt64 {
        if let session { return session }
        if let connectTask { return try await connectTask.value }
        let profile = profile()
        phase = .connecting
        log.append(.app, .info, "connecting to \(profile.serverName) at \(profile.address) as \(profile.username)")
        let started = Date.now
        let task = Task<UInt64, Error> {
            guard let password = await Keychain.load(for: profile.id) else {
                throw CoreError(code: "Unauthenticated", message: "Enter the password for \(profile.username).")
            }
            struct Reply: Decodable, Sendable { let session: UInt64; let capabilities: Capabilities }
            let reply: Reply = try await Core.call("connect", [
                "state_dir": .string(Paths.state.path),
                "server": profile.endpoint,
                "trust": profile.trust.request,
                "username": .string(profile.username),
                "password": .string(password),
                "timeout_ms": profile.transfer.timeoutMs,
                "transfer": profile.transfer.request,
            ])
            await MainActor.run { self.capabilities = reply.capabilities }
            return reply.session
        }
        connectTask = task
        defer { connectTask = nil }
        do {
            let id = try await task.value
            session = id
            phase = .online
            let elapsed = Date.now.timeIntervalSince(started) * 1000
            log.append(.app, .notice, String(format: "session %llu authenticated in %.0f ms", id, elapsed))
            if let capabilities {
                log.append(.link, .info,
                    "epoch \(capabilities.serverEpoch.uuidString.lowercased().prefix(8)) · \(capabilities.writable ? "read-write" : "read-only") · reads ≤ \(Format.bytes(capabilities.maxReadSize)) · writes ≤ \(Format.bytes(capabilities.maxWriteSize))")
            }
            startSampling()
            return id
        } catch let error as CoreError {
            phase = .failed(error)
            log.append(.app, .error, "connect failed [\(error.code)]: \(error.message)")
            throw error
        } catch {
            let wrapped = CoreError(code: "client", message: error.localizedDescription)
            phase = .failed(wrapped)
            throw wrapped
        }
    }

    func disconnect() {
        sampler?.cancel()
        sampler = nil
        if let session {
            Task { _ = try? await Core.call("disconnect", ["session": .uint(session)], as: EmptyReply.self) }
        }
        session = nil
        phase = .offline
        latency.removeAll()
    }

    /// Run a session-scoped request, reconnecting once if the session died.
    func request<Reply: Decodable & Sendable>(
        _ op: String, _ parameters: [String: JSONValue] = [:], as type: Reply.Type = Reply.self
    ) async throws -> Reply {
        var attempt = 0
        while true {
            let id = try await connect()
            var fields = parameters
            fields["session"] = .uint(id)
            do {
                return try await Core.call(op, fields, as: type)
            } catch let error as CoreError where error.isConnectionLoss && attempt == 0 {
                attempt += 1
                log.append(.link, .warning, "session \(id) lost [\(error.code)]; reconnecting")
                if session == id { session = nil }
                sampler?.cancel()
                // Release the dead session's resources in the core.
                Task { _ = try? await Core.call("disconnect", ["session": .uint(id)], as: EmptyReply.self) }
            }
        }
    }

    func refreshStats() async {
        stats = try? await request("filesystem_stats", as: FilesystemStats.self)
    }

    func refreshCapabilities() async {
        if let fresh = try? await request("capabilities", as: Capabilities.self) { capabilities = fresh }
    }

    @discardableResult
    func ping() async -> Double? {
        struct Reply: Decodable, Sendable { let rttMs: Double }
        guard let reply = try? await request("ping", as: Reply.self) else { return nil }
        latency.append(reply.rttMs)
        if latency.count > Self.latencySamples { latency.removeFirst(latency.count - Self.latencySamples) }
        return reply.rttMs
    }

    private func startSampling() {
        sampler?.cancel()
        sampler = Task { [weak self] in
            await self?.refreshStats()
            while !Task.isCancelled {
                guard let self, self.session != nil else { return }
                await self.ping()
                try? await Task.sleep(for: .seconds(5))
            }
        }
    }
}
