// SPDX-License-Identifier: Apache-2.0
import SwiftUI

/// Edits one saved server. Changes save as they are made and apply from the
/// next connection.
struct ServerSettingsView: View {
    let profile: ServerProfile
    @Environment(AppModel.self) private var model
    @State private var draft: ServerProfile
    @State private var changingPassword = false
    @State private var confirmingForget = false
    @State private var repairing = false
    @State private var resolving = false
    @State private var choosingBundle = false
    @State private var failure: String?

    init(profile: ServerProfile) {
        self.profile = profile
        _draft = State(initialValue: profile)
    }

    var body: some View {
        Form {
            Section {
                TextField("Name", text: $draft.name)
                LabeledContent("Address") {
                    HStack {
                        TextField("Host", text: $draft.host)
                            .multilineTextAlignment(.trailing)
                            .textInputAutocapitalization(.never)
                            .autocorrectionDisabled()
                        Button {
                            Task { await reresolve() }
                        } label: {
                            if resolving { ProgressView() } else { Image(systemName: "arrow.triangle.2.circlepath") }
                        }
                        .buttonStyle(.borderless)
                        .accessibilityLabel("Resolve again")
                    }
                }
                LabeledContent("TLS name") {
                    TextField("TLS name", text: $draft.serverName)
                        .multilineTextAlignment(.trailing)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                }
                LabeledContent("Username") {
                    TextField("Username", text: $draft.username)
                        .multilineTextAlignment(.trailing)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                }
                Button("Change Password") { changingPassword = true }
            } footer: {
                Text(draft.address).monospaced()
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
                        VStack(alignment: .leading, spacing: 4) {
                            Text("SHA-256").font(.caption).foregroundStyle(.secondary)
                            Text(pin.sha256)
                                .font(.caption.monospaced())
                                .textSelection(.enabled)
                        }
                        Button("Forget Identity", role: .destructive) { confirmingForget = true }
                    } else {
                        Button("Pair", systemImage: "key.horizontal") { repairing = true }
                    }
                case .enterpriseCA(let path):
                    Button(path.isEmpty ? "Choose Bundle" : AddServerView.bundleName(URL(fileURLWithPath: path)),
                           systemImage: "doc") { choosingBundle = true }
                case .systemRoots:
                    EmptyView()
                }
            }

            Section {
                Stepper(value: $draft.transfer.stallTimeoutSeconds, in: 10...300, step: 5) {
                    LabeledContent("Stall timeout", value: "\(draft.transfer.stallTimeoutSeconds) s")
                }
                Stepper(value: $draft.transfer.retryMinutes, in: 1...60) {
                    LabeledContent("Keep retrying for", value: "\(draft.transfer.retryMinutes) min")
                }
                Stepper(value: $draft.transfer.connections, in: 1...4) {
                    LabeledContent("Connections per download", value: "\(draft.transfer.connections)")
                }
                Toggle("BBR for uploads", isOn: $draft.transfer.bbr)
                Toggle("Size requests to the link", isOn: $draft.transfer.adaptive)
                Stepper(value: $draft.transfer.maxDepth, in: 1...32) {
                    LabeledContent(draft.transfer.adaptive ? "Most requests in flight" : "Requests in flight",
                                   value: "\(draft.transfer.maxDepth)")
                }
                Picker(draft.transfer.adaptive ? "Largest request" : "Request size", selection: $draft.transfer.maxChunkKiB) {
                    ForEach([256, 512, 1_024, 2_048, 4_096, 8_192, 16_384], id: \.self) { kib in
                        Text(Format.bytes(UInt64(kib) * 1024)).tag(kib)
                    }
                }
                Button("Restore Defaults") { draft.transfers = nil }
                    .disabled(draft.transfers == nil || draft.transfer == TransferOptions())
            } header: {
                Text("Transfers")
            } footer: {
                Text("A request fails after the stall timeout with no data moving, then retries for the set time before offering Resume. More connections help on lossy or relayed links, at one extra login each. BBR holds more speed on lossy or long-distance links for what this device sends.")
            }

            if let failure {
                Section { Text(failure).foregroundStyle(.red).textSelection(.enabled) }
            }
        }
        .onChange(of: draft) { model.update(draft) }
        .onChange(of: profile) { if profile != draft { draft = profile } }
        .sheet(isPresented: $changingPassword) { PasswordSheet(profile: profile) }
        .sheet(isPresented: $repairing) { RepairSheet(profile: draft) }
        .fileImporter(isPresented: $choosingBundle, allowedContentTypes: [.x509Certificate, .data]) { result in
            if case .success(let url) = result {
                do { draft.trust = .enterpriseCA(path: try Paths.importCertificate(url).path) } catch {
                    failure = error.localizedDescription
                }
            }
        }
        .confirmationDialog("Forget the pinned identity?", isPresented: $confirmingForget, titleVisibility: .visible) {
            Button("Forget", role: .destructive) {
                Task {
                    do { try await model.forget(draft) } catch { failure = error.localizedDescription }
                }
            }
        } message: {
            Text("Connecting again will require pairing.")
        }
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
                    choosingBundle = true
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
        NavigationStack {
            VStack(spacing: 18) {
                HStack {
                    TextField("Pairing ID", text: $pairingID)
                        .font(.callout.monospaced())
                        .textFieldStyle(.roundedBorder)
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                    PasteButton(payloadType: String.self) { strings in
                        let parsed = PairingCode.parse(strings.joined(separator: "\n"))
                        if let id = parsed.id { pairingID = id.uuidString.lowercased() }
                        if let value = parsed.code { code = value }
                    }
                    .labelStyle(.iconOnly)
                    .buttonBorderShape(.capsule)
                }
                CodeEntry(code: $code)
                if let failure {
                    Text(failure).font(.callout).foregroundStyle(.red)
                }
                Spacer()
            }
            .padding(24)
            .navigationTitle("Pair")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Cancel", role: .cancel) { dismiss() } }
                ToolbarItem(placement: .confirmationAction) {
                    if working {
                        ProgressView()
                    } else {
                        Button("Pair", action: pair)
                            .disabled(UUID(uuidString: pairingID) == nil || code.count != PairingCode.length)
                    }
                }
            }
        }
        .presentationDetents([.medium, .large])
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

/// Ask for (or replace) a server's password, then reconnect.
struct PasswordSheet: View {
    let profile: ServerProfile
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    @State private var password = ""
    @FocusState private var focused: Bool

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    SecureField("Password", text: $password)
                        .textContentType(.password)
                        .focused($focused)
                        .submitLabel(.go)
                        .onSubmit(save)
                } footer: {
                    Text("\(profile.username)@\(profile.host)")
                }
            }
            .navigationTitle(profile.name)
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Cancel", role: .cancel) { dismiss() } }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Save", action: save).disabled(password.isEmpty)
                }
            }
        }
        .presentationDetents([.height(220)])
        .onAppear { focused = true }
    }

    private func save() {
        guard !password.isEmpty else { return }
        model.setPassword(password, for: profile.id)
        let state = model.state(profile.id)
        Task { _ = try? await state.connection.connect() }
        dismiss()
    }
}
