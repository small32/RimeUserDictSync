import AppKit

// Vector artwork in a 1024 pt canvas. Keep the legacy ICNS silhouette inset
// so Finder and the Dock display it at the same visual size as other Mac apps.
let output = URL(fileURLWithPath: CommandLine.arguments[1], isDirectory: true)
try FileManager.default.createDirectory(at: output, withIntermediateDirectories: true)

func render(_ pixels: Int) -> Data {
    let bitmap = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: pixels,
        pixelsHigh: pixels, bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true,
        isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: bitmap)
    let context = NSGraphicsContext.current!.cgContext
    context.scaleBy(x: CGFloat(pixels) / 1024, y: CGFloat(pixels) / 1024)

    let tile = NSBezierPath(roundedRect: NSRect(x: 100, y: 100, width: 824, height: 824),
        xRadius: 190, yRadius: 190)
    NSGraphicsContext.saveGraphicsState()
    let shadow = NSShadow()
    shadow.shadowColor = NSColor.black.withAlphaComponent(0.22)
    shadow.shadowOffset = NSSize(width: 0, height: -10)
    shadow.shadowBlurRadius = 18
    shadow.set()
    NSColor(calibratedWhite: 0.16, alpha: 1).setFill()
    tile.fill()
    NSGraphicsContext.restoreGraphicsState()
    NSGradient(starting: NSColor(calibratedWhite: 0.10, alpha: 1),
        ending: NSColor(calibratedWhite: 0.29, alpha: 1))!.draw(in: tile, angle: 90)
    NSColor.white.withAlphaComponent(0.18).setStroke()
    tile.lineWidth = 3
    tile.stroke()

    // RIME's familiar three-stroke mark, redrawn as vectors for small sizes.
    let mark = NSBezierPath()
    mark.move(to: NSPoint(x: 246, y: 740))
    for point in [NSPoint(x: 330, y: 740), NSPoint(x: 324, y: 432),
                  NSPoint(x: 470, y: 432), NSPoint(x: 470, y: 796),
                  NSPoint(x: 554, y: 796), NSPoint(x: 550, y: 432),
                  NSPoint(x: 696, y: 432), NSPoint(x: 690, y: 740),
                  NSPoint(x: 774, y: 740), NSPoint(x: 774, y: 356),
                  NSPoint(x: 550, y: 356), NSPoint(x: 550, y: 246),
                  NSPoint(x: 800, y: 246), NSPoint(x: 800, y: 170),
                  NSPoint(x: 224, y: 170), NSPoint(x: 224, y: 246),
                  NSPoint(x: 470, y: 246), NSPoint(x: 470, y: 356),
                  NSPoint(x: 246, y: 356)] { mark.line(to: point) }
    mark.close()
    // Scale the mark inside the tile, leaving a comfortable optical margin.
    let transform = AffineTransform(translationByX: 512, byY: 512)
    var placement = transform
    placement.scale(0.82)
    placement.translate(x: -512, y: -480)
    mark.transform(using: placement)
    NSColor(calibratedWhite: 0.97, alpha: 1).setFill()
    mark.fill()
    NSGraphicsContext.restoreGraphicsState()
    return bitmap.representation(using: .png, properties: [:])!
}

for size in [16, 32, 128, 256, 512] {
    try render(size).write(to: output.appendingPathComponent("icon_\(size)x\(size).png"))
    try render(size * 2).write(to: output.appendingPathComponent("icon_\(size)x\(size)@2x.png"))
}
