// SPDX-License-Identifier: Apache-2.0
import Foundation
import Observation

/// Live state for one saved server: its browsing session and its mount.
@MainActor
@Observable
final class ServerState: Identifiable {
    let id: UUID
    let log = ConsoleLog()
    let connection: ServerConnection
    let mount: MountController
    /// Kept per server so navigation and transfers survive switching views.
    let browser: BrowserModel
    let telemetry: Telemetry

    init(id: UUID, profile: @escaping @MainActor () -> ServerProfile) {
        self.id = id
        connection = ServerConnection(profile: profile, log: log)
        mount = MountController(log: log)
        browser = BrowserModel(connection: connection, log: log)
        telemetry = Telemetry(connection: connection, mount: mount, log: log)
    }
}

@MainActor
@Observable
final class AppModel {
    private(set) var profiles: [ServerProfile] = []
    private(set) var states: [UUID: ServerState] = [:]
    private(set) var pins: [PinnedServer] = []
    private(set) var environment: CoreEnvironment?
    private(set) var environmentError: String?
    var selection: UUID?
    var isAddingServer = false
    /// The server whose password the app is asking for.
    var passwordRequest: UUID?

    init() {
        Paths.prepare()
        load()
        #if DEBUG
        // Lets UI checks open the add flow without clicking: `-showAddServer`.
        isAddingServer = ProcessInfo.processInfo.arguments.contains("-showAddServer")
        // `-seedPassword <password>` saves it for every server, as the
        // password sheet would, so UI checks need no typing.
        let arguments = ProcessInfo.processInfo.arguments
        if let index = arguments.firstIndex(of: "-seedPassword"), index + 1 < arguments.count {
            for profile in profiles { Keychain.store(arguments[index + 1], for: profile.id) }
        }
        #endif
    }

    var macfuseInstalled: Bool { environment?.macfuseInstalled ?? true }

    func bootstrap() async {
        do {
            environment = try await Core.call("environment")
            environmentError = nil
        } catch {
            environmentError = error.localizedDescription
        }
        await refreshPins()
        if macfuseInstalled {
            for profile in profiles where profile.mount.mountAtLaunch { mount(profile.id) }
        }
    }

    func profile(_ id: UUID) -> ServerProfile? { profiles.first { $0.id == id } }

    func state(_ id: UUID) -> ServerState {
        if let state = states[id] { return state }
        let state = ServerState(id: id) { [weak self] in
            self?.profile(id) ?? ServerProfile(name: "", host: "", address: "", serverName: "", username: "")
        }
        states[id] = state
        return state
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
    /// is set, since other tools may share the same trust store entry.
    func remove(_ id: UUID, forgetIdentity: Bool) async {
        guard let profile = profile(id) else { return }
        if let state = states[id] {
            state.connection.disconnect()
            await state.mount.shutdown()
        }
        if forgetIdentity { try? await forget(profile) }
        Keychain.remove(for: id)
        profiles.removeAll { $0.id == id }
        states[id] = nil
        if selection == id { selection = profiles.first?.id }
        save()
    }

    func forget(_ profile: ServerProfile) async throws {
        _ = try await Core.call(
            "forget", ["state_dir": .string(Paths.state.path), "server": profile.endpoint],
            as: EmptyReply.self)
        states[profile.id]?.connection.disconnect()
        await refreshPins()
    }

    // MARK: Mounts

    /// Mount with `password`, or the saved one; asks when neither exists.
    func mount(_ id: UUID, password: String? = nil) {
        guard profile(id) != nil else { return }
        let mount = state(id).mount
        guard !mount.isActive else { return }
        mount.prepare()
        Task {
            let secret: String?
            if let password { secret = password } else { secret = await Keychain.load(for: id) }
            guard let secret, let profile = profile(id) else {
                mount.cancelPreparation()
                passwordRequest = id
                return
            }
            mount.mount(profile, password: secret)
        }
    }

    func unmount(_ id: UUID) { states[id]?.mount.unmount() }

    /// Eject, wait for the helper to exit, and mount again with current settings.
    func remount(_ id: UUID) {
        guard let state = states[id] else { return }
        Task {
            await state.mount.shutdown()
            mount(id)
        }
    }

    func toggleMount(_ id: UUID) {
        if state(id).mount.isActive { unmount(id) } else { mount(id) }
    }

    var activeMounts: Int { states.values.filter { $0.mount.isActive }.count }

    // MARK: Offline cache

    struct CacheUsage: Decodable, Sendable {
        let bytes: UInt64
        let files: UInt64
        let namespaces: UInt64
    }

    private(set) var cacheUsage: CacheUsage?

    func refreshCacheUsage() async {
        cacheUsage = try? await Core.call("cache_usage", ["cache_dir": .string(Paths.cache.path)])
    }

    /// Delete every server's cached data. Refused while anything is mounted,
    /// because a live mount keeps its cache files open.
    func clearCache() async throws -> UInt64 {
        guard activeMounts == 0 else {
            throw CoreError(code: "busy", message: "Eject all servers before clearing the cache.")
        }
        struct Reply: Decodable, Sendable { let freed: UInt64 }
        let reply: Reply = try await Core.call("cache_clear", ["cache_dir": .string(Paths.cache.path)])
        for state in states.values {
            state.log.append(.app, .notice, "offline cache cleared (\(Format.bytes(reply.freed)) freed)")
        }
        await refreshCacheUsage()
        return reply.freed
    }

    func shutdown() async {
        await withTaskGroup(of: Void.self) { group in
            for state in states.values where state.mount.isActive {
                group.addTask { await state.mount.shutdown() }
            }
        }
    }

    // MARK: Persistence

    private func load() {
        guard let data = try? Data(contentsOf: Paths.profiles) else { return }
        profiles = (try? JSONDecoder().decode([ServerProfile].self, from: data)) ?? []
        selection = profiles.first?.id
    }

    private func save() {
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.prettyPrinted, .sortedKeys]
        guard let data = try? encoder.encode(profiles) else { return }
        try? data.write(to: Paths.profiles, options: [.atomic])
        try? FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: Paths.profiles.path)
    }
}
