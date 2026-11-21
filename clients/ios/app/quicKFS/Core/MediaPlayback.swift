// SPDX-License-Identifier: Apache-2.0
import AVFoundation
import Observation
import UniformTypeIdentifiers

/// Plays a remote video or audio file while it streams.
///
/// The player is handed a private URL scheme, so AVFoundation asks this
/// app, not the network, for every byte range it wants: the header, the
/// index (often at the end of the file), then the stretch after the
/// playhead, and a new stretch on each seek. Each range is read from the
/// core's cached view of the file, whose read-ahead keeps the next blocks in
/// flight. Nothing is downloaded up front and nothing is written outside
/// the bounded streaming cache.
@MainActor
@Observable
final class MediaPlayback {
    enum Phase {
        case opening
        case ready(AVPlayer)
        case failed(String)
    }

    let node: RemoteNode
    private(set) var phase: Phase = .opening

    @ObservationIgnored private let connection: ServerConnection
    @ObservationIgnored private let cacheBytes: UInt64
    @ObservationIgnored private var stream: RemoteStream?
    @ObservationIgnored private var loader: MediaResourceLoader?
    @ObservationIgnored private var player: AVPlayer?
    @ObservationIgnored private var statusObservation: NSKeyValueObservation?
    @ObservationIgnored private var closed = false

    init(node: RemoteNode, connection: ServerConnection, cacheBytes: UInt64) {
        self.node = node
        self.connection = connection
        self.cacheBytes = cacheBytes
    }

    /// Whether AVFoundation can play files of this type at all.
    nonisolated static func canPlay(_ node: RemoteNode) -> Bool {
        guard let type = node.contentType else { return false }
        let playable = AVURLAsset.audiovisualTypes().compactMap { UTType($0.rawValue) }
        return playable.contains { type.conforms(to: $0) }
    }

    func open() async {
        guard stream == nil, !closed else { return }
        let opened: RemoteStream
        do {
            opened = try await RemoteStream.open(node, connection: connection, cacheBytes: cacheBytes)
        } catch {
            phase = .failed(error.localizedDescription)
            return
        }
        guard !closed else {
            opened.close()
            return
        }
        stream = opened
        let type = node.contentType?.identifier ?? UTType.movie.identifier
        let loader = MediaResourceLoader(stream: opened, contentType: type)
        self.loader = loader
        var components = URLComponents()
        components.scheme = MediaResourceLoader.scheme
        components.host = "stream"
        // Keep the name: AVFoundation reads the extension as a hint.
        components.path = "/\(opened.id)/\(node.displayName)"
        guard let url = components.url else {
            phase = .failed("The file name cannot be played.")
            return
        }
        let asset = AVURLAsset(url: url)
        asset.resourceLoader.setDelegate(loader, queue: loader.delegateQueue)
        let item = AVPlayerItem(asset: asset)
        let player = AVPlayer(playerItem: item)
        self.player = player
        statusObservation = item.observe(\.status) { [weak self] item, _ in
            guard item.status == .failed else { return }
            let message = item.error?.localizedDescription ?? "This file cannot be played."
            Task { @MainActor in self?.phase = .failed(message) }
        }
        phase = .ready(player)
        player.play()
    }

    /// Stop playback and release the remote file.
    func close() {
        closed = true
        statusObservation = nil
        player?.pause()
        player?.replaceCurrentItem(with: nil)
        player = nil
        loader?.cancelAll()
        loader = nil
        stream?.close()
        stream = nil
    }
}

/// Answers AVFoundation's byte-range requests from a `RemoteStream`.
final class MediaResourceLoader: NSObject, AVAssetResourceLoaderDelegate, @unchecked Sendable {
    static let scheme = "quickfs-stream"
    /// Bytes handed over per response. Small enough that a seek cancels an
    /// old range promptly; the core's read-ahead keeps the link busy anyway.
    private static let chunk = 1 << 20

    let delegateQueue = DispatchQueue(label: "org.quickfs.media.loader", qos: .userInitiated)
    private let work = DispatchQueue(
        label: "org.quickfs.media.read", qos: .userInitiated, attributes: .concurrent)
    private let stream: RemoteStream
    private let contentType: String
    private let lock = NSLock()
    private var stopped = false

    init(stream: RemoteStream, contentType: String) {
        self.stream = stream
        self.contentType = contentType
    }

    func cancelAll() {
        lock.withLock { stopped = true }
    }

    private var isStopped: Bool { lock.withLock { stopped } }

    func resourceLoader(
        _ resourceLoader: AVAssetResourceLoader,
        shouldWaitForLoadingOfRequestedResource loadingRequest: AVAssetResourceLoadingRequest
    ) -> Bool {
        guard !isStopped else { return false }
        if let information = loadingRequest.contentInformationRequest {
            information.contentType = contentType
            information.contentLength = Int64(stream.size)
            information.isByteRangeAccessSupported = true
        }
        guard let dataRequest = loadingRequest.dataRequest else {
            loadingRequest.finishLoading()
            return true
        }
        let start = UInt64(max(0, dataRequest.currentOffset))
        let end = dataRequest.requestsAllDataToEndOfResource
            ? stream.size
            : min(stream.size, UInt64(max(0, dataRequest.requestedOffset)) + UInt64(max(0, dataRequest.requestedLength)))
        let request = Unchecked(loadingRequest)
        work.async { self.serve(request.value, from: start, to: end) }
        return true
    }

    private func serve(_ request: AVAssetResourceLoadingRequest, from start: UInt64, to end: UInt64) {
        var offset = start
        while offset < end {
            if request.isCancelled || request.isFinished || isStopped { return }
            do {
                let data = try stream.read(at: offset, count: Int(min(UInt64(Self.chunk), end - offset)))
                if data.isEmpty { break }
                if request.isCancelled || isStopped { return }
                request.dataRequest?.respond(with: data)
                offset += UInt64(data.count)
            } catch {
                if !request.isCancelled { request.finishLoading(with: error) }
                return
            }
        }
        if !request.isCancelled, !request.isFinished { request.finishLoading() }
    }
}

/// Carries a value AVFoundation hands us across to the reading queue.
private struct Unchecked<Value>: @unchecked Sendable {
    let value: Value
    init(_ value: Value) { self.value = value }
}
