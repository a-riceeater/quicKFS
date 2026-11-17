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
    private(set) var mountedAt: Date?
    private(set) var pid: Int32?
    /// The exact helper invocation, for the console. Holds no secrets: the
    /// password travels on stdin.
    private(set) var commandLine: String?
    /// Where the running helper publishes link statistics.
    private(set) var statsFile: URL?
    /// Ask the next mount to stream every kernel request to the console.
    var traceKernel = false
    private(set) var tracing = false
    private var recentOutput: [String] = []
    /// Bytes after the last newline: pipe reads split lines (and UTF-8
    /// sequences) at arbitrary points.
    private var partialLine = Data()
    private var process: Process?
    private var mountpoint: URL?
    private let log: ConsoleLog

    private static let readyMarker = "quicKFS mount is ready at"

    init(log: ConsoleLog) {
        self.log = log
    }

    var isActive: Bool { phase == .starting || phase == .mounted || phase == .stopping }
    var isMounted: Bool { phase == .mounted }

    /// Show the mount as starting while its password is fetched.
    func prepare() {
        if !isActive { phase = .starting }
    }

    func cancelPreparation() {
        if phase == .starting && process == nil { phase = .idle }
    }

    func mount(_ profile: ServerProfile, password: String) {
        guard process == nil else { return }
        let mountpoint = profile.mountpointURL
        do {
            try FileManager.default.createDirectory(at: mountpoint, withIntermediateDirectories: true)
        } catch {
            fail(error.localizedDescription)
            return
        }
        guard FileManager.default.isExecutableFile(atPath: Paths.mountHelper.path) else {
            fail("The mount helper is missing from this copy of quicKFS.")
            return
        }

        let statsFile = Paths.telemetry.appendingPathComponent("\(profile.id.uuidString).json")
        try? FileManager.default.removeItem(at: statsFile)
        let arguments = Self.arguments(for: profile, mountpoint: mountpoint)
            + ["--stats-file", statsFile.path, "--exit-with-parent"]
        let process = Process()
        process.executableURL = Paths.mountHelper
        process.arguments = arguments
        var environment = ProcessInfo.processInfo.environment
        environment["QUICKFS_CLIENT_STATE_DIR"] = Paths.state.path
        if traceKernel { environment["QUICKFS_FUSE_DEBUG"] = "1" }
        process.environment = environment

        let input = Pipe()
        let output = Pipe()
        process.standardInput = input
        process.standardOutput = output
        process.standardError = output
        output.fileHandleForReading.readabilityHandler = { [weak self] handle in
            let data = handle.availableData
            guard !data.isEmpty else { return }
            Task { @MainActor in self?.ingest(data) }
        }
        process.terminationHandler = { [weak self] finished in
            let status = finished.terminationStatus
            let reason = finished.terminationReason
            Task { @MainActor in self?.finished(status: status, reason: reason) }
        }

        recentOutput.removeAll()
        partialLine.removeAll()
        phase = .starting
        tracing = traceKernel
        self.mountpoint = mountpoint
        self.statsFile = statsFile
        commandLine = ([Paths.mountHelper.lastPathComponent] + arguments).map(Self.shellQuoted).joined(separator: " ")
        log.append(.app, .info, "mounting \(profile.serverName) (\(profile.address)) at \(mountpoint.path)"
            + (traceKernel ? " with kernel tracing" : ""))
        do {
            try process.run()
        } catch {
            fail(error.localizedDescription)
            return
        }
        self.process = process
        pid = process.processIdentifier
        log.append(.app, .debug, "helper started, pid \(process.processIdentifier)")
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
        log.append(.app, .info, "ejecting (SIGINT to pid \(process.processIdentifier))")
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

    private func fail(_ message: String) {
        phase = .failed(message)
        log.append(.app, .error, message)
    }

    private func ingest(_ data: Data) {
        partialLine.append(data)
        while let newline = partialLine.firstIndex(of: 0x0A) {
            let bytes = partialLine[partialLine.startIndex..<newline]
            partialLine.removeSubrange(partialLine.startIndex...newline)
            let line = String(decoding: bytes, as: UTF8.self).trimmingCharacters(in: .newlines)
            guard !line.isEmpty else { continue }
            if line.contains(Self.readyMarker), phase == .starting {
                phase = .mounted
                mountedAt = .now
            }
            log.ingestHelperOutput(line)
            if !line.hasPrefix("[") {
                recentOutput.append(line)
                if recentOutput.count > 60 { recentOutput.removeFirst(recentOutput.count - 60) }
            }
        }
    }

    private func finished(status: Int32, reason: Process.TerminationReason) {
        if !partialLine.isEmpty {
            partialLine.append(0x0A)
            ingest(Data())
        }
        process?.standardOutput.flatMap { $0 as? Pipe }?.fileHandleForReading.readabilityHandler = nil
        process = nil
        pid = nil
        mountedAt = nil
        tracing = false
        if let statsFile { try? FileManager.default.removeItem(at: statsFile) }
        let wasStopping = phase == .stopping || phase == .mounted
        if status == 0 || (wasStopping && (status == 130 || reason == .uncaughtSignal)) {
            phase = .idle
            log.append(.app, .notice, "helper exited cleanly; volume detached")
        } else {
            let summary = Self.failureSummary(from: recentOutput) ?? "The mount ended unexpectedly."
            phase = .failed(summary)
            log.append(.app, .error, "helper exited with status \(status): \(summary)")
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

    private static func shellQuoted(_ value: String) -> String {
        let safe = CharacterSet.alphanumerics.union(CharacterSet(charactersIn: "-_./:=@,+"))
        if !value.isEmpty, value.unicodeScalars.allSatisfy(safe.contains) { return value }
        return "'" + value.replacingOccurrences(of: "'", with: "'\\''") + "'"
    }

    static func arguments(for profile: ServerProfile, mountpoint: URL) -> [String] {
        let options = profile.mount
        let cacheBytes = UInt64(max(0.25, options.cacheMaxGiB) * 1_073_741_824)
        let readAheadBytes = UInt64(max(0, options.readAheadMiB)) * 1_048_576
        let initialBackoff = max(0, options.reconnectInitialBackoffMs)
        let maxBackoff = max(initialBackoff, options.reconnectMaxBackoffMs)
        var arguments: [String] = [mountpoint.path]
        let pairs: [(String, String)] = [
            ("--server", profile.address),
            ("--server-name", profile.serverName),
            ("--username", profile.username),
            ("--state-dir", Paths.state.path),
            ("--cache-dir", Paths.cache.path),
            ("--cache-max-bytes", String(cacheBytes)),
            ("--cache-block-kib", String(max(64, options.cacheBlockKiB))),
            ("--read-ahead-max-bytes", String(readAheadBytes)),
            ("--timeout-ms", String(max(1, options.timeoutSeconds) * 1000)),
            ("--callback-timeout-ms", String(max(1, options.callbackTimeoutSeconds) * 1000)),
            ("--reconnect-attempts", String(max(1, options.reconnectAttempts))),
            ("--reconnect-initial-backoff-ms", String(initialBackoff)),
            ("--reconnect-max-backoff-ms", String(maxBackoff)),
        ]
        for (flag, value) in pairs { arguments += [flag, value] }
        arguments.append("--password-stdin")
        if let name = options.volumeName?.trimmingCharacters(in: .whitespaces), !name.isEmpty {
            arguments += ["--volume-name", name.replacingOccurrences(of: ",", with: " ")]
        }
        if options.backend == .fskit { arguments += ["--macfuse-backend", "fskit"] }
        arguments += profile.trust.mountArguments
        return arguments
    }
}
