// SPDX-License-Identifier: Apache-2.0
import Foundation

/// Where the app keeps private state. The trust store and per-server caches
/// live in directories the Rust core secures to 0700.
enum Paths {
    static let applicationSupport: URL = {
        let base = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
        return base.appendingPathComponent("quicKFS", isDirectory: true)
    }()

    /// Client state (the pinned-identity trust store) shared with the core.
    static var state: URL { applicationSupport.appendingPathComponent("state", isDirectory: true) }

    static var profiles: URL { applicationSupport.appendingPathComponent("servers.json") }

    /// Persistent offline read cache for mounts.
    static let cache: URL = {
        let base = FileManager.default.urls(for: .cachesDirectory, in: .userDomainMask)[0]
        return base.appendingPathComponent("org.quickfs.client", isDirectory: true)
    }()

    /// Mounts default to ~/Volumes/<server> so no administrator rights are needed.
    static var defaultMountRoot: URL {
        FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent(
            "Volumes", isDirectory: true)
    }

    static var mountHelper: URL {
        Bundle.main.bundleURL.appendingPathComponent("Contents/Helpers/quickfs-mount")
    }

    static func prepare() {
        let manager = FileManager.default
        for directory in [applicationSupport, cache] {
            try? manager.createDirectory(
                at: directory, withIntermediateDirectories: true,
                attributes: [.posixPermissions: 0o700])
        }
    }

    /// A Finder-safe folder name: no slashes or colons, never empty.
    static func safeFolderName(_ name: String) -> String {
        let cleaned = name.components(separatedBy: CharacterSet(charactersIn: "/:\0"))
            .joined(separator: "-")
            .trimmingCharacters(in: .whitespacesAndNewlines)
        return cleaned.isEmpty ? "quicKFS" : cleaned
    }
}
