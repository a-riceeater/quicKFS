// SPDX-License-Identifier: Apache-2.0
import Combine
import Foundation
import Observation
import Sparkle

/// Sparkle updates. The updater only starts when the build carries both a
/// feed URL and an EdDSA public key, so unsigned local builds never nag.
@MainActor
@Observable
final class Updater {
    private let controller: SPUStandardUpdaterController
    private var observation: AnyCancellable?
    private(set) var canCheck = false
    let isConfigured: Bool

    init() {
        controller = SPUStandardUpdaterController(
            startingUpdater: false, updaterDelegate: nil, userDriverDelegate: nil)
        let info = Bundle.main.infoDictionary ?? [:]
        let feed = (info["SUFeedURL"] as? String) ?? ""
        let key = (info["SUPublicEDKey"] as? String) ?? ""
        isConfigured = !feed.isEmpty && !key.isEmpty
        guard isConfigured else { return }
        controller.startUpdater()
        observation = controller.updater.publisher(for: \.canCheckForUpdates)
            .receive(on: RunLoop.main)
            .sink { [weak self] value in self?.canCheck = value }
    }

    var automaticallyChecks: Bool {
        get { isConfigured && controller.updater.automaticallyChecksForUpdates }
        set { controller.updater.automaticallyChecksForUpdates = newValue }
    }

    var automaticallyDownloads: Bool {
        get { isConfigured && controller.updater.automaticallyDownloadsUpdates }
        set { controller.updater.automaticallyDownloadsUpdates = newValue }
    }

    var lastChecked: Date? { controller.updater.lastUpdateCheckDate }

    func checkForUpdates() {
        guard isConfigured else { return }
        controller.checkForUpdates(nil)
    }
}
