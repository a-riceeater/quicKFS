// SPDX-License-Identifier: Apache-2.0
import SwiftUI

/// Every download and upload this session, newest last, as in the Mac's tray.
struct TransfersView: View {
    let browser: BrowserModel
    let server: UUID
    @Environment(\.dismiss) private var dismiss
    @State private var opening: URL?

    var body: some View {
        NavigationStack {
            List {
                ForEach(browser.transfers) { item in
                    NavigationLink {
                        TransferDetailView(item: item) { opening = $0 }
                    } label: {
                        TransferRow(item: item)
                    }
                    .swipeActions {
                        if item.isRunning {
                            Button("Cancel", systemImage: "xmark", role: .destructive) { item.cancel() }
                        } else if item.canResume {
                            Button("Resume", systemImage: "arrow.clockwise") { item.resume() }
                                .tint(.orange)
                        }
                    }
                    .contextMenu {
                        if let destination = item.destination, item.status?.state == .finished {
                            Button("Open", systemImage: "eye") { opening = destination }
                            ShareLink(item: destination)
                        }
                    }
                }
            }
            .overlay {
                if browser.transfers.isEmpty { Placard(symbol: "arrow.up.arrow.down", title: "No transfers") }
            }
            .navigationTitle("Transfers")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Clear") {
                        browser.dismissFinishedTransfers()
                        // Resumable uploads still read their staged copy.
                        if !browser.transfers.contains(where: { $0.direction != .download }) {
                            Paths.clearStaging(for: server)
                        }
                    }
                    .disabled(!browser.transfers.contains { !$0.isRunning && !$0.canResume })
                }
                ToolbarItem(placement: .confirmationAction) { Button("Done") { dismiss() } }
            }
            .fullScreenCover(item: $opening) { url in
                NavigationStack {
                    QuickLookView(url: url)
                        .ignoresSafeArea(edges: .bottom)
                        .navigationTitle(url.lastPathComponent)
                        .navigationBarTitleDisplayMode(.inline)
                        .toolbar {
                            ToolbarItem(placement: .cancellationAction) {
                                Button("Close", systemImage: "xmark") { opening = nil }
                            }
                            ToolbarItem(placement: .primaryAction) { ShareLink(item: url) }
                        }
                }
            }
        }
    }
}

extension URL: @retroactive Identifiable {
    public var id: URL { self }
}

private struct TransferRow: View {
    let item: TransferItem

    var body: some View {
        HStack(spacing: 12) {
            Image(systemName: symbol)
                .foregroundStyle(tint)
                .font(.title2)
                .contentTransition(.symbolEffect(.replace))
            VStack(alignment: .leading, spacing: 5) {
                Text(item.title).lineLimit(1).truncationMode(.middle)
                if item.isRunning {
                    ProgressView(value: item.status?.fraction ?? 0)
                }
                Text(detail)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .monospacedDigit()
                    .lineLimit(2)
            }
        }
        .padding(.vertical, 2)
    }

    private var symbol: String {
        switch item.status?.state {
        case .finished: "checkmark.circle.fill"
        case .failed: item.canResume ? "pause.circle.fill" : "exclamationmark.circle.fill"
        case .cancelled: "minus.circle.fill"
        default:
            item.status?.retryInMs != nil ? "arrow.triangle.2.circlepath.circle"
                : item.direction == .download ? "arrow.down.circle" : "arrow.up.circle"
        }
    }

    private var tint: Color {
        switch item.status?.state {
        case .finished: .green
        case .failed: item.canResume ? .orange : .red
        case .cancelled: .secondary
        default: item.status?.retryInMs != nil ? .orange : .accentColor
        }
    }

    private var detail: String {
        guard let status = item.status else { return "Starting" }
        switch status.state {
        case .failed:
            return item.canResume
                ? "Stopped at \(Format.bytes(status.bytes)) of \(Format.bytes(status.total))"
                : status.error?.message ?? ""
        case .cancelled: return "Cancelled"
        case .finished: return Format.bytes(status.total)
        case .running where status.retryInMs != nil:
            return "Retrying in \(Int((Double(status.retryInMs ?? 0) / 1000).rounded(.up))) s"
        case .running:
            let files = status.filesTotal > 1 ? "\(status.files)/\(status.filesTotal) · " : ""
            let rate = item.currentRate > 0 ? " · \(Rate.text(item.currentRate))" : ""
            return files + "\(Format.bytes(status.bytes)) of \(Format.bytes(status.total))" + rate
        }
    }
}
