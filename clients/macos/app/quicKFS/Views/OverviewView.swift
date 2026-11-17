// SPDX-License-Identifier: Apache-2.0
import AppKit
import Charts
import SwiftUI

/// Throughput, link quality, storage, and what the server supports. The GUI
/// form of the CLI `ping`, `capabilities`, and `filesystem-stats` commands.
struct OverviewView: View {
    let profile: ServerProfile
    let state: ServerState
    @Environment(AppModel.self) private var model

    private var connection: ServerConnection { state.connection }
    private var telemetry: Telemetry { state.telemetry }

    var body: some View {
        ScrollView {
            GlassEffectContainer(spacing: 16) {
                VStack(spacing: 16) {
                    ThroughputCard(telemetry: telemetry)
                    HStack(alignment: .top, spacing: 16) {
                        LatencyCard(telemetry: telemetry)
                        LossCard(telemetry: telemetry)
                        TransferredCard(telemetry: telemetry)
                    }
                    .fixedSize(horizontal: false, vertical: true)
                    HStack(alignment: .top, spacing: 16) {
                        StorageCard(stats: connection.stats)
                        ServerCard(profile: profile, state: state, pin: model.pin(for: profile))
                    }
                    .fixedSize(horizontal: false, vertical: true)
                    if let capabilities = connection.capabilities {
                        FeatureCard(capabilities: capabilities)
                    }
                }
                .padding(20)
            }
        }
        .task {
            _ = try? await connection.connect()
            await connection.refreshStats()
            await connection.refreshCapabilities()
        }
        .task { await telemetry.watch() }
    }
}

// MARK: - Building blocks

struct Card<Content: View>: View {
    var padding: CGFloat = 18
    @ViewBuilder var content: () -> Content

    var body: some View {
        VStack(alignment: .leading, spacing: 12, content: content)
            .padding(padding)
            .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
            .glassEffect(.regular, in: .rect(cornerRadius: 22))
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

    static func megabits(_ bytesPerSecond: Double) -> String {
        String(format: "%.1f Mbit/s", bytesPerSecond * 8 / 1_000_000)
    }
}

// MARK: - Cards

private struct ThroughputCard: View {
    let telemetry: Telemetry

    var body: some View {
        Card {
            HStack(alignment: .top) {
                CardTitle(text: "Throughput", symbol: "arrow.up.arrow.down")
                Spacer()
                SourceBadge(source: telemetry.source)
            }
            HStack(alignment: .bottom, spacing: 28) {
                direction("arrow.down", .accentColor, telemetry.current?.receiveRate ?? 0, peak: telemetry.peakReceive)
                direction("arrow.up", .orange, telemetry.current?.sendRate ?? 0, peak: telemetry.peakSend)
                Spacer()
            }
            Chart(telemetry.samples) { sample in
                AreaMark(x: .value("Time", sample.date), y: .value("Rate", sample.receiveRate),
                         series: .value("Direction", "down"))
                    .foregroundStyle(.linearGradient(colors: [Color.accentColor.opacity(0.35), .clear],
                                                     startPoint: .top, endPoint: .bottom))
                    .interpolationMethod(.monotone)
                LineMark(x: .value("Time", sample.date), y: .value("Rate", sample.receiveRate),
                         series: .value("Direction", "down"))
                    .foregroundStyle(Color.accentColor)
                    .interpolationMethod(.monotone)
                LineMark(x: .value("Time", sample.date), y: .value("Rate", sample.sendRate),
                         series: .value("Direction", "up"))
                    .foregroundStyle(Color.orange.opacity(0.8))
                    .lineStyle(.init(lineWidth: 1.2))
                    .interpolationMethod(.monotone)
            }
            .chartXAxis(.hidden)
            .chartYAxis {
                AxisMarks(position: .trailing, values: .automatic(desiredCount: 3)) { value in
                    AxisGridLine().foregroundStyle(.quaternary)
                    AxisValueLabel {
                        if let rate = value.as(Double.self) {
                            let (figure, unit) = Rate.parts(rate)
                            Text("\(figure) \(unit)")
                        }
                    }
                }
            }
            .frame(height: 120)
            .overlay {
                if telemetry.samples.count < 2 {
                    Text(telemetry.source == .none ? "Waiting for a connection" : "Measuring")
                        .font(.callout)
                        .foregroundStyle(.secondary)
                }
            }
        }
    }

    private func direction(_ symbol: String, _ tint: Color, _ rate: Double, peak: Double) -> some View {
        let (figure, unit) = Rate.parts(rate)
        return VStack(alignment: .leading, spacing: 2) {
            HStack(alignment: .firstTextBaseline, spacing: 6) {
                Image(systemName: symbol)
                    .font(.title3.weight(.bold))
                    .foregroundStyle(tint)
                Figure(value: figure, unit: unit, size: .largeTitle)
            }
            Text("\(Rate.megabits(rate))  ·  peak \(Rate.parts(peak).0) \(Rate.parts(peak).1)")
                .font(.caption)
                .foregroundStyle(.secondary)
                .monospacedDigit()
        }
    }
}

private struct SourceBadge: View {
    let source: Telemetry.Source

    var body: some View {
        if source != .none {
            HStack(spacing: 5) {
                Circle().fill(.green).frame(width: 6, height: 6)
                Text(source == .mount ? "MOUNT" : "SESSION")
            }
            .font(.caption2.weight(.semibold).monospaced())
            .foregroundStyle(.secondary)
            .padding(.horizontal, 8)
            .padding(.vertical, 3)
            .glassEffect(.regular, in: .capsule)
            .help(source == .mount
                ? "All Finder and app traffic through the mounted volume"
                : "This window's own connection to the server")
        }
    }
}

private struct LatencyCard: View {
    let telemetry: Telemetry

    var body: some View {
        Card {
            CardTitle(text: "Latency", symbol: "timer")
            Figure(value: String(format: (telemetry.current?.rttMs ?? 0) < 10 ? "%.1f" : "%.0f",
                                 telemetry.current?.rttMs ?? 0), unit: "ms")
            Text(String(format: "jitter %.1f ms", telemetry.jitterMs))
                .font(.caption)
                .foregroundStyle(.secondary)
                .monospacedDigit()
            Sparkline(values: telemetry.samples.map(\.rttMs), tint: .accentColor)
        }
        .help("Smoothed QUIC round-trip time and its variation over the last 20 seconds")
    }
}

private struct LossCard: View {
    let telemetry: Telemetry

    var body: some View {
        Card {
            CardTitle(text: "Packet loss", symbol: "waveform.path.badge.minus")
            Figure(value: String(format: "%.2f", telemetry.lossPercent), unit: "%")
            Text("\(telemetry.latest?.packetsLost ?? 0) of \(telemetry.latest?.packetsSent ?? 0) packets")
                .font(.caption)
                .foregroundStyle(.secondary)
                .monospacedDigit()
            Sparkline(values: telemetry.samples.map { $0.lossFraction * 100 }, tint: .red)
        }
        .help("Packets the sender declared lost on this connection")
    }
}

private struct TransferredCard: View {
    let telemetry: Telemetry

    var body: some View {
        Card {
            CardTitle(text: "Transferred", symbol: "arrow.left.arrow.right")
            HStack(alignment: .firstTextBaseline, spacing: 14) {
                Label(Format.bytes(telemetry.totalReceived), systemImage: "arrow.down")
                Label(Format.bytes(telemetry.totalSent), systemImage: "arrow.up")
            }
            .font(.title3.weight(.semibold))
            .monospacedDigit()
            Text(reconnects)
                .font(.caption)
                .foregroundStyle(.secondary)
                .monospacedDigit()
        }
        .help("Traffic measured while this view has been open")
    }

    private var reconnects: String {
        switch telemetry.latest?.reconnects ?? 0 {
        case 0: "No reconnects"
        case 1: "1 reconnect"
        case let count: "\(count) reconnects"
        }
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

private struct StorageCard: View {
    let stats: FilesystemStats?

    var body: some View {
        Card {
            CardTitle(text: "Storage", symbol: "internaldrive")
            HStack(spacing: 18) {
                ZStack {
                    Circle().stroke(.quaternary, lineWidth: 11)
                    Circle()
                        .trim(from: 0, to: stats?.usedFraction ?? 0)
                        .stroke(ringColor.gradient, style: .init(lineWidth: 11, lineCap: .round))
                        .rotationEffect(.degrees(-90))
                    Text(stats?.usedFraction ?? 0, format: .percent.precision(.fractionLength(0)))
                        .font(.callout.weight(.semibold))
                        .monospacedDigit()
                }
                .frame(width: 84, height: 84)
                .animation(.smooth, value: stats?.usedFraction)
                if let stats {
                    VStack(alignment: .leading, spacing: 4) {
                        Figure(value: Format.bytes(stats.availableBytes))
                        Text("free of \(Format.bytes(stats.totalBytes))")
                            .foregroundStyle(.secondary)
                        Text("\(stats.filesFree.formatted()) file slots free")
                            .font(.caption)
                            .foregroundStyle(.tertiary)
                    }
                } else {
                    ProgressView().controlSize(.small)
                }
            }
        }
    }

    private var ringColor: Color {
        switch stats?.usedFraction ?? 0 {
        case ..<0.75: .accentColor
        case ..<0.9: .orange
        default: .red
        }
    }
}

private struct ServerCard: View {
    let profile: ServerProfile
    let state: ServerState
    let pin: PinnedServer?

    private var capabilities: Capabilities? { state.connection.capabilities }

    var body: some View {
        Card {
            CardTitle(text: "Server", symbol: "server.rack")
            Grid(alignment: .leading, horizontalSpacing: 14, verticalSpacing: 7) {
                row("Access", capabilities.map { $0.writable ? "Read and write" : "Read only" } ?? "Unknown")
                GridRow {
                    Text("Identity").foregroundStyle(.secondary)
                    HStack(spacing: 6) {
                        Text(identity).lineLimit(1).truncationMode(.middle)
                        if let pin {
                            Button("Copy fingerprint", systemImage: "doc.on.doc") {
                                NSPasteboard.general.clearContents()
                                NSPasteboard.general.setString(pin.sha256, forType: .string)
                            }
                            .labelStyle(.iconOnly)
                            .buttonStyle(.borderless)
                            .help("Copy the full SHA-256 fingerprint")
                        }
                    }
                }
                row("Volume", capabilities.map { $0.volumeName.isEmpty ? profile.name : $0.volumeName } ?? profile.name)
                if let proto = state.telemetry.latest?.protocol {
                    row("Protocol", "\(proto.alpn) · v\(ProtocolSnapshot.version(proto.serverVersion))"
                        + (proto.compression ? " · zstd frames" : ""))
                }
                row("Largest request", capabilities.map {
                    "\(Format.bytes($0.maxReadSize)) read · \(Format.bytes($0.maxWriteSize)) write"
                } ?? "Unknown")
                GridRow {
                    Text("Mount").foregroundStyle(.secondary)
                    HStack(spacing: 6) {
                        Text(state.mount.isMounted ? profile.mountpointURL.path(percentEncoded: false) : "Not mounted")
                            .lineLimit(1)
                            .truncationMode(.middle)
                        if state.mount.isMounted {
                            Button("Open in Finder", systemImage: "arrow.up.forward.app") { state.mount.revealInFinder() }
                                .labelStyle(.iconOnly)
                                .buttonStyle(.borderless)
                        }
                    }
                }
            }
            .font(.callout)
        }
    }

    private var identity: String {
        switch profile.trust {
        case .pinned:
            guard let pin else { return "Not paired" }
            return "Pinned · \(pin.sha256.prefix(8))…\(pin.sha256.suffix(8))"
        case .enterpriseCA(let path): return "CA · \((path as NSString).lastPathComponent)"
        case .systemRoots: return "System trust · \(profile.serverName)"
        }
    }

    private func row(_ label: String, _ value: String) -> some View {
        GridRow {
            Text(label).foregroundStyle(.secondary)
            Text(value).lineLimit(1).truncationMode(.middle).textSelection(.enabled)
        }
    }
}

/// What the server supports, in words.
private struct FeatureCard: View {
    let capabilities: Capabilities

    private var features: [(String, String, Bool)] {
        [
            ("Writes", "Create, change, and delete files", capabilities.writable),
            ("File locking", "Apps can lock byte ranges across clients", capabilities.supportsLocks),
            ("Safe saves", "Atomic rename for save-in-place", capabilities.supportsAtomicRename),
            ("Server-side copy", "Duplicates never cross the network", capabilities.supportsCopyFileRange),
            ("Symbolic links", "Links are created and followed", capabilities.supportsSymlinks),
            ("Hard links", "Several names for one file", capabilities.supportsHardLinks),
            ("Tags and metadata", "Finder tags and extended attributes", capabilities.supportsXattrs),
            ("Sparse files", "Holes are skipped when reading", capabilities.supportsSeekDataHole),
            ("Preallocation", "Space reserved before writing", capabilities.supportsPreallocation),
            ("Stable file IDs", "Aliases survive server restarts", capabilities.persistentNodeIds),
            ("Lock recovery", "Locks restored after a reconnect", capabilities.restartLockReplay),
            ("Fast listings", "Folders load in one round trip", capabilities.supportsReaddirplus),
        ]
    }

    var body: some View {
        Card {
            CardTitle(text: "Features", symbol: "checklist")
            LazyVGrid(columns: [GridItem(.flexible(), alignment: .topLeading), GridItem(.flexible(), alignment: .topLeading)],
                      alignment: .leading, spacing: 10) {
                ForEach(features, id: \.0) { name, detail, supported in
                    HStack(alignment: .firstTextBaseline, spacing: 8) {
                        Image(systemName: supported ? "checkmark.circle.fill" : "minus.circle")
                            .foregroundStyle(supported ? AnyShapeStyle(.green) : AnyShapeStyle(.tertiary))
                        VStack(alignment: .leading, spacing: 1) {
                            Text(name).font(.callout.weight(.medium))
                            Text(detail).font(.caption).foregroundStyle(.secondary)
                        }
                    }
                    .opacity(supported ? 1 : 0.6)
                }
            }
        }
    }
}
