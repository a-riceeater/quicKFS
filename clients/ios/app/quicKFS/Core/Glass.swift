// SPDX-License-Identifier: Apache-2.0
import SwiftUI

/// Liquid Glass on iOS 26, and the closest material on iOS 18, so every view
/// can ask for glass once without availability checks of its own.
extension View {
    @ViewBuilder
    func glassSurface(_ shape: some Shape = Capsule(), tint: Color? = nil, interactive: Bool = false) -> some View {
        if #available(iOS 26, *) {
            glassEffect(Self.glass(tint: tint, interactive: interactive), in: shape)
        } else {
            background {
                shape.fill(.regularMaterial)
                    .overlay { if let tint { shape.fill(tint) } }
                    .overlay { shape.stroke(.white.opacity(0.08), lineWidth: 0.5) }
            }
        }
    }

    @available(iOS 26, *)
    private static func glass(tint: Color?, interactive: Bool) -> Glass {
        var glass = Glass.regular
        if let tint { glass = glass.tint(tint) }
        if interactive { glass = glass.interactive() }
        return glass
    }

    /// The glass button style, or a bordered one before iOS 26.
    @ViewBuilder
    func glassButton(prominent: Bool = false) -> some View {
        if #available(iOS 26, *) {
            if prominent { buttonStyle(.glassProminent) } else { buttonStyle(.glass) }
        } else {
            if prominent { buttonStyle(.borderedProminent) } else { buttonStyle(.bordered) }
        }
    }
}

/// Groups glass shapes so they blend and morph together on iOS 26.
struct GlassGroup<Content: View>: View {
    var spacing: CGFloat = 8
    @ViewBuilder var content: () -> Content

    var body: some View {
        if #available(iOS 26, *) {
            GlassEffectContainer(spacing: spacing) { content() }
        } else {
            content()
        }
    }
}
