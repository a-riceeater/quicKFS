// SPDX-License-Identifier: Apache-2.0
import Charts
import SwiftUI
import UniformTypeIdentifiers

extension UUID: @retroactive Identifiable {
    public var id: UUID { self }
}

/// The single visual vocabulary for a server's condition.
enum ServerCondition: Equatable {
    case idle, connecting, online, failed

    var tint: Color {
        switch self {
        case .idle: .secondary
        case .connecting: .orange
        case .online: .green
        case .failed: .red
        }
    }

    var symbol: String {
        switch self {
        case .failed: "externaldrive.badge.exclamationmark"
        case .online: "externaldrive.fill.badge.wifi"
        case .idle, .connecting: "externaldrive.badge.wifi"
        }
    }

    var label: String {
        switch self {
        case .idle: "Not connected"
        case .connecting: "Connecting"
        case .online: "Connected"
        case .failed: "Needs attention"
        }
    }
}

struct StatusDot: View {
    let condition: ServerCondition

    var body: some View {
        Circle()
            .fill(condition.tint.gradient)
            .frame(width: 8, height: 8)
            .shadow(color: condition.tint.opacity(0.6), radius: condition == .online ? 4 : 0)
            .accessibilityLabel(condition.label)
    }
}

/// A rounded glass tile carrying the server's condition.
struct ServerGlyph: View {
    let condition: ServerCondition
    var size: CGFloat = 40

    var body: some View {
        Image(systemName: condition.symbol)
            .font(.system(size: size * 0.45, weight: .semibold))
            .symbolRenderingMode(.hierarchical)
            .foregroundStyle(condition == .idle ? Color.secondary : condition.tint)
            .contentTransition(.symbolEffect(.replace))
            .frame(width: size, height: size)
            .glassSurface(RoundedRectangle(cornerRadius: size * 0.3, style: .continuous),
                          tint: condition == .idle ? nil : condition.tint.opacity(0.18))
    }
}

/// A compact empty or message state: a large symbol, one line, optional actions.
struct Placard<Actions: View>: View {
    let symbol: String
    let title: String
    var detail: String?
    var tint: Color = .secondary
    @ViewBuilder var actions: () -> Actions

    var body: some View {
        VStack(spacing: 14) {
            Image(systemName: symbol)
                .font(.system(size: 44, weight: .light))
                .symbolRenderingMode(.hierarchical)
                .foregroundStyle(tint)
            Text(title)
                .font(.title3.weight(.semibold))
            if let detail {
                Text(detail)
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .multilineTextAlignment(.center)
                    .frame(maxWidth: 380)
                    .textSelection(.enabled)
            }
            HStack(spacing: 10) { actions() }
                .padding(.top, 4)
        }
        .padding(32)
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

extension Placard where Actions == EmptyView {
    init(symbol: String, title: String, detail: String? = nil, tint: Color = .secondary) {
        self.init(symbol: symbol, title: title, detail: detail, tint: tint) { EmptyView() }
    }
}

// MARK: - Files

/// What a file is, for its icon and for how it previews.
enum FileKind {
    case folder, link, video, audio, image, pdf, text, code, archive, document, other

    init(_ node: RemoteNode) {
        switch node.kind {
        case .directory: self = .folder
        case .symlink: self = .link
        case .file: self.init(type: node.contentType)
        default: self = .other
        }
    }

    init(type: UTType?) {
        guard let type else { self = .other; return }
        if type.conforms(to: .movie) || type.conforms(to: .video) { self = .video }
        else if type.conforms(to: .audio) { self = .audio }
        else if type.conforms(to: .image) { self = .image }
        else if type.conforms(to: .pdf) { self = .pdf }
        else if type.conforms(to: .sourceCode) || type.conforms(to: .script) || type.conforms(to: .json)
            || type.conforms(to: .xml) { self = .code }
        else if type.conforms(to: .text) { self = .text }
        else if type.conforms(to: .archive) { self = .archive }
        else if type.conforms(to: .content) || type.conforms(to: .presentation)
            || type.conforms(to: .spreadsheet) { self = .document }
        else { self = .other }
    }

    var symbol: String {
        switch self {
        case .folder: "folder.fill"
        case .link: "arrow.turn.up.right"
        case .video: "film"
        case .audio: "waveform"
        case .image: "photo"
        case .pdf: "doc.richtext"
        case .text: "doc.text"
        case .code: "chevron.left.forwardslash.chevron.right"
        case .archive: "archivebox"
        case .document: "doc"
        case .other: "doc"
        }
    }

    var tint: Color {
        switch self {
        case .folder: .accentColor
        case .video: .purple
        case .audio: .pink
        case .image: .orange
        case .pdf: .red
        case .code: .teal
        case .archive: .brown
        default: .secondary
        }
    }
}

extension RemoteNode {
    var contentType: UTType? {
        let ext = (displayName as NSString).pathExtension
        return ext.isEmpty ? nil : UTType(filenameExtension: ext)
    }
}

struct NodeIcon: View {
    let node: RemoteNode
    var size: CGFloat = 30

    var body: some View {
        let kind = FileKind(node)
        Image(systemName: kind.symbol)
            .font(.system(size: size * 0.5, weight: .medium))
            .symbolRenderingMode(.hierarchical)
            .foregroundStyle(kind.tint)
            .frame(width: size, height: size)
            .background(kind.tint.opacity(kind == .folder ? 0.14 : 0.1),
                        in: RoundedRectangle(cornerRadius: size * 0.26, style: .continuous))
    }
}

// MARK: - Telemetry pieces

struct Card<Content: View>: View {
    var padding: CGFloat = 16
    @ViewBuilder var content: () -> Content

    var body: some View {
        VStack(alignment: .leading, spacing: 12, content: content)
            .padding(padding)
            .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
            .glassSurface(RoundedRectangle(cornerRadius: 22, style: .continuous))
    }
}

/// A small uppercase caption above a figure, used sparingly.
struct CardTitle: View {
    let text: String
    var symbol: String?

    var body: some View {
        HStack(spacing: 6) {
            if let symbol { Image(systemName: symbol) }
            Text(text.uppercased())
                .tracking(0.8)
        }
        .font(.caption2.weight(.semibold))
        .foregroundStyle(.secondary)
    }
}

struct Figure: View {
    let value: String
    var unit: String?
    var size: Font = .title2

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 3) {
            Text(value)
                .font(size.weight(.semibold))
                .monospacedDigit()
                .contentTransition(.numericText())
            if let unit {
                Text(unit).font(.callout).foregroundStyle(.secondary)
            }
        }
    }
}

enum Rate {
    /// Split a byte rate into a figure and unit, such as ("12.4", "MB/s").
    static func parts(_ bytesPerSecond: Double) -> (String, String) {
        let units = ["B/s", "KB/s", "MB/s", "GB/s"]
        var value = max(0, bytesPerSecond)
        var index = 0
        while value >= 1000, index < units.count - 1 {
            value /= 1000
            index += 1
        }
        let text = value >= 100 || index == 0 ? String(format: "%.0f", value) : String(format: "%.1f", value)
        return (text, units[index])
    }

    static func text(_ bytesPerSecond: Double) -> String {
        let (figure, unit) = parts(bytesPerSecond)
        return "\(figure) \(unit)"
    }

    static func megabits(_ bytesPerSecond: Double) -> String {
        String(format: "%.1f Mbit/s", bytesPerSecond * 8 / 1_000_000)
    }
}

struct Sparkline: View {
    let values: [Double]
    let tint: Color

    var body: some View {
        Chart(Array(values.enumerated()), id: \.offset) { index, value in
            LineMark(x: .value("t", index), y: .value("v", value))
                .foregroundStyle(tint)
                .interpolationMethod(.monotone)
        }
        .chartXAxis(.hidden)
        .chartYAxis(.hidden)
        .frame(height: 28)
        // A flat line at zero reads as a problem; show nothing instead.
        .opacity(values.count > 1 && values.contains { $0 > 0 } ? 1 : 0)
    }
}

enum Clock {
    static func duration(_ seconds: TimeInterval?) -> String {
        guard let seconds, seconds.isFinite else { return "—" }
        if seconds < 1 { return String(format: "%.0f ms", seconds * 1000) }
        if seconds < 60 { return String(format: "%.1f s", seconds) }
        let total = Int(seconds.rounded())
        return total >= 3600
            ? String(format: "%d:%02d:%02d", total / 3600, total / 60 % 60, total % 60)
            : String(format: "%d:%02d", total / 60, total % 60)
    }
}
