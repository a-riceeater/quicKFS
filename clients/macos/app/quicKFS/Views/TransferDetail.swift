// SPDX-License-Identifier: Apache-2.0
import AppKit
import Charts
import SwiftUI

/// Everything about one transfer: progress and speed, what the QUIC link did
/// while it ran, and how the quickfs protocol carried it.
struct TransferDetail: View {
    let item: TransferItem
    @State private var showActivity = false

    var body: some View {
        TimelineView(.periodic(from: .now, by: 1)) { _ in
            VStack(alignment: .leading, spacing: 14) {
                header
                speed
                situation
                chart
                activity
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

    // MARK: What is happening

    /// One line, always present so the window never changes height on its
    /// own: what the pipeline is doing, or what went wrong and what happens next.
    private var situation: some View {
        let (symbol, color, text, detail) = situationParts
        return HStack(alignment: .firstTextBaseline, spacing: 8) {
            Image(systemName: symbol)
                .foregroundStyle(color)
                .frame(width: 16)
            VStack(alignment: .leading, spacing: 2) {
                Text(text)
                    .font(.callout.weight(color == .secondary ? .regular : .medium))
                    .foregroundStyle(color == .secondary ? .secondary : .primary)
                    .lineLimit(2)
                if let detail {
                    Text(detail)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(3)
                        .textSelection(.enabled)
                }
            }
            Spacer(minLength: 0)
        }
        .frame(maxWidth: .infinity, minHeight: 34, alignment: .topLeading)
    }

    private var situationParts: (String, Color, String, String?) {
        guard let status = item.status else {
            return ("ellipsis", .secondary, "Starting", nil)
        }
        let stall = Double(status.stallTimeoutMs ?? 30_000) / 1000
        let window = Double(status.retryWindowMs ?? 300_000) / 1000
        let lastIssue = status.events?.last { $0.level != "info" }?.message
        switch status.state {
        case .failed:
            return (item.canResume ? "pause.circle.fill" : "xmark.octagon.fill", item.canResume ? .orange : .red,
                    status.reason ?? status.error?.message ?? "Failed", status.hint)
        case .cancelled:
            return ("xmark.circle", .secondary, "Cancelled", nil)
        case .finished:
            // Retries and reconnects alike: every warning in the log was
            // something the link did that the transfer rode out.
            let interruptions = status.events?.filter { $0.level == "warning" }.count ?? Int(status.retries ?? 0)
            return ("checkmark.circle", .secondary,
                    interruptions == 0 ? "Finished without interruptions"
                        : "Finished after riding out \(interruptions) \(interruptions == 1 ? "interruption" : "interruptions")",
                    nil)
        case .running:
            if let wait = status.retryInMs {
                return ("arrow.triangle.2.circlepath", .orange,
                        "Retrying in \(Int((Double(wait) / 1000).rounded(.up))) s, continuing from \(Format.bytes(status.bytes))",
                        lastIssue)
            }
            let idle = Double(status.sinceCommitMs ?? 0) / 1000
            if idle >= max(5, stall / 2) {
                return ("hourglass", .orange,
                        "Nothing has landed for \(Int(idle)) s",
                        "A request fails after \(Int(stall)) s with no data moving, then retries for up to \(Self.span(window)).")
            }
            guard let depth = status.pipelineDepth, let chunk = status.chunkBytes, depth > 0 else {
                return ("ellipsis", .secondary, "Starting", nil)
            }
            let rate = status.linkRate.map { " at \(Rate.parts($0).0) \(Rate.parts($0).1)" } ?? ""
            let small = status.adaptive != false && (chunk < 4 << 20 || depth < 8)
            return (small ? "tortoise" : "bolt.horizontal", .secondary,
                    "\(depth) × \(Self.binary(chunk)) in flight\(rate)",
                    small && (status.linkRate ?? .infinity) < 2_000_000
                        ? "Requests are kept small so each finishes in seconds on this link." : nil)
        }
    }

    /// The transfer's own log, collapsed until asked for.
    @ViewBuilder private var activity: some View {
        if let events = item.status?.events, !events.isEmpty {
            DisclosureGroup(isExpanded: $showActivity) {
                ScrollViewReader { reader in
                    ScrollView {
                        LazyVStack(alignment: .leading, spacing: 3) {
                            ForEach(Array(events.enumerated()), id: \.offset) { index, event in
                                HStack(alignment: .firstTextBaseline, spacing: 8) {
                                    Text(String(format: "%.1fs", Double(event.atMs) / 1000))
                                        .foregroundStyle(.tertiary)
                                        .frame(width: 52, alignment: .trailing)
                                    Circle()
                                        .fill(event.level == "error" ? .red : event.level == "warning" ? .orange : .secondary)
                                        .frame(width: 5, height: 5)
                                    Text(event.message)
                                        .foregroundStyle(event.level == "info" ? .secondary : .primary)
                                        .textSelection(.enabled)
                                }
                                .id(index)
                            }
                        }
                        .font(.system(size: 11, design: .monospaced))
                        .padding(.vertical, 4)
                    }
                    .frame(height: 110)
                    .onChange(of: events.count) { reader.scrollTo(events.count - 1, anchor: .bottom) }
                    .onAppear { reader.scrollTo(events.count - 1, anchor: .bottom) }
                }
            } label: {
                let issues = events.filter { $0.level != "info" }.count
                HStack(spacing: 6) {
                    Text("Activity").font(.callout)
                    Text("\(events.count)").font(.caption.monospacedDigit()).foregroundStyle(.secondary)
                    if issues > 0 {
                        Text("\(issues) \(issues == 1 ? "issue" : "issues")")
                            .font(.caption.weight(.semibold))
                            .foregroundStyle(.orange)
                    }
                }
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
                if let chunk = item.status?.chunkBytes, let depth = item.status?.pipelineDepth, depth > 0 {
                    row("pipeline", "\(depth) × \(Self.binary(chunk))\(item.status?.adaptive == false ? " fixed" : "")")
                }
                if let retries = item.status?.retries, retries > 0 {
                    row("retries", "\(retries)", alert: true)
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
            } else if item.canResume {
                Button("Discard", systemImage: "trash", role: .destructive) { item.cancel() }
                    .buttonStyle(.glass)
                    .help("Delete the partial data and stop for good")
                Button("Resume", systemImage: "arrow.clockwise") { item.resume() }
                    .buttonStyle(.glassProminent)
                    .disabled(item.owner == nil)
                    .help(item.owner == nil ? "Reconnect to the server first" : "Continue from where it stopped")
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
        case .failed: item.canResume ? .orange : .red
        case .cancelled: .secondary
        case .finished: .green
        default: item.direction == .download ? .accentColor : .orange
        }
    }

    private var stateLabel: String {
        switch item.status?.state {
        case .finished: "DONE"
        case .failed: item.canResume ? "STOPPED" : "FAILED"
        case .cancelled: "CANCELLED"
        default:
            item.status?.retryInMs != nil ? "RETRYING"
                : item.direction == .download ? "DOWNLOADING" : "UPLOADING"
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

    /// A setting-sized span in words ("90 s", "5 min"), not a clock reading.
    private static func span(_ seconds: Double) -> String {
        seconds < 120 ? "\(Int(seconds)) s" : "\(Int((seconds / 60).rounded())) min"
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

    /// Resolved up front so the window's first layout is its final size;
    /// growing after placement is what pushed it off screen.
    private var item: TransferItem? { transfer.flatMap { TransferItem.all[$0] } }

    var body: some View {
        ZStack {
            if let item {
                TransferDetail(item: item)
                    .padding(.top, 10)
            } else {
                Placard(symbol: "arrow.up.arrow.down.circle", title: "Transfer unavailable")
                    .frame(width: 600, height: 220)
            }
        }
        .containerBackground(.regularMaterial, for: .window)
        .background(WindowLevel())
    }
}

/// Keep transfer windows above the main window while they are open.
private struct WindowLevel: NSViewRepresentable {
    func makeNSView(context: Context) -> NSView {
        let view = NSView()
        DispatchQueue.main.async {
            guard let window = view.window else { return }
            window.level = .floating
            window.isMovableByWindowBackground = true
            // Keep the whole window on screen, whatever size it settled at.
            if let screen = window.screen ?? NSScreen.main {
                let visible = screen.visibleFrame
                if !visible.contains(window.frame) {
                    window.setFrame(window.constrainFrameRect(window.frame, to: screen), display: true)
                    if !visible.contains(window.frame) { window.center() }
                }
            }
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
