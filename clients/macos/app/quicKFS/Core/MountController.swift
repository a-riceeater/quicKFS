// SPDX-License-Identifier: Apache-2.0
import AppKit
import Foundation
import Observation

/// One `quickfs-mount` helper process for one server.
///
/// The helper owns the FUSE session for its whole lifetime: SIGINT starts a
/// graceful unmount (forced after three seconds, process exit after eight),
/// and an external `umount` ends the process on its own. The controller
/// only launches, signals, and observes it.
@MainActor
@Observable
final class MountController {
    enum Phase: Equatable {
        case idle
        case starting
        case mounted
        case stopping
        case failed(String)
    }

    private(set) var phase: Phase = .idle
    private(set) var log: [String] = []
    private(set) var mountedAt: Date?
    private var process: Process?
    private var mountpoint: URL?

    private static let maxLogLines = 400
    private static let readyMarker = "quicKFS mount is ready at"

    var isActive: Bool { phase == .starting || phase == .mounted || phase == .stopping }
    var isMounted: Bool { phase == .mounted }

    func mount(_ profile: ServerProfile, password: String) {
        guard !isActive else { return }
        let mountpoint = profile.mountpointURL
        do {
            try FileManager.default.createDirectory(at: mountpoint, withIntermediateDirectories: true)
        } catch {
            phase = .failed(error.localizedDescription)
            return
        }
        guard FileManager.default.isExecutableFile(atPath: Paths.mountHelper.path) else {
            phase = .failed("The mount helper is missing from this copy of quicKFS.")
            return
        }

        let process = Process()
        process.executableURL = Paths.mountHelper
        process.arguments = Self.arguments(for: profile, mountpoint: mountpoint)
        var environment = ProcessInfo.processInfo.environment
        environment["QUICKFS_CLIENT_STATE_DIR"] = Paths.state.path
        process.environment = environment

        let input = Pipe()
        let output = Pipe()
        process.standardInput = input
        process.standardOutput = output
        process.standardError = output
        output.fileHandleForReading.readabilityHandler = { [weak self] handle in
            let data = handle.availableData
            guard !data.isEmpty, let text = String(data: data, encoding: .utf8) else { return }
            Task { @MainActor in self?.ingest(text) }
        }
        process.terminationHandler = { [weak self] finished in
            let status = finished.terminationStatus
            let reason = finished.terminationReason
            Task { @MainActor in self?.finished(status: status, reason: reason) }
        }

        log.removeAll()
        phase = .starting
        self.mountpoint = mountpoint
        do {
            try process.run()
        } catch {
            phase = .failed(error.localizedDescription)
            return
        }
        self.process = process
        let handle = input.fileHandleForWriting
        // The helper may exit before reading (for example on a trust error);
        // SIGPIPE is ignored app-wide, so this write simply fails.
        try? handle.write(contentsOf: Data((password + "\n").utf8))
        try? handle.close()
    }

    /// Ask the helper to unmount. It exits on its own once the volume detaches.
    func unmount() {
        guard let process, process.isRunning else { return }
        phase = .stopping
        process.interrupt()
    }

    func revealInFinder() {
        guard let mountpoint, isMounted else { return }
        NSWorkspace.shared.open(mountpoint)
    }

    /// Unmount and wait for the helper to exit, bounded by its own watchdog.
    func shutdown() async {
        guard let process, process.isRunning else { return }
        unmount()
        for _ in 0..<100 where process.isRunning {
            try? await Task.sleep(for: .milliseconds(100))
        }
        if process.isRunning { process.terminate() }
    }

    private func ingest(_ text: String) {
        for line in text.split(whereSeparator: \.isNewline) {
            let line = String(line)
            if line.contains(Self.readyMarker), phase == .starting {
                phase = .mounted
                mountedAt = .now
            }
            log.append(line)
        }
        if log.count > Self.maxLogLines { log.removeFirst(log.count - Self.maxLogLines) }
    }

    private func finished(status: Int32, reason: Process.TerminationReason) {
        process?.standardOutput.flatMap { $0 as? Pipe }?.fileHandleForReading.readabilityHandler = nil
        process = nil
        mountedAt = nil
        let wasStopping = phase == .stopping || phase == .mounted
        if status == 0 || (wasStopping && (status == 130 || reason == .uncaughtSignal)) {
            phase = .idle
        } else {
            phase = .failed(Self.failureSummary(from: log) ?? "The mount ended unexpectedly.")
        }
    }

    /// The helper reports failures through anyhow's `Error: ...` chain.
    private static func failureSummary(from log: [String]) -> String? {
        if let index = log.lastIndex(where: { $0.hasPrefix("Error:") }) {
            let rest = log[index...].dropFirst().map { $0.trimmingCharacters(in: .whitespaces) }
                .filter { !$0.isEmpty && $0 != "Caused by:" }
                .map { $0.replacingOccurrences(of: #"^\d+:\s*"#, with: "", options: .regularExpression) }
            let head = log[index].dropFirst("Error:".count).trimmingCharacters(in: .whitespaces)
            return ([head] + rest).joined(separator: ": ")
        }
        return log.last
    }

    static func arguments(for profile: ServerProfile, mountpoint: URL) -> [String] {
        let options = profile.mount
        var arguments = [
            mountpoint.path,
            "--server", profile.address,
            "--server-name", profile.serverName,
            "--username", profile.username,
            "--state-dir", Paths.state.path,
            "--cache-dir", Paths.cache.path,
            "--cache-max-bytes", String(UInt64(max(0.25, options.cacheMaxGiB) * 1_073_741_824)),
            "--cache-block-kib", String(max(64, options.cacheBlockKiB)),
            "--read-ahead-max-bytes", String(UInt64(max(0, options.readAheadMiB)) * 1_048_576),
            "--timeout-ms", String(max(1, options.timeoutSeconds) * 1000),
            "--callback-timeout-ms", String(max(1, options.callbackTimeoutSeconds) * 1000),
            "--reconnect-attempts", String(max(1, options.reconnectAttempts)),
            "--reconnect-initial-backoff-ms", String(max(0, options.reconnectInitialBackoffMs)),
            "--reconnect-max-backoff-ms",
            String(max(options.reconnectInitialBackoffMs, options.reconnectMaxBackoffMs)),
            "--password-stdin",
        ]
        if let name = options.volumeName?.trimmingCharacters(in: .whitespaces), !name.isEmpty {
            arguments += ["--volume-name", name.replacingOccurrences(of: ",", with: " ")]
        }
        if options.backend == .fskit { arguments += ["--macfuse-backend", "fskit"] }
        arguments += profile.trust.mountArguments
        return arguments
    }
}
