// SPDX-License-Identifier: Apache-2.0
import AppKit
import SwiftUI

/// Edits one saved server. Changes save as they are made; mount options
/// take effect the next time the server is mounted.
struct ServerSettingsView: View {
    let profile: ServerProfile
    @Environment(AppModel.self) private var model
    @State private var draft: ServerProfile
    @State private var changingPassword = false
    @State private var confirmingForget = false
    @State private var repairing = false
    @State private var resolving = false
    @State private var failure: String?

    init(profile: ServerProfile) {
        self.profile = profile
        _draft = State(initialValue: profile)
    }

    private var mounted: Bool { model.state(profile.id).mount.isActive }
    private var mountChanged: Bool { mounted && draft.mount != profile.mount }

    var body: some View {
        Form {
            Section {
                TextField("Name", text: $draft.name)
                LabeledContent("Address") {
                    HStack {
                        TextField("Host", text: $draft.host).labelsHidden()
                        Button {
                            Task { await reresolve() }
                        } label: {
                            if resolving { ProgressView().controlSize(.small) } else { Image(systemName: "arrow.triangle.2.circlepath") }
                        }
                        .buttonStyle(.borderless)
                        .help("Resolve again (\(draft.address))")
                    }
                }
                TextField("TLS name", text: $draft.serverName)
                TextField("Username", text: $draft.username)
                LabeledContent("Password") {
                    Button("Change…") { changingPassword = true }
                }
            }

            Section("Trust") {
                Picker("Method", selection: trustKind) {
                    Label("Pinned", systemImage: "pin").tag(0)
                    Label("Enterprise CA", systemImage: "building.columns").tag(1)
                    Label("System", systemImage: "checkmark.seal").tag(2)
                }
                switch draft.trust {
                case .pinned:
                    if let pin = model.pin(for: draft) {
                        LabeledContent("SHA-256") {
                            Text(pin.sha256)
                                .font(.caption.monospaced())
                                .textSelection(.enabled)
                                .lineLimit(2)
                                .multilineTextAlignment(.trailing)
                        }
                        Button("Forget Identity…", role: .destructive) { confirmingForget = true }
                    } else {
                        LabeledContent("Not paired") {
                            Button("Pair…") { repairing = true }
                        }
                    }
                case .enterpriseCA(let path):
                    LabeledContent("Bundle") {
                        Button((path as NSString).lastPathComponent.isEmpty ? "Choose…" : (path as NSString).lastPathComponent) {
                            if let url = choosePEM() { draft.trust = .enterpriseCA(path: url.path) }
                        }
                    }
                case .systemRoots:
                    EmptyView()
                }
            }

            Section {
                LabeledContent("Mount point") {
                    HStack {
                        Text(draft.mountpointURL.path(percentEncoded: false))
                            .lineLimit(1)
                            .truncationMode(.middle)
                            .foregroundStyle(.secondary)
                        Button("Choose…") { chooseMountpoint() }
                    }
                }
                TextField("Volume name", text: Binding(
                    get: { draft.mount.volumeName ?? "" },
                    set: { draft.mount.volumeName = $0.isEmpty ? nil : $0 }),
                    prompt: Text(draft.mountpointURL.lastPathComponent))
                Toggle("Mount when quicKFS opens", isOn: $draft.mount.mountAtLaunch)
                Picker("macFUSE backend", selection: $draft.mount.backend) {
                    Text("Automatic").tag(MountBackend.automatic)
                    Text("FSKit").tag(MountBackend.fskit)
                }
            } header: {
                HStack {
                    Text("Mount")
                    if mountChanged {
                        Image(systemName: "arrow.clockwise.circle")
                            .foregroundStyle(.orange)
                            .help("Eject and mount again to apply")
                    }
                }
            }

            Section("Performance") {
                LabeledContent("Offline cache") {
                    HStack {
                        Slider(value: cacheSlider, in: 0...10)
                            .frame(minWidth: 140)
                        Text(Format.bytes(UInt64(draft.mount.cacheMaxGiB * 1_073_741_824)))
                            .monospacedDigit()
                            .frame(width: 70, alignment: .trailing)
                    }
                }
                Picker("Read block", selection: $draft.mount.cacheBlockKiB) {
                    ForEach([1_024, 2_048, 4_096, 8_192, 16_384], id: \.self) { kib in
                        Text(Format.bytes(UInt64(kib) * 1024)).tag(kib)
                    }
                }
                Stepper(value: $draft.mount.readAheadMiB, in: 0...1_024, step: 16) {
                    LabeledContent("Read-ahead", value: draft.mount.readAheadMiB == 0
                        ? "Off" : Format.bytes(UInt64(draft.mount.readAheadMiB) * 1_048_576))
                }
                Stepper(value: $draft.mount.timeoutSeconds, in: 5...600, step: 5) {
                    LabeledContent("Network timeout", value: "\(draft.mount.timeoutSeconds) s")
                }
                Stepper(value: $draft.mount.callbackTimeoutSeconds, in: 10...900, step: 10) {
                    LabeledContent("Operation timeout", value: "\(draft.mount.callbackTimeoutSeconds) s")
                }
                Stepper(value: $draft.mount.reconnectAttempts, in: 1...20) {
                    LabeledContent("Reconnect attempts", value: "\(draft.mount.reconnectAttempts)")
                }
                LabeledContent("Reconnect backoff") {
                    HStack(spacing: 4) {
                        TextField("", value: $draft.mount.reconnectInitialBackoffMs, format: .number)
                            .frame(width: 60)
                        Text("to")
                            .foregroundStyle(.secondary)
                        TextField("", value: $draft.mount.reconnectMaxBackoffMs, format: .number)
                            .frame(width: 60)
                        Text("ms").foregroundStyle(.secondary)
                    }
                    .labelsHidden()
                    .multilineTextAlignment(.trailing)
                }
                Button("Restore Defaults") {
                    let keep = draft.mount
                    draft.mount = MountOptions()
                    draft.mount.mountpoint = keep.mountpoint
                    draft.mount.volumeName = keep.volumeName
                    draft.mount.mountAtLaunch = keep.mountAtLaunch
                    draft.mount.backend = keep.backend
                }
            }

            if let failure {
                Section { Text(failure).foregroundStyle(.red).textSelection(.enabled) }
            }
        }
        .formStyle(.grouped)
        .onChange(of: draft) { model.update(draft) }
        .onChange(of: profile) { if profile != draft { draft = profile } }
        .sheet(isPresented: $changingPassword) { PasswordSheet(profile: profile) }
        .sheet(isPresented: $repairing) { RepairSheet(profile: draft) }
        .confirmationDialog("Forget the pinned identity?", isPresented: $confirmingForget) {
            Button("Forget", role: .destructive) {
                Task {
                    do { try await model.forget(draft) } catch { failure = error.localizedDescription }
                }
            }
        } message: {
            Text("Connecting again will require pairing.")
        }
    }

    /// Logarithmic cache size: 0...10 maps to 1 GiB...1 TiB.
    private var cacheSlider: Binding<Double> {
        Binding(
            get: { log2(max(1, draft.mount.cacheMaxGiB)) },
            set: { draft.mount.cacheMaxGiB = pow(2, $0.rounded()) })
    }

    private var trustKind: Binding<Int> {
        Binding(
            get: {
                switch draft.trust {
                case .pinned: 0
                case .enterpriseCA: 1
                case .systemRoots: 2
                }
            },
            set: { kind in
                switch kind {
                case 1:
                    if case .enterpriseCA = draft.trust { return }
                    if let url = choosePEM() { draft.trust = .enterpriseCA(path: url.path) }
                case 2: draft.trust = .systemRoots
                default: draft.trust = .pinned
                }
            })
    }

    private func reresolve() async {
        resolving = true
        defer { resolving = false }
        struct Reply: Decodable, Sendable { let addresses: [String] }
        do {
            let reply: Reply = try await Core.call("resolve", ["host": .string(draft.host)])
            draft.address = reply.addresses.first { !$0.hasPrefix("[") } ?? reply.addresses[0]
            failure = nil
        } catch {
            failure = error.localizedDescription
        }
    }

    private func chooseMountpoint() {
        let panel = NSOpenPanel()
        panel.canChooseFiles = false
        panel.canChooseDirectories = true
        panel.canCreateDirectories = true
        panel.directoryURL = draft.mountpointURL.deletingLastPathComponent()
        guard panel.runModal() == .OK, let url = panel.url else { return }
        draft.mount.mountpoint = url.path
    }

    private func choosePEM() -> URL? {
        let panel = NSOpenPanel()
        panel.canChooseFiles = true
        panel.canChooseDirectories = false
        return panel.runModal() == .OK ? panel.url : nil
    }
}

/// Pair an existing server again after its identity was forgotten.
private struct RepairSheet: View {
    let profile: ServerProfile
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    @State private var pairingID = ""
    @State private var code = ""
    @State private var working = false
    @State private var failure: String?

    var body: some View {
        VStack(spacing: 16) {
            Image(systemName: "key.horizontal")
                .font(.system(size: 28))
                .foregroundStyle(.tint)
                .frame(width: 60, height: 60)
                .glassEffect(.regular.tint(.accentColor.opacity(0.2)), in: .circle)
            HStack {
                TextField("Pairing ID", text: $pairingID)
                    .font(.callout.monospaced())
                    .textFieldStyle(.roundedBorder)
                Button("Paste", systemImage: "doc.on.clipboard") {
                    let parsed = PairingCode.parse(NSPasteboard.general.string(forType: .string) ?? "")
                    if let id = parsed.id { pairingID = id.uuidString.lowercased() }
                    if let value = parsed.code { code = value }
                }
                .labelStyle(.iconOnly)
            }
            CodeEntry(code: $code)
            if let failure {
                Text(failure).font(.callout).foregroundStyle(.red)
            }
            HStack {
                Spacer()
                Button("Cancel", role: .cancel) { dismiss() }
                    .keyboardShortcut(.cancelAction)
                Button("Pair") { pair() }
                    .buttonStyle(.glassProminent)
                    .keyboardShortcut(.defaultAction)
                    .disabled(working || UUID(uuidString: pairingID) == nil || code.count != PairingCode.length)
            }
        }
        .padding(24)
        .frame(width: 480)
    }

    private func pair() {
        working = true
        Task {
            defer { working = false }
            do {
                _ = try await Core.call("pair", [
                    "state_dir": .string(Paths.state.path),
                    "server": profile.endpoint,
                    "pairing_id": .string(pairingID.lowercased()),
                    "code": .string(code),
                ], as: EmptyReply.self)
                await model.refreshPins()
                dismiss()
            } catch {
                failure = error.localizedDescription
            }
        }
    }
}
