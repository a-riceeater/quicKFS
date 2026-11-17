// SPDX-License-Identifier: Apache-2.0
import Charts
import SwiftUI

/// Storage, link quality, and what the server supports. The GUI form of the
/// CLI `ping`, `capabilities`, and `filesystem-stats` commands.
struct OverviewView: View {
    let profile: ServerProfile
    let state: ServerState
    @Environment(AppModel.self) private var model

    private var connection: ServerConnection { state.connection }

    var body: some View {
        ScrollView {
            GlassEffectContainer(spacing: 16) {
                VStack(spacing: 16) {
                    HStack(alignment: .top, spacing: 16) {
                        StorageCard(stats: connection.stats)
                        LatencyCard(samples: connection.latency)
                    }
                    .fixedSize(horizontal: false, vertical: true)
                    HStack(alignment: .top, spacing: 16) {
                        MountCard(profile: profile, mount: state.mount)
                        IdentityCard(profile: profile, pin: model.pin(for: profile),
                                     capabilities: connection.capabilities)
                    }
                    .fixedSize(horizontal: false, vertical: true)
                    if let capabilities = connection.capabilities {
                        CapabilityGrid(capabilities: capabilities)
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
        .refreshable {
            await connection.refreshStats()
            await connection.ping()
        }
    }
}

private struct Card<Content: View>: View {
    @ViewBuilder var content: () -> Content

    var body: some View {
        VStack(alignment: .leading, spacing: 12, content: content)
            .padding(18)
            .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .topLeading)
            .glassEffect(.regular, in: .rect(cornerRadius: 22))
    }
}

private struct StorageCard: View {
    let stats: FilesystemStats?

    var body: some View {
        Card {
            HStack(spacing: 18) {
                ZStack {
                    Circle().stroke(.quaternary, lineWidth: 12)
                    Circle()
                        .trim(from: 0, to: stats?.usedFraction ?? 0)
                        .stroke(ringColor.gradient, style: .init(lineWidth: 12, lineCap: .round))
                        .rotationEffect(.degrees(-90))
                    Text((stats?.usedFraction ?? 0), format: .percent.precision(.fractionLength(0)))
                        .font(.title3.weight(.semibold))
                        .monospacedDigit()
                        .contentTransition(.numericText())
                }
                .frame(width: 96, height: 96)
                .animation(.smooth, value: stats?.usedFraction)
                VStack(alignment: .leading, spacing: 6) {
                    Image(systemName: "internaldrive")
                        .foregroundStyle(.secondary)
                    if let stats {
                        Text(Format.bytes(stats.availableBytes))
                            .font(.title2.weight(.semibold))
                            .monospacedDigit()
                        Text("free of \(Format.bytes(stats.totalBytes))")
                            .foregroundStyle(.secondary)
                        Text("\(stats.filesFree.formatted()) inodes free")
                            .font(.caption)
                            .foregroundStyle(.tertiary)
                    } else {
                        ProgressView().controlSize(.small)
                    }
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

private struct LatencyCard: View {
    let samples: [Double]

    var body: some View {
        Card {
            HStack(alignment: .firstTextBaseline) {
                Image(systemName: "bolt.horizontal")
                    .foregroundStyle(.secondary)
                Spacer()
                if let last = samples.last {
                    Text(Format.milliseconds(last))
                        .font(.title2.weight(.semibold))
                        .monospacedDigit()
                        .contentTransition(.numericText())
                }
            }
            if samples.count > 1 {
                Chart(Array(samples.enumerated()), id: \.offset) { index, value in
                    AreaMark(x: .value("Sample", index), y: .value("RTT", value))
                        .foregroundStyle(.linearGradient(
                            colors: [Color.accentColor.opacity(0.35), .clear], startPoint: .top, endPoint: .bottom))
                        .interpolationMethod(.monotone)
                    LineMark(x: .value("Sample", index), y: .value("RTT", value))
                        .foregroundStyle(Color.accentColor)
                        .interpolationMethod(.monotone)
                }
                .chartXAxis(.hidden)
                .chartYAxis {
                    AxisMarks(position: .trailing, values: .automatic(desiredCount: 3)) { value in
                        AxisGridLine()
                        AxisValueLabel { if let ms = value.as(Double.self) { Text(Format.milliseconds(ms)) } }
                    }
                }
                .frame(height: 70)
                .animation(.smooth, value: samples)
            } else {
                Spacer(minLength: 70)
            }
        }
        .help("Round-trip time, sampled every five seconds")
    }
}

private struct MountCard: View {
    let profile: ServerProfile
    let mount: MountController

    var body: some View {
        Card {
            HStack {
                Image(systemName: mount.isMounted ? "externaldrive.fill.badge.checkmark" : "externaldrive")
                    .font(.title2)
                    .foregroundStyle(mount.isMounted ? .green : .secondary)
                    .contentTransition(.symbolEffect(.replace))
                Spacer()
                if let since = mount.mountedAt {
                    Text(since, style: .relative)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .monospacedDigit()
                }
            }
            Text(profile.mountpointURL.path(percentEncoded: false))
                .font(.callout.monospaced())
                .lineLimit(2)
                .truncationMode(.middle)
                .textSelection(.enabled)
            if mount.isMounted {
                Button("Open in Finder", systemImage: "folder") { mount.revealInFinder() }
                    .buttonStyle(.glass)
            }
        }
    }
}

private struct IdentityCard: View {
    let profile: ServerProfile
    let pin: PinnedServer?
    let capabilities: Capabilities?

    var body: some View {
        Card {
            HStack {
                Image(systemName: trustSymbol)
                    .font(.title2)
                    .foregroundStyle(.tint)
                Spacer()
                if let capabilities {
                    Image(systemName: capabilities.writable ? "pencil" : "lock")
                        .foregroundStyle(.secondary)
                        .help(capabilities.writable ? "Read and write" : "Read only")
                }
            }
            if let pin {
                Text(fingerprint(pin.sha256))
                    .font(.caption.monospaced())
                    .foregroundStyle(.secondary)
                    .textSelection(.enabled)
                    .help("Pinned certificate SHA-256")
            } else {
                Text(trustDescription).foregroundStyle(.secondary)
            }
            if let capabilities {
                Text("\(capabilities.volumeName.isEmpty ? profile.name : capabilities.volumeName)  ·  \(Format.bytes(capabilities.maxReadSize)) reads  ·  \(Format.bytes(capabilities.maxWriteSize)) writes")
                    .font(.caption)
                    .foregroundStyle(.tertiary)
            }
        }
    }

    private var trustSymbol: String {
        switch profile.trust {
        case .pinned: "pin.fill"
        case .enterpriseCA: "building.columns"
        case .systemRoots: "checkmark.seal"
        }
    }

    private var trustDescription: String {
        switch profile.trust {
        case .pinned: "Not paired"
        case .enterpriseCA(let path): (path as NSString).lastPathComponent
        case .systemRoots: profile.serverName
        }
    }

    private func fingerprint(_ hex: String) -> String {
        stride(from: 0, to: hex.count, by: 4).map { offset in
            let start = hex.index(hex.startIndex, offsetBy: offset)
            return String(hex[start..<hex.index(start, offsetBy: 4, limitedBy: hex.endIndex)!])
        }.joined(separator: " ")
    }
}

private struct CapabilityGrid: View {
    let capabilities: Capabilities

    private var items: [(String, String, Bool)] {
        [
            ("pencil", "Writes", capabilities.writable),
            ("lock.rectangle.stack", "Byte-range locks", capabilities.supportsLocks),
            ("arrow.left.arrow.right", "Atomic rename", capabilities.supportsAtomicRename),
            ("arrow.triangle.2.circlepath", "Directory sync", capabilities.supportsDirectorySync),
            ("square.dashed.inset.filled", "Preallocation", capabilities.supportsPreallocation),
            ("arrow.turn.up.right", "Symbolic links", capabilities.supportsSymlinks),
            ("tag", "Extended attributes", capabilities.supportsXattrs),
            ("link", "Hard links", capabilities.supportsHardLinks),
            ("cpu", "Special files", capabilities.supportsSpecialNodes),
            ("doc.on.doc", "Server-side copy", capabilities.supportsCopyFileRange),
            ("rectangle.split.3x1", "Sparse seek", capabilities.supportsSeekDataHole),
            ("arrow.2.squarepath", "Exchange data", capabilities.supportsExchangeData),
            ("character.cursor.ibeam", "Volume rename", capabilities.supportsVolumeRename),
            ("clock.badge.checkmark", "Backup time", capabilities.supportsBackupTime),
            ("list.bullet.rectangle", "Directory views", capabilities.supportsReaddirplus),
            ("number", "Stable node IDs", capabilities.persistentNodeIds),
            ("arrow.clockwise.circle", "Lock replay", capabilities.restartLockReplay),
            ("bell", "Poll", capabilities.supportsPoll),
        ]
    }

    var body: some View {
        LazyVGrid(columns: [GridItem(.adaptive(minimum: 52), spacing: 10)], spacing: 10) {
            ForEach(items, id: \.1) { symbol, name, supported in
                Image(systemName: symbol)
                    .font(.title3)
                    .foregroundStyle(supported ? AnyShapeStyle(.tint) : AnyShapeStyle(.quaternary))
                    .frame(width: 52, height: 52)
                    .glassEffect(supported ? .regular.tint(.accentColor.opacity(0.15)) : .regular,
                                 in: .rect(cornerRadius: 14))
                    .help(name + (supported ? "" : " (unavailable)"))
                    .accessibilityLabel(name)
                    .accessibilityValue(supported ? "Supported" : "Unavailable")
            }
        }
    }
}

/// The mount helper's own output, for diagnosing a mount.
struct ActivityView: View {
    let mount: MountController

    var body: some View {
        if mount.log.isEmpty {
            Placard(symbol: "waveform.path.ecg", title: "Quiet")
        } else {
            ScrollViewReader { proxy in
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 2) {
                        ForEach(Array(mount.log.enumerated()), id: \.offset) { index, line in
                            Text(line)
                                .font(.system(.caption, design: .monospaced))
                                .foregroundStyle(line.hasPrefix("Error") ? .red : .primary)
                                .textSelection(.enabled)
                                .frame(maxWidth: .infinity, alignment: .leading)
                                .id(index)
                        }
                    }
                    .padding(16)
                }
                .onChange(of: mount.log.count) { proxy.scrollTo(mount.log.count - 1, anchor: .bottom) }
            }
        }
    }
}
