// SPDX-License-Identifier: Apache-2.0
import AVFoundation
import SwiftUI

@main
struct QuickFSApp: App {
    @State private var model = AppModel()

    init() {
        // Videos play with sound even with the ring switch off, and audio
        // keeps playing with the screen locked, as in any media app.
        try? AVAudioSession.sharedInstance().setCategory(.playback, mode: .moviePlayback)
    }

    var body: some Scene {
        WindowGroup {
            RootView()
                .environment(model)
                .task { await model.bootstrap() }
        }
    }
}
