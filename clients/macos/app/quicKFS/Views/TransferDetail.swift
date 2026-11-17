// SPDX-License-Identifier: Apache-2.0
import AppKit
import Charts
import SwiftUI

/// Everything about one transfer: progress and speed, what the QUIC link did
/// while it ran, and how the quickfs protocol carried it.
struct TransferDetail: View {
    let item: TransferItem

    var body: some View {
        TimelineView(.periodic(from: .now, by: 1)) { _ in
            VStack(alignment: .leading, spacing: 14) {
                header
                speed
                chart
                Divider()
                HStack(alignment: .top, spacing: 18) {
                    linkSection
                    protocolSection
                }
                footer
            }
            .padding(18)
            .frame(width: 600)
        }
    }

    // MARK: Header and progress

    private var header: some View {
        HStack(alignment: .top, spacing: 12) {
            Image(systemName: item.direction == .download ? "arrow.down.circle.fill" : "arrow.up.circle.fill")
                .font(.system(size: 30))
                .foregroundStyle(tint)
                .symbolRenderingMode(.hierarchical)
            VStack(alignment: .leading, spacing: 3) {
                Text(item.title)
                    .font(.headline)
                    .lineLimit(1)
                    .truncationMode(.middle)
                if let current = item.status?.current, !current.isEmpty, item.isRunning {
                    Text(current)
                        .font(.caption.monospaced())
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                        .truncationMode(.head)
                }
            }
            Spacer()
            Text(stateLabel)
                .font(.caption2.weight(.bold).monospaced())
                .foregroundStyle(tint)
                .padding(.horizontal, 8)
                .padding(.vertical, 3)
                .glassEffect(.regular.tint(tint.opacity(0.15)), in: .capsule)
        }
    }

    private var speed: some View {
        let status = item.status
        // Once finished, the headline is the average rather than a zero.
        let headline = item.isRunning ? item.currentRate : item.rate
        let (figure, unit) = Rate.parts(headline)
        return VStack(alignment: .leading, spacing: 8) {
            HStack(alignment: .firstTextBaseline) {
                Figure(value: figure, unit: unit, size: .largeTitle)
                Text(item.isRunning ? Rate.megabits(headline) : "\(Rate.megabits(headline)) average")
                    .font(.callout)
                    .foregroundStyle(.secondary)
                    .monospacedDigit()
                Spacer()
                Text(status?.fraction ?? 0, format: .percent.precision(.fractionLength(1)))
                    .font(.title2.weight(.semibold))
                    .monospacedDigit()
            }
            GeometryReader { geometry in
                ZStack(alignment: .leading) {
                    Capsule().fill(.quaternary)
                    Capsule()
                        .fill(tint.gradient)
                        .frame(width: max(6, geometry.size.width * (status?.fraction ?? 0)))
                }
            }
            .frame(height: 6)
            .animation(.smooth, value: status?.fraction)
            HStack {
                stat("transferred", "\(Format.bytes(status?.bytes ?? 0)) of \(Format.bytes(status?.total ?? 0))")
                Spacer()
                if (status?.filesTotal ?? 0) > 1 {
                    stat("files", "\(status?.files ?? 0) of \(status?.filesTotal ?? 0)")
                    Spacer()
                }
                stat("average", "\(Rate.parts(item.rate).0) \(Rate.parts(item.rate).1)")
                Spacer()
                stat("peak", "\(Rate.parts(item.peakRate).0) \(Rate.parts(item.peakRate).1)")
                Spacer()
                stat(item.isRunning ? "remaining" : "took", item.isRunning ? duration(item.remaining) : duration(item.elapsed))
            }
        }
    }

    private var chart: some View {
        Chart(item.rates) { sample in
            AreaMark(x: .value("Seconds", sample.elapsed), y: .value("Rate", sample.rate))
                .foregroundStyle(.linearGradient(colors: [tint.opacity(0.35), .clear], startPoint: .top, endPoint: .bottom))
                .interpolationMethod(.monotone)
            LineMark(x: .value("Seconds", sample.elapsed), y: .value("Rate", sample.rate))
                .foregroundStyle(tint)
                .interpolationMethod(.monotone)
        }
        .chartXAxis {
            AxisMarks(values: .automatic(desiredCount: 4)) { value in
                AxisGridLine().foregroundStyle(.quaternary)
                AxisValueLabel { if let seconds = value.as(Double.self) { Text("\(Int(seconds))s") } }
            }
        }
        .chartYAxis {
            AxisMarks(position: .trailing, values: .automatic(desiredCount: 3)) { value in
                AxisGridLine().foregroundStyle(.quaternary)
                AxisValueLabel { if let rate = value.as(Double.self) { Text("\(Rate.parts(rate).0) \(Rate.parts(rate).1)") } }
            }
        }
        .frame(height: 110)
        .overlay {
            if item.rates.count < 2 {
                Text("Measuring").font(.callout).foregroundStyle(.secondary)
            }
        }
    }

    // MARK: Link

    private var linkSection: some View {
        DetailBlock(title: "QUIC link", symbol: "antenna.radiowaves.left.and.right") {
            if let link = item.link {
                let sent = item.delta(\.packetsSent)
                let lost = item.delta(\.packetsLost)
                let wire = item.delta(\.bytesReceived) + item.delta(\.bytesSent)
                let data = item.protocolDelta(\.dataBytesReceived) + item.protocolDelta(\.dataBytesSent)
                row("round trip", String(format: "%.2f ms", Double(link.rttUs) / 1000))
                row("packets sent", sent.formatted())
                row("packets lost", lost == 0 ? "0" : String(format: "%llu (%.2f%%)", lost, Double(lost) / Double(max(1, sent)) * 100),
                    alert: lost > 0)
                row("congestion events", "\(item.delta(\.congestionEvents))", alert: item.delta(\.congestionEvents) > 0)
                row(item.direction == .download ? "our send window" : "send window", Format.bytes(link.congestionWindow))
                row("path MTU", "\(link.mtu) B")
                row("on the wire", Format.bytes(wire))
                if data > 0, wire > data {
                    row("overhead", String(format: "%.1f%%", (Double(wire) / Double(data) - 1) * 100))
                }
                if link.reconnects > (item.linkStart?.reconnects ?? link.reconnects) {
                    row("reconnects", "\(link.reconnects - (item.linkStart?.reconnects ?? 0))", alert: true)
                }
            } else {
                Text("waiting for link counters").foregroundStyle(.tertiary)
            }
        }
    }

    // MARK: Protocol

    private var protocolSection: some View {
        DetailBlock(title: "quickfs protocol", symbol: "point.3.connected.trianglepath.dotted") {
            if let proto = item.link?.protocol {
                row("ALPN", proto.alpn)
                row("version", "\(ProtocolSnapshot.version(proto.clientVersion)) ↔ \(ProtocolSnapshot.version(proto.serverVersion))")
                row("compression", proto.compression ? "zstd ≥ 1 KiB" : "off")
                if let chunk = item.status?.chunkBytes, let depth = item.status?.pipelineDepth {
                    row("pipeline", "\(Self.binary(chunk)) × \(depth)")
                }
                row("request streams", item.protocolDelta(\.streams).formatted())
                row("frames out / in", "\(item.protocolDelta(\.framesSent).formatted()) / \(item.protocolDelta(\.framesReceived).formatted())")
                let compressed = item.protocolDelta(\.compressedFramesSent) + item.protocolDelta(\.compressedFramesReceived)
                if compressed > 0 { row("compressed frames", compressed.formatted()) }
                let control = item.protocolDelta(\.frameBytesSent) + item.protocolDelta(\.frameBytesReceived)
                row("control bytes", Format.bytes(control))
                row(item.direction == .download ? "file data in" : "file data out",
                    Format.bytes(item.direction == .download ? item.protocolDelta(\.dataBytesReceived) : item.protocolDelta(\.dataBytesSent)))
                // Savings for this transfer's frames only, not the whole session.
                let payload = item.protocolDelta(\.payloadBytesSent) + item.protocolDelta(\.payloadBytesReceived)
                if payload > control, control > 0 {
                    row("saved by zstd", String(format: "%.0f%% of control", (1 - Double(control) / Double(payload)) * 100))
                }
            } else {
                Text("not negotiated yet").foregroundStyle(.tertiary)
            }
        }
    }

    // MARK: Footer

    private var footer: some View {
        HStack {
            Button("Copy Report", systemImage: "doc.on.doc") {
                NSPasteboard.general.clearContents()
                NSPasteboard.general.setString(item.summary, forType: .string)
            }
            Spacer()
            if item.isRunning {
                Button("Cancel", systemImage: "xmark", role: .destructive) { item.cancel() }
                    .buttonStyle(.glass)
            } else if let destination = item.destination, item.status?.state == .finished {
                Button("Show in Finder", systemImage: "magnifyingglass") {
                    NSWorkspace.shared.activateFileViewerSelecting([destination])
                }
                .buttonStyle(.glassProminent)
            }
        }
    }

    // MARK: Helpers

    private var tint: Color {
        switch item.status?.state {
        case .failed: .red
        case .cancelled: .secondary
        case .finished: .green
        default: item.direction == .download ? .accentColor : .orange
        }
    }

    private var stateLabel: String {
        switch item.status?.state {
        case .finished: "DONE"
        case .failed: "FAILED"
        case .cancelled: "CANCELLED"
        default: item.direction == .download ? "DOWNLOADING" : "UPLOADING"
        }
    }

    private func stat(_ label: String, _ value: String) -> some View {
        VStack(alignment: .leading, spacing: 1) {
            Text(label.uppercased())
                .font(.system(size: 9, weight: .semibold))
                .tracking(0.8)
                .foregroundStyle(.secondary)
            Text(value).font(.callout).monospacedDigit()
        }
    }

    private func row(_ label: String, _ value: String, alert: Bool = false) -> some View {
        GridRow {
            Text(label).foregroundStyle(.secondary)
            Text(value)
                .foregroundStyle(alert ? .orange : .primary)
                .textSelection(.enabled)
        }
    }

    /// Request sizes are powers of two, so show them as such ("4 MiB").
    private static func binary(_ bytes: UInt64) -> String {
        bytes >= 1 << 20 && bytes % (1 << 20) == 0 ? "\(bytes >> 20) MiB"
            : bytes >= 1 << 10 && bytes % (1 << 10) == 0 ? "\(bytes >> 10) KiB" : "\(bytes) B"
    }

    private func duration(_ seconds: TimeInterval?) -> String {
        guard let seconds, seconds.isFinite else { return "—" }
        let total = Int(seconds.rounded())
        return total >= 3600
            ? String(format: "%d:%02d:%02d", total / 3600, total / 60 % 60, total % 60)
            : String(format: "%d:%02d", total / 60, total % 60)
    }
}

/// The window that hosts a transfer's details. It floats above the main
/// window, is dragged by its background, and keeps the last state after the
/// transfer ends.
struct TransferWindow: View {
    static let id = "transfer"
    let transfer: UInt64?
    @Environment(AppModel.self) private var model
    @State private var item: TransferItem?

    var body: some View {
        ZStack {
            if let item {
                TransferDetail(item: item)
                    .padding(.top, 10)
            } else {
                Placard(symbol: "arrow.up.arrow.down.circle", title: "Transfer cleared")
                    .frame(width: 360, height: 220)
            }
        }
        .containerBackground(.regularMaterial, for: .window)
        .onAppear {
            // Hold the item so the window survives the tray being cleared.
            if item == nil, let transfer { item = model.transfer(transfer) }
        }
        .background(WindowLevel())
    }
}

/// Keep transfer windows above the main window while they are open.
private struct WindowLevel: NSViewRepresentable {
    func makeNSView(context: Context) -> NSView {
        let view = NSView()
        DispatchQueue.main.async {
            view.window?.level = .floating
            view.window?.isMovableByWindowBackground = true
        }
        return view
    }

    func updateNSView(_ nsView: NSView, context: Context) {}
}

/// A titled monospaced grid used for the link and protocol columns.
private struct DetailBlock<Content: View>: View {
    let title: String
    let symbol: String
    @ViewBuilder var content: () -> Content

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Label(title.uppercased(), systemImage: symbol)
                .font(.system(size: 10, weight: .semibold, design: .monospaced))
                .tracking(1)
                .foregroundStyle(.secondary)
            Grid(alignment: .leading, horizontalSpacing: 12, verticalSpacing: 4) {
                content()
            }
            .font(.system(size: 11, design: .monospaced))
        }
        .padding(12)
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
        .glassEffect(.regular, in: .rect(cornerRadius: 14))
    }
}
