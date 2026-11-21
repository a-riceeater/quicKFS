// SPDX-License-Identifier: Apache-2.0
import SwiftUI

/// Details for one entry, the touch form of the CLI `stat` command.
struct NodeInspectorView: View {
    let node: RemoteNode
    let browser: BrowserModel
    @Environment(\.dismiss) private var dismiss
    @State private var detail: RemoteNode?
    @State private var linkTarget: String?

    private var shown: RemoteNode { detail ?? node }

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    VStack(spacing: 10) {
                        NodeIcon(node: shown, size: 72)
                        Text(shown.displayName)
                            .font(.headline)
                            .multilineTextAlignment(.center)
                            .textSelection(.enabled)
                        if !shown.isDirectory {
                            Text(Format.bytes(shown.size))
                                .font(.title2.weight(.semibold))
                                .monospacedDigit()
                                .contentTransition(.numericText())
                        }
                    }
                    .frame(maxWidth: .infinity)
                    .listRowBackground(Color.clear)
                }
                Section {
                    if let created = shown.created { row("Created", "calendar.badge.plus", created.formatted()) }
                    row("Modified", "pencil.and.outline", shown.modified.formatted())
                    row("Opened", "eye", shown.accessed.formatted())
                    if !shown.isDirectory {
                        row("On disk", "internaldrive", Format.bytes(shown.allocatedBytes))
                        row("Bytes", "number", shown.size.formatted())
                    }
                    row("Links", "link", "\(shown.linkCount)")
                    row("Revision", "clock.arrow.circlepath", "r\(shown.revision)")
                    if let linkTarget { row("Target", "arrow.turn.up.right", linkTarget) }
                    row("ID", "tag", shown.node.uuidString.lowercased(), monospaced: true)
                }
                Section("Permissions") {
                    PermissionEditor(node: shown, browser: browser)
                }
            }
            .navigationTitle("Info")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) { Button("Done") { dismiss() } }
            }
            .task { await load() }
        }
    }

    private func load() async {
        if let fresh: RemoteNode = try? await browser.connection.request("stat", ["node": .uuid(node.node)]) {
            // `stat` reports the node without its name.
            detail = RemoteNode(
                node: fresh.node, name: node.name, rawName: node.rawName, kind: fresh.kind, size: fresh.size,
                mode: fresh.mode, allocatedBytes: fresh.allocatedBytes, revision: fresh.revision,
                accessedMs: fresh.accessedMs, modifiedMs: fresh.modifiedMs, createdMs: fresh.createdMs,
                backupMs: fresh.backupMs, linkCount: fresh.linkCount)
        }
        if node.kind == .symlink {
            struct Reply: Decodable, Sendable { let target: String }
            let reply: Reply? = try? await browser.connection.request("read_link", ["node": .uuid(node.node)])
            linkTarget = reply?.target
        }
    }

    private func row(_ label: String, _ symbol: String, _ value: String, monospaced: Bool = false) -> some View {
        LabeledContent {
            Text(value)
                .font(monospaced ? .caption.monospaced() : .body)
                .textSelection(.enabled)
                .lineLimit(2)
                .multilineTextAlignment(.trailing)
        } label: {
            Label(label, systemImage: symbol)
        }
    }
}

/// An owner, group, and other by read, write, and execute grid of toggles.
private struct PermissionEditor: View {
    let node: RemoteNode
    let browser: BrowserModel
    @State private var mode: UInt32 = 0
    @State private var saving = false

    private let columns: [(String, UInt32)] = [("r", 4), ("w", 2), ("x", 1)]
    private let rows: [(String, UInt32)] = [("person", 6), ("person.2", 3), ("globe", 0)]

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Grid(horizontalSpacing: 12, verticalSpacing: 8) {
                ForEach(rows, id: \.1) { symbol, shift in
                    GridRow {
                        Image(systemName: symbol)
                            .foregroundStyle(.secondary)
                            .frame(width: 26)
                        ForEach(columns, id: \.1) { letter, bit in
                            let mask = bit << shift
                            Toggle(letter, isOn: Binding(
                                get: { mode & mask != 0 },
                                set: { mode = $0 ? mode | mask : mode & ~mask }
                            ))
                            .toggleStyle(.button)
                            .font(.body.monospaced())
                            .disabled(!browser.writable)
                        }
                    }
                }
            }
            HStack {
                Text(String(format: "%04o", mode & 0o7777))
                    .font(.callout.monospaced())
                    .foregroundStyle(.secondary)
                Text(Format.mode(mode, directory: node.isDirectory))
                    .font(.callout.monospaced())
                    .foregroundStyle(.tertiary)
                Spacer()
                if mode != node.mode & 0o7777 {
                    Button("Apply") {
                        saving = true
                        Task {
                            _ = await browser.setMode(node, mode)
                            saving = false
                        }
                    }
                    .glassButton(prominent: true)
                    .disabled(saving)
                }
            }
        }
        .padding(.vertical, 4)
        .onAppear { mode = node.mode & 0o7777 }
        .onChange(of: node) { mode = node.mode & 0o7777 }
    }
}
