// SPDX-License-Identifier: Apache-2.0
import Charts
import SwiftUI

/// Throughput, link quality, storage, and what the server supports. The
/// touch form of the CLI `ping`, `capabilities`, and `filesystem-stats`.
struct OverviewView: View {
    let profile: ServerProfile
    let state: ServerState
    @Environment(AppModel.self) private var model
    @Environment(\.horizontalSizeClass) private var sizeClass

    private var connection: ServerConnection { state.connection }
    private var telemetry: Telemetry { state.telemetry }

    var body: some View {
        ScrollView {
            GlassGroup(spacing: 14) {
                VStack(spacing: 14) {
                    ThroughputCard(telemetry: telemetry)
                    let small = [GridItem(.flexible(), spacing: 14, alignment: .top),
                                 GridItem(.flexible(), spacing: 14, alignment: .top)]
                    LazyVGrid(columns: sizeClass == .regular ? small + [GridItem(.flexible(), alignment: .top)] : small,
                              spacing: 14) {
                        LatencyCard(telemetry: telemetry)
                        LossCard(telemetry: telemetry)
                        TransferredCard(telemetry: telemetry)
                        StorageCard(stats: connection.stats)
                    }
                    ServerCard(profile: profile, state: state, pin: model.pin(for: profile))
                    if let capabilities = connection.capabilities {
                        FeatureCard(capabilities: capabilities)
                    }
                }
                .padding(16)
            }
        }
        .refreshable {
            await connection.refreshStats()
            await connection.refreshCapabilities()
        }
        .task {
            _ = try? await connection.connect()
            await connection.refreshStats()
            await connection.refreshCapabilities()
        }
        .task { await telemetry.watch() }
    }
}

private struct ThroughputCard: View {
    let telemetry: Telemetry

    var body: some View {
        Card {
            HStack(alignment: .top) {
                CardTitle(text: "Throughput", symbol: "arrow.up.arrow.down")
                Spacer()
                if telemetry.source != .none {
                    HStack(spacing: 5) {
                        Circle().fill(.green).frame(width: 6, height: 6)
                        Text("LIVE")
                    }
                    .font(.caption2.weight(.semibold).monospaced())
                    .foregroundStyle(.secondary)
                }
            }
            HStack(alignment: .bottom, spacing: 24) {
                direction("arrow.down", .accentColor, telemetry.current?.receiveRate ?? 0, peak: telemetry.peakReceive)
                direction("arrow.up", .orange, telemetry.current?.sendRate ?? 0, peak: telemetry.peakSend)
                Spacer(minLength: 0)
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
                    AxisValueLabel { if let rate = value.as(Double.self) { Text(Rate.text(rate)) } }
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
                Figure(value: figure, unit: unit, size: .title)
            }
            Text("peak \(Rate.text(peak))")
                .font(.caption)
                .foregroundStyle(.secondary)
                .monospacedDigit()
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
    }
}

private struct TransferredCard: View {
    let telemetry: Telemetry

    var body: some View {
        Card {
            CardTitle(text: "Transferred", symbol: "arrow.left.arrow.right")
            VStack(alignment: .leading, spacing: 4) {
                Label(Format.bytes(telemetry.totalReceived), systemImage: "arrow.down")
                Label(Format.bytes(telemetry.totalSent), systemImage: "arrow.up")
            }
            .font(.headline)
            .monospacedDigit()
            Text(reconnects)
                .font(.caption)
                .foregroundStyle(.secondary)
        }
    }

    private var reconnects: String {
        switch telemetry.latest?.reconnects ?? 0 {
        case 0: "No reconnects"
        case 1: "1 reconnect"
        case let count: "\(count) reconnects"
        }
    }
}

private struct StorageCard: View {
    let stats: FilesystemStats?

    var body: some View {
        Card {
            CardTitle(text: "Storage", symbol: "internaldrive")
            HStack(spacing: 12) {
                ZStack {
                    Circle().stroke(.quaternary, lineWidth: 8)
                    Circle()
                        .trim(from: 0, to: stats?.usedFraction ?? 0)
                        .stroke(ringColor.gradient, style: .init(lineWidth: 8, lineCap: .round))
                        .rotationEffect(.degrees(-90))
                    Text(stats?.usedFraction ?? 0, format: .percent.precision(.fractionLength(0)))
                        .font(.caption.weight(.semibold))
                        .monospacedDigit()
                }
                .frame(width: 56, height: 56)
                .animation(.smooth, value: stats?.usedFraction)
                if let stats {
                    VStack(alignment: .leading, spacing: 2) {
                        Text(Format.bytes(stats.availableBytes))
                            .font(.headline)
                            .monospacedDigit()
                        Text("free of \(Format.bytes(stats.totalBytes))")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                } else {
                    ProgressView()
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
            VStack(spacing: 8) {
                row("Access", capabilities.map { $0.writable ? "Read and write" : "Read only" } ?? "Unknown")
                LabeledContent("Identity") {
                    HStack(spacing: 6) {
                        Text(identity).lineLimit(1).truncationMode(.middle)
                        if let pin {
                            Button("Copy fingerprint", systemImage: "doc.on.doc") {
                                UIPasteboard.general.string = pin.sha256
                            }
                            .labelStyle(.iconOnly)
                            .buttonStyle(.borderless)
                        }
                    }
                }
                row("Volume", capabilities.map { $0.volumeName.isEmpty ? profile.name : $0.volumeName } ?? profile.name)
                if let proto = state.telemetry.latest?.protocol {
                    row("Protocol", "\(proto.alpn) · v\(ProtocolSnapshot.version(proto.serverVersion))"
                        + (proto.compression ? " · zstd" : ""))
                }
                row("Largest request", capabilities.map {
                    "\(Format.bytes($0.maxReadSize)) read · \(Format.bytes($0.maxWriteSize)) write"
                } ?? "Unknown")
            }
            .font(.callout)
        }
    }

    private var identity: String {
        switch profile.trust {
        case .pinned:
            guard let pin else { return "Not paired" }
            return "Pinned · \(pin.sha256.prefix(8))…\(pin.sha256.suffix(8))"
        case .enterpriseCA(let path): return "CA · \(AddServerView.bundleName(URL(fileURLWithPath: path)))"
        case .systemRoots: return "System trust · \(profile.serverName)"
        }
    }

    private func row(_ label: String, _ value: String) -> some View {
        LabeledContent(label) {
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
            ("Tags and metadata", "Extended attributes", capabilities.supportsXattrs),
            ("Sparse files", "Holes are skipped when reading", capabilities.supportsSeekDataHole),
            ("Preallocation", "Space reserved before writing", capabilities.supportsPreallocation),
            ("Stable file IDs", "Links survive server restarts", capabilities.persistentNodeIds),
            ("Lock recovery", "Locks restored after a reconnect", capabilities.restartLockReplay),
            ("Fast listings", "Folders load in one round trip", capabilities.supportsReaddirplus),
        ]
    }

    var body: some View {
        Card {
            CardTitle(text: "Features", symbol: "checklist")
            ForEach(features, id: \.0) { name, detail, supported in
                HStack(alignment: .firstTextBaseline, spacing: 10) {
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
