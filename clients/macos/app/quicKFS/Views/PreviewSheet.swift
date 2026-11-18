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

    init(node: RemoteNode, connection: ServerConnection) {
        self.connection = connection
        _loader = State(initialValue: PreviewLoader(node: node, connection: connection))
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
                    HexPanel(dump: loader.hexDump, failed: loader.headFailed, length: PreviewLoader.headLength)
                        .frame(width: 470)
                        .transition(.move(edge: .trailing).combined(with: .opacity))
                }
            }
        }
        .frame(minWidth: 760, idealWidth: 980, minHeight: 480, idealHeight: 680)
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
                .frame(width: 320)
                HStack {
                    Text(amount)
                    Spacer()
                    if loader.rate > 0 { Text("\(Format.bytes(UInt64(loader.rate)))/s") }
                }
                .font(.callout.monospacedDigit())
                .foregroundStyle(.secondary)
                .frame(width: 320)
                Text(detail)
                    .font(.caption)
                    .foregroundStyle(warning == nil ? .tertiary : .secondary)
                    .lineLimit(2)
                    .multilineTextAlignment(.center)
                    .frame(width: 320)
            }
        }
        .padding()
    }

    private var amount: String {
        guard let status = loader.status, status.bytes > 0 else { return "Opening" }
        return "\(Format.bytes(status.bytes)) of \(Format.bytes(max(status.total, loader.node.size)))"
    }

    /// The latest retry or warning from the transfer, if it is still current.
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
        }
        if let connections = loader.status?.connections, connections > 1 {
            parts.append("\(connections) connections")
        }
        if parts.isEmpty, let started = loader.started {
            parts.append("\(PreviewSheet.duration(Date.now.timeIntervalSince(started)))")
        }
        return parts.joined(separator: " · ")
    }
}

/// The file's first bytes as a hex dump, beside the preview.
private struct HexPanel: View {
    let dump: String?
    let failed: Bool
    let length: UInt64

    var body: some View {
        VStack(alignment: .leading, spacing: 0) {
            Text("First \(Format.bytes(length))")
                .font(.caption.weight(.semibold))
                .foregroundStyle(.secondary)
                .padding(.horizontal, 14)
                .padding(.vertical, 8)
            Divider()
            if let dump {
                ScrollView {
                    Text(dump.isEmpty ? "Empty file" : dump)
                        .font(.system(size: 10.5, design: .monospaced))
                        .textSelection(.enabled)
                        .frame(maxWidth: .infinity, alignment: .leading)
                        .padding(14)
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
