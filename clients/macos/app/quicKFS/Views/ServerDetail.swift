// SPDX-License-Identifier: Apache-2.0
import SwiftUI

struct ServerDetail: View {
    let profile: ServerProfile
    let state: ServerState
    @Environment(AppModel.self) private var model
    @AppStorage("serverTab") private var tab: Tab = .files

    enum Tab: String, CaseIterable, Identifiable {
        case files, overview, console, settings
        var id: Self { self }

        var symbol: String {
            switch self {
            case .files: "folder"
            case .overview: "gauge.with.dots.needle.67percent"
            case .console: "terminal"
            case .settings: "slider.horizontal.3"
            }
        }

        var title: String { rawValue.capitalized }
    }

    var body: some View {
        content
            .navigationTitle(profile.name)
            .navigationSubtitle("\(profile.username)@\(profile.host)")
            .toolbar {
                ToolbarItem(placement: .principal) {
                    Picker("View", selection: $tab) {
                        ForEach(Tab.allCases) { tab in
                            Image(systemName: tab.symbol)
                                .help(tab.title)
                                .tag(tab)
                        }
                    }
                    .pickerStyle(.segmented)
                    .labelsHidden()
                }
                ToolbarSpacer(.flexible)
                ToolbarItemGroup(placement: .primaryAction) {
                    if state.mount.isMounted {
                        Button("Open in Finder", systemImage: "folder") { state.mount.revealInFinder() }
                            .help("Open in Finder")
                    }
                    MountButton(state: state) { model.toggleMount(profile.id) }
                }
            }
            .safeAreaInset(edge: .bottom, spacing: 0) {
                if let failure = state.failure {
                    FailureBanner(message: failure, fixSymbol: needsPassword ? "key.fill" : "wrench.and.screwdriver") {
                        if case .failed = state.mount.phase {
                            model.mount(profile.id)
                        } else {
                            Task { await state.browser.reload() }
                        }
                    } fix: {
                        if needsPassword {
                            model.passwordRequest = profile.id
                        } else {
                            tab = .settings
                        }
                    }
                    .padding(12)
                    .transition(.move(edge: .bottom).combined(with: .opacity))
                }
            }
            .animation(.smooth, value: state.failure)
    }

    private var needsPassword: Bool {
        if case .failed(let error) = state.connection.phase { return error.isAuthentication }
        return false
    }

    @ViewBuilder
    private var content: some View {
        switch tab {
        case .files: FileBrowser(profile: profile, connection: state.connection, browser: state.browser)
        case .overview: OverviewView(profile: profile, state: state)
        case .console: ConsoleView(profile: profile, state: state)
        case .settings: ServerSettingsView(profile: profile)
        }
    }
}

private struct MountButton: View {
    let state: ServerState
    let action: () -> Void

    var body: some View {
        let phase = state.mount.phase
        Button(action: action) {
            HStack(spacing: 6) {
                if phase == .starting || phase == .stopping {
                    ProgressView().controlSize(.small)
                } else {
                    Image(systemName: state.mount.isMounted ? "eject.fill" : "externaldrive.fill.badge.plus")
                        .contentTransition(.symbolEffect(.replace))
                }
                Text(title(phase))
            }
            .padding(.horizontal, 4)
        }
        .buttonStyle(.glassProminent)
        .tint(state.mount.isMounted ? .green : .accentColor)
        .disabled(phase == .stopping)
        .keyboardShortcut("m", modifiers: [.command, .shift])
        .animation(.smooth, value: phase)
    }

    private func title(_ phase: MountController.Phase) -> String {
        switch phase {
        case .idle, .failed: "Mount"
        case .starting: "Mounting"
        case .mounted: "Eject"
        case .stopping: "Ejecting"
        }
    }
}

private struct FailureBanner: View {
    let message: String
    let fixSymbol: String
    let retry: () -> Void
    let fix: () -> Void

    var body: some View {
        HStack(spacing: 12) {
            Image(systemName: "exclamationmark.triangle.fill")
                .foregroundStyle(.orange)
            Text(message)
                .font(.callout)
                .lineLimit(3)
                .textSelection(.enabled)
                .frame(maxWidth: .infinity, alignment: .leading)
            Button("Fix", systemImage: fixSymbol, action: fix)
                .labelStyle(.iconOnly)
                .help(fixSymbol == "key.fill" ? "Enter password" : "Review settings")
            Button("Retry", systemImage: "arrow.clockwise", action: retry)
                .buttonStyle(.glassProminent)
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 10)
        .glassEffect(.regular.tint(.orange.opacity(0.12)), in: .rect(cornerRadius: 16))
    }
}

/// Asks for (or replaces) a server password and stores it in the Keychain.
struct PasswordSheet: View {
    let profile: ServerProfile
    var mountAfter = false
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    @State private var password = ""

    var body: some View {
        VStack(spacing: 18) {
            Image(systemName: "key.fill")
                .font(.system(size: 30))
                .foregroundStyle(.tint)
                .frame(width: 60, height: 60)
                .glassEffect(.regular.tint(.accentColor.opacity(0.2)), in: .circle)
            VStack(spacing: 4) {
                Text(profile.name).font(.title3.weight(.semibold))
                Text(profile.username).foregroundStyle(.secondary)
            }
            SecureField("Password", text: $password)
                .textFieldStyle(.roundedBorder)
                .frame(width: 260)
                .onSubmit(save)
            HStack {
                Button("Cancel", role: .cancel) { dismiss() }
                    .keyboardShortcut(.cancelAction)
                Button(mountAfter ? "Mount" : "Save", action: save)
                    .buttonStyle(.glassProminent)
                    .keyboardShortcut(.defaultAction)
                    .disabled(password.isEmpty)
            }
        }
        .padding(28)
    }

    private func save() {
        guard !password.isEmpty else { return }
        model.setPassword(password, for: profile.id)
        dismiss()
        if mountAfter { model.mount(profile.id, password: password) }
    }
}
