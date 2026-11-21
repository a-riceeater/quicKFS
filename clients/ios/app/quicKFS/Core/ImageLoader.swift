// SPDX-License-Identifier: Apache-2.0
import ImageIO
import Observation
import UIKit

/// Streams a remote image into memory and decodes it at screen size.
///
/// Images read through the same cached view as video, so reopening one
/// costs no network. Decoding goes through ImageIO's thumbnailer, which
/// never inflates a huge photo to full resolution just to show it on a phone.
@MainActor
@Observable
final class ImageLoader {
    enum Phase {
        case loading
        case ready(UIImage)
        case failed(String)
    }

    /// Larger images open as documents instead, through Quick Look.
    static let limit: UInt64 = 192 << 20
    /// The longest side to decode to; enough for a sharp full-screen zoom.
    private nonisolated static let maxPixels = 4_096

    let node: RemoteNode
    private(set) var phase: Phase = .loading
    private(set) var received: UInt64 = 0

    @ObservationIgnored private let connection: ServerConnection
    @ObservationIgnored private let cacheBytes: UInt64

    init(node: RemoteNode, connection: ServerConnection, cacheBytes: UInt64) {
        self.node = node
        self.connection = connection
        self.cacheBytes = cacheBytes
    }

    var fraction: Double {
        node.size == 0 ? 0 : min(1, Double(received) / Double(node.size))
    }

    func load() async {
        phase = .loading
        received = 0
        do {
            let stream = try await RemoteStream.open(node, connection: connection, cacheBytes: cacheBytes)
            defer { stream.close() }
            let data = try await stream.readAll { [weak self] bytes in
                Task { @MainActor in self?.received = bytes }
            }
            let image = await Task.detached(priority: .userInitiated) { Self.decode(data) }.value
            guard let image else {
                phase = .failed("This image could not be decoded.")
                return
            }
            phase = .ready(image)
        } catch is CancellationError {
            return
        } catch {
            phase = .failed(error.localizedDescription)
        }
    }

    private nonisolated static func decode(_ data: Data) -> UIImage? {
        guard let source = CGImageSourceCreateWithData(data as CFData, nil) else { return nil }
        let options: [CFString: Any] = [
            kCGImageSourceCreateThumbnailFromImageAlways: true,
            kCGImageSourceCreateThumbnailWithTransform: true,
            kCGImageSourceShouldCacheImmediately: true,
            kCGImageSourceThumbnailMaxPixelSize: maxPixels,
        ]
        guard let image = CGImageSourceCreateThumbnailAtIndex(source, 0, options as CFDictionary) else {
            return UIImage(data: data)
        }
        return UIImage(cgImage: image)
    }
}
