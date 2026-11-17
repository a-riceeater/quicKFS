// SPDX-License-Identifier: Apache-2.0
#if DEBUG
import Foundation

/// Launch arguments that drive UI states without clicking, for checks on
/// debug builds only. Run with `QUICKFS_APP_DATA_DIR` so real data is untouched.
@MainActor
enum DebugHooks {
    private static let arguments = ProcessInfo.processInfo.arguments
    private static var downloadStarted = false

    /// `-showTransferDetail`: open each transfer's detail popup as it appears.
    static var showTransferDetail: Bool { arguments.contains("-showTransferDetail") }

    /// `-debugDownload /remote/path`: download that item into a temporary
    /// folder once the browser is connected.
    static func startDownloadIfRequested(browser: BrowserModel) async {
        guard !downloadStarted, let index = arguments.firstIndex(of: "-debugDownload"),
            index + 1 < arguments.count
        else { return }
        downloadStarted = true
        struct Reply: Decodable, Sendable { let node: RemoteNode }
        guard let reply: Reply = try? await browser.connection.request(
            "resolve_path", ["path": .string(arguments[index + 1])])
        else { return }
        let folder = FileManager.default.temporaryDirectory
            .appendingPathComponent("quickfs-debug-\(UUID().uuidString.prefix(8))", isDirectory: true)
        try? FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
        let named = RemoteNodeNamed(reply.node, name: (arguments[index + 1] as NSString).lastPathComponent)
        browser.download([named], into: folder)
    }

    /// `resolve_path` returns the node without its name; give it one.
    private static func RemoteNodeNamed(_ node: RemoteNode, name: String) -> RemoteNode {
        RemoteNode(
            node: node.node, name: name, rawName: nil, kind: node.kind, size: node.size, mode: node.mode,
            allocatedBytes: node.allocatedBytes, revision: node.revision, accessedMs: node.accessedMs,
            modifiedMs: node.modifiedMs, createdMs: node.createdMs, backupMs: node.backupMs,
            linkCount: node.linkCount)
    }
}
#endif
