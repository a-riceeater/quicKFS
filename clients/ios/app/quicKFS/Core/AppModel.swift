// SPDX-License-Identifier: Apache-2.0
import Foundation
import Observation

/// Live state for one saved server: its browsing session, the browser kept
/// across navigation, and link telemetry. iOS has no mount.
@MainActor
@Observable
final class ServerState: Identifiable {
    let id: UUID
    let log = ConsoleLog()
    let connection: ServerConnection
    let browser: BrowserModel
    let telemetry: Telemetry

    init(id: UUID, profile: @escaping @MainActor () -> ServerProfile) {
        self.id = id
        connection = ServerConnection(profile: profile, log: log)
        browser = BrowserModel(connection: connection, log: log)
        telemetry = Telemetry(connection: connection, mount: nil, log: log)
    }

    var condition: ServerCondition {
        switch connection.phase {
        case .online: .online
        case .connecting: .connecting
        case .failed: .failed
        case .offline: .idle
        }
    }

    var failure: String? {
        if case .failed(let error) = connection.phase { return error.message }
        return nil
    }

    var needsPassword: Bool {
        if case .failed(let error) = connection.phase { return error.isAuthentication }
        return false
    }
}

@MainActor
@Observable
final class AppModel {
    private(set) var profiles: [ServerProfile] = []
    private(set) var states: [UUID: ServerState] = [:]
    private(set) var pins: [PinnedServer] = []
    private(set) var environment: CoreEnvironment?
    var selection: UUID?
    var isAddingServer = false
    /// The server whose password the app is asking for.
    var passwordRequest: UUID?

    /// Disk budget for the streaming cache of each server, in GiB.
    var streamCacheGiB: Double {
        didSet { UserDefaults.standard.set(streamCacheGiB, forKey: Self.streamCacheKey) }
    }

    private static let streamCacheKey = "streamCacheGiB"
    static let defaultStreamCacheGiB: Double = 2

    init() {
        let saved = UserDefaults.standard.double(forKey: Self.streamCacheKey)
        streamCacheGiB = saved > 0 ? saved : Self.defaultStreamCacheGiB
        Paths.prepare()
        Paths.prepareMobile()
        load()
        for profile in profiles { states[profile.id] = makeState(profile.id) }
        #if DEBUG
        isAddingServer = ProcessInfo.processInfo.arguments.contains("-showAddServer")
        #endif
    }

    var streamCacheBytes: UInt64 { UInt64(streamCacheGiB * 1_073_741_824) }

    func bootstrap() async {
        environment = try? await Core.call("environment")
        await refreshPins()
        #if DEBUG
        await DebugHooks.seed(self)
        #endif
    }

    func profile(_ id: UUID) -> ServerProfile? { profiles.first { $0.id == id } }

    /// The live state for a saved server. Every profile gets one when it is
    /// loaded or added, so views never create state while they render.
    func state(_ id: UUID) -> ServerState {
        if let state = states[id] { return state }
        let state = makeState(id)
        states[id] = state
        return state
    }

    private func makeState(_ id: UUID) -> ServerState {
        ServerState(id: id) { [weak self] in
            self?.profile(id) ?? ServerProfile(name: "", host: "", address: "", serverName: "", username: "")
        }
    }

    func pin(for profile: ServerProfile) -> PinnedServer? {
        pins.first { $0.address == profile.address && $0.serverName == profile.serverName }
    }

    func refreshPins() async {
        struct Reply: Decodable, Sendable { let servers: [PinnedServer] }
        let reply: Reply? = try? await Core.call("trust_list", ["state_dir": .string(Paths.state.path)])
        pins = reply?.servers ?? []
    }

    // MARK: Profiles

    func add(_ profile: ServerProfile, password: String) {
        Keychain.store(password, for: profile.id)
        profiles.append(profile)
        states[profile.id] = makeState(profile.id)
        save()
        selection = profile.id
    }

    func update(_ profile: ServerProfile) {
        guard let index = profiles.firstIndex(where: { $0.id == profile.id }) else { return }
        let previous = profiles[index]
        profiles[index] = profile
        save()
        if previous.address != profile.address || previous.serverName != profile.serverName
            || previous.username != profile.username || previous.trust != profile.trust
        {
            states[profile.id]?.connection.disconnect()
        }
    }

    func setPassword(_ password: String, for id: UUID) {
        Keychain.store(password, for: id)
        states[id]?.connection.disconnect()
    }

    /// Remove a server. Its pinned identity is kept unless `forgetIdentity`
    /// is set, since another profile may share the same trust store entry.
    func remove(_ id: UUID, forgetIdentity: Bool) async {
        guard let profile = profile(id) else { return }
        states[id]?.connection.disconnect()
        if forgetIdentity { try? await forget(profile) }
        Keychain.remove(for: id)
        profiles.removeAll { $0.id == id }
        states[id] = nil
        if selection == id { selection = nil }
        save()
    }

    func forget(_ profile: ServerProfile) async throws {
        _ = try await Core.call(
            "forget", ["state_dir": .string(Paths.state.path), "server": profile.endpoint],
            as: EmptyReply.self)
        states[profile.id]?.connection.disconnect()
        await refreshPins()
    }

    // MARK: Streaming cache

    struct CacheUsage: Decodable, Sendable {
        let bytes: UInt64
        let files: UInt64
        let namespaces: UInt64
    }

    private(set) var cacheUsage: CacheUsage?

    func refreshCacheUsage() async {
        cacheUsage = try? await Core.call("cache_usage", ["cache_dir": .string(Paths.cache.path)])
    }

    /// Delete every server's cached data. The core refuses while a preview
    /// is reading from the cache.
    func clearCache() async throws -> UInt64 {
        struct Reply: Decodable, Sendable { let freed: UInt64 }
        let reply: Reply = try await Core.call("cache_clear", ["cache_dir": .string(Paths.cache.path)])
        for state in states.values {
            state.log.append(.app, .notice, "streaming cache cleared (\(Format.bytes(reply.freed)) freed)")
        }
        await refreshCacheUsage()
        return reply.freed
    }

    // MARK: Persistence

    private func load() {
        guard let data = try? Data(contentsOf: Paths.profiles) else { return }
        profiles = (try? JSONDecoder().decode([ServerProfile].self, from: data)) ?? []
    }

    private func save() {
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
        guard let data = try? encoder.encode(profiles) else { return }
        try? data.write(to: Paths.profiles, options: [.atomic, .completeFileProtectionUntilFirstUserAuthentication])
    }
}
