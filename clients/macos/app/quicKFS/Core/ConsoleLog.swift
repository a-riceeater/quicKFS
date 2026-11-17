// SPDX-License-Identifier: Apache-2.0
import Foundation
import Observation

/// One line in a server's console.
struct ConsoleLine: Identifiable, Hashable, Sendable {
    enum Source: String, CaseIterable, Sendable {
        case app, link, mount, kernel
    }

    enum Level: Int, Comparable, Sendable {
        case debug, info, notice, warning, error

        static func < (lhs: Level, rhs: Level) -> Bool { lhs.rawValue < rhs.rawValue }

        var tag: String {
            switch self {
            case .debug: "DBG"
            case .info: "INF"
            case .notice: "OK "
            case .warning: "WRN"
            case .error: "ERR"
            }
        }
    }

    let id: UInt64
    let date: Date
    let source: Source
    let level: Level
    let text: String
}

/// A bounded, per-server event stream: app events, link changes, the mount
/// helper's output, and (when tracing) every kernel request it served.
@MainActor
@Observable
final class ConsoleLog {
    private(set) var lines: [ConsoleLine] = []
    private var nextID: UInt64 = 0
    /// Kernel tracing can emit thousands of lines a second; keep the newest.
    private let capacity = 5_000

    func append(_ source: ConsoleLine.Source, _ level: ConsoleLine.Level, _ text: String) {
        nextID += 1
        lines.append(ConsoleLine(id: nextID, date: .now, source: source, level: level, text: text))
        if lines.count > capacity { lines.removeFirst(lines.count - capacity) }
    }

    /// Classify one line of mount-helper output.
    func ingestHelperOutput(_ line: String) {
        let trimmed = line.trimmingCharacters(in: .whitespaces)
        guard !trimmed.isEmpty else { return }
        // `QUICKFS_FUSE_DEBUG` lines look like "[  12.345 DEBUG fuser::request] ...".
        if trimmed.hasPrefix("["), trimmed.contains("fuser") {
            let level: ConsoleLine.Level =
                trimmed.contains(" ERROR ") ? .error : trimmed.contains(" WARN ") ? .warning : .debug
            let body = trimmed.firstIndex(of: "]").map { String(trimmed[trimmed.index(after: $0)...]) } ?? trimmed
            append(.kernel, level, body.trimmingCharacters(in: .whitespaces))
            return
        }
        let lower = trimmed.lowercased()
        let level: ConsoleLine.Level
        if lower.hasPrefix("error") || lower.contains("failed") || lower.contains("caused by") {
            level = .error
        } else if lower.contains("warn") || lower.contains("retry") || lower.contains("remount") {
            level = .warning
        } else if lower.contains("ready") {
            level = .notice
        } else {
            level = .info
        }
        append(.mount, level, trimmed)
    }

    func clear() { lines.removeAll() }

    /// Plain text for copying or exporting.
    func export(_ lines: [ConsoleLine]) -> String {
        let formatter = DateFormatter()
        formatter.dateFormat = "yyyy-MM-dd HH:mm:ss.SSS"
        return lines.map {
            "\(formatter.string(from: $0.date))  \($0.source.rawValue.uppercased().padding(toLength: 6, withPad: " ", startingAt: 0)) \($0.level.tag)  \($0.text)"
        }.joined(separator: "\n")
    }
}
