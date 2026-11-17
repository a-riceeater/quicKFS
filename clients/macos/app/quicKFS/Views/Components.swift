// SPDX-License-Identifier: Apache-2.0
import SwiftUI

extension UUID: @retroactive Identifiable {
    public var id: UUID { self }
}

/// The single visual vocabulary for a server's condition.
enum ServerCondition: Equatable {
    case idle, connecting, online, mounting, mounted, failed

    var tint: Color {
        switch self {
        case .idle: .secondary
        case .connecting, .mounting: .orange
        case .online: .accentColor
        case .mounted: .green
        case .failed: .red
        }
    }

    var symbol: String {
        switch self {
        case .mounted: "externaldrive.fill.badge.checkmark"
        case .mounting: "externaldrive.badge.timemachine"
        case .failed: "externaldrive.badge.exclamationmark"
        case .online: "externaldrive.fill.badge.wifi"
        case .idle, .connecting: "externaldrive.badge.wifi"
        }
    }

    var help: String {
        switch self {
        case .idle: "Not connected"
        case .connecting: "Connecting"
        case .online: "Connected"
        case .mounting: "Mounting"
        case .mounted: "Mounted"
        case .failed: "Needs attention"
        }
    }
}

extension ServerState {
    var condition: ServerCondition {
        switch mount.phase {
        case .mounted: return .mounted
        case .starting, .stopping: return .mounting
        case .failed: return .failed
        case .idle: break
        }
        switch connection.phase {
        case .online: return .online
        case .connecting: return .connecting
        case .failed: return .failed
        case .offline: return .idle
        }
    }

    /// The most relevant failure to surface, mount first.
    var failure: String? {
        if case .failed(let message) = mount.phase { return message }
        if case .failed(let error) = connection.phase { return error.message }
        return nil
    }
}

struct StatusDot: View {
    let condition: ServerCondition

    var body: some View {
        Circle()
            .fill(condition.tint.gradient)
            .frame(width: 8, height: 8)
            .shadow(color: condition.tint.opacity(0.6), radius: condition == .mounted ? 4 : 0)
            .help(condition.help)
            .accessibilityLabel(condition.help)
    }
}

/// A rounded tile carrying an SF Symbol, used for server identity.
struct ServerGlyph: View {
    let condition: ServerCondition
    var size: CGFloat = 28

    var body: some View {
        Image(systemName: condition.symbol)
            .font(.system(size: size * 0.5, weight: .semibold))
            .symbolRenderingMode(.hierarchical)
            .foregroundStyle(condition == .idle ? Color.secondary : condition.tint)
            .contentTransition(.symbolEffect(.replace))
            .frame(width: size, height: size)
            .glassEffect(.regular.tint(condition.tint.opacity(condition == .idle ? 0 : 0.18)),
                         in: .rect(cornerRadius: size * 0.3))
    }
}

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
