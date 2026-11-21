// SPDX-License-Identifier: Apache-2.0
import SwiftUI

/// Pick a folder anywhere on the server to move entries into, the touch
/// form of dragging them onto a folder on the Mac.
struct MoveSheet: View {
    let nodes: [RemoteNode]
    let browser: BrowserModel
    @Environment(\.dismiss) private var dismiss
    @State private var path: [RemoteNode] = []

    var body: some View {
        NavigationStack(path: $path) {
            FolderPicker(node: nil, title: "/", nodes: nodes, browser: browser, done: { dismiss() })
                .navigationDestination(for: RemoteNode.self) { folder in
                    FolderPicker(node: folder, title: folder.displayName, nodes: nodes, browser: browser,
                                 done: { dismiss() })
                }
        }
    }
}

private struct FolderPicker: View {
    /// Nil for the root.
    let node: RemoteNode?
    let title: String
    let nodes: [RemoteNode]
    let browser: BrowserModel
    let done: () -> Void
    @State private var listing: DirectoryListing?
    @State private var failure: String?
    @State private var moving = false

    private var moved: Set<UUID> { Set(nodes.map(\.id)) }

    var body: some View {
        List {
            if let listing {
                ForEach(listing.entries.filter { $0.isDirectory && !$0.displayName.hasPrefix(".") }
                    .sorted { $0.displayName.localizedStandardCompare($1.displayName) == .orderedAscending }
                ) { folder in
                    NavigationLink(value: folder) { FileRow(node: folder) }
                        .disabled(moved.contains(folder.id))
                }
            }
        }
        .listStyle(.plain)
        .overlay {
            if let failure {
                Placard(symbol: "exclamationmark.triangle", title: "Could not open", detail: failure)
            } else if listing == nil {
                ProgressView()
            }
        }
        .navigationTitle(title)
        .navigationBarTitleDisplayMode(.inline)
        .toolbar {
            ToolbarItem(placement: .cancellationAction) { Button("Cancel", role: .cancel, action: done) }
            ToolbarItem(placement: .bottomBar) {
                Button {
                    guard let destination = listing?.directory else { return }
                    moving = true
                    Task {
                        await browser.move(nodes, into: destination)
                        done()
                    }
                } label: {
                    if moving { ProgressView() } else {
                        Text(nodes.count == 1 ? "Move \u{201C}\(nodes[0].displayName)\u{201D} Here" : "Move \(nodes.count) Items Here")
                    }
                }
                .glassButton(prominent: true)
                .disabled(!canMoveHere || moving)
            }
        }
        .task { await load() }
    }

    /// Not into the folder they are already in, nor into themselves.
    private var canMoveHere: Bool {
        guard let listing else { return false }
        return listing.directory.node != browser.directory && !moved.contains(listing.directory.node)
    }

    private func load() async {
        do {
            listing = try await browser.connection.request("list", ["node": .uuid(node?.node ?? RemoteNode.rootID)])
        } catch {
            failure = error.localizedDescription
        }
    }
}
