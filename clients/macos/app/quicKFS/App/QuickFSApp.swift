// SPDX-License-Identifier: Apache-2.0
import AppKit
import SwiftUI

@main
struct QuickFSApp: App {
    @NSApplicationDelegateAdaptor(AppDelegate.self) private var delegate
    @State private var model = AppModel()
    @State private var updater = Updater()

    var body: some Scene {
        Window("quicKFS", id: "main") {
            RootView()
                .environment(model)
                .environment(updater)
                .task {
                    delegate.model = model
                    await model.bootstrap()
                }
        }
        .defaultSize(width: 1040, height: 680)
        .windowToolbarStyle(.unified)
        .commands {
            CommandGroup(after: .appInfo) {
                Button("Check for Updates…") { updater.checkForUpdates() }
                    .disabled(!updater.canCheck)
            }
            CommandGroup(replacing: .newItem) {
                Button("New Server…") { model.isAddingServer = true }
                    .keyboardShortcut("n")
            }
            ServerCommands(model: model)
        }

        MenuBarExtra {
            MenuBarView()
                .environment(model)
        } label: {
            Image(systemName: model.activeMounts > 0 ? "externaldrive.fill.badge.wifi" : "externaldrive.badge.wifi")
        }
        .menuBarExtraStyle(.window)

        Settings {
            SettingsView()
                .environment(model)
                .environment(updater)
        }
    }
}

/// Keyboard access to the selected server's primary actions.
struct ServerCommands: Commands {
    let model: AppModel

    var body: some Commands {
        CommandMenu("Server") {
            Button(mountTitle) {
                if let id = model.selection { model.toggleMount(id) }
            }
            .keyboardShortcut("m", modifiers: [.command, .shift])
            .disabled(model.selection == nil)

            Button("Open in Finder") {
                if let id = model.selection { model.state(id).mount.revealInFinder() }
            }
            .keyboardShortcut("o", modifiers: [.command, .shift])
            .disabled(!(model.selection.map { model.state($0).mount.isMounted } ?? false))
        }
    }

    private var mountTitle: String {
        guard let id = model.selection else { return "Mount" }
        return model.state(id).mount.isActive ? "Unmount" : "Mount"
    }
}

final class AppDelegate: NSObject, NSApplicationDelegate {
    @MainActor var model: AppModel?

    func applicationDidFinishLaunching(_ notification: Notification) {
        // The mount helper can exit before reading its password; a write to
        // its closed stdin must fail instead of terminating the app.
        signal(SIGPIPE, SIG_IGN)
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool { false }

    @MainActor
    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        guard let model, model.activeMounts > 0 else { return .terminateNow }
        Task {
            await model.shutdown()
            sender.reply(toApplicationShouldTerminate: true)
        }
        return .terminateLater
    }
}
