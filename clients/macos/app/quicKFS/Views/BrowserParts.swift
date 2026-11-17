// SPDX-License-Identifier: Apache-2.0
import AppKit
import SwiftUI

/// Breadcrumbs in a glass capsule. Clicking the capsule's tail edits the path.
struct PathBar: View {
    let browser: BrowserModel
    @State private var editing = false
    @State private var typed = ""
    @FocusState private var focused: Bool

    var body: some View {
        HStack(spacing: 2) {
            Button("Up", systemImage: "arrow.up") { Task { await browser.goUp() } }
                .labelStyle(.iconOnly)
                .buttonStyle(.borderless)
                .disabled(browser.path.count <= 1)
                .keyboardShortcut(.upArrow, modifiers: .command)
                .padding(.trailing, 6)
            if editing {
                TextField("Path", text: $typed)
                    .textFieldStyle(.plain)
                    .font(.callout.monospaced())
                    .focused($focused)
                    .onSubmit {
                        editing = false
                        Task { await browser.goTo(path: typed) }
                    }
                    .onExitCommand { editing = false }
                    .onChange(of: focused) { if !focused { editing = false } }
            } else {
                HStack(spacing: 2) {
                    ForEach(Array(browser.path.enumerated()), id: \.offset) { index, crumb in
                        if index > 0 {
                            Image(systemName: "chevron.compact.right")
                                .foregroundStyle(.tertiary)
                        }
                        Button {
                            Task { await browser.jump(to: crumb) }
                        } label: {
                            if index == 0 {
                                Image(systemName: "externaldrive")
                            } else {
                                Text(crumb.name).lineLimit(1).truncationMode(.middle)
                            }
                        }
                        .buttonStyle(.borderless)
                        .foregroundStyle(index == browser.path.count - 1 ? .primary : .secondary)
                        .layoutPriority(index == browser.path.count - 1 ? 1 : 0)
                    }
                }
                Color.clear
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
                    .contentShape(.rect)
                    .onTapGesture {
                        typed = browser.pathString
                        editing = true
                        focused = true
                    }
                    .help("Go to path")
            }
            if browser.loading {
                ProgressView().controlSize(.small).padding(.leading, 6)
            }
        }
        .font(.callout)
        .padding(.horizontal, 12)
        .frame(height: 32)
        .glassEffect(.regular, in: .capsule)
    }
}

struct TransferTray: View {
    let browser: BrowserModel

    var body: some View {
        GlassEffectContainer(spacing: 8) {
            VStack(spacing: 6) {
                ForEach(browser.transfers) { item in
                    TransferRow(item: item)
                }
                if browser.transfers.contains(where: { !$0.isRunning }) {
                    Button("Clear", systemImage: "xmark") { browser.dismissFinishedTransfers() }
                        .labelStyle(.iconOnly)
                        .buttonStyle(.glass)
                        .frame(maxWidth: .infinity, alignment: .trailing)
                }
            }
        }
        .frame(maxWidth: 520)
        .frame(maxWidth: .infinity, alignment: .trailing)
    }
}

private struct TransferRow: View {
    let item: TransferItem
    @Environment(\.openWindow) private var openWindow
    @State private var hovering = false

    var body: some View {
        HStack(spacing: 10) {
            Image(systemName: symbol)
                .foregroundStyle(tint)
                .font(.title3)
                .contentTransition(.symbolEffect(.replace))
            VStack(alignment: .leading, spacing: 4) {
                HStack {
                    Text(item.title).lineLimit(1).truncationMode(.middle)
                    Spacer()
                    Text(detail)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .monospacedDigit()
                }
                if item.isRunning {
                    ProgressView(value: item.status?.fraction ?? 0)
                        .progressViewStyle(.linear)
                        .controlSize(.small)
                }
            }
            Image(systemName: "chevron.right")
                .font(.caption.weight(.semibold))
                .foregroundStyle(.tertiary)
                .opacity(hovering ? 1 : 0.4)
            if item.isRunning {
                Button("Cancel", systemImage: "xmark.circle.fill") { item.cancel() }
                    .labelStyle(.iconOnly)
                    .buttonStyle(.borderless)
                    .foregroundStyle(.secondary)
            } else if let destination = item.destination, item.status?.state == .finished {
                Button("Show in Finder", systemImage: "magnifyingglass.circle.fill") {
                    NSWorkspace.shared.activateFileViewerSelecting([destination])
                }
                .labelStyle(.iconOnly)
                .buttonStyle(.borderless)
            }
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 10)
        .contentShape(.rect)
        .glassEffect(hovering ? .regular.tint(.accentColor.opacity(0.12)).interactive() : .regular.interactive(),
                     in: .rect(cornerRadius: 14))
        .onHover { hovering = $0 }
        .onTapGesture { openWindow(id: TransferWindow.id, value: item.id) }
        .help("Show transfer details")
        #if DEBUG
        .onAppear { if DebugHooks.showTransferDetail { openWindow(id: TransferWindow.id, value: item.id) } }
        #endif
        .animation(.smooth(duration: 0.15), value: hovering)
    }

    private var symbol: String {
        switch item.status?.state {
        case .finished: "checkmark.circle.fill"
        case .failed: "exclamationmark.circle.fill"
        case .cancelled: "minus.circle.fill"
        default: item.direction == .download ? "arrow.down.circle" : "arrow.up.circle"
        }
    }

    private var tint: Color {
        switch item.status?.state {
        case .finished: .green
        case .failed: .red
        case .cancelled: .secondary
        default: .accentColor
        }
    }

    private var detail: String {
        guard let status = item.status else { return "" }
        switch status.state {
        case .failed: return status.error?.message ?? ""
        case .cancelled: return ""
        case .finished: return Format.bytes(status.total)
        case .running:
            let files = status.filesTotal > 1 ? "\(status.files)/\(status.filesTotal)  " : ""
            let rate = item.rate > 0 ? "  \(Format.bytes(UInt64(item.rate)))/s" : ""
            return files + "\(Format.bytes(status.bytes)) / \(Format.bytes(status.total))" + rate
        }
    }
}

/// What a name sheet is asking for.
enum NamePrompt: Identifiable {
    case newFolder, newFile
    case rename(RemoteNode)
    case truncate(RemoteNode)
    case write(RemoteNode)

    var id: String {
        switch self {
        case .newFolder: "folder"
        case .newFile: "file"
        case .rename(let node): "rename-\(node.id)"
        case .truncate(let node): "truncate-\(node.id)"
        case .write(let node): "write-\(node.id)"
        }
    }
}

struct NameSheet: View {
    let prompt: NamePrompt
    let browser: BrowserModel
    @Environment(\.dismiss) private var dismiss
    @State private var text = ""
    @State private var sync = true
    @State private var source: URL?
    @State private var failure: String?
    @State private var working = false

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            Label(title, systemImage: symbol)
                .font(.title3.weight(.semibold))
            switch prompt {
            case .truncate(let node):
                TextField("Bytes", text: $text)
                    .textFieldStyle(.roundedBorder)
                    .monospacedDigit()
                Text("\(Format.bytes(node.size)) now")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            case .write:
                HStack {
                    Button(source?.lastPathComponent ?? "Choose File…", systemImage: "doc") { chooseSource() }
                    Spacer()
                }
                TextField("Offset", text: $text)
                    .textFieldStyle(.roundedBorder)
                    .monospacedDigit()
                Toggle("Sync to disk", isOn: $sync)
            default:
                TextField("Name", text: $text)
                    .textFieldStyle(.roundedBorder)
                    .onSubmit(commit)
            }
            if let failure {
                Text(failure).font(.callout).foregroundStyle(.red)
            }
            HStack {
                Spacer()
                Button("Cancel", role: .cancel) { dismiss() }
                    .keyboardShortcut(.cancelAction)
                Button(action: commit) {
                    if working { ProgressView().controlSize(.small) } else { Text(confirmTitle) }
                }
                .buttonStyle(.glassProminent)
                .keyboardShortcut(.defaultAction)
                .disabled(!valid || working)
            }
        }
        .padding(24)
        .frame(width: 380)
        .onAppear(perform: prefill)
    }

    private var title: String {
        switch prompt {
        case .newFolder: "New Folder"
        case .newFile: "New File"
        case .rename(let node): node.displayName
        case .truncate(let node): node.displayName
        case .write(let node): node.displayName
        }
    }

    private var symbol: String {
        switch prompt {
        case .newFolder: "folder.badge.plus"
        case .newFile: "doc.badge.plus"
        case .rename: "pencil"
        case .truncate: "arrow.left.and.right"
        case .write: "square.and.arrow.down.on.square"
        }
    }

    private var confirmTitle: String {
        switch prompt {
        case .newFolder, .newFile: "Create"
        case .rename: "Rename"
        case .truncate: "Resize"
        case .write: "Write"
        }
    }

    private var valid: Bool {
        switch prompt {
        case .truncate: UInt64(text) != nil
        case .write: source != nil && UInt64(text) != nil
        default:
            !text.isEmpty && !text.contains("/") && text != "." && text != ".."
        }
    }

    private func prefill() {
        switch prompt {
        case .newFolder: text = "untitled folder"
        case .newFile: text = "untitled"
        case .rename(let node): text = node.displayName
        case .truncate(let node): text = String(node.size)
        case .write: text = "0"
        }
    }

    private func chooseSource() {
        let panel = NSOpenPanel()
        panel.canChooseFiles = true
        panel.canChooseDirectories = false
        guard panel.runModal() == .OK else { return }
        source = panel.url
    }

    private func commit() {
        guard valid else { return }
        working = true
        Task {
            let error: CoreError? =
                switch prompt {
                case .newFolder: await browser.createFolder(named: text)
                case .newFile: await browser.createFile(named: text)
                case .rename(let node): await browser.rename(node, to: text)
                case .truncate(let node): await browser.truncate(node, to: UInt64(text) ?? node.size)
                case .write(let node):
                    { () -> CoreError? in
                        if let source { browser.write(source, into: node, offset: UInt64(text) ?? 0, sync: sync) }
                        return nil
                    }()
                }
            working = false
            if let error {
                failure = error.message
                browser.clearError()
            } else {
                dismiss()
            }
        }
    }
}
