// SPDX-License-Identifier: Apache-2.0
import SwiftUI

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

struct NamePromptSheet: View {
    let prompt: NamePrompt
    let browser: BrowserModel
    let server: UUID
    @Environment(\.dismiss) private var dismiss
    @State private var text = ""
    @State private var sync = true
    @State private var source: URL?
    @State private var choosingSource = false
    @State private var failure: String?
    @State private var working = false
    @FocusState private var focused: Bool

    var body: some View {
        NavigationStack {
            Form {
                switch prompt {
                case .truncate(let node):
                    Section {
                        TextField("Bytes", text: $text)
                            .keyboardType(.numberPad)
                            .monospacedDigit()
                            .focused($focused)
                    } footer: {
                        Text("\(Format.bytes(node.size)) now")
                    }
                case .write:
                    Section {
                        Button(source?.lastPathComponent ?? "Choose File", systemImage: "doc") { choosingSource = true }
                        LabeledContent("Offset") {
                            TextField("0", text: $text)
                                .keyboardType(.numberPad)
                                .multilineTextAlignment(.trailing)
                                .monospacedDigit()
                        }
                        Toggle("Sync to disk", isOn: $sync)
                    }
                default:
                    Section {
                        TextField("Name", text: $text)
                            .textInputAutocapitalization(.never)
                            .autocorrectionDisabled()
                            .submitLabel(.done)
                            .focused($focused)
                            .onSubmit(commit)
                    }
                }
                if let failure {
                    Section { Text(failure).foregroundStyle(.red) }
                }
            }
            .navigationTitle(title)
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel", role: .cancel) { dismiss() }
                }
                ToolbarItem(placement: .confirmationAction) {
                    if working {
                        ProgressView()
                    } else {
                        Button(confirmTitle, action: commit).disabled(!valid)
                    }
                }
            }
            .fileImporter(isPresented: $choosingSource, allowedContentTypes: [.item]) { result in
                if case .success(let url) = result {
                    do { source = try Paths.stage(url, for: server) } catch { failure = error.localizedDescription }
                }
            }
        }
        .presentationDetents([.medium])
        .onAppear(perform: prefill)
    }

    private var title: String {
        switch prompt {
        case .newFolder: "New Folder"
        case .newFile: "New File"
        case .rename(let node), .truncate(let node), .write(let node): node.displayName
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
        default: !text.isEmpty && !text.contains("/") && text != "." && text != ".."
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
        focused = true
    }

    private func commit() {
        guard valid, !working else { return }
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
