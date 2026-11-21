// SPDX-License-Identifier: Apache-2.0
import Foundation

/// One remote file held open for random-access reads.
///
/// Bytes come through the core's cached view of the session, so sequential
/// readers get read-ahead and every block lands in the bounded streaming
/// cache. Reads use the bridge's binary call: raw bytes into a buffer, with
/// no JSON or hex in between.
final class RemoteStream: Sendable {
    let id: UInt64
    let size: UInt64

    /// The most one call returns; the core caps reads at the same size.
    static let maxRead = 4 << 20

    private static let queue = DispatchQueue(
        label: "org.quickfs.stream", qos: .userInitiated, attributes: .concurrent)

    private init(id: UInt64, size: UInt64) {
        self.id = id
        self.size = size
    }

    /// Open `node` on the connection's session, reconnecting once if the
    /// session was lost.
    @MainActor
    static func open(_ node: RemoteNode, connection: ServerConnection, cacheBytes: UInt64) async throws
        -> RemoteStream
    {
        struct Reply: Decodable, Sendable { let stream: UInt64; let size: UInt64 }
        let reply: Reply = try await connection.request("stream_open", [
            "node": .uuid(node.node),
            "cache_dir": .string(Paths.cache.path),
            "cache_max_bytes": .uint(cacheBytes),
        ])
        return RemoteStream(id: reply.stream, size: reply.size)
    }

    /// Read up to `count` bytes at `offset`; fewer only at the end of the
    /// file. Blocks until the bytes arrive, so call it off the main thread.
    func read(at offset: UInt64, count: Int) throws -> Data {
        let wanted = max(0, min(count, Self.maxRead))
        var data = Data(count: wanted)
        var length: UInt = 0
        let failure = data.withUnsafeMutableBytes { buffer in
            quickfs_bridge_stream_read(
                id, offset, buffer.baseAddress?.assumingMemoryBound(to: UInt8.self), UInt(wanted), &length)
        }
        if let failure {
            defer { quickfs_bridge_free(failure) }
            struct Envelope: Decodable { let error: CoreError }
            let reply = Data(bytes: failure, count: strlen(failure))
            throw (try? JSONDecoder().decode(Envelope.self, from: reply).error)
                ?? CoreError(code: "invalid", message: "The quicKFS core sent an unreadable error.")
        }
        data.count = Int(length)
        return data
    }

    /// `read` on a background queue.
    func read(at offset: UInt64, count: Int) async throws -> Data {
        try await withCheckedThrowingContinuation { continuation in
            Self.queue.async {
                continuation.resume(with: Result { try self.read(at: offset, count: count) })
            }
        }
    }

    /// The whole file, read in the largest pieces the core allows.
    func readAll(progress: @escaping @Sendable (UInt64) -> Void = { _ in }) async throws -> Data {
        var data = Data()
        data.reserveCapacity(Int(size))
        while UInt64(data.count) < size {
            try Task.checkCancellation()
            let piece = try await read(at: UInt64(data.count), count: Self.maxRead)
            if piece.isEmpty { break }
            data.append(piece)
            progress(UInt64(data.count))
        }
        return data
    }

    /// Release the file. Reads still in flight fail harmlessly.
    func close() {
        let id = id
        Task.detached {
            _ = try? await Core.call("stream_close", ["stream": .uint(id)], as: EmptyReply.self)
        }
    }
}
