// SPDX-License-Identifier: Apache-2.0
import AppKit
import SwiftUI
import UniformTypeIdentifiers

/// Details for the selected entry, the GUI form of the CLI `stat` command.
struct NodeInspector: View {
    let browser: BrowserModel
    @State private var detail: RemoteNode?
    @State private var linkTarget: String?

    var body: some View {
        ZStack {
            if let node = browser.selected.first, browser.selected.count == 1 {
                ScrollView {
                    content(detail?.id == node.id ? detail ?? node : node)
                        .padding(18)
                }
                .task(id: node.id) { await load(node) }
            } else if browser.selected.count > 1 {
                Placard(symbol: "square.stack", title: "\(browser.selected.count) items",
                        detail: Format.bytes(browser.selected.reduce(0) { $0 + $1.size }))
            } else {
                Placard(symbol: "info.circle", title: "No selection")
            }
        }
    }

    private func load(_ node: RemoteNode) async {
        detail = nil
        linkTarget = nil
        if let fresh: RemoteNode = try? await browser.connection.request("stat", ["node": .uuid(node.node)]) {
            detail = fresh
        }
        if node.kind == .symlink {
            struct Reply: Decodable, Sendable { let target: String }
            let reply: Reply? = try? await browser.connection.request("read_link", ["node": .uuid(node.node)])
            linkTarget = reply?.target
        }
    }

    @ViewBuilder
    private func content(_ node: RemoteNode) -> some View {
        VStack(alignment: .leading, spacing: 18) {
            VStack(spacing: 10) {
                NodeIcon(node: node, size: 72)
                Text(node.displayName)
                    .font(.headline)
                    .multilineTextAlignment(.center)
                    .textSelection(.enabled)
                if !node.isDirectory {
                    Text(Format.bytes(node.size))
                        .font(.title2.weight(.semibold))
                        .monospacedDigit()
                        .contentTransition(.numericText())
                }
            }
            .frame(maxWidth: .infinity)

            Grid(alignment: .leading, horizontalSpacing: 12, verticalSpacing: 8) {
                if let created = node.created { row("calendar.badge.plus", created.formatted()) }
                row("pencil.and.outline", node.modified.formatted())
                row("eye", node.accessed.formatted())
                if !node.isDirectory {
                    row("internaldrive", Format.bytes(node.allocatedBytes))
                    row("number", "\(node.size) bytes")
                }
                row("link", "\(node.linkCount)")
                row("clock.arrow.circlepath", "r\(node.revision)")
                if let linkTarget { row("arrow.turn.up.right", linkTarget) }
                row("tag", node.node.uuidString.lowercased(), monospaced: true)
            }
            .font(.callout)

            Divider()
            PermissionEditor(node: node, browser: browser)
        }
    }

    private func row(_ symbol: String, _ value: String, monospaced: Bool = false) -> some View {
        GridRow {
            Image(systemName: symbol)
                .foregroundStyle(.secondary)
                .frame(width: 18)
            Text(value)
                .font(monospaced ? .caption.monospaced() : .callout)
                .textSelection(.enabled)
                .lineLimit(2)
        }
    }
}

struct NodeIcon: View {
    let node: RemoteNode
    var size: CGFloat = 18

    var body: some View {
        NodeLabelIcon(node: node)
            .frame(width: size, height: size)
    }
}

private struct NodeLabelIcon: View {
    let node: RemoteNode
    var body: some View {
        Image(nsImage: NSWorkspace.shared.icon(for: node.isDirectory ? .folder : .data))
            .resizable()
            .overlay {
                if !node.isDirectory {
                    let ext = (node.displayName as NSString).pathExtension
                    if let type = UTType(filenameExtension: ext) {
                        Image(nsImage: NSWorkspace.shared.icon(for: type)).resizable()
                    }
                }
            }
    }
}

/// An owner/group/other by read/write/execute grid of toggles.
private struct PermissionEditor: View {
    let node: RemoteNode
    let browser: BrowserModel
    @State private var mode: UInt32 = 0
    @State private var saving = false

    private let columns: [(String, UInt32)] = [("r", 4), ("w", 2), ("x", 1)]
    private let rows: [(String, UInt32)] = [("person", 6), ("person.2", 3), ("globe", 0)]

    var body: some View {
        VStack(alignment: .leading, spacing: 10) {
            Grid(horizontalSpacing: 10, verticalSpacing: 6) {
                ForEach(rows, id: \.1) { symbol, shift in
                    GridRow {
                        Image(systemName: symbol)
                            .foregroundStyle(.secondary)
                            .frame(width: 22)
                        ForEach(columns, id: \.1) { letter, bit in
                            let mask = bit << shift
                            Toggle(letter, isOn: Binding(
                                get: { mode & mask != 0 },
                                set: { mode = $0 ? mode | mask : mode & ~mask }
                            ))
                            .toggleStyle(.button)
                            .font(.callout.monospaced())
                            .disabled(!browser.writable)
                        }
                    }
                }
            }
            HStack {
                Text(String(format: "%04o", mode & 0o7777))
                    .font(.callout.monospaced())
                    .foregroundStyle(.secondary)
                Spacer()
                if mode != node.mode & 0o7777 {
                    Button("Apply") {
                        saving = true
                        Task {
                            _ = await browser.setMode(node, mode)
                            saving = false
                        }
                    }
                    .buttonStyle(.glassProminent)
                    .disabled(saving)
                }
            }
        }
        .onAppear { mode = node.mode & 0o7777 }
        .onChange(of: node) { mode = node.mode & 0o7777 }
    }
}

extension Data {
    init(hex: String) {
        var bytes = [UInt8]()
        bytes.reserveCapacity(hex.utf8.count / 2)
        var high: UInt8?
        for character in hex.utf8 {
            let value: UInt8
            switch character {
            case 48...57: value = character - 48
            case 97...102: value = character - 87
            case 65...70: value = character - 55
            default: continue
            }
            if let current = high {
                bytes.append(current << 4 | value)
                high = nil
            } else {
                high = value
            }
        }
        self.init(bytes)
    }
}
