// SPDX-License-Identifier: Apache-2.0
import AppKit
import SwiftUI

/// Add a server in three steps: where it is, how to trust it, who you are.
struct AddServerSheet: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss

    enum Step: Int { case address, trust, account }
    enum Method: String, CaseIterable, Identifiable {
        case pair, fingerprint, certificate, authority, system
        var id: Self { self }

        var symbol: String {
            switch self {
            case .pair: "key.horizontal"
            case .fingerprint: "number"
            case .certificate: "doc.badge.gearshape"
            case .authority: "building.columns"
            case .system: "checkmark.seal"
            }
        }

        var title: String {
            switch self {
            case .pair: "Pairing code"
            case .fingerprint: "Certificate fingerprint"
            case .certificate: "Certificate file"
            case .authority: "Enterprise CA"
            case .system: "System trust"
            }
        }
    }

    @State private var step = Step.address
    @State private var host = ""
    @State private var name = ""
    @State private var serverName = ""
    @State private var addresses: [String] = []
    @State private var address = ""
    @State private var method = Method.pair
    @State private var pairingID = ""
    @State private var code = ""
    @State private var fingerprint = ""
    @State private var certificate: URL?
    @State private var username = NSUserName()
    @State private var password = ""
    @State private var trust = TrustMode.pinned
    @State private var working = false
    @State private var failure: String?
    @Namespace private var glass

    private var alreadyPinned: Bool {
        model.pins.contains { $0.address == address && $0.serverName == serverName }
    }

    var body: some View {
        VStack(spacing: 0) {
            StepDots(step: step)
                .padding(.top, 18)
            Group {
                switch step {
                case .address: addressStep
                case .trust: trustStep
                case .account: accountStep
                }
            }
            .padding(.horizontal, 28)
            .padding(.vertical, 20)
            .frame(maxWidth: .infinity, maxHeight: .infinity, alignment: .top)
            .transition(.asymmetric(insertion: .move(edge: .trailing).combined(with: .opacity),
                                    removal: .move(edge: .leading).combined(with: .opacity)))
            if let failure {
                Label(failure, systemImage: "exclamationmark.triangle.fill")
                    .font(.callout)
                    .foregroundStyle(.red)
                    .textSelection(.enabled)
                    .padding(.horizontal, 28)
                    .frame(maxWidth: .infinity, alignment: .leading)
            }
            footer
                .padding(20)
        }
        .frame(width: 520, height: 440)
        .animation(.smooth, value: step)
        .animation(.smooth, value: failure)
    }

    // MARK: Steps

    private var addressStep: some View {
        VStack(spacing: 18) {
            Image(systemName: "server.rack")
                .font(.system(size: 34))
                .foregroundStyle(.tint)
                .frame(width: 72, height: 72)
                .glassEffect(.regular.tint(.accentColor.opacity(0.18)), in: .circle)
            TextField("host or host:port", text: $host)
                .textFieldStyle(.plain)
                .font(.title2)
                .multilineTextAlignment(.center)
                .padding(.vertical, 10)
                .padding(.horizontal, 16)
                .glassEffect(.regular, in: .capsule)
                .onSubmit(next)
                .onChange(of: host) {
                    addresses = []
                    let bare = host.split(separator: ":").first.map(String.init) ?? host
                    name = bare
                }
            HStack(spacing: 10) {
                TextField("Name", text: $name)
                TextField("TLS name", text: $serverName, prompt: Text("TLS name (auto)"))
                    .help("The name the server certificate is issued for")
            }
            .textFieldStyle(.roundedBorder)
            if addresses.count > 1 {
                Picker("Address", selection: $address) {
                    ForEach(addresses, id: \.self) { Text($0).tag($0) }
                }
                .pickerStyle(.menu)
                .labelsHidden()
            }
        }
    }

    private var trustStep: some View {
        VStack(spacing: 18) {
            if alreadyPinned {
                Placard(symbol: "checkmark.seal.fill", title: "Already paired",
                        detail: "\(serverName) at \(address)", tint: .green)
            } else {
                GlassEffectContainer(spacing: 6) {
                    HStack(spacing: 6) {
                        ForEach(Method.allCases) { option in
                            Button {
                                method = option
                                failure = nil
                            } label: {
                                Image(systemName: option.symbol)
                                    .font(.title3)
                                    .frame(width: 44, height: 36)
                            }
                            .buttonStyle(.plain)
                            .foregroundStyle(method == option ? Color.accentColor : .secondary)
                            .glassEffect(method == option ? .regular.tint(.accentColor.opacity(0.25)).interactive()
                                         : .regular.interactive(), in: .capsule)
                            .glassEffectID(option, in: glass)
                            .help(option.title)
                        }
                    }
                }
                Text(method.title)
                    .font(.headline)
                switch method {
                case .pair: pairingEntry
                case .fingerprint:
                    TextField("SHA-256", text: $fingerprint, axis: .vertical)
                        .font(.callout.monospaced())
                        .textFieldStyle(.roundedBorder)
                        .lineLimit(2...3)
                case .certificate, .authority:
                    Button(certificate?.lastPathComponent ?? "Choose PEM…", systemImage: "doc") { choosePEM() }
                        .buttonStyle(.glass)
                        .controlSize(.large)
                case .system:
                    Image(systemName: "checkmark.seal")
                        .font(.system(size: 40))
                        .foregroundStyle(.secondary)
                }
            }
        }
    }

    private var pairingEntry: some View {
        VStack(spacing: 14) {
            HStack {
                TextField("Pairing ID", text: $pairingID)
                    .font(.callout.monospaced())
                    .textFieldStyle(.roundedBorder)
                Button("Paste", systemImage: "doc.on.clipboard") { pasteFromClipboard() }
                    .labelStyle(.iconOnly)
                    .help("Paste pairing details")
            }
            CodeEntry(code: $code)
        }
    }

    private var accountStep: some View {
        VStack(spacing: 18) {
            Image(systemName: "person.crop.circle")
                .font(.system(size: 34))
                .foregroundStyle(.tint)
                .frame(width: 72, height: 72)
                .glassEffect(.regular.tint(.accentColor.opacity(0.18)), in: .circle)
            VStack(spacing: 10) {
                TextField("Username", text: $username)
                SecureField("Password", text: $password)
                    .onSubmit(next)
            }
            .textFieldStyle(.roundedBorder)
            .frame(width: 280)
        }
    }

    private var footer: some View {
        HStack {
            if step != .address {
                Button("Back", systemImage: "chevron.left") {
                    failure = nil
                    step = Step(rawValue: step.rawValue - 1) ?? .address
                }
                .buttonStyle(.glass)
            }
            Spacer()
            Button("Cancel", role: .cancel) { dismiss() }
                .keyboardShortcut(.cancelAction)
            Button(action: next) {
                if working {
                    ProgressView().controlSize(.small).frame(minWidth: 60)
                } else {
                    Text(step == .account ? "Connect" : "Continue").frame(minWidth: 60)
                }
            }
            .buttonStyle(.glassProminent)
            .keyboardShortcut(.defaultAction)
            .disabled(!canContinue || working)
        }
    }

    private var canContinue: Bool {
        switch step {
        case .address: !host.trimmingCharacters(in: .whitespaces).isEmpty
        case .trust:
            alreadyPinned || {
                switch method {
                case .pair: UUID(uuidString: pairingID.trimmingCharacters(in: .whitespaces)) != nil
                    && code.count == PairingCode.length
                case .fingerprint: fingerprint.filter(\.isHexDigit).count == 64
                case .certificate, .authority: certificate != nil
                case .system: true
                }
            }()
        case .account: !username.isEmpty && !password.isEmpty
        }
    }

    // MARK: Actions

    private func next() {
        guard canContinue, !working else { return }
        failure = nil
        working = true
        Task {
            defer { working = false }
            do {
                switch step {
                case .address: try await resolve()
                case .trust: try await establishTrust()
                case .account: try await connectAndSave()
                }
            } catch {
                failure = error.localizedDescription
            }
        }
    }

    private func resolve() async throws {
        struct Reply: Decodable, Sendable { let addresses: [String]; let serverName: String }
        let reply: Reply = try await Core.call("resolve", ["host": .string(host)])
        addresses = reply.addresses
        // Prefer IPv4, which the trust store and most LAN servers use.
        address = reply.addresses.first { !$0.hasPrefix("[") } ?? reply.addresses[0]
        if serverName.trimmingCharacters(in: .whitespaces).isEmpty { serverName = reply.serverName }
        if name.isEmpty { name = host }
        await model.refreshPins()
        step = .trust
    }

    private func establishTrust() async throws {
        let base: [String: JSONValue] = [
            "state_dir": .string(Paths.state.path),
            "server": ["address": .string(address), "server_name": .string(serverName)],
        ]
        if alreadyPinned {
            trust = .pinned
        } else {
            switch method {
            case .pair:
                var fields = base
                fields["pairing_id"] = .string(pairingID.trimmingCharacters(in: .whitespaces).lowercased())
                fields["code"] = .string(code)
                _ = try await Core.call("pair", fields, as: EmptyReply.self)
                trust = .pinned
            case .fingerprint:
                var fields = base
                fields["sha256"] = .string(fingerprint)
                _ = try await Core.call("trust_import", fields, as: EmptyReply.self)
                trust = .pinned
            case .certificate:
                var fields = base
                fields["certificate"] = .string(certificate?.path ?? "")
                _ = try await Core.call("trust_import", fields, as: EmptyReply.self)
                trust = .pinned
            case .authority:
                trust = .enterpriseCA(path: certificate?.path ?? "")
            case .system:
                trust = .systemRoots
            }
            await model.refreshPins()
        }
        step = .account
    }

    private func connectAndSave() async throws {
        let profile = ServerProfile(
            name: name.trimmingCharacters(in: .whitespaces).isEmpty ? host : name,
            host: host.trimmingCharacters(in: .whitespaces), address: address,
            serverName: serverName, username: username, trust: trust)
        struct Reply: Decodable, Sendable { let session: UInt64 }
        let reply: Reply = try await Core.call("connect", [
            "state_dir": .string(Paths.state.path),
            "server": profile.endpoint,
            "trust": trust.request,
            "username": .string(username),
            "password": .string(password),
        ])
        _ = try? await Core.call("disconnect", ["session": .uint(reply.session)], as: EmptyReply.self)
        model.add(profile, password: password)
        dismiss()
    }

    private func pasteFromClipboard() {
        guard let text = NSPasteboard.general.string(forType: .string) else { return }
        let parsed = PairingCode.parse(text)
        if let id = parsed.id { pairingID = id.uuidString.lowercased() }
        if let value = parsed.code { code = value }
    }

    private func choosePEM() {
        let panel = NSOpenPanel()
        panel.canChooseFiles = true
        panel.canChooseDirectories = false
        panel.allowedContentTypes = [.x509Certificate, .data]
        guard panel.runModal() == .OK else { return }
        certificate = panel.url
    }
}

private struct StepDots: View {
    let step: AddServerSheet.Step

    var body: some View {
        HStack(spacing: 8) {
            ForEach(0..<3) { index in
                Capsule()
                    .fill(index <= step.rawValue ? Color.accentColor : Color.secondary.opacity(0.3))
                    .frame(width: index == step.rawValue ? 22 : 8, height: 8)
            }
        }
        .animation(.smooth, value: step)
    }
}

/// Grouped character slots over an invisible text field, as in the previous
/// client: typing or pasting fills the slots, the next slot is highlighted.
struct CodeEntry: View {
    @Binding var code: String
    @FocusState private var focused: Bool

    var body: some View {
        ZStack {
            TextField("", text: Binding(get: { code }, set: { code = PairingCode.normalize($0) }))
                .textFieldStyle(.plain)
                .focused($focused)
                .opacity(0.02)
                .frame(width: 1, height: 1)
            HStack(spacing: 10) {
                ForEach(Array(PairingCode.split(code).enumerated()), id: \.offset) { groupIndex, group in
                    HStack(spacing: 2) {
                        ForEach(Array(group.enumerated()), id: \.offset) { offset, character in
                            let index = groupIndex * 4 + offset
                            slot(character, current: focused && index == code.count)
                        }
                    }
                }
            }
            .contentShape(.rect)
            .onTapGesture { focused = true }
        }
        .padding(.vertical, 10)
        .padding(.horizontal, 14)
        .glassEffect(.regular.tint(code.count == PairingCode.length ? .green.opacity(0.18) : .clear),
                     in: .rect(cornerRadius: 14))
        .onAppear { focused = true }
        .accessibilityLabel("Pairing code, \(code.count) of \(PairingCode.length) characters")
    }

    private func slot(_ character: Character?, current: Bool) -> some View {
        VStack(spacing: 3) {
            Text(character.map(String.init) ?? " ")
                .font(.system(.title3, design: .monospaced).weight(.medium))
                .frame(width: 14, height: 22)
            Capsule()
                .fill(current ? Color.accentColor : character == nil ? Color.secondary.opacity(0.35) : .clear)
                .frame(width: 12, height: 2)
        }
    }
}
