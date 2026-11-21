// SPDX-License-Identifier: Apache-2.0
import Foundation

/// Where the app keeps private state. The trust store and per-server caches
/// live in directories the Rust core secures to 0700.
enum Paths {
    /// `QUICKFS_APP_DATA_DIR` moves all app data (servers, pins, cache) to
    /// another folder, so tests and experiments never touch a
    /// person's real servers or pairing.
    private static let overrideRoot: URL? = ProcessInfo.processInfo.environment["QUICKFS_APP_DATA_DIR"]
        .flatMap { $0.isEmpty ? nil : URL(fileURLWithPath: $0, isDirectory: true) }

    static let applicationSupport: URL = {
        if let overrideRoot { return overrideRoot }
        let base = FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
        return base.appendingPathComponent("quicKFS", isDirectory: true)
    }()

    /// Client state (the pinned-identity trust store) shared with the core.
    static var state: URL { applicationSupport.appendingPathComponent("state", isDirectory: true) }

    static var profiles: URL { applicationSupport.appendingPathComponent("servers.json") }

    /// Persistent offline read cache for mounts.
    static let cache: URL = {
        if let overrideRoot { return overrideRoot.appendingPathComponent("cache", isDirectory: true) }
        let base = FileManager.default.urls(for: .cachesDirectory, in: .userDomainMask)[0]
        return base.appendingPathComponent("org.quickfs.client", isDirectory: true)
    }()

    #if os(macOS)
    /// Mounts default to ~/Volumes/<server> so no administrator rights are needed.
    static var defaultMountRoot: URL {
        FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent(
            "Volumes", isDirectory: true)
    }

    static var mountHelper: URL {
        Bundle.main.bundleURL.appendingPathComponent("Contents/Helpers/quickfs-mount")
    }
    #endif

    static func prepare() {
        let manager = FileManager.default
        for directory in [applicationSupport, cache] {
            try? manager.createDirectory(
                at: directory, withIntermediateDirectories: true,
                attributes: [.posixPermissions: 0o700])
            // The core refuses caches and state other users could read, so
            // tighten folders left behind by older builds too.
            try? manager.setAttributes([.posixPermissions: 0o700], ofItemAtPath: directory.path)
        }
    }

    /// Where a saved enterprise CA bundle is now. iOS moves the app's
    /// container when the app updates, so a bundle the app keeps in its own
    /// `certificates` folder is found again by name.
    static func certificateLocation(_ path: String) -> String {
        #if os(iOS)
        guard !FileManager.default.fileExists(atPath: path) else { return path }
        return applicationSupport.appendingPathComponent("certificates", isDirectory: true)
            .appendingPathComponent((path as NSString).lastPathComponent).path
        #else
        return path
        #endif
    }

    /// A Finder-safe folder name: no slashes or colons, never empty.
    static func safeFolderName(_ name: String) -> String {
        let cleaned = name.components(separatedBy: CharacterSet(charactersIn: "/:\0"))
            .joined(separator: "-")
            .trimmingCharacters(in: .whitespacesAndNewlines)
        return cleaned.isEmpty ? "quicKFS" : cleaned
    }
}
