// SPDX-License-Identifier: Apache-2.0
// Regenerate the iOS icon from the macOS one:
//   swift scripts/make-icon.swift ../../macos/app/quicKFS/Resources/Assets.xcassets/AppIcon.appiconset/icon_512x512@2x.png quicKFS/Resources/Assets.xcassets/AppIcon.appiconset/icon-1024.png
// App Store Connect rejects an iOS icon with an alpha channel, and iOS masks
// the icon itself, so the result is opaque and full bleed.
import CoreGraphics
import Foundation
import ImageIO
import UniformTypeIdentifiers

// Takes the opaque interior of the macOS icon's rounded tile (well clear of
// its corners and rim), scales it so the glyph keeps its proportion, and
// continues the tile's diagonal gradient out to the edges: an opaque,
// full-bleed 1024 square for iOS to mask.
let args = CommandLine.arguments
let source = CGImageSourceCreateWithURL(URL(fileURLWithPath: args[1]) as CFURL, nil)!
let mac = CGImageSourceCreateImageAtIndex(source, 0, nil)!

func pixel(_ x: Int, _ y: Int) -> CGColor {
    var data = [UInt8](repeating: 0, count: 4)
    let context = CGContext(data: &data, width: 1, height: 1, bitsPerComponent: 8, bytesPerRow: 4,
                            space: CGColorSpace(name: CGColorSpace.sRGB)!,
                            bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
    context.draw(mac, in: CGRect(x: -x, y: -(mac.height - 1 - y), width: mac.width, height: mac.height))
    return CGColor(srgbRed: CGFloat(data[0]) / 255, green: CGFloat(data[1]) / 255, blue: CGFloat(data[2]) / 255, alpha: 1)
}

let inset = 140, span = 744           // interior square in macOS icon pixels
let scale = 1024.0 / 920.0            // the tile is 920 px wide in the macOS icon
let drawn = Double(span) * scale
let origin = (1024 - drawn) / 2

let size = 1024
let space = CGColorSpace(name: CGColorSpace.sRGB)!
let context = CGContext(data: nil, width: size, height: size, bitsPerComponent: 8, bytesPerRow: 0,
                        space: space, bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue)!
let gradient = CGGradient(colorsSpace: space,
                          colors: [pixel(inset + 2, inset + 2), pixel(inset + span - 3, inset + span - 3)] as CFArray,
                          locations: [0, 1])!
// Same endpoints as the interior's corners, extended past both ends.
context.drawLinearGradient(gradient,
                           start: CGPoint(x: origin, y: Double(size) - origin),
                           end: CGPoint(x: origin + drawn, y: Double(size) - origin - drawn),
                           options: [.drawsBeforeStartLocation, .drawsAfterEndLocation])
let interior = mac.cropping(to: CGRect(x: inset, y: inset, width: span, height: span))!
context.interpolationQuality = .high
context.draw(interior, in: CGRect(x: origin, y: origin, width: drawn, height: drawn))
let destination = CGImageDestinationCreateWithURL(URL(fileURLWithPath: args[2]) as CFURL, UTType.png.identifier as CFString, 1, nil)!
CGImageDestinationAddImage(destination, context.makeImage()!, nil)
CGImageDestinationFinalize(destination)
