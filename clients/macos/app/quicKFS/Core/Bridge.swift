// SPDX-License-Identifier: Apache-2.0
import Foundation

/// A JSON value used to build bridge requests without `Any`.
enum JSONValue: Encodable, Sendable, ExpressibleByStringLiteral, ExpressibleByIntegerLiteral,
    ExpressibleByBooleanLiteral, ExpressibleByDictionaryLiteral
{
    case string(String)
    case int(Int64)
    case uint(UInt64)
    case bool(Bool)
    case object([String: JSONValue])
    case null

    init(stringLiteral value: String) { self = .string(value) }
    init(integerLiteral value: Int64) { self = .int(value) }
    init(booleanLiteral value: Bool) { self = .bool(value) }
    init(dictionaryLiteral elements: (String, JSONValue)...) {
        self = .object(Dictionary(elements, uniquingKeysWith: { $1 }))
    }

    static func uuid(_ value: UUID) -> JSONValue { .string(value.uuidString.lowercased()) }
    static func optional(_ value: String?) -> JSONValue { value.map(JSONValue.string) ?? .null }

    func encode(to encoder: Encoder) throws {
        var container = encoder.singleValueContainer()
        switch self {
        case .string(let value): try container.encode(value)
        case .int(let value): try container.encode(value)
        case .uint(let value): try container.encode(value)
        case .bool(let value): try container.encode(value)
        case .object(let value): try container.encode(value)
        case .null: try container.encodeNil()
        }
    }
}

/// An error reported by the Rust core, with a stable machine-readable code.
struct CoreError: Error, LocalizedError, Decodable, Sendable, Equatable {
    let code: String
    let message: String

    var errorDescription: String? { message }

    /// The connection is gone and a fresh session may succeed.
    var isConnectionLoss: Bool {
        ["session", "transport", "offline", "Offline", "Timeout"].contains(code)
    }

    var isAuthentication: Bool { code == "Unauthenticated" }
}

/// Placeholder for replies whose payload carries nothing the app needs.
struct EmptyReply: Decodable, Sendable {}

private struct Envelope<Payload: Decodable>: Decodable {
    let ok: Payload?
    let error: CoreError?
}

/// Calls into `quickfs-core-bridge`. Each call blocks a background thread in
/// Rust, so requests run on a concurrent dispatch queue and never on the
/// main actor.
enum Core {
    private static let queue = DispatchQueue(
        label: "org.quickfs.core", qos: .userInitiated, attributes: .concurrent)

    static func call<Reply: Decodable & Sendable>(
        _ op: String, _ parameters: [String: JSONValue] = [:], as _: Reply.Type = Reply.self
    ) async throws -> Reply {
        var request = parameters
        request["op"] = .string(op)
        let body = try JSONEncoder().encode(request)
        let reply: Data = await withCheckedContinuation { continuation in
            queue.async { continuation.resume(returning: raw(body)) }
        }
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        let envelope = try decoder.decode(Envelope<Reply>.self, from: reply)
        if let error = envelope.error { throw error }
        guard let payload = envelope.ok else {
            throw CoreError(code: "invalid", message: "The quicKFS core sent an empty reply.")
        }
        return payload
    }

    private static func raw(_ body: Data) -> Data {
        var bytes = [UInt8](body)
        bytes.append(0)
        let pointer = bytes.withUnsafeBufferPointer { buffer in
            buffer.baseAddress!.withMemoryRebound(to: CChar.self, capacity: buffer.count) {
                quickfs_bridge_call($0)
            }
        }
        // The request may hold a password or pairing code; clear our copy.
        for index in bytes.indices { bytes[index] = 0 }
        guard let pointer else { return Data() }
        defer { quickfs_bridge_free(pointer) }
        return Data(bytes: pointer, count: strlen(pointer))
    }
}
