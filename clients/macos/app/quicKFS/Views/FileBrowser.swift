// SPDX-License-Identifier: Apache-2.0
import AppKit
import SwiftUI
import UniformTypeIdentifiers

struct FileBrowser: View {
    let profile: ServerProfile
    let connection: ServerConnection
    let browser: BrowserModel
    @State private var inspecting = false
    @State private var prompt: NamePrompt?
    @State private var confirmingDelete: [RemoteNode] = []
    @State private var previewing: RemoteNode?
    @State private var dropTargeted = false

    var body: some View {
        Group {
            if !browser.entries.isEmpty || connection.isOnline {
                table
            } else if case .failed(let error) = connection.phase {
                Placard(symbol: "bolt.slash", title: "Unavailable", detail: error.message) {
                    Button("Retry", systemImage: "arrow.clockwise") { Task { await browser.reload() } }
                        .buttonStyle(.glass)
                }
            } else {
                ProgressView()
                    .controlSize(.large)
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
            }
        }
        .task(id: connection.isOnline) {
            if browser.entries.isEmpty { await browser.reload() }
        }
        .safeAreaInset(edge: .top, spacing: 0) { PathBar(browser: browser).padding(.horizontal, 12).padding(.top, 8) }
        .safeAreaInset(edge: .bottom, spacing: 0) {
            if !browser.transfers.isEmpty {
                TransferTray(browser: browser).padding(12)
            }
        }
        .inspector(isPresented: $inspecting) {
            NodeInspector(browser: browser)
                .inspectorColumnWidth(min: 240, ideal: 280, max: 360)
        }
        .toolbar { toolbar }
        .sheet(item: $prompt) { prompt in NameSheet(prompt: prompt, browser: browser) }
        .sheet(item: $previewing) { node in PreviewSheet(node: node, connection: connection) }
        .confirmationDialog(
            deleteTitle, isPresented: .init(
                get: { !confirmingDelete.isEmpty }, set: { if !$0 { confirmingDelete = [] } })
        ) {
            Button("Delete", role: .destructive) {
                let nodes = confirmingDelete
                Task { await browser.delete(nodes) }
            }
        } message: {
            Text("This cannot be undone.")
        }
        .alert(
            browser.error?.message ?? "", isPresented: .init(
                get: { browser.error != nil && !browser.entries.isEmpty },
                set: { if !$0 { browser.clearError() } })
        ) {
            Button("OK", role: .cancel) { browser.clearError() }
        }
    }

    private var deleteTitle: String {
        confirmingDelete.count == 1
            ? "Delete \u{201C}\(confirmingDelete[0].displayName)\u{201D}?"
            : "Delete \(confirmingDelete.count) items?"
    }

    private var table: some View {
        @Bindable var browser = browser
        return Table(browser.visibleEntries, selection: $browser.selection, sortOrder: $browser.sortOrder) {
            TableColumn("Name", value: \.displayName, comparator: .localizedStandard) { node in
                NodeLabel(node: node)
                    .draggable(NodeTransfer(id: node.node))
                    .dropDestination(for: NodeTransfer.self) { items, _ in
                        guard node.isDirectory, browser.writable else { return false }
                        let moving = browser.entries.filter { item in items.contains { $0.id == item.node } }
                        Task { await browser.move(moving, into: node) }
                        return true
                    }
            }
            TableColumn("Modified", value: \.modifiedMs) { node in
                Text(node.modified, format: .dateTime.year().month(.abbreviated).day().hour().minute())
                    .foregroundStyle(.secondary)
            }
            .width(min: 120, ideal: 160)
            TableColumn("Size", value: \.size) { node in
                Text(node.isDirectory ? "" : Format.bytes(node.size))
                    .foregroundStyle(.secondary)
                    .monospacedDigit()
            }
            .width(min: 60, ideal: 80)
        }
        .alternatingRowBackgrounds(.disabled)
        .contextMenu(forSelectionType: UUID.self) { ids in
            rowMenu(browser.entries.filter { ids.contains($0.id) })
        } primaryAction: { ids in
            guard let node = browser.entries.first(where: { ids.contains($0.id) }) else { return }
            if node.isDirectory {
                Task { await browser.open(node) }
            } else {
                previewing = node
            }
        }
        .dropDestination(for: URL.self) { urls, _ in
            guard browser.writable else { return false }
            browser.upload(urls.filter(\.isFileURL))
            return true
        } isTargeted: { dropTargeted = $0 }
        .overlay {
            if dropTargeted {
                RoundedRectangle(cornerRadius: 14)
                    .strokeBorder(Color.accentColor, style: .init(lineWidth: 2, dash: [8, 6]))
                    .padding(6)
                    .allowsHitTesting(false)
            } else if browser.visibleEntries.isEmpty && !browser.loading {
                Placard(symbol: "folder", title: "Empty")
                    .allowsHitTesting(false)
            }
        }
        .onDeleteCommand { if browser.writable { confirmingDelete = browser.selected } }
        .onKeyPress(.space) {
            guard let node = browser.selected.first, !node.isDirectory else { return .ignored }
            previewing = node
            return .handled
        }
    }

    @ViewBuilder
    private func rowMenu(_ nodes: [RemoteNode]) -> some View {
        if let node = nodes.first, nodes.count == 1 {
            if node.isDirectory {
                Button("Open", systemImage: "folder") { Task { await browser.open(node) } }
            } else {
                Button("Quick Look", systemImage: "eye") { previewing = node }
            }
        }
        if !nodes.isEmpty {
            Button("Download", systemImage: "arrow.down.circle") { download(nodes) }
        }
        if browser.writable, let node = nodes.first, nodes.count == 1 {
            Divider()
            Button("Rename…", systemImage: "pencil") { prompt = .rename(node) }
            if node.kind == .file {
                Button("Overwrite from File…", systemImage: "square.and.arrow.down.on.square") {
                    prompt = .write(node)
                }
                Button("Resize…", systemImage: "arrow.left.and.right") { prompt = .truncate(node) }
            }
        }
        if let node = nodes.first, nodes.count == 1 {
            Button("Copy Path", systemImage: "doc.on.doc") {
                let path = (browser.pathString as NSString).appendingPathComponent(node.displayName)
                NSPasteboard.general.clearContents()
                NSPasteboard.general.setString(path, forType: .string)
            }
            Button("Get Info", systemImage: "info.circle") {
                browser.selection = [node.id]
                inspecting = true
            }
        }
        if browser.writable, !nodes.isEmpty {
            Divider()
            Button("Delete…", systemImage: "trash", role: .destructive) { confirmingDelete = nodes }
        }
    }

    @ToolbarContentBuilder
    private var toolbar: some ToolbarContent {
        ToolbarItemGroup(placement: .navigation) {
            Button("Back", systemImage: "chevron.left") { Task { await browser.goBack() } }
                .disabled(browser.backStack.isEmpty)
            Button("Forward", systemImage: "chevron.right") { Task { await browser.goForward() } }
                .disabled(browser.forwardStack.isEmpty)
        }
        ToolbarItemGroup(placement: .secondaryAction) {
            Menu("New", systemImage: "plus") {
                Button("New Folder", systemImage: "folder.badge.plus") { prompt = .newFolder }
                Button("New File", systemImage: "doc.badge.plus") { prompt = .newFile }
            }
            .disabled(!browser.writable)
            .help("New")
            Button("Upload", systemImage: "arrow.up.circle") { chooseUploads() }
                .disabled(!browser.writable)
                .help("Upload")
            Button("Download", systemImage: "arrow.down.circle") { download(browser.selected) }
                .disabled(browser.selected.isEmpty)
                .help("Download")
            Button("Refresh", systemImage: "arrow.clockwise") { Task { await browser.reload() } }
                .keyboardShortcut("r")
                .help("Refresh")
            Button("Info", systemImage: "info.circle") { inspecting.toggle() }
                .keyboardShortcut("i")
                .help("Info")
        }
    }

    private func chooseUploads() {
        let panel = NSOpenPanel()
        panel.canChooseFiles = true
        panel.canChooseDirectories = true
        panel.allowsMultipleSelection = true
        panel.prompt = "Upload"
        guard panel.runModal() == .OK else { return }
        browser.upload(panel.urls)
    }

    private func download(_ nodes: [RemoteNode]) {
        guard !nodes.isEmpty else { return }
        let panel = NSOpenPanel()
        panel.canChooseFiles = false
        panel.canChooseDirectories = true
        panel.canCreateDirectories = true
        panel.prompt = "Download Here"
        panel.directoryURL = FileManager.default.urls(for: .downloadsDirectory, in: .userDomainMask).first
        guard panel.runModal() == .OK, let folder = panel.url else { return }
        browser.download(nodes, into: folder)
    }
}

/// Drag payload for moving entries between folders inside the browser.
struct NodeTransfer: Codable, Transferable {
    let id: UUID
    static var transferRepresentation: some TransferRepresentation {
        CodableRepresentation(contentType: .quickfsNode)
    }
}

extension UTType {
    static let quickfsNode = UTType(exportedAs: "org.quickfs.node")
}

struct NodeLabel: View {
    let node: RemoteNode

    var body: some View {
        HStack(spacing: 8) {
            Image(nsImage: icon)
                .resizable()
                .frame(width: 18, height: 18)
            Text(node.displayName)
                .lineLimit(1)
                .truncationMode(.middle)
            if node.kind == .symlink {
                Image(systemName: "arrow.turn.up.right")
                    .font(.caption2)
                    .foregroundStyle(.secondary)
            }
        }
    }

    private var icon: NSImage {
        switch node.kind {
        case .directory: NSWorkspace.shared.icon(for: .folder)
        case .symlink: NSWorkspace.shared.icon(for: .symbolicLink)
        case .file:
            NSWorkspace.shared.icon(
                for: UTType(filenameExtension: (node.displayName as NSString).pathExtension) ?? .data)
        default: NSWorkspace.shared.icon(for: .item)
        }
    }
}
