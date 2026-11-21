// SPDX-License-Identifier: Apache-2.0
import SwiftUI
import UniformTypeIdentifiers

/// Add a server in three steps: where it is, how to trust it, who you are.
struct AddServerView: View {
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
    @State private var choosingCertificate = false
    @State private var username = ""
    @State private var password = ""
    @State private var trust = TrustMode.pinned
    @State private var working = false
    @State private var failure: String?
    @FocusState private var hostFocused: Bool

    private var alreadyPinned: Bool {
        model.pins.contains { $0.address == address && $0.serverName == serverName }
    }

    var body: some View {
        NavigationStack {
            ScrollView {
                VStack(spacing: 24) {
                    StepDots(step: step)
                    Group {
                        switch step {
                        case .address: addressStep
                        case .trust: trustStep
                        case .account: accountStep
                        }
                    }
                    .transition(.asymmetric(insertion: .move(edge: .trailing).combined(with: .opacity),
                                            removal: .move(edge: .leading).combined(with: .opacity)))
                    if let failure {
                        Label(failure, systemImage: "exclamationmark.triangle.fill")
                            .font(.callout)
                            .foregroundStyle(.red)
                            .textSelection(.enabled)
                            .frame(maxWidth: .infinity, alignment: .leading)
                    }
                }
                .padding(24)
                .frame(maxWidth: 520)
                .frame(maxWidth: .infinity)
            }
            .scrollDismissesKeyboard(.interactively)
            .safeAreaInset(edge: .bottom) { footer }
            .navigationTitle("Add Server")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel", role: .cancel) { dismiss() }
                }
            }
            .animation(.smooth, value: step)
            .animation(.smooth, value: failure)
            .fileImporter(isPresented: $choosingCertificate,
                          allowedContentTypes: [.x509Certificate, .data]) { result in
                if case .success(let url) = result {
                    do { certificate = try Paths.importCertificate(url) } catch {
                        failure = error.localizedDescription
                    }
                }
            }
        }
        .interactiveDismissDisabled(working)
    }

    // MARK: Steps

    private var addressStep: some View {
        VStack(spacing: 18) {
            Image(systemName: "server.rack")
                .font(.system(size: 34))
                .foregroundStyle(.tint)
                .frame(width: 76, height: 76)
                .glassSurface(Circle(), tint: .accentColor.opacity(0.18))
            TextField("host or host:port", text: $host)
                .font(.title2)
                .multilineTextAlignment(.center)
                .textInputAutocapitalization(.never)
                .autocorrectionDisabled()
                .keyboardType(.URL)
                .submitLabel(.next)
                .focused($hostFocused)
                .padding(.vertical, 12)
                .padding(.horizontal, 18)
                .glassSurface()
                .onSubmit(next)
                .onChange(of: host) {
                    addresses = []
                    name = host.split(separator: ":").first.map(String.init) ?? host
                }
            VStack(spacing: 10) {
                TextField("Name", text: $name)
                TextField("TLS name", text: $serverName, prompt: Text("TLS name (automatic)"))
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
            }
            .textFieldStyle(.roundedBorder)
            if addresses.count > 1 {
                Picker("Address", selection: $address) {
                    ForEach(addresses, id: \.self) { Text($0).tag($0) }
                }
                .pickerStyle(.menu)
            }
        }
        .onAppear { hostFocused = true }
    }

    private var trustStep: some View {
        VStack(spacing: 18) {
            if alreadyPinned {
                Placard(symbol: "checkmark.seal.fill", title: "Already paired",
                        detail: "\(serverName) at \(address)", tint: .green)
            } else {
                GlassGroup(spacing: 6) {
                    HStack(spacing: 6) {
                        ForEach(Method.allCases) { option in
                            Button {
                                method = option
                                failure = nil
                            } label: {
                                Image(systemName: option.symbol)
                                    .font(.title3)
                                    .frame(width: 48, height: 40)
                            }
                            .buttonStyle(.plain)
                            .foregroundStyle(method == option ? Color.accentColor : .secondary)
                            .glassSurface(tint: method == option ? .accentColor.opacity(0.25) : nil, interactive: true)
                            .accessibilityLabel(option.title)
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
                        .textInputAutocapitalization(.never)
                        .autocorrectionDisabled()
                        .lineLimit(2...4)
                case .certificate, .authority:
                    Button(certificate.map { Self.bundleName($0) } ?? "Choose PEM", systemImage: "doc") {
                        choosingCertificate = true
                    }
                    .glassButton()
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
        }
    }

    private var accountStep: some View {
        VStack(spacing: 18) {
            Image(systemName: "person.crop.circle")
                .font(.system(size: 34))
                .foregroundStyle(.tint)
                .frame(width: 76, height: 76)
                .glassSurface(Circle(), tint: .accentColor.opacity(0.18))
            VStack(spacing: 10) {
                TextField("Username", text: $username)
                    .textContentType(.username)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                SecureField("Password", text: $password)
                    .textContentType(.password)
                    .submitLabel(.go)
                    .onSubmit(next)
            }
            .textFieldStyle(.roundedBorder)
            .frame(maxWidth: 320)
        }
    }

    private var footer: some View {
        HStack {
            if step != .address {
                Button("Back", systemImage: "chevron.left") {
                    failure = nil
                    step = Step(rawValue: step.rawValue - 1) ?? .address
                }
                .labelStyle(.iconOnly)
                .glassButton()
                .controlSize(.large)
            }
            Button(action: next) {
                Group {
                    if working {
                        ProgressView()
                    } else {
                        Text(step == .account ? "Connect" : "Continue")
                    }
                }
                .frame(maxWidth: .infinity)
            }
            .glassButton(prominent: true)
            .controlSize(.large)
            .disabled(!canContinue || working)
        }
        .padding(.horizontal, 24)
        .padding(.bottom, 8)
        .frame(maxWidth: 520)
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

    /// The picked bundle's own name, without the prefix that keeps copies apart.
    static func bundleName(_ url: URL) -> String {
        let name = url.lastPathComponent
        guard let dash = name.firstIndex(of: "-"), name.distance(from: name.startIndex, to: dash) == 8 else {
            return name
        }
        return String(name[name.index(after: dash)...])
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
                // The pin holds the fingerprint; the copy is no longer needed.
                if let certificate { try? FileManager.default.removeItem(at: certificate) }
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
}

private struct StepDots: View {
    let step: AddServerView.Step

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

/// Grouped character slots over an invisible text field: typing or pasting
/// fills the slots, and the next slot is highlighted.
struct CodeEntry: View {
    @Binding var code: String
    @FocusState private var focused: Bool

    var body: some View {
        ZStack {
            TextField("", text: Binding(get: { code }, set: { code = PairingCode.normalize($0) }))
                .textInputAutocapitalization(.never)
                .autocorrectionDisabled()
                .keyboardType(.asciiCapable)
                .focused($focused)
                .opacity(0.02)
                .frame(width: 1, height: 1)
            VStack(spacing: 8) {
                ForEach(Array(rows.enumerated()), id: \.offset) { _, row in
                    HStack(spacing: 10) {
                        ForEach(row, id: \.self) { groupIndex in
                            group(groupIndex)
                        }
                    }
                }
            }
            .contentShape(.rect)
            .onTapGesture { focused = true }
        }
        .padding(.vertical, 12)
        .padding(.horizontal, 14)
        .glassSurface(RoundedRectangle(cornerRadius: 16, style: .continuous),
                      tint: code.count == PairingCode.length ? .green.opacity(0.18) : nil)
        .onAppear { focused = true }
        .accessibilityLabel("Pairing code, \(code.count) of \(PairingCode.length) characters")
    }

    /// Seven groups on two rows, so the code fits a phone.
    private var rows: [[Int]] { [[0, 1, 2, 3], [4, 5, 6]] }

    private func group(_ groupIndex: Int) -> some View {
        let groups = PairingCode.split(code)
        return HStack(spacing: 2) {
            ForEach(Array(groups[groupIndex].enumerated()), id: \.offset) { offset, character in
                let index = groupIndex * 4 + offset
                slot(character, current: focused && index == code.count)
            }
        }
    }

    private func slot(_ character: Character?, current: Bool) -> some View {
        VStack(spacing: 3) {
            Text(character.map(String.init) ?? " ")
                .font(.system(.title3, design: .monospaced).weight(.medium))
                .frame(width: 15, height: 24)
            Capsule()
                .fill(current ? Color.accentColor : character == nil ? Color.secondary.opacity(0.35) : .clear)
                .frame(width: 12, height: 2)
        }
    }
}
