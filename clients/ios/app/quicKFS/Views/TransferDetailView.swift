// SPDX-License-Identifier: Apache-2.0
import Charts
import SwiftUI

/// Everything about one transfer: progress and speed, what the QUIC link did
/// while it ran, and how the quickfs protocol carried it.
struct TransferDetailView: View {
    let item: TransferItem
    let open: (URL) -> Void
    @State private var showActivity = false

    var body: some View {
        TimelineView(.periodic(from: .now, by: 1)) { _ in
            ScrollView {
                VStack(alignment: .leading, spacing: 16) {
                    header
                    speed
                    situation
                    chart
                    activity
                    linkSection
                    protocolSection
                }
                .padding(18)
            }
        }
        .navigationTitle(item.title)
        .navigationBarTitleDisplayMode(.inline)
        .safeAreaInset(edge: .bottom) { footer }
    }

    // MARK: Header and progress

    private var header: some View {
        HStack(alignment: .center, spacing: 12) {
            Image(systemName: item.direction == .download ? "arrow.down.circle.fill" : "arrow.up.circle.fill")
                .font(.system(size: 30))
                .foregroundStyle(tint)
                .symbolRenderingMode(.hierarchical)
            VStack(alignment: .leading, spacing: 3) {
                Text(item.title)
                    .font(.headline)
                    .lineLimit(2)
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
                .glassSurface(tint: tint.opacity(0.15))
        }
    }

    private var speed: some View {
        let status = item.status
        // Once finished, the headline is the average rather than a zero.
        let headline = item.isRunning ? item.currentRate : item.rate
        let (figure, unit) = Rate.parts(headline)
        return VStack(alignment: .leading, spacing: 10) {
            HStack(alignment: .firstTextBaseline) {
                Figure(value: figure, unit: unit, size: .largeTitle)
                Spacer()
                Text(status?.fraction ?? 0, format: .percent.precision(.fractionLength(1)))
                    .font(.title2.weight(.semibold))
                    .monospacedDigit()
            }
            Text(item.isRunning ? Rate.megabits(headline) : "\(Rate.megabits(headline)) average")
                .font(.callout)
                .foregroundStyle(.secondary)
                .monospacedDigit()
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
            LazyVGrid(columns: [GridItem(.flexible(), alignment: .leading), GridItem(.flexible(), alignment: .leading)],
                      alignment: .leading, spacing: 10) {
                stat("transferred", "\(Format.bytes(status?.bytes ?? 0)) of \(Format.bytes(status?.total ?? 0))")
                if (status?.filesTotal ?? 0) > 1 {
                    stat("files", "\(status?.files ?? 0) of \(status?.filesTotal ?? 0)")
                }
                stat("average", Rate.text(item.rate))
                stat("peak", Rate.text(item.peakRate))
                stat(item.isRunning ? "remaining" : "took", Clock.duration(item.isRunning ? item.remaining : item.elapsed))
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
                AxisValueLabel { if let rate = value.as(Double.self) { Text(Rate.text(rate)) } }
            }
        }
        .frame(height: 130)
        .overlay {
            if item.rates.count < 2 {
                Text("Measuring").font(.callout).foregroundStyle(.secondary)
            }
        }
    }

    // MARK: What is happening

    private var situation: some View {
        let (symbol, color, text, detail) = situationParts
        return HStack(alignment: .firstTextBaseline, spacing: 8) {
            Image(systemName: symbol)
                .foregroundStyle(color)
                .frame(width: 18)
            VStack(alignment: .leading, spacing: 2) {
                Text(text)
                    .font(.callout.weight(color == .secondary ? .regular : .medium))
                    .foregroundStyle(color == .secondary ? .secondary : .primary)
                if let detail {
                    Text(detail)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .textSelection(.enabled)
                }
            }
            Spacer(minLength: 0)
        }
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
            let rate = status.linkRate.map { " at \(Rate.text($0))" } ?? ""
            let across = (status.connections ?? 1) > 1 ? " over \(status.connections ?? 1) connections" : ""
            let small = status.adaptive != false && (chunk < 4 << 20 || depth < 8)
            return (small ? "tortoise" : "bolt.horizontal", .secondary,
                    "\(depth) × \(Self.binary(chunk)) in flight\(across)\(rate)",
                    small && (status.linkRate ?? .infinity) < 2_000_000
                        ? "Requests are kept small so each finishes in seconds on this link." : nil)
        }
    }

    /// The transfer's own log, collapsed until asked for.
    @ViewBuilder private var activity: some View {
        if let events = item.status?.events, !events.isEmpty {
            DisclosureGroup(isExpanded: $showActivity) {
                VStack(alignment: .leading, spacing: 4) {
                    ForEach(Array(events.enumerated()), id: \.offset) { _, event in
                        HStack(alignment: .firstTextBaseline, spacing: 8) {
                            Text(String(format: "%.1fs", Double(event.atMs) / 1000))
                                .foregroundStyle(.tertiary)
                                .frame(width: 48, alignment: .trailing)
                            Circle()
                                .fill(event.level == "error" ? .red : event.level == "warning" ? .orange : .secondary)
                                .frame(width: 5, height: 5)
                            Text(event.message)
                                .foregroundStyle(event.level == "info" ? .secondary : .primary)
                                .textSelection(.enabled)
                        }
                    }
                }
                .font(.system(size: 11, design: .monospaced))
                .padding(.top, 6)
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

    // MARK: Link and protocol

    private var linkSection: some View {
        let connections = item.status?.connections ?? 1
        return DetailBlock(title: connections > 1 ? "QUIC link 1 of \(connections)" : "QUIC link",
                           symbol: "antenna.radiowaves.left.and.right") {
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

    private var protocolSection: some View {
        DetailBlock(title: "quickfs protocol", symbol: "point.3.connected.trianglepath.dotted") {
            if let proto = item.link?.protocol {
                row("ALPN", proto.alpn)
                row("version", "\(ProtocolSnapshot.version(proto.clientVersion)) ↔ \(ProtocolSnapshot.version(proto.serverVersion))")
                row("compression", proto.compression ? "zstd ≥ 1 KiB" : "off")
                if let chunk = item.status?.chunkBytes, let depth = item.status?.pipelineDepth, depth > 0 {
                    row("pipeline", "\(depth) × \(Self.binary(chunk))\(item.status?.adaptive == false ? " fixed" : "")")
                }
                if let connections = item.status?.connections, connections > 1 {
                    row("connections", "\(connections)")
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
        HStack(spacing: 10) {
            Button("Copy Report", systemImage: "doc.on.doc") { UIPasteboard.general.string = item.summary }
                .labelStyle(.iconOnly)
                .glassButton()
            Spacer()
            if item.isRunning {
                Button("Cancel", systemImage: "xmark", role: .destructive) { item.cancel() }
                    .glassButton()
            } else if item.canResume {
                Button("Discard", systemImage: "trash", role: .destructive) { item.cancel() }
                    .glassButton()
                Button("Resume", systemImage: "arrow.clockwise") { item.resume() }
                    .glassButton(prominent: true)
                    .disabled(item.owner == nil)
            } else if let destination = item.destination, item.status?.state == .finished {
                ShareLink(item: destination).glassButton()
                Button("Open", systemImage: "eye") { open(destination) }
                    .glassButton(prominent: true)
            }
        }
        .controlSize(.large)
        .padding(.horizontal, 18)
        .padding(.bottom, 8)
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
}

/// A titled monospaced grid used for the link and protocol sections.
struct DetailBlock<Content: View>: View {
    let title: String
    let symbol: String
    @ViewBuilder var content: () -> Content

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Label(title.uppercased(), systemImage: symbol)
                .font(.system(size: 10, weight: .semibold, design: .monospaced))
                .tracking(1)
                .foregroundStyle(.secondary)
            Grid(alignment: .leading, horizontalSpacing: 14, verticalSpacing: 5) {
                content()
            }
            .font(.system(size: 12, design: .monospaced))
        }
        .padding(14)
        .frame(maxWidth: .infinity, alignment: .topLeading)
        .glassSurface(RoundedRectangle(cornerRadius: 16, style: .continuous))
    }
}
