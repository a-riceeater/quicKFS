// SPDX-License-Identifier: Apache-2.0
import Foundation

enum Format {
    static func bytes(_ value: UInt64) -> String {
        ByteCountFormatter.string(fromByteCount: Int64(clamping: value), countStyle: .file)
    }

    static func milliseconds(_ value: Double) -> String {
        value < 10 ? String(format: "%.1f ms", value) : String(format: "%.0f ms", value)
    }

    static func mode(_ mode: UInt32, directory: Bool) -> String {
        let flags: [(UInt32, Character)] = [
            (0o400, "r"), (0o200, "w"), (0o100, "x"),
            (0o040, "r"), (0o020, "w"), (0o010, "x"),
            (0o004, "r"), (0o002, "w"), (0o001, "x"),
        ]
        return String(directory ? "d" : "-") + String(flags.map { mode & $0.0 != 0 ? $0.1 : "-" })
    }
}

extension Data {
    init(hex: String) {
        var bytes = [UInt8]()
        bytes.reserveCapacity(hex.utf8.count / 2)
        var high: UInt8?
        for character in hex.utf8 {
            let value: UInt8
            switch character {
            case 48...57: value = character - 48
            case 97...102: value = character - 87
            case 65...70: value = character - 55
            default: continue
            }
            if let current = high {
                bytes.append(current << 4 | value)
                high = nil
            } else {
                high = value
            }
        }
        self.init(bytes)
    }
}
