// SPDX-License-Identifier: Apache-2.0
import PhotosUI
import SwiftUI
import UniformTypeIdentifiers

/// What the browser is presenting. One object, shared by every folder on
/// the stack, so a sheet survives the folder that opened it.
@MainActor
@Observable
final class BrowserUI {
    var previewing: RemoteNode?
    var prompt: NamePrompt?
    var moving: [RemoteNode] = []
    var inspecting: RemoteNode?
    var confirmingDelete: [RemoteNode] = []
    var showingTransfers = false
    var panel: ServerPanel.Tab?
    var importingFiles = false
    var goingToPath = false
    var photoItems: [PhotosPickerItem] = []
    var staging = false
    var stageFailure: String?
}

/// One server's files: a navigation stack of folders that mirrors the
/// browser model's path, so pushing a folder opens it and swiping back
/// returns to its parent.
struct ServerBrowser: View {
    let profile: ServerProfile
    let state: ServerState
    @Environment(AppModel.self) private var model
    @State private var ui = BrowserUI()

    private var browser: BrowserModel { state.browser }

    var body: some View {
        NavigationStack(path: path) {
            FolderView(crumb: browser.path[0], title: profile.name, state: state)
                .environment(ui)
                .navigationDestination(for: BrowserModel.Crumb.self) { crumb in
                    // On iPhone the split view folds this stack into its own,
                    // and a pushed view inherits the split view's environment,
                    // not this stack's, so each folder gets the UI state itself.
                    FolderView(crumb: crumb, title: crumb.name, state: state)
                        .environment(ui)
                }
        }
        .environment(ui)
        .task(id: state.connection.isOnline) {
            if browser.entries.isEmpty { await browser.reload() }
            #if DEBUG
            await DebugHooks.drive(browser: browser, ui: ui)
            #endif
        }
        .fullScreenCover(item: $ui.previewing) { node in
            PreviewScreen(node: node, connection: state.connection, browser: browser,
                          cacheBytes: model.streamCacheBytes, log: state.log)
        }
        .sheet(item: $ui.prompt) { prompt in NamePromptSheet(prompt: prompt, browser: browser, server: profile.id) }
        .sheet(item: $ui.inspecting) { node in
            NodeInspectorView(node: node, browser: browser)
                .presentationDetents([.medium, .large])
        }
        .sheet(isPresented: moving) {
            MoveSheet(nodes: ui.moving, browser: browser)
        }
        .sheet(isPresented: $ui.showingTransfers) { TransfersView(browser: browser, server: profile.id) }
        .sheet(item: $ui.panel) { tab in ServerPanel(profile: profile, state: state, tab: tab) }
        .sheet(isPresented: $ui.goingToPath) { GoToPathSheet(browser: browser) }
        .fileImporter(isPresented: $ui.importingFiles, allowedContentTypes: [.item, .folder],
                      allowsMultipleSelection: true) { result in
            if case .success(let urls) = result { upload(urls) }
        }
        .onChange(of: ui.photoItems) { uploadPhotos() }
        .confirmationDialog(deleteTitle, isPresented: confirmingDelete, titleVisibility: .visible) {
            Button("Delete", role: .destructive) {
                let nodes = ui.confirmingDelete
                Task { await browser.delete(nodes) }
            }
        } message: {
            Text("This cannot be undone.")
        }
        .alert(browser.error?.message ?? "", isPresented: errorShown) {
            Button("OK", role: .cancel) { browser.clearError() }
        }
        .alert(ui.stageFailure ?? "", isPresented: .init(
            get: { ui.stageFailure != nil }, set: { if !$0 { ui.stageFailure = nil } })
        ) {
            Button("OK", role: .cancel) {}
        }
    }

    private var path: Binding<[BrowserModel.Crumb]> {
        Binding(
            get: { Array(browser.path.dropFirst()) },
            set: { browser.navigate(to: [browser.path[0]] + $0) })
    }

    private var moving: Binding<Bool> {
        .init(get: { !ui.moving.isEmpty }, set: { if !$0 { ui.moving = [] } })
    }

    private var confirmingDelete: Binding<Bool> {
        .init(get: { !ui.confirmingDelete.isEmpty }, set: { if !$0 { ui.confirmingDelete = [] } })
    }

    /// Errors with a listing to fall back on; a folder that failed to load
    /// shows its own placard instead.
    private var errorShown: Binding<Bool> {
        .init(get: { browser.error != nil && !browser.entries.isEmpty },
              set: { if !$0 { browser.clearError() } })
    }

    private var deleteTitle: String {
        ui.confirmingDelete.count == 1
            ? "Delete \u{201C}\(ui.confirmingDelete[0].displayName)\u{201D}?"
            : "Delete \(ui.confirmingDelete.count) items?"
    }

    private func upload(_ urls: [URL]) {
        ui.staging = true
        Task {
            defer { ui.staging = false }
            var staged: [URL] = []
            for url in urls {
                do {
                    let server = profile.id
                    let copy = try await Task.detached { try Paths.stage(url, for: server) }.value
                    staged.append(copy)
                } catch {
                    ui.stageFailure = "\(url.lastPathComponent): \(error.localizedDescription)"
                }
            }
            if !staged.isEmpty { browser.upload(staged) }
        }
    }

    private func uploadPhotos() {
        let items = ui.photoItems
        guard !items.isEmpty else { return }
        ui.photoItems = []
        ui.staging = true
        Task {
            defer { ui.staging = false }
            var staged: [URL] = []
            for item in items {
                do {
                    if let file = try await item.loadTransferable(type: StagedFile.self) {
                        staged.append(try Paths.adopt(file.url, for: profile.id))
                    }
                } catch {
                    ui.stageFailure = error.localizedDescription
                }
            }
            if !staged.isEmpty { browser.upload(staged) }
        }
    }
}

/// A photo or video from the library, copied into upload staging under its
/// original name.
struct StagedFile: Transferable {
    let url: URL

    static var transferRepresentation: some TransferRepresentation {
        FileRepresentation(importedContentType: .item) { received in
            let folder = Paths.uploadStaging.appendingPathComponent(UUID().uuidString, isDirectory: true)
            try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
            let copy = folder.appendingPathComponent(received.file.lastPathComponent)
            try FileManager.default.copyItem(at: received.file, to: copy)
            return StagedFile(url: copy)
        }
    }
}

// MARK: - Folder

struct FolderView: View {
    let crumb: BrowserModel.Crumb
    let title: String
    let state: ServerState
    @Environment(AppModel.self) private var model
    @Environment(BrowserUI.self) private var ui
    @State private var query = ""
    @State private var editMode = EditMode.inactive

    private var browser: BrowserModel { state.browser }
    private var connection: ServerConnection { state.connection }
    private var isCurrent: Bool { browser.directory == crumb.node }

    private var entries: [RemoteNode] {
        let all = browser.visibleEntries(in: crumb.node)
        guard !query.isEmpty else { return all }
        return all.filter { $0.displayName.localizedCaseInsensitiveContains(query) }
    }

    var body: some View {
        @Bindable var browser = browser
        List(selection: $browser.selection) {
            ForEach(entries) { node in
                row(node)
            }
        }
        .listStyle(.plain)
        .environment(\.editMode, $editMode)
        .overlay { overlay }
        .navigationTitle(title)
        .navigationBarTitleDisplayMode(crumb.node == RemoteNode.rootID ? .large : .inline)
        .searchable(text: $query, placement: .navigationBarDrawer(displayMode: .automatic))
        .refreshable { if isCurrent { await browser.reload() } }
        .toolbar { toolbar }
        .toolbarTitleMenu { titleMenu }
        .safeAreaInset(edge: .bottom, spacing: 0) { bottomBar }
        .onChange(of: isCurrent) { if !isCurrent { editMode = .inactive } }
        .onChange(of: editMode) { if !editMode.isEditing { browser.selection = [] } }
    }

    @ViewBuilder
    private func row(_ node: RemoteNode) -> some View {
        Group {
            if editMode.isEditing {
                FileRow(node: node)
            } else if node.isDirectory {
                NavigationLink(value: BrowserModel.Crumb(node: node.node, name: node.displayName)) {
                    FileRow(node: node)
                }
            } else {
                Button { ui.previewing = node } label: { FileRow(node: node) }
                    .tint(.primary)
            }
        }
        .tag(node.id)
        .contextMenu { RowMenu(nodes: [node], browser: browser) }
        .swipeActions(edge: .trailing) {
            if browser.writable {
                Button("Delete", systemImage: "trash", role: .destructive) { ui.confirmingDelete = [node] }
                Button("Rename", systemImage: "pencil") { ui.prompt = .rename(node) }
                    .tint(.indigo)
            }
        }
        .swipeActions(edge: .leading) {
            Button("Download", systemImage: "arrow.down.circle") {
                browser.download([node], into: Paths.downloads)
            }
            .tint(.accentColor)
        }
    }

    @ViewBuilder
    private var overlay: some View {
        if !isCurrent {
            EmptyView()
        } else if browser.entries.isEmpty && !connection.isOnline {
            if case .failed = connection.phase {
                Placard(symbol: "bolt.slash", title: "Unavailable")
            } else {
                ProgressView().controlSize(.large)
            }
        } else if browser.loading && browser.entries.isEmpty {
            ProgressView().controlSize(.large)
        } else if let error = browser.error, browser.entries.isEmpty {
            Placard(symbol: "exclamationmark.triangle", title: "Could not open", detail: error.message) {
                Button("Try Again") { Task { await browser.reload() } }.glassButton()
            }
        } else if entries.isEmpty {
            Placard(symbol: query.isEmpty ? "folder" : "magnifyingglass",
                    title: query.isEmpty ? "Empty" : "No matches")
        }
    }

    @ToolbarContentBuilder
    private var toolbar: some ToolbarContent {
        ToolbarItemGroup(placement: .topBarTrailing) {
            if !browser.transfers.isEmpty {
                TransfersButton(browser: browser) { ui.showingTransfers = true }
            }
            if editMode.isEditing {
                Button("Done") { withAnimation { editMode = .inactive } }
                    .fontWeight(.semibold)
            } else {
                moreMenu
            }
        }
    }

    private var moreMenu: some View {
        @Bindable var ui = ui
        @Bindable var browser = browser
        return Menu("More", systemImage: "ellipsis") {
            Section {
                Button("Select", systemImage: "checkmark.circle") { withAnimation { editMode = .active } }
                    .disabled(entries.isEmpty)
            }
            if browser.writable {
                Section {
                    Button("New Folder", systemImage: "folder.badge.plus") { ui.prompt = .newFolder }
                    Button("New File", systemImage: "doc.badge.plus") { ui.prompt = .newFile }
                    Button("Upload Files", systemImage: "square.and.arrow.up") { ui.importingFiles = true }
                    PhotosPicker(selection: $ui.photoItems, matching: .any(of: [.images, .videos]),
                                 preferredItemEncoding: .current) {
                        Label("Upload Photos", systemImage: "photo.on.rectangle")
                    }
                }
            }
            Section {
                Picker("Sort", selection: sortKey) {
                    Label("Name", systemImage: "textformat").tag(SortKey.name)
                    Label("Date", systemImage: "calendar").tag(SortKey.date)
                    Label("Size", systemImage: "internaldrive").tag(SortKey.size)
                }
                Toggle("Show Hidden Files", systemImage: "eye", isOn: $browser.showHidden)
            }
            Section {
                Button("Overview", systemImage: "gauge.with.dots.needle.67percent") { ui.panel = .overview }
                Button("Console", systemImage: "terminal") { ui.panel = .console }
                Button("Server Settings", systemImage: "slider.horizontal.3") { ui.panel = .settings }
            }
        }
    }

    @ViewBuilder
    private var titleMenu: some View {
        ForEach(Array(browser.path.enumerated().reversed()), id: \.offset) { index, crumb in
            if index < browser.path.count - 1 {
                Button(index == 0 ? "/" : crumb.name, systemImage: index == 0 ? "externaldrive" : "folder") {
                    browser.navigate(to: Array(browser.path[...index]))
                }
            }
        }
        Button("Go to Path", systemImage: "arrow.right.circle") { ui.goingToPath = true }
        Button("Copy Path", systemImage: "doc.on.doc") { UIPasteboard.general.string = browser.pathString }
    }

    @ViewBuilder
    private var bottomBar: some View {
        VStack(spacing: 10) {
            if let failure = state.failure, isCurrent {
                FailureBanner(message: failure, needsPassword: state.needsPassword) {
                    Task { await browser.reload() }
                } fix: {
                    if state.needsPassword { model.passwordRequest = state.id } else { ui.panel = .settings }
                }
                .transition(.move(edge: .bottom).combined(with: .opacity))
            }
            if ui.staging {
                Label("Preparing upload", systemImage: "square.and.arrow.up")
                    .font(.callout)
                    .padding(.horizontal, 16)
                    .padding(.vertical, 10)
                    .glassSurface()
            }
            if editMode.isEditing {
                SelectionBar(browser: browser)
            }
        }
        .padding(.horizontal, 16)
        .padding(.bottom, 8)
        .animation(.smooth, value: state.failure)
    }

    private enum SortKey: Hashable { case name, date, size }
    private static let byDate = KeyPathComparator(\RemoteNode.modifiedMs, order: .reverse)
    private static let bySize = KeyPathComparator(\RemoteNode.size, order: .reverse)

    private var sortKey: Binding<SortKey> {
        Binding(
            get: {
                switch browser.sortOrder.first {
                case Self.byDate: .date
                case Self.bySize: .size
                default: .name
                }
            },
            set: { key in
                switch key {
                case .name:
                    browser.sortOrder = [KeyPathComparator(\RemoteNode.displayName, comparator: .localizedStandard)]
                case .date: browser.sortOrder = [Self.byDate]
                case .size: browser.sortOrder = [Self.bySize]
                }
            })
    }
}

struct FileRow: View {
    let node: RemoteNode

    var body: some View {
        HStack(spacing: 12) {
            NodeIcon(node: node, size: 36)
            VStack(alignment: .leading, spacing: 3) {
                HStack(spacing: 4) {
                    Text(node.displayName)
                        .lineLimit(1)
                        .truncationMode(.middle)
                    if node.kind == .symlink {
                        Image(systemName: "arrow.turn.up.right")
                            .font(.caption2)
                            .foregroundStyle(.secondary)
                    }
                }
                Text(subtitle)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .monospacedDigit()
            }
        }
        .padding(.vertical, 2)
        .contentShape(.rect)
    }

    private var subtitle: String {
        let date = node.modified.formatted(.dateTime.year().month(.abbreviated).day().hour().minute())
        return node.isDirectory ? date : "\(Format.bytes(node.size)) · \(date)"
    }
}

/// Everything that can be done to one or more entries, as on the Mac.
struct RowMenu: View {
    let nodes: [RemoteNode]
    let browser: BrowserModel
    @Environment(BrowserUI.self) private var ui

    var body: some View {
        if let node = nodes.first, nodes.count == 1 {
            if node.isDirectory {
                Button("Open", systemImage: "folder") {
                    browser.navigate(to: browser.path + [BrowserModel.Crumb(node: node.node, name: node.displayName)])
                }
            } else {
                Button("Preview", systemImage: "eye") { ui.previewing = node }
            }
        }
        Button("Download", systemImage: "arrow.down.circle") { browser.download(nodes, into: Paths.downloads) }
        if browser.writable, let node = nodes.first, nodes.count == 1 {
            Section {
                Button("Rename", systemImage: "pencil") { ui.prompt = .rename(node) }
                Button("Move", systemImage: "folder") { ui.moving = nodes }
                if node.kind == .file {
                    Button("Overwrite from File", systemImage: "square.and.arrow.down.on.square") {
                        ui.prompt = .write(node)
                    }
                    Button("Resize", systemImage: "arrow.left.and.right") { ui.prompt = .truncate(node) }
                }
            }
        }
        if let node = nodes.first, nodes.count == 1 {
            Section {
                Button("Copy Path", systemImage: "doc.on.doc") {
                    UIPasteboard.general.string = (browser.pathString as NSString).appendingPathComponent(node.displayName)
                }
                Button("Get Info", systemImage: "info.circle") { ui.inspecting = node }
            }
        }
        if browser.writable, !nodes.isEmpty {
            Section {
                Button("Delete", systemImage: "trash", role: .destructive) { ui.confirmingDelete = nodes }
            }
        }
    }
}

/// Actions for the entries picked in selection mode.
private struct SelectionBar: View {
    let browser: BrowserModel
    @Environment(BrowserUI.self) private var ui

    var body: some View {
        let selected = browser.selected
        HStack(spacing: 18) {
            Text(selected.isEmpty ? "Select items" : "\(selected.count) selected")
                .font(.callout.weight(.medium))
                .monospacedDigit()
            Spacer()
            Button("Download", systemImage: "arrow.down.circle") {
                browser.download(selected, into: Paths.downloads)
                browser.selection = []
            }
            if browser.writable {
                Button("Move", systemImage: "folder") { ui.moving = selected }
                Button("Delete", systemImage: "trash", role: .destructive) { ui.confirmingDelete = selected }
                    .tint(.red)
            }
        }
        .labelStyle(.iconOnly)
        .font(.title3)
        .disabled(selected.isEmpty)
        .padding(.horizontal, 20)
        .padding(.vertical, 12)
        .glassSurface()
    }
}

/// A server failure with a retry and a way to fix what caused it.
struct FailureBanner: View {
    let message: String
    let needsPassword: Bool
    let retry: () -> Void
    let fix: () -> Void

    var body: some View {
        HStack(spacing: 12) {
            Image(systemName: "exclamationmark.triangle.fill")
                .foregroundStyle(.orange)
            Text(message)
                .font(.callout)
                .lineLimit(3)
                .frame(maxWidth: .infinity, alignment: .leading)
            Button("Fix", systemImage: needsPassword ? "key.fill" : "wrench.and.screwdriver", action: fix)
                .labelStyle(.iconOnly)
            Button("Retry", systemImage: "arrow.clockwise", action: retry)
                .labelStyle(.iconOnly)
        }
        .padding(.horizontal, 16)
        .padding(.vertical, 12)
        .glassSurface(RoundedRectangle(cornerRadius: 20, style: .continuous), tint: .orange.opacity(0.12))
    }
}

/// A ring of overall progress that opens the transfer list.
private struct TransfersButton: View {
    let browser: BrowserModel
    let action: () -> Void

    var body: some View {
        let running = browser.transfers.filter(\.isRunning)
        let bytes = running.reduce(UInt64(0)) { $0 + ($1.status?.bytes ?? 0) }
        let total = running.reduce(UInt64(0)) { $0 + ($1.status?.total ?? 0) }
        let failed = browser.transfers.contains { $0.status?.state == .failed }
        Button(action: action) {
            ZStack {
                Circle().stroke(.quaternary, lineWidth: 3)
                Circle()
                    .trim(from: 0, to: running.isEmpty ? 1 : total == 0 ? 0.02 : Double(bytes) / Double(total))
                    .stroke(failed ? Color.orange : running.isEmpty ? .green : .accentColor,
                            style: .init(lineWidth: 3, lineCap: .round))
                    .rotationEffect(.degrees(-90))
                Image(systemName: running.isEmpty ? (failed ? "exclamationmark" : "checkmark") : "arrow.up.arrow.down")
                    .font(.system(size: 10, weight: .bold))
            }
            .frame(width: 24, height: 24)
            .animation(.smooth, value: bytes)
        }
        .accessibilityLabel("Transfers")
    }
}

/// Type an absolute path, as with the CLI's paths.
private struct GoToPathSheet: View {
    let browser: BrowserModel
    @Environment(\.dismiss) private var dismiss
    @State private var text = ""

    var body: some View {
        NavigationStack {
            Form {
                TextField("/path/to/folder", text: $text)
                    .font(.body.monospaced())
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                    .submitLabel(.go)
                    .onSubmit(go)
            }
            .navigationTitle("Go to Path")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Cancel", role: .cancel) { dismiss() } }
                ToolbarItem(placement: .confirmationAction) { Button("Go", action: go).disabled(text.isEmpty) }
            }
        }
        .presentationDetents([.height(200)])
        .onAppear { text = browser.pathString }
    }

    private func go() {
        let target = text
        dismiss()
        Task { await browser.goTo(path: target) }
    }
}
