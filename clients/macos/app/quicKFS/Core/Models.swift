// SPDX-License-Identifier: Apache-2.0
import Foundation

/// How a server is authenticated before any credential is sent.
enum TrustMode: Codable, Hashable, Sendable {
    /// An exact certificate pin from pairing or a managed import.
    case pinned
    /// A PEM enterprise CA bundle plus the server name.
    case enterpriseCA(path: String)
    /// The system's public and managed roots plus the server name.
    case systemRoots

    var request: JSONValue {
        switch self {
        case .pinned: ["kind": "pinned"]
        case .enterpriseCA(let path): ["kind": "enterprise_ca", "certificate": .string(path)]
        case .systemRoots: ["kind": "system_roots"]
        }
    }

    /// Mount helper arguments that select this trust policy.
    var mountArguments: [String] {
        switch self {
        case .pinned: []
        case .enterpriseCA(let path): ["--ca-cert", path]
        case .systemRoots: ["--trust-system-roots"]
        }
    }
}

enum MountBackend: String, Codable, CaseIterable, Sendable {
    case automatic
    case fskit
}

/// Tunables passed to `quickfs-mount`. Defaults match the helper's own.
struct MountOptions: Codable, Hashable, Sendable {
    var mountpoint: String?
    var volumeName: String?
    var cacheMaxGiB: Double = 20
    var cacheBlockKiB: Int = 16_384
    var readAheadMiB: Int = 64
    var timeoutSeconds: Int = 60
    var callbackTimeoutSeconds: Int = 120
    var reconnectAttempts: Int = 3
    var reconnectInitialBackoffMs: Int = 100
    var reconnectMaxBackoffMs: Int = 2_000
    var backend: MountBackend = .automatic
    var mountAtLaunch = false
}

/// A saved server. Passwords live in the Keychain, never in this record.
struct ServerProfile: Codable, Identifiable, Hashable, Sendable {
    var id = UUID()
    var name: String
    /// What the user typed, kept for display and re-resolution.
    var host: String
    /// The resolved socket address the trust store is keyed by.
    var address: String
    var serverName: String
    var username: String
    var trust: TrustMode = .pinned
    var mount = MountOptions()

    var endpoint: JSONValue {
        ["address": .string(address), "server_name": .string(serverName)]
    }

    var mountpointURL: URL {
        if let path = mount.mountpoint, !path.isEmpty {
            return URL(fileURLWithPath: (path as NSString).expandingTildeInPath, isDirectory: true)
        }
        return Paths.defaultMountRoot.appendingPathComponent(
            Paths.safeFolderName(name), isDirectory: true)
    }
}

struct Capabilities: Decodable, Hashable, Sendable {
    let serverEpoch: UUID
    let writable: Bool
    let supportsLocks: Bool
    let supportsAtomicRename: Bool
    let supportsDirectorySync: Bool
    let supportsPreallocation: Bool
    let supportsSymlinks: Bool
    let supportsXattrs: Bool
    let supportsHardLinks: Bool
    let supportsSpecialNodes: Bool
    let supportsCopyFileRange: Bool
    let supportsSeekDataHole: Bool
    let supportsSafeIoctl: Bool
    let supportsPoll: Bool
    let supportsBmap: Bool
    let supportsExchangeData: Bool
    let supportsVolumeRename: Bool
    let supportsBackupTime: Bool
    let supportsReaddirplus: Bool
    let persistentNodeIds: Bool
    let restartLockReplay: Bool
    let volumeName: String
    let maxReadSize: UInt64
    let maxWriteSize: UInt64
}

struct FilesystemStats: Decodable, Hashable, Sendable {
    let blocks: UInt64
    let blocksFree: UInt64
    let blocksAvailable: UInt64
    let files: UInt64
    let filesFree: UInt64
    let blockSize: UInt32
    let nameLength: UInt32
    let fragmentSize: UInt32

    private var unit: UInt64 { UInt64(fragmentSize > 0 ? fragmentSize : blockSize) }
    var totalBytes: UInt64 { blocks &* unit }
    var availableBytes: UInt64 { blocksAvailable &* unit }
    var usedBytes: UInt64 { (blocks &- blocksFree) &* unit }
    var usedFraction: Double {
        totalBytes == 0 ? 0 : min(1, Double(usedBytes) / Double(totalBytes))
    }
}

enum NodeKind: String, Decodable, Sendable {
    case file, directory, symlink, fifo
    case charDevice = "char_device"
    case blockDevice = "block_device"
    case socket
}

struct RemoteNode: Decodable, Identifiable, Hashable, Sendable {
    let node: UUID
    let name: String?
    let rawName: String?
    let kind: NodeKind
    let size: UInt64
    let mode: UInt32
    let allocatedBytes: UInt64
    let revision: UInt64
    let accessedMs: UInt64
    let modifiedMs: UInt64
    let createdMs: UInt64?
    let backupMs: UInt64?
    let linkCount: UInt32

    var id: UUID { node }
    var displayName: String { name ?? "" }
    var isDirectory: Bool { kind == .directory }
    var modified: Date { Date(timeIntervalSince1970: Double(modifiedMs) / 1000) }
    var created: Date? { createdMs.map { Date(timeIntervalSince1970: Double($0) / 1000) } }
    var accessed: Date { Date(timeIntervalSince1970: Double(accessedMs) / 1000) }

    /// Request fields that address this entry by name inside its parent.
    var nameFields: [String: JSONValue] {
        var fields: [String: JSONValue] = ["name": .string(displayName)]
        if let rawName { fields["raw_name"] = .string(rawName) }
        return fields
    }

    static let rootID = UUID(uuid: (0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0))
}

struct DirectoryListing: Decodable, Sendable {
    let directory: RemoteNode
    let entries: [RemoteNode]
}

struct TransferStatus: Decodable, Sendable {
    enum State: String, Decodable, Sendable { case running, finished, failed, cancelled }
    let state: State
    let bytes: UInt64
    let total: UInt64
    let files: UInt64
    let filesTotal: UInt64
    let error: CoreError?
    /// The file being copied now, relative to the transfer's root.
    let current: String?
    /// Bytes per pipelined request and how many requests stay in flight.
    let chunkBytes: UInt64?
    let pipelineDepth: Int?

    var fraction: Double { total == 0 ? 0 : min(1, Double(bytes) / Double(total)) }
}

struct PinnedServer: Decodable, Hashable, Sendable {
    let address: String
    let serverName: String
    let sha256: String
}

struct CoreEnvironment: Decodable, Sendable {
    let macfuseInstalled: Bool
    let macfuseInstallUrl: String
    let maxClientReadSize: UInt64
    let maxClientWriteSize: UInt64
    let coreVersion: String
}
