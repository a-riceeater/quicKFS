// SPDX-License-Identifier: Apache-2.0
import AppKit
import SwiftUI
import UniformTypeIdentifiers

/// The advanced view: live link readouts, the full event stream (app, link,
/// mount helper, and optionally every kernel request), the helper's exact
/// invocation, and raw QUIC counters.
struct ConsoleView: View {
    let profile: ServerProfile
    let state: ServerState
    @Environment(AppModel.self) private var model
    @State private var filter: Filter = .all
    @State private var query = ""
    @State private var follow = true
    @State private var showDetails = true

    enum Filter: String, CaseIterable, Identifiable {
        case all, app, link, mount, kernel, problems
        var id: Self { self }
        var title: String { self == .problems ? "Problems" : rawValue.capitalized }
    }

    private var lines: [ConsoleLine] {
        state.log.lines.filter { line in
            let sourceMatches: Bool =
                switch filter {
                case .all: true
                case .app: line.source == .app
                case .link: line.source == .link
                case .mount: line.source == .mount
                case .kernel: line.source == .kernel
                case .problems: line.level >= .warning
                }
            return sourceMatches && (query.isEmpty || line.text.localizedCaseInsensitiveContains(query))
        }
    }

    var body: some View {
        VStack(spacing: 12) {
            Readouts(state: state)
            controls
            Terminal(lines: lines, follow: follow)
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .layoutPriority(-1)
            if showDetails {
                HStack(alignment: .top, spacing: 12) {
                    Counters(telemetry: state.telemetry)
                    ProtocolPanel(telemetry: state.telemetry)
                    Invocation(commandLine: state.mount.commandLine)
                }
                .frame(height: 132)
                .transition(.move(edge: .bottom).combined(with: .opacity))
            }
        }
        .padding(16)
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
        .animation(.smooth, value: showDetails)
        .task { await state.telemetry.watch() }
    }

    private var controls: some View {
        @Bindable var mount = state.mount
        return HStack(spacing: 10) {
            Picker("Filter", selection: $filter) {
                ForEach(Filter.allCases) { Text($0.title).tag($0) }
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .fixedSize()

            HStack(spacing: 6) {
                Image(systemName: "magnifyingglass").foregroundStyle(.secondary)
                TextField("Filter", text: $query)
                    .textFieldStyle(.plain)
                    .font(.callout.monospaced())
            }
            .padding(.horizontal, 10)
            .frame(height: 28)
            .glassEffect(.regular, in: .capsule)

            Toggle(isOn: $follow) { Image(systemName: "arrow.down.to.line") }
                .toggleStyle(.button)
                .help("Follow new lines")

            Toggle(isOn: $mount.traceKernel) {
                Label("Trace", systemImage: "cpu")
            }
            .toggleStyle(.button)
            .help("Log every request macOS sends the mount. Takes effect on the next mount.")

            if state.mount.isMounted, state.mount.traceKernel != state.mount.tracing {
                Button("Remount", systemImage: "arrow.clockwise") { model.remount(profile.id) }
                    .buttonStyle(.glassProminent)
                    .help("Eject and mount again to apply tracing")
            }

            Menu {
                Button("Copy Visible Lines", systemImage: "doc.on.doc") {
                    NSPasteboard.general.clearContents()
                    NSPasteboard.general.setString(state.log.export(lines), forType: .string)
                }
                Button("Export Log…", systemImage: "square.and.arrow.up") { export() }
                Divider()
                Button(showDetails ? "Hide Details" : "Show Details", systemImage: "rectangle.bottomhalf.filled") {
                    showDetails.toggle()
                }
                Button("Clear", systemImage: "trash", role: .destructive) { state.log.clear() }
            } label: {
                Image(systemName: "ellipsis.circle")
            }
            .menuIndicator(.hidden)
            .fixedSize()
        }
    }

    private func export() {
        let panel = NSSavePanel()
        panel.nameFieldStringValue = "quicKFS \(profile.name) \(Date.now.formatted(.iso8601.year().month().day())).log"
        panel.allowedContentTypes = [.log, .plainText]
        guard panel.runModal() == .OK, let url = panel.url else { return }
        try? state.log.export(state.log.lines).write(to: url, atomically: true, encoding: .utf8)
    }
}

// MARK: - Readouts

/// The live strip across the top: one tile per figure, each with a trace.
private struct Readouts: View {
    let state: ServerState

    private var telemetry: Telemetry { state.telemetry }

    var body: some View {
        GlassEffectContainer(spacing: 8) {
            HStack(spacing: 8) {
                LiveIndicator(source: telemetry.source)
                LazyVGrid(columns: Array(repeating: GridItem(.flexible(minimum: 56), spacing: 8), count: 8), spacing: 8) {
                    figures
                }
                .frame(maxWidth: .infinity)
            }
        }
        .frame(height: 64)
    }

    @ViewBuilder
    private var figures: some View {
        Group {
                tile("RTT", String(format: "%.2f", telemetry.current?.rttMs ?? 0), "ms",
                     trace: telemetry.samples.map(\.rttMs), tint: .accentColor)
                tile("RX", rate(telemetry.current?.receiveRate).0, rate(telemetry.current?.receiveRate).1,
                     trace: telemetry.samples.map(\.receiveRate), tint: .accentColor)
                tile("TX", rate(telemetry.current?.sendRate).0, rate(telemetry.current?.sendRate).1,
                     trace: telemetry.samples.map(\.sendRate), tint: .orange)
                tile("LOSS", String(format: "%.2f", telemetry.lossPercent), "%",
                     trace: telemetry.samples.map(\.lossFraction), tint: .red)
                tile("CWND", Format.bytes(telemetry.latest?.congestionWindow ?? 0), nil)
                tile("MTU", "\(telemetry.latest?.mtu ?? 0)", "B")
                tile("PID", state.mount.pid.map(String.init) ?? "—", nil)
                uptime
        }
    }

    private func rate(_ value: Double?) -> (String, String) { Rate.parts(value ?? 0) }

    private func tile(_ label: String, _ value: String, _ unit: String?, trace: [Double] = [], tint: Color = .secondary) -> some View {
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
            Sparkline(values: trace.suffix(40).map { $0 }, tint: tint)
                .frame(height: 12)
                .opacity(trace.isEmpty ? 0 : 1)
        }
        .padding(.horizontal, 10)
        .padding(.vertical, 7)
        .frame(maxWidth: .infinity, minHeight: 64, maxHeight: 64, alignment: .leading)
        .glassEffect(.regular, in: .rect(cornerRadius: 12))
    }

    private var uptime: some View {
        TimelineView(.periodic(from: .now, by: 1)) { context in
            tile("UP", elapsed(from: telemetry.mountStarted ?? state.mount.mountedAt, to: context.date), nil)
        }
    }

    private func elapsed(from start: Date?, to now: Date) -> String {
        guard let start else { return "—" }
        let seconds = max(0, Int(now.timeIntervalSince(start)))
        return String(format: "%02d:%02d:%02d", seconds / 3600, seconds / 60 % 60, seconds % 60)
    }
}

private struct LiveIndicator: View {
    let source: Telemetry.Source
    @State private var pulse = false

    var body: some View {
        VStack(spacing: 4) {
            Circle()
                .fill(source == .none ? Color.secondary : .green)
                .frame(width: 8, height: 8)
                .shadow(color: source == .none ? .clear : .green, radius: pulse ? 6 : 1)
                .animation(.easeInOut(duration: 1).repeatForever(autoreverses: true), value: pulse)
            Text(source == .mount ? "MNT" : source == .session ? "SES" : "OFF")
                .font(.system(size: 9, weight: .bold, design: .monospaced))
                .foregroundStyle(.secondary)
        }
        .frame(width: 40, height: 64)
        .glassEffect(.regular, in: .rect(cornerRadius: 12))
        .onAppear { pulse = true }
        .help(source == .mount ? "Live from the mounted volume" : source == .session ? "Live from this window's session" : "No live link")
    }
}

// MARK: - Terminal

private struct Terminal: View {
    let lines: [ConsoleLine]
    let follow: Bool

    var body: some View {
        ScrollView {
            LazyVStack(alignment: .leading, spacing: 1) {
                ForEach(lines) { line in
                    ConsoleRow(line: line)
                }
            }
            .padding(12)
            .frame(maxWidth: .infinity, alignment: .leading)
        }
        // Following keeps the newest line in view as the log grows, without
        // reaching into any other scroll view in the window.
        .defaultScrollAnchor(follow ? .bottom : .top, for: .sizeChanges)
        .overlay {
            if lines.isEmpty {
                Text("no events")
                    .font(.callout.monospaced())
                    .foregroundStyle(.tertiary)
            }
        }
        .background(Color.black.opacity(0.55), in: .rect(cornerRadius: 16))
        .overlay {
            RoundedRectangle(cornerRadius: 16)
                .strokeBorder(Color.accentColor.opacity(0.18), lineWidth: 1)
        }
        .environment(\.colorScheme, .dark)
    }
}

private struct ConsoleRow: View {
    let line: ConsoleLine

    private static let time: DateFormatter = {
        let formatter = DateFormatter()
        formatter.dateFormat = "HH:mm:ss.SSS"
        return formatter
    }()

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 10) {
            Text(Self.time.string(from: line.date))
                .foregroundStyle(.white.opacity(0.35))
            Text(line.source.rawValue.uppercased())
                .foregroundStyle(sourceTint)
                .frame(width: 46, alignment: .leading)
            Text(line.level.tag)
                .foregroundStyle(levelTint)
                .frame(width: 26, alignment: .leading)
            Text(line.text)
                .foregroundStyle(line.level >= .warning ? levelTint : .white.opacity(line.level == .debug ? 0.6 : 0.88))
                .frame(maxWidth: .infinity, alignment: .leading)
        }
        .font(.system(size: 11, design: .monospaced))
        .textSelection(.enabled)
        .padding(.vertical, 1)
    }

    private var sourceTint: Color {
        switch line.source {
        case .app: .accentColor
        case .link: .cyan
        case .mount: .purple
        case .kernel: .gray
        }
    }

    private var levelTint: Color {
        switch line.level {
        case .debug: .gray
        case .info: .white.opacity(0.7)
        case .notice: .green
        case .warning: .orange
        case .error: .red
        }
    }
}

// MARK: - Details

private struct Counters: View {
    let telemetry: Telemetry

    var body: some View {
        DetailPanel(title: "QUIC counters") {
            if let link = telemetry.latest {
                Grid(alignment: .leading, horizontalSpacing: 16, verticalSpacing: 3) {
                    row("bytes rx / tx", "\(Format.bytes(link.bytesReceived)) / \(Format.bytes(link.bytesSent))")
                    row("datagrams rx / tx", "\(link.datagramsReceived) / \(link.datagramsSent)")
                    row("packets lost", "\(link.packetsLost) (\(Format.bytes(link.bytesLost)))")
                    row("congestion events", "\(link.congestionEvents)")
                    row("reconnects", "\(link.reconnects)")
                }
            } else {
                Text("no link").foregroundStyle(.tertiary)
            }
        }
    }

    private func row(_ name: String, _ value: String) -> some View {
        GridRow {
            Text(name).foregroundStyle(.secondary)
            Text(value).textSelection(.enabled)
        }
    }
}

private struct ProtocolPanel: View {
    let telemetry: Telemetry

    var body: some View {
        DetailPanel(title: "quickfs protocol") {
            if let proto = telemetry.latest?.protocol {
                Grid(alignment: .leading, horizontalSpacing: 16, verticalSpacing: 3) {
                    row("alpn / version", "\(proto.alpn) · \(ProtocolSnapshot.version(proto.clientVersion)) ↔ \(ProtocolSnapshot.version(proto.serverVersion))")
                    row("compression", proto.compression
                        ? String(format: "zstd · %llu frames · %.0f%% saved",
                                 proto.compressedFramesSent + proto.compressedFramesReceived,
                                 proto.compressionSavings * 100)
                        : "off")
                    row("streams", proto.streams.formatted())
                    row("frames out / in", "\(proto.framesSent.formatted()) / \(proto.framesReceived.formatted())")
                    row("data in / out", "\(Format.bytes(proto.dataBytesReceived)) / \(Format.bytes(proto.dataBytesSent))")
                }
            } else {
                Text("no session").foregroundStyle(.tertiary)
            }
        }
    }

    private func row(_ name: String, _ value: String) -> some View {
        GridRow {
            Text(name).foregroundStyle(.secondary)
            Text(value).textSelection(.enabled).lineLimit(1)
        }
    }
}

private struct Invocation: View {
    let commandLine: String?

    var body: some View {
        DetailPanel(title: "Mount invocation") {
            if let commandLine {
                HStack(alignment: .top) {
                    Text("$ " + commandLine)
                        .textSelection(.enabled)
                        .lineLimit(5)
                        .frame(maxWidth: .infinity, alignment: .leading)
                    Button("Copy", systemImage: "doc.on.doc") {
                        NSPasteboard.general.clearContents()
                        NSPasteboard.general.setString(commandLine, forType: .string)
                    }
                    .labelStyle(.iconOnly)
                    .buttonStyle(.borderless)
                }
            } else {
                Text("not mounted this session").foregroundStyle(.tertiary)
            }
        }
    }
}

private struct DetailPanel<Content: View>: View {
    let title: String
    @ViewBuilder var content: () -> Content

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text(title.uppercased())
                .font(.system(size: 9, weight: .semibold, design: .monospaced))
                .tracking(1.2)
                .foregroundStyle(.secondary)
            content()
                .font(.system(size: 11, design: .monospaced))
        }
        .padding(12)
        .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
        .glassEffect(.regular, in: .rect(cornerRadius: 14))
    }
}
