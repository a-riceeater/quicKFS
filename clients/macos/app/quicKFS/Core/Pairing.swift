// SPDX-License-Identifier: Apache-2.0
import Foundation

/// The server's one-time pairing code: 27 base64url characters, displayed in
/// groups of four. Mirrors the rules of the previous desktop client.
enum PairingCode {
    static let length = 27
    static let groups = [4, 4, 4, 4, 4, 4, 3]

    private static let allowed = CharacterSet(
        charactersIn: "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_-")

    /// Strip whitespace and, for a fully grouped code, the group dashes;
    /// keep only base64url characters, capped at the code length.
    static func normalize(_ value: String) -> String {
        let compact = value.filter { !$0.isWhitespace }
        let ungrouped = removeSeparators(compact)
        let scalars = ungrouped.unicodeScalars.filter { allowed.contains($0) }
        return String(String.UnicodeScalarView(scalars).prefix(length))
    }

    /// The code padded to full length and split into display groups.
    static func split(_ value: String) -> [[Character?]] {
        var characters = Array(value).map(Optional.some)
        characters += Array(repeating: nil, count: max(0, length - characters.count))
        var cursor = 0
        return groups.map { size in
            defer { cursor += size }
            return Array(characters[cursor..<cursor + size])
        }
    }

    private static func removeSeparators(_ value: String) -> String {
        let groupedLength = length + groups.count - 1
        guard value.count == groupedLength else { return value }
        var result = ""
        var index = value.startIndex
        for (position, size) in groups.enumerated() {
            let end = value.index(index, offsetBy: size)
            result += value[index..<end]
            index = end
            if position < groups.count - 1 {
                guard value[index] == "-" else { return value }
                index = value.index(after: index)
            }
        }
        return result
    }

    /// Pull a pairing id and code out of pasted server output such as
    /// `Pairing ID: <uuid>` / `Pairing code: <code>`, or a bare uuid and code.
    static func parse(_ text: String) -> (id: UUID?, code: String?) {
        var id: UUID?
        var code: String?
        for line in text.split(whereSeparator: \.isNewline) {
            let parts = line.split(separator: ":", maxSplits: 1).map {
                $0.trimmingCharacters(in: .whitespaces)
            }
            if parts.count == 2 {
                let key = parts[0].lowercased()
                if key.contains("id"), let value = UUID(uuidString: parts[1]) { id = value }
                if key.contains("code") {
                    let candidate = normalize(parts[1])
                    if candidate.count == length { code = candidate }
                }
            } else if let value = UUID(uuidString: String(line).trimmingCharacters(in: .whitespaces)) {
                id = value
            } else {
                let candidate = normalize(String(line))
                if candidate.count == length { code = candidate }
            }
        }
        return (id, code)
    }
}
