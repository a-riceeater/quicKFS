// SPDX-License-Identifier: Apache-2.0
import Foundation

/// Where the iOS app keeps what the Mac keeps in the person's own folders.
extension Paths {
    /// Downloads land in Documents, which the Files app shows under
    /// On My iPhone › quicKFS.
    static var downloads: URL {
        FileManager.default.urls(for: .documentDirectory, in: .userDomainMask)[0]
    }

    /// Copies of picked files waiting to upload. The core reads uploads by
    /// path, and a picked file is only reachable while its security scope is
    /// open, so each one is copied here first.
    static var uploadStaging: URL {
        FileManager.default.temporaryDirectory.appendingPathComponent("Uploads", isDirectory: true)
    }

    /// Enterprise CA bundles the person picked, kept inside the app so the
    /// trust setting survives the original file moving.
    static var certificates: URL {
        applicationSupport.appendingPathComponent("certificates", isDirectory: true)
    }

    static func prepareMobile() {
        let manager = FileManager.default
        // Nothing survives a relaunch that could still need a staged copy.
        try? manager.removeItem(at: uploadStaging)
        for directory in [uploadStaging, certificates] {
            try? manager.createDirectory(
                at: directory, withIntermediateDirectories: true,
                attributes: [.posixPermissions: 0o700])
        }
    }

    /// One server's staged uploads, so clearing one server's finished
    /// transfers never removes a copy another server is still sending.
    static func uploadStaging(for server: UUID) -> URL {
        uploadStaging.appendingPathComponent(server.uuidString, isDirectory: true)
    }

    /// Copy a picked file or folder into a fresh staging folder, keeping
    /// its name, while its security scope is open.
    static func stage(_ url: URL, for server: UUID) throws -> URL {
        let scoped = url.startAccessingSecurityScopedResource()
        defer { if scoped { url.stopAccessingSecurityScopedResource() } }
        let folder = uploadStaging(for: server).appendingPathComponent(UUID().uuidString, isDirectory: true)
        try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
        let copy = folder.appendingPathComponent(url.lastPathComponent)
        var coordinationError: NSError?
        var copyError: Error?
        NSFileCoordinator().coordinate(readingItemAt: url, options: [], error: &coordinationError) { readable in
            do { try FileManager.default.copyItem(at: readable, to: copy) } catch { copyError = error }
        }
        if let error = coordinationError ?? copyError { throw error }
        return copy
    }

    /// Move a file staged without a server (a photo) under `server`.
    static func adopt(_ file: URL, for server: UUID) throws -> URL {
        let folder = uploadStaging(for: server).appendingPathComponent(UUID().uuidString, isDirectory: true)
        try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
        let destination = folder.appendingPathComponent(file.lastPathComponent)
        try FileManager.default.moveItem(at: file, to: destination)
        try? FileManager.default.removeItem(at: file.deletingLastPathComponent())
        return destination
    }

    /// Drop a server's staged copies once none of its uploads can need one.
    static func clearStaging(for server: UUID) {
        try? FileManager.default.removeItem(at: uploadStaging(for: server))
    }

    /// Keep a picked certificate bundle inside the app and return its copy.
    static func importCertificate(_ url: URL) throws -> URL {
        let scoped = url.startAccessingSecurityScopedResource()
        defer { if scoped { url.stopAccessingSecurityScopedResource() } }
        let destination = certificates.appendingPathComponent(
            "\(UUID().uuidString.prefix(8))-\(safeFolderName(url.lastPathComponent))")
        try FileManager.default.copyItem(at: url, to: destination)
        return destination
    }
}
