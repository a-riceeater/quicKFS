// SPDX-License-Identifier: Apache-2.0
import AppKit
import Darwin
import Foundation
import Observation

/// One link snapshot from the mount helper and when it arrived.
struct LinkReading: Equatable {
    let link: LinkSnapshot
    let at: Date
    let helperStarted: Date
}

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
    /// The helper's most recent link snapshot, answered over its pipes.
    private(set) var linkReading: LinkReading?
    /// Ask the next mount to stream every kernel request to the console.
    var traceKernel = false
    private(set) var tracing = false
    private var recentOutput: [String] = []
    /// Bytes after the last newline on stdout and stderr: pipe reads split
    /// lines (and UTF-8 sequences) at arbitrary points.
    private var partialLines: [Stream: Data] = [:]
    private var process: Process?
    /// The helper's standard input, kept open for stats requests. Writes are
    /// non-blocking so a stuck helper can never stall the main thread.
    @ObservationIgnored private var control: FileHandle?
    @ObservationIgnored private var statsRequestedAt: Date?
    private var mountpoint: URL?
    private let log: ConsoleLog

    private static let readyMarker = "quicKFS mount is ready at"
    private static let statsPrefix = "quickfs-stats "

    /// Standard output carries the banner and stats replies; standard error
    /// carries the log. Separate pipes keep a long reply from interleaving
    /// with a log line.
    private enum Stream { case output, error }

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

        let arguments = Self.arguments(for: profile, mountpoint: mountpoint)
            + ["--stats-on-request", "--exit-with-parent"]
        let process = Process()
        process.executableURL = Paths.mountHelper
        process.arguments = arguments
        var environment = ProcessInfo.processInfo.environment
        environment["QUICKFS_CLIENT_STATE_DIR"] = Paths.state.path
        if traceKernel { environment["QUICKFS_FUSE_DEBUG"] = "1" }
        process.environment = environment

        let input = Pipe()
        let output = Pipe()
        let errors = Pipe()
        process.standardInput = input
        process.standardOutput = output
        process.standardError = errors
        for (pipe, stream) in [(output, Stream.output), (errors, Stream.error)] {
            pipe.fileHandleForReading.readabilityHandler = { [weak self] handle in
                let data = handle.availableData
                guard !data.isEmpty else { return }
                Task { @MainActor in self?.ingest(data, from: stream) }
            }
        }
        process.terminationHandler = { [weak self] finished in
            let status = finished.terminationStatus
            let reason = finished.terminationReason
            Task { @MainActor in self?.finished(status: status, reason: reason) }
        }

        recentOutput.removeAll()
        partialLines.removeAll()
        linkReading = nil
        statsRequestedAt = nil
        phase = .starting
        tracing = traceKernel
        self.mountpoint = mountpoint
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
        let launched = process.processIdentifier
        Task { [weak self] in
            try? await Task.sleep(for: .seconds(15))
            guard let self, self.phase == .starting, self.pid == launched else { return }
            self.log.append(.app, .warning,
                "macFUSE has not attached the volume after 15 s; Eject cancels the attempt")
        }
        log.append(.app, .debug, "helper started, pid \(process.processIdentifier)")
        let handle = input.fileHandleForWriting
        // The helper may exit before reading (for example on a trust error);
        // SIGPIPE is ignored app-wide, so this write simply fails.
        try? handle.write(contentsOf: Data((password + "\n").utf8))
        let flags = fcntl(handle.fileDescriptor, F_GETFL)
        if flags >= 0, fcntl(handle.fileDescriptor, F_SETFL, flags | O_NONBLOCK) == 0 {
            control = handle
        } else {
            try? handle.close()
        }
    }

    /// Ask the helper for a fresh link snapshot; the reply lands in
    /// `linkReading`. At most one request is outstanding, so an unresponsive
    /// helper sees one request every few seconds, not a backlog.
    func requestLinkStats() {
        guard let control, isMounted else { return }
        if let pending = statsRequestedAt, Date.now.timeIntervalSince(pending) < 3 { return }
        let request = Array("stats\n".utf8)
        let written = request.withUnsafeBytes { Darwin.write(control.fileDescriptor, $0.baseAddress, $0.count) }
        if written == request.count { statsRequestedAt = .now }
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

    private func ingest(_ data: Data, from stream: Stream) {
        var buffer = partialLines[stream, default: Data()]
        buffer.append(data)
        defer { partialLines[stream] = buffer }
        while let newline = buffer.firstIndex(of: 0x0A) {
            let bytes = buffer[buffer.startIndex..<newline]
            buffer.removeSubrange(buffer.startIndex...newline)
            let line = String(decoding: bytes, as: UTF8.self).trimmingCharacters(in: .newlines)
            guard !line.isEmpty else { continue }
            if stream == .output, line.hasPrefix(Self.statsPrefix) {
                receiveStats(line.dropFirst(Self.statsPrefix.count))
                continue
            }
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

    private func receiveStats(_ json: Substring) {
        struct Reply: Decodable { let startedMs: UInt64; let link: LinkSnapshot? }
        statsRequestedAt = nil
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        guard let reply = try? decoder.decode(Reply.self, from: Data(json.utf8)), let link = reply.link else {
            return
        }
        linkReading = LinkReading(
            link: link, at: .now,
            helperStarted: Date(timeIntervalSince1970: Double(reply.startedMs) / 1000))
    }

    private func finished(status: Int32, reason: Process.TerminationReason) {
        for stream in [Stream.output, .error] where !(partialLines[stream]?.isEmpty ?? true) {
            ingest(Data([0x0A]), from: stream)
        }
        for pipe in [process?.standardOutput, process?.standardError] {
            (pipe as? Pipe)?.fileHandleForReading.readabilityHandler = nil
        }
        try? control?.close()
        control = nil
        statsRequestedAt = nil
        linkReading = nil
        process = nil
        pid = nil
        mountedAt = nil
        tracing = false
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
        if profile.transfer.bbr { arguments += ["--congestion-control", "bbr"] }
        arguments += profile.trust.mountArguments
        return arguments
    }
}
