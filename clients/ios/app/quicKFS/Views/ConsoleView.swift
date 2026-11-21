// SPDX-License-Identifier: Apache-2.0
import SwiftUI

/// The advanced view: live link readouts, the full event stream, and raw
/// QUIC and protocol counters.
struct ConsoleView: View {
    let profile: ServerProfile
    let state: ServerState
    @State private var filter: Filter = .all
    @State private var query = ""
    @State private var follow = true
    @State private var showDetails = false

    enum Filter: String, CaseIterable, Identifiable {
        case all, app, link, problems
        var id: Self { self }
        var title: String { rawValue.capitalized }
    }

    private var lines: [ConsoleLine] {
        state.log.lines.filter { line in
            let sourceMatches: Bool =
                switch filter {
                case .all: true
                case .app: line.source == .app
                case .link: line.source == .link
                case .problems: line.level >= .warning
                }
            return sourceMatches && (query.isEmpty || line.text.localizedCaseInsensitiveContains(query))
        }
    }

    var body: some View {
        VStack(spacing: 12) {
            Readouts(telemetry: state.telemetry)
            Picker("Filter", selection: $filter) {
                ForEach(Filter.allCases) { Text($0.title).tag($0) }
            }
            .pickerStyle(.segmented)
            Terminal(lines: lines, follow: follow)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
            if showDetails {
                ScrollView(.horizontal, showsIndicators: false) {
                    HStack(alignment: .top, spacing: 12) {
                        Counters(telemetry: state.telemetry)
                        ProtocolPanel(telemetry: state.telemetry)
                    }
                }
                .transition(.move(edge: .bottom).combined(with: .opacity))
            }
        }
        .padding(16)
        .searchable(text: $query, prompt: "Filter lines")
        .animation(.smooth, value: showDetails)
        .toolbar {
            ToolbarItem(placement: .topBarLeading) {
                Menu("More", systemImage: "ellipsis.circle") {
                    Toggle("Follow New Lines", systemImage: "arrow.down.to.line", isOn: $follow)
                    Toggle("Counters", systemImage: "rectangle.bottomhalf.filled", isOn: $showDetails)
                    Button("Copy Visible Lines", systemImage: "doc.on.doc") {
                        UIPasteboard.general.string = state.log.export(lines)
                    }
                    ShareLink(item: state.log.export(state.log.lines),
                              preview: SharePreview("quicKFS \(profile.name) log")) {
                        Label("Export Log", systemImage: "square.and.arrow.up")
                    }
                    Button("Clear", systemImage: "trash", role: .destructive) { state.log.clear() }
                }
            }
        }
        .task { await state.telemetry.watch() }
    }
}

/// The live strip across the top: one tile per figure, each with a trace.
private struct Readouts: View {
    let telemetry: Telemetry

    var body: some View {
        GlassGroup(spacing: 8) {
            LazyVGrid(columns: Array(repeating: GridItem(.flexible(minimum: 60), spacing: 8), count: 3), spacing: 8) {
                tile("RTT", String(format: "%.2f", telemetry.current?.rttMs ?? 0), "ms",
                     trace: telemetry.samples.map(\.rttMs), tint: .accentColor)
                tile("RX", Rate.parts(telemetry.current?.receiveRate ?? 0).0, Rate.parts(telemetry.current?.receiveRate ?? 0).1,
                     trace: telemetry.samples.map(\.receiveRate), tint: .accentColor)
                tile("TX", Rate.parts(telemetry.current?.sendRate ?? 0).0, Rate.parts(telemetry.current?.sendRate ?? 0).1,
                     trace: telemetry.samples.map(\.sendRate), tint: .orange)
                tile("LOSS", String(format: "%.2f", telemetry.lossPercent), "%",
                     trace: telemetry.samples.map(\.lossFraction), tint: .red)
                tile("CWND", Format.bytes(telemetry.latest?.congestionWindow ?? 0), nil)
                tile("MTU", "\(telemetry.latest?.mtu ?? 0)", "B")
            }
        }
    }

    private func tile(_ label: String, _ value: String, _ unit: String?, trace: [Double] = [],
                      tint: Color = .secondary) -> some View {
        VStack(alignment: .leading, spacing: 3) {
            Text(label)
                .font(.system(size: 9, weight: .semibold, design: .monospaced))
                .tracking(1.2)
                .foregroundStyle(.secondary)
            HStack(alignment: .firstTextBaseline, spacing: 2) {
                Text(value)
                    .font(.system(.callout, design: .monospaced).weight(.semibold))
                    .contentTransition(.numericText())
                if let unit {
                    Text(unit).font(.system(size: 10, design: .monospaced)).foregroundStyle(.secondary)
                }
            }
            .lineLimit(1)
            .minimumScaleFactor(0.6)
            Sparkline(values: Array(trace.suffix(40)), tint: tint)
                .frame(height: 12)
                .opacity(trace.isEmpty ? 0 : 1)
        }
        .padding(.horizontal, 10)
        .padding(.vertical, 7)
        .frame(maxWidth: .infinity, minHeight: 60, alignment: .leading)
        .glassSurface(RoundedRectangle(cornerRadius: 12, style: .continuous))
    }
}

private struct Terminal: View {
    let lines: [ConsoleLine]
    let follow: Bool

    var body: some View {
        ScrollViewReader { reader in
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 2) {
                    ForEach(lines) { line in
                        ConsoleRow(line: line).id(line.id)
                    }
                }
                .padding(12)
                .frame(maxWidth: .infinity, alignment: .leading)
            }
            .background(.background.secondary, in: RoundedRectangle(cornerRadius: 14, style: .continuous))
            .overlay {
                if lines.isEmpty { Text("No lines").font(.callout).foregroundStyle(.tertiary) }
            }
            .onChange(of: lines.last?.id) {
                if follow, let last = lines.last?.id { reader.scrollTo(last, anchor: .bottom) }
            }
            .onAppear {
                if let last = lines.last?.id { reader.scrollTo(last, anchor: .bottom) }
            }
        }
    }
}

private struct ConsoleRow: View {
    let line: ConsoleLine

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 6) {
            Text(line.date.formatted(.dateTime.hour(.twoDigits(amPM: .omitted)).minute().second()))
                .foregroundStyle(.tertiary)
            Text(line.level.tag)
                .foregroundStyle(color)
                .fontWeight(.semibold)
            Text(line.text)
                .foregroundStyle(line.level == .debug ? .secondary : .primary)
                .textSelection(.enabled)
        }
        .font(.system(size: 11, design: .monospaced))
    }

    private var color: Color {
        switch line.level {
        case .error: .red
        case .warning: .orange
        case .notice: .green
        case .info: .accentColor
        case .debug: .secondary
        }
    }
}

private struct Counters: View {
    let telemetry: Telemetry

    var body: some View {
        DetailBlock(title: "QUIC", symbol: "antenna.radiowaves.left.and.right") {
            if let link = telemetry.latest {
                row("bytes rx / tx", "\(Format.bytes(link.bytesReceived)) / \(Format.bytes(link.bytesSent))")
                row("datagrams rx / tx", "\(link.datagramsReceived) / \(link.datagramsSent)")
                row("packets lost", "\(link.packetsLost) (\(Format.bytes(link.bytesLost)))")
                row("congestion events", "\(link.congestionEvents)")
                row("reconnects", "\(link.reconnects)")
            } else {
                Text("no link").foregroundStyle(.tertiary)
            }
        }
        .frame(width: 300)
    }

    private func row(_ name: String, _ value: String) -> some View {
        GridRow {
            Text(name).foregroundStyle(.secondary)
            Text(value)
        }
    }
}

private struct ProtocolPanel: View {
    let telemetry: Telemetry

    var body: some View {
        DetailBlock(title: "Protocol", symbol: "point.3.connected.trianglepath.dotted") {
            if let proto = telemetry.latest?.protocol {
                row("alpn / version", "\(proto.alpn) · \(ProtocolSnapshot.version(proto.clientVersion)) ↔ \(ProtocolSnapshot.version(proto.serverVersion))")
                row("compression", proto.compression
                    ? String(format: "zstd · saved %.0f%%", proto.compressionSavings * 100) : "off")
                row("streams", proto.streams.formatted())
                row("frames out / in", "\(proto.framesSent.formatted()) / \(proto.framesReceived.formatted())")
                row("data in / out", "\(Format.bytes(proto.dataBytesReceived)) / \(Format.bytes(proto.dataBytesSent))")
            } else {
                Text("not negotiated").foregroundStyle(.tertiary)
            }
        }
        .frame(width: 320)
    }

    private func row(_ name: String, _ value: String) -> some View {
        GridRow {
            Text(name).foregroundStyle(.secondary)
            Text(value)
        }
    }
}
