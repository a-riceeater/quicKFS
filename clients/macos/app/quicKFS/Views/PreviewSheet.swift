// SPDX-License-Identifier: Apache-2.0
import Quartz
import SwiftUI

/// Quick Look for remote files: the whole file is fetched, then shown with
/// the system's own previewers. The raw bytes sit in an optional side panel.
struct PreviewSheet: View {
    let connection: ServerConnection
    @Environment(\.dismiss) private var dismiss
    @State private var loader: PreviewLoader
    @AppStorage("previewShowsHex") private var showsHex = false
    /// Room the parent window leaves for the sheet. Sheets open at their
    /// minimum size, so the size is set outright from this.
    @State private var room: CGSize

    private static let previewWidth: CGFloat = 960
    private static let hexWidth: CGFloat = 560

    init(node: RemoteNode, connection: ServerConnection) {
        self.connection = connection
        _loader = State(initialValue: PreviewLoader(node: node, connection: connection))
        let window = (NSApp.mainWindow ?? NSApp.keyWindow)?.frame.size ?? CGSize(width: 1280, height: 860)
        _room = State(initialValue: CGSize(width: max(640, window.width - 80), height: max(440, window.height - 110)))
    }

    private var width: CGFloat {
        min(room.width, Self.previewWidth + (showsHex ? Self.hexWidth : 0))
    }

    private var node: RemoteNode { loader.node }

    var body: some View {
        VStack(spacing: 0) {
            header
            Divider()
            HStack(spacing: 0) {
                content
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
                if showsHex {
                    Divider()
                    HexPanel(rows: loader.hexRows, failed: loader.headFailed, length: PreviewLoader.headLength)
                        .frame(width: min(Self.hexWidth, width * 0.6))
                        .transition(.move(edge: .trailing).combined(with: .opacity))
                }
            }
        }
        .frame(width: width, height: min(room.height, 760))
        .task { await loader.open() }
        .onDisappear { loader.close() }
    }

    private var header: some View {
        HStack(spacing: 10) {
            NodeIcon(node: node, size: 22)
            Text(node.displayName).font(.headline).lineLimit(1).truncationMode(.middle)
            Spacer()
            Text(summary).foregroundStyle(.secondary).monospacedDigit()
            Button("Bytes", systemImage: "number") {
                withAnimation(.snappy) { showsHex.toggle() }
            }
            .labelStyle(.iconOnly)
            .buttonStyle(.glass)
            .tint(showsHex ? .accentColor : nil)
            .help(showsHex ? "Hide bytes" : "Show bytes")
            Button("Close", systemImage: "xmark") { dismiss() }
                .labelStyle(.iconOnly)
                .buttonStyle(.glass)
                .keyboardShortcut(.cancelAction)
        }
        .padding(14)
    }

    /// Size, and once loaded, how long the fetch took.
    private var summary: String {
        if case .ready = loader.phase, let elapsed = loader.elapsed {
            return "\(Format.bytes(node.size)) · \(Self.duration(elapsed))"
        }
        return Format.bytes(node.size)
    }

    @ViewBuilder
    private var content: some View {
        switch loader.phase {
        case .loading:
            LoadingStatus(loader: loader)
        case .confirm:
            Placard(
                symbol: "arrow.down.circle", title: "Large file",
                detail: loader.fitsOnDisk
                    ? "Previewing downloads all \(Format.bytes(node.size)) first."
                    : "Not enough free space to preview \(Format.bytes(node.size))."
            ) {
                Button("Load Preview") { Task { await loader.loadAnyway() } }
                    .buttonStyle(.glassProminent)
                    .disabled(!loader.fitsOnDisk)
            }
        case .ready(let url):
            QuickLookView(url: url)
        case .failed(let message):
            Placard(symbol: "eye.slash", title: "No preview", detail: message) {
                Button("Try Again") { Task { await loader.loadAnyway() } }
                    .buttonStyle(.glass)
            }
        }
    }

    static func duration(_ seconds: TimeInterval) -> String {
        if seconds < 1 { return String(format: "%.0f ms", seconds * 1000) }
        if seconds < 60 { return String(format: "%.1f s", seconds) }
        return String(format: "%d:%02d", Int(seconds) / 60, Int(seconds) % 60)
    }
}

/// What the fetch is doing right now, so opening a file is never a blank wait.
private struct LoadingStatus: View {
    let loader: PreviewLoader

    var body: some View {
        VStack(spacing: 18) {
            NodeIcon(node: loader.node, size: 52)
            VStack(spacing: 8) {
                Group {
                    if let fraction = loader.fraction, (loader.status?.bytes ?? 0) > 0 {
                        ProgressView(value: fraction)
                    } else {
                        ProgressView().progressViewStyle(.linear)
                    }
                }
                HStack {
                    Text(amount)
                    Spacer(minLength: 12)
                    if loader.displayRate > 0 { Text("\(Format.bytes(UInt64(loader.displayRate)))/s") }
                }
                .font(.callout.monospacedDigit())
                .foregroundStyle(.secondary)
                .lineLimit(1)
                Text(detail)
                    .font(.caption.monospacedDigit())
                    .foregroundStyle(warning == nil ? .tertiary : .secondary)
                    .lineLimit(2)
                    .multilineTextAlignment(.center)
            }
            .frame(maxWidth: 340)
        }
        .padding(.horizontal, 28)
    }

    private var amount: String {
        guard let status = loader.status, status.bytes > 0 else { return "Opening" }
        return "\(Format.bytes(status.bytes)) of \(Format.bytes(max(status.total, loader.node.size)))"
    }

    /// A retry in progress, or the latest warning while no data is landing.
    private var warning: String? {
        guard let status = loader.status else { return nil }
        if let wait = status.retryInMs {
            return "Retrying in \(PreviewSheet.duration(Double(wait) / 1000))"
        }
        guard let event = status.events?.last, event.level != "info",
              let sinceCommit = status.sinceCommitMs, sinceCommit > 1_500
        else { return nil }
        return event.message
    }

    private var detail: String {
        if let warning { return warning }
        var parts: [String] = []
        if let remaining = loader.remaining, remaining >= 1 {
            parts.append("\(PreviewSheet.duration(remaining)) left")
        } else if let started = loader.started {
            parts.append(PreviewSheet.duration(Date.now.timeIntervalSince(started)))
        }
        if let connections = loader.status?.connections, connections > 1 {
            parts.append("\(connections) connections")
        }
        return parts.joined(separator: " · ")
    }
}

/// The file's first bytes as a hex dump beside the preview: offset, hex in
/// two groups of eight, and the printable text, one row per 16 bytes. Rows
/// never wrap; a narrow panel scrolls sideways instead.
private struct HexPanel: View {
    let rows: [HexRow]?
    let failed: Bool
    let length: UInt64

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            HStack(spacing: 0) {
                // Column widths in characters, matching each row's layout:
                // offset and bytes are each followed by two spaces.
                Text("Offset").frame(width: 10 * Self.advance, alignment: .leading)
                Text("Bytes").frame(width: 50 * Self.advance, alignment: .leading)
                Text("Text")
                Spacer(minLength: 0)
            }
            .font(.system(size: Self.size, weight: .semibold, design: .monospaced))
            .foregroundStyle(.secondary)
            .padding(.horizontal, 16)
            .padding(.vertical, 9)
            Divider()
            if let rows {
                if rows.isEmpty {
                    Placard(symbol: "doc", title: "Empty file")
                        .frame(maxWidth: .infinity, maxHeight: .infinity)
                } else {
                    ScrollView([.vertical, .horizontal]) {
                        Text(Self.dump(rows))
                            .font(.system(size: Self.size, design: .monospaced))
                            .lineSpacing(2)
                            .textSelection(.enabled)
                            .fixedSize()
                            .frame(maxWidth: .infinity, alignment: .leading)
                            .padding(.horizontal, 16)
                            .padding(.vertical, 12)
                    }
                    Divider()
                    Text("First \(Format.bytes(min(length, UInt64(rows.count) * 16)))")
                        .font(.caption)
                        .foregroundStyle(.tertiary)
                        .padding(.horizontal, 16)
                        .padding(.vertical, 7)
                }
            } else if failed {
                Placard(symbol: "number", title: "Unavailable")
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
            } else {
                ProgressView().frame(maxWidth: .infinity, maxHeight: .infinity)
            }
        }
        .background(.background.secondary)
    }

    private static let size: CGFloat = 11
    /// Width of one monospaced character at `size`.
    private static let advance: CGFloat = {
        let font = NSFont.monospacedSystemFont(ofSize: size, weight: .regular)
        return ("0" as NSString).size(withAttributes: [.font: font]).width
    }()

    /// One selectable block, with offsets and text dimmer than the bytes.
    private static func dump(_ rows: [HexRow]) -> AttributedString {
        var result = AttributedString()
        let dim = Color(nsColor: .tertiaryLabelColor)
        let soft = Color(nsColor: .secondaryLabelColor)
        for (index, row) in rows.enumerated() {
            var offset = AttributedString(row.offset + "  ")
            offset.foregroundColor = dim
            var hex = AttributedString(row.hex + "  ")
            hex.foregroundColor = Color(nsColor: .labelColor)
            var text = AttributedString(row.text + (index == rows.count - 1 ? "" : "\n"))
            text.foregroundColor = soft
            result += offset
            result += hex
            result += text
        }
        return result
    }
}

/// The system Quick Look view for a local file.
private struct QuickLookView: NSViewRepresentable {
    let url: URL

    /// `QLPreviewView` can fail to initialise; an empty view stands in.
    func makeNSView(context: Context) -> NSView {
        guard let view = QLPreviewView(frame: .zero, style: .normal) else { return NSView() }
        view.autostarts = true
        view.previewItem = url as NSURL
        return view
    }

    func updateNSView(_ view: NSView, context: Context) {
        guard let view = view as? QLPreviewView else { return }
        if (view.previewItem as? NSURL) as URL? != url { view.previewItem = url as NSURL }
    }

    static func dismantleNSView(_ view: NSView, coordinator: ()) {
        (view as? QLPreviewView)?.close()
    }
}
