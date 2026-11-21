// SPDX-License-Identifier: Apache-2.0
#if DEBUG
import Foundation

/// Launch arguments that drive the app without touches, for checks in the
/// simulator on debug builds only (`xcrun simctl launch … -debugServer …`).
@MainActor
enum DebugHooks {
    private static let arguments = ProcessInfo.processInfo.arguments
    private static var driven = false

    private static func value(_ name: String) -> String? {
        guard let index = arguments.firstIndex(of: name), index + 1 < arguments.count else { return nil }
        return arguments[index + 1]
    }

    /// `-debugServer "host:port|user|password|sha256"`: pin the fingerprint
    /// and save the server, unless one with that host already exists.
    static func seed(_ model: AppModel) async {
        guard let spec = value("-debugServer") else { return }
        let parts = spec.split(separator: "|", omittingEmptySubsequences: false).map(String.init)
        guard parts.count == 4 else { return }
        if let existing = model.profiles.first(where: { $0.host == parts[0] }) {
            model.selection = existing.id
            return
        }
        struct Resolved: Decodable, Sendable { let addresses: [String]; let serverName: String }
        guard let resolved: Resolved = try? await Core.call("resolve", ["host": .string(parts[0])]) else { return }
        let address = resolved.addresses.first { !$0.hasPrefix("[") } ?? resolved.addresses[0]
        _ = try? await Core.call("trust_import", [
            "state_dir": .string(Paths.state.path),
            "server": ["address": .string(address), "server_name": .string(resolved.serverName)],
            "sha256": .string(parts[3]),
        ], as: EmptyReply.self)
        await model.refreshPins()
        model.add(
            ServerProfile(name: "Debug", host: parts[0], address: address,
                          serverName: resolved.serverName, username: parts[1]),
            password: parts[2])
    }

    /// `-debugBrowse /path`, `-debugPreview /path/file`, `-debugDownload
    /// /path`, `-debugPanel overview|console|settings`, `-debugTransfers`.
    static func drive(browser: BrowserModel, ui: BrowserUI) async {
        guard !driven, browser.connection.isOnline else { return }
        driven = true
        if let path = value("-debugBrowse") { await browser.goTo(path: path) }
        if let path = value("-debugPreview"), let node = await node(at: path, browser: browser) {
            ui.previewing = node
        }
        if let path = value("-debugDownload"), let node = await node(at: path, browser: browser) {
            browser.download([node], into: Paths.downloads)
        }
        if let panel = value("-debugPanel").flatMap(ServerPanel.Tab.init(rawValue:)) { ui.panel = panel }
        if arguments.contains("-debugTransfers") {
            try? await Task.sleep(for: .seconds(1))
            ui.showingTransfers = true
        }
    }

    /// `resolve_path` returns the node without its name; give it one.
    private static func node(at path: String, browser: BrowserModel) async -> RemoteNode? {
        struct Reply: Decodable, Sendable { let node: RemoteNode }
        guard let reply: Reply = try? await browser.connection.request("resolve_path", ["path": .string(path)])
        else { return nil }
        let node = reply.node
        return RemoteNode(
            node: node.node, name: (path as NSString).lastPathComponent, rawName: nil, kind: node.kind,
            size: node.size, mode: node.mode, allocatedBytes: node.allocatedBytes, revision: node.revision,
            accessedMs: node.accessedMs, modifiedMs: node.modifiedMs, createdMs: node.createdMs,
            backupMs: node.backupMs, linkCount: node.linkCount)
    }
}
#endif
