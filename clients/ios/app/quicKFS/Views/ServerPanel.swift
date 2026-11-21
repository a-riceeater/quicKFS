// SPDX-License-Identifier: Apache-2.0
import SwiftUI

/// A server's overview, console, and settings, as the Mac's detail tabs.
struct ServerPanel: View {
    enum Tab: String, CaseIterable, Identifiable {
        case overview, console, settings
        var id: Self { self }

        var symbol: String {
            switch self {
            case .overview: "gauge.with.dots.needle.67percent"
            case .console: "terminal"
            case .settings: "slider.horizontal.3"
            }
        }

        var title: String { rawValue.capitalized }
    }

    let profile: ServerProfile
    let state: ServerState
    @State var tab: Tab
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        NavigationStack {
            Group {
                switch tab {
                case .overview: OverviewView(profile: profile, state: state)
                case .console: ConsoleView(profile: profile, state: state)
                case .settings: ServerSettingsView(profile: profile)
                }
            }
            .navigationTitle(profile.name)
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .principal) {
                    Picker("View", selection: $tab) {
                        ForEach(Tab.allCases) { tab in
                            Image(systemName: tab.symbol)
                                .accessibilityLabel(tab.title)
                                .tag(tab)
                        }
                    }
                    .pickerStyle(.segmented)
                    .frame(maxWidth: 220)
                }
                ToolbarItem(placement: .confirmationAction) { Button("Done") { dismiss() } }
            }
        }
    }
}
