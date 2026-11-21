// SPDX-License-Identifier: Apache-2.0
import AVKit
import QuickLook
import SwiftUI

/// A remote file, shown the fastest way its type allows.
///
/// Video and audio stream straight into the player, so playback starts
/// after the first block instead of after the whole file. Images stream into
/// memory. Everything else downloads to a private temporary file for Quick
/// Look, as on the Mac. The first bytes are always one tap away as hex.
///
/// The screen owns the player and the document fetch and ends them only when
/// the preview closes. Not on `onDisappear`: entering full screen video, or
/// any full-screen presentation, takes this screen out of the window, and
/// SwiftUI reports that as disappearing while playback must go on.
struct PreviewScreen: View {
    enum Mode { case media, image, document }

    let node: RemoteNode
    let connection: ServerConnection
    let browser: BrowserModel
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    @State private var mode: Mode
    @State private var playback: MediaPlayback?
    @State private var document: PreviewLoader?
    @State private var showsBytes = false
    @State private var downloaded = false

    init(node: RemoteNode, connection: ServerConnection, browser: BrowserModel, cacheBytes: UInt64) {
        self.node = node
        self.connection = connection
        self.browser = browser
        let mode = Self.mode(for: node)
        _mode = State(initialValue: mode)
        _playback = State(initialValue: mode == .media
            ? MediaPlayback(node: node, connection: connection, cacheBytes: cacheBytes) : nil)
        _document = State(initialValue: mode == .document ? PreviewLoader(node: node, connection: connection) : nil)
    }

    static func mode(for node: RemoteNode) -> Mode {
        if MediaPlayback.canPlay(node) { return .media }
        if FileKind(node) == .image, node.size <= ImageLoader.limit { return .image }
        return .document
    }

    var body: some View {
        NavigationStack {
            content
                .navigationTitle(node.displayName)
                .navigationBarTitleDisplayMode(.inline)
                .toolbar {
                    ToolbarItem(placement: .cancellationAction) {
                        Button("Close", systemImage: "xmark", action: close)
                    }
                    ToolbarItemGroup(placement: .primaryAction) {
                        Button("Bytes", systemImage: "number") { showsBytes = true }
                        Button("Download", systemImage: downloaded ? "checkmark.circle" : "arrow.down.circle") {
                            browser.download([node], into: Paths.downloads)
                            withAnimation { downloaded = true }
                        }
                        .disabled(downloaded)
                    }
                }
                .sheet(isPresented: $showsBytes) {
                    HexSheet(node: node, connection: connection)
                        .presentationDetents([.medium, .large])
                }
        }
    }

    @ViewBuilder
    private var content: some View {
        switch mode {
        case .media:
            if let playback { MediaPreview(playback: playback, fallback: openAsDocument) }
        case .image:
            ImagePreview(node: node, connection: connection, cacheBytes: model.streamCacheBytes,
                         fallback: openAsDocument)
        case .document:
            if let document { DocumentPreview(loader: document) }
        }
    }

    /// Release the stream or the temporary copy, then leave.
    private func close() {
        playback?.close()
        document?.close()
        dismiss()
    }

    private func openAsDocument() {
        playback?.close()
        playback = nil
        if document == nil { document = PreviewLoader(node: node, connection: connection) }
        mode = .document
    }
}

extension View {
    /// Run `action` the first time this view appears, and never again when it
    /// reappears after a full-screen presentation covered it.
    func onFirstAppear(_ action: @escaping () -> Void) -> some View {
        modifier(FirstAppear(action: action))
    }
}

private struct FirstAppear: ViewModifier {
    let action: () -> Void
    @State private var done = false

    func body(content: Content) -> some View {
        content.onAppear {
            guard !done else { return }
            done = true
            action()
        }
    }
}

// MARK: - Media

private struct MediaPreview: View {
    let playback: MediaPlayback
    let fallback: () -> Void

    var body: some View {
        ZStack {
            Color.black.ignoresSafeArea()
            switch playback.phase {
            case .opening:
                ProgressView().tint(.white).controlSize(.large)
            case .ready(let player):
                PlayerView(player: player).ignoresSafeArea(edges: .bottom)
            case .failed(let message):
                Placard(symbol: "play.slash", title: "Cannot play", detail: message) {
                    Button("Open as Document", action: fallback).glassButton()
                }
                .environment(\.colorScheme, .dark)
            }
        }
        // Not `.task`: hiding the view cancels a task, and the player must
        // keep its stream while full screen covers this view.
        .onFirstAppear { Task { await playback.open() } }
    }
}

/// The system player: scrubbing, Picture in Picture, AirPlay, full screen.
private struct PlayerView: UIViewControllerRepresentable {
    let player: AVPlayer

    func makeUIViewController(context: Context) -> AVPlayerViewController {
        let controller = AVPlayerViewController()
        controller.player = player
        controller.allowsPictureInPicturePlayback = true
        controller.canStartPictureInPictureAutomaticallyFromInline = true
        #if DEBUG
        DebugHooks.enterFullScreenIfRequested(controller)
        #endif
        return controller
    }

    func updateUIViewController(_ controller: AVPlayerViewController, context: Context) {
        if controller.player !== player { controller.player = player }
    }
}

// MARK: - Image

private struct ImagePreview: View {
    @State private var loader: ImageLoader
    let fallback: () -> Void

    init(node: RemoteNode, connection: ServerConnection, cacheBytes: UInt64, fallback: @escaping () -> Void) {
        _loader = State(initialValue: ImageLoader(node: node, connection: connection, cacheBytes: cacheBytes))
        self.fallback = fallback
    }

    var body: some View {
        ZStack {
            switch loader.phase {
            case .loading:
                VStack(spacing: 14) {
                    ProgressView(value: loader.fraction)
                        .progressViewStyle(.circular)
                        .controlSize(.large)
                    Text("\(Format.bytes(loader.received)) of \(Format.bytes(loader.node.size))")
                        .font(.callout.monospacedDigit())
                        .foregroundStyle(.secondary)
                }
            case .ready(let image):
                ZoomableImage(image: image).ignoresSafeArea(edges: .bottom)
            case .failed(let message):
                Placard(symbol: "photo.badge.exclamationmark", title: "No preview", detail: message) {
                    Button("Open as Document", action: fallback).glassButton()
                }
            }
        }
        .task { await loader.load() }
    }
}

/// Pinch and double-tap zoom, as in Photos.
private struct ZoomableImage: UIViewRepresentable {
    let image: UIImage

    func makeCoordinator() -> Coordinator { Coordinator() }

    func makeUIView(context: Context) -> UIScrollView {
        let scroll = UIScrollView()
        scroll.delegate = context.coordinator
        scroll.maximumZoomScale = 8
        scroll.minimumZoomScale = 1
        scroll.showsVerticalScrollIndicator = false
        scroll.showsHorizontalScrollIndicator = false
        scroll.contentInsetAdjustmentBehavior = .never
        let view = UIImageView(image: image)
        view.contentMode = .scaleAspectFit
        view.frame = scroll.bounds
        view.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        scroll.addSubview(view)
        context.coordinator.imageView = view
        let doubleTap = UITapGestureRecognizer(target: context.coordinator, action: #selector(Coordinator.toggleZoom(_:)))
        doubleTap.numberOfTapsRequired = 2
        scroll.addGestureRecognizer(doubleTap)
        return scroll
    }

    func updateUIView(_ scroll: UIScrollView, context: Context) {
        context.coordinator.imageView?.image = image
    }

    @MainActor
    final class Coordinator: NSObject, UIScrollViewDelegate {
        weak var imageView: UIImageView?

        func viewForZooming(in scrollView: UIScrollView) -> UIView? { imageView }

        @objc func toggleZoom(_ gesture: UITapGestureRecognizer) {
            guard let scroll = gesture.view as? UIScrollView else { return }
            if scroll.zoomScale > 1 {
                scroll.setZoomScale(1, animated: true)
            } else {
                let point = gesture.location(in: imageView)
                let size = CGSize(width: scroll.bounds.width / 3, height: scroll.bounds.height / 3)
                scroll.zoom(to: CGRect(x: point.x - size.width / 2, y: point.y - size.height / 2,
                                       width: size.width, height: size.height), animated: true)
            }
        }
    }
}

// MARK: - Document

private struct DocumentPreview: View {
    let loader: PreviewLoader

    var body: some View {
        ZStack {
            switch loader.phase {
            case .loading:
                LoadingStatus(loader: loader)
            case .confirm:
                Placard(
                    symbol: "arrow.down.circle", title: "Large file",
                    detail: loader.fitsOnDisk
                        ? "Previewing downloads all \(Format.bytes(loader.node.size)) first."
                        : "Not enough free space to preview \(Format.bytes(loader.node.size))."
                ) {
                    Button("Load Preview") { Task { await loader.loadAnyway() } }
                        .glassButton(prominent: true)
                        .disabled(!loader.fitsOnDisk)
                }
            case .ready(let url):
                if QLPreviewController.canPreview(url as NSURL) {
                    QuickLookView(url: url).ignoresSafeArea(edges: .bottom)
                } else {
                    Placard(symbol: "eye.slash", title: "No preview",
                            detail: "Another app may be able to open it.") {
                        ShareLink(item: url) { Label("Open In", systemImage: "square.and.arrow.up") }
                            .glassButton(prominent: true)
                    }
                }
            case .failed(let message):
                Placard(symbol: "eye.slash", title: "No preview", detail: message) {
                    Button("Try Again") { Task { await loader.loadAnyway() } }.glassButton()
                }
            }
        }
        .toolbar {
            if case .ready(let url) = loader.phase {
                ToolbarItem(placement: .bottomBar) {
                    ShareLink(item: url)
                }
            }
        }
        .onFirstAppear { Task { await loader.open() } }
    }
}

/// What the fetch is doing right now, so opening a file is never a blank wait.
private struct LoadingStatus: View {
    let loader: PreviewLoader

    var body: some View {
        VStack(spacing: 18) {
            NodeIcon(node: loader.node, size: 64)
            VStack(spacing: 8) {
                Group {
                    if let fraction = loader.fraction, (loader.status?.bytes ?? 0) > 0 {
                        ProgressView(value: fraction)
                    } else {
                        ProgressView().progressViewStyle(.linear)
                    }
                }
                HStack {
                    Text(amount)
                    Spacer(minLength: 12)
                    if loader.displayRate > 0 { Text(Rate.text(loader.displayRate)) }
                }
                .font(.callout.monospacedDigit())
                .foregroundStyle(.secondary)
                .lineLimit(1)
                Text(detail)
                    .font(.caption.monospacedDigit())
                    .foregroundStyle(.tertiary)
                    .lineLimit(2)
                    .multilineTextAlignment(.center)
            }
            .frame(maxWidth: 340)
        }
        .padding(.horizontal, 28)
    }

    private var amount: String {
        guard let status = loader.status, status.bytes > 0 else { return "Opening" }
        return "\(Format.bytes(status.bytes)) of \(Format.bytes(max(status.total, loader.node.size)))"
    }

    private var detail: String {
        if let wait = loader.status?.retryInMs {
            return "Retrying in \(Clock.duration(Double(wait) / 1000))"
        }
        var parts: [String] = []
        if let remaining = loader.remaining, remaining >= 1 {
            parts.append("\(Clock.duration(remaining)) left")
        } else if let started = loader.started {
            parts.append(Clock.duration(Date.now.timeIntervalSince(started)))
        }
        if let connections = loader.status?.connections, connections > 1 {
            parts.append("\(connections) connections")
        }
        return parts.joined(separator: " · ")
    }
}

/// The system Quick Look viewer for a local file.
struct QuickLookView: UIViewControllerRepresentable {
    let url: URL

    func makeCoordinator() -> Source { Source(url: url) }

    func makeUIViewController(context: Context) -> QLPreviewController {
        let controller = QLPreviewController()
        controller.dataSource = context.coordinator
        return controller
    }

    func updateUIViewController(_ controller: QLPreviewController, context: Context) {
        if context.coordinator.url != url {
            context.coordinator.url = url
            controller.reloadData()
        }
    }

    final class Source: NSObject, QLPreviewControllerDataSource {
        var url: URL

        init(url: URL) { self.url = url }

        func numberOfPreviewItems(in controller: QLPreviewController) -> Int { 1 }

        func previewController(_ controller: QLPreviewController, previewItemAt index: Int) -> QLPreviewItem {
            url as NSURL
        }
    }
}

// MARK: - Bytes

/// The file's first bytes as a hex dump: offset, hex in two groups of
/// eight, and the printable text, one row per 16 bytes.
private struct HexSheet: View {
    let node: RemoteNode
    let connection: ServerConnection
    @Environment(\.dismiss) private var dismiss
    @State private var rows: [HexRow]?
    @State private var failed = false

    var body: some View {
        NavigationStack {
            ZStack {
                if let rows {
                    if rows.isEmpty {
                        Placard(symbol: "doc", title: "Empty file")
                    } else {
                        ScrollView([.vertical, .horizontal]) {
                            Text(Self.dump(rows))
                                .font(.system(size: 11, design: .monospaced))
                                .lineSpacing(2)
                                .textSelection(.enabled)
                                .fixedSize()
                                .padding(16)
                        }
                    }
                } else if failed {
                    Placard(symbol: "number", title: "Unavailable")
                } else {
                    ProgressView()
                }
            }
            .navigationTitle(rows.map { "First \(Format.bytes(UInt64($0.count) * 16))" } ?? "Bytes")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) { Button("Done") { dismiss() } }
            }
            .task { await load() }
        }
    }

    private func load() async {
        struct Reply: Decodable, Sendable { let data: String }
        do {
            let reply: Reply = try await connection.request(
                "read", ["node": .uuid(node.node), "offset": 0, "length": .uint(PreviewLoader.headLength)])
            rows = PreviewLoader.hexRows(Data(hex: reply.data))
        } catch {
            failed = true
        }
    }

    /// One selectable block, with offsets and text dimmer than the bytes.
    private static func dump(_ rows: [HexRow]) -> AttributedString {
        var result = AttributedString()
        for (index, row) in rows.enumerated() {
            var offset = AttributedString(row.offset + "  ")
            offset.foregroundColor = Color(uiColor: .tertiaryLabel)
            var hex = AttributedString(row.hex + "  ")
            hex.foregroundColor = Color(uiColor: .label)
            var text = AttributedString(row.text + (index == rows.count - 1 ? "" : "\n"))
            text.foregroundColor = Color(uiColor: .secondaryLabel)
            result += offset
            result += hex
            result += text
        }
        return result
    }
}
