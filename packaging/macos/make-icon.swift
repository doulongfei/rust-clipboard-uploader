// Draw a resolution-independent clipboard/upload icon using system AppKit only.
import AppKit

let output = CommandLine.arguments[1]
try FileManager.default.createDirectory(atPath: output, withIntermediateDirectories: true)
for size in [16, 32, 128, 256, 512] {
    for scale in [1, 2] {
        let pixels = size * scale
        let bitmap = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: pixels, pixelsHigh: pixels,
                                      bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true,
                                      isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
        NSGraphicsContext.saveGraphicsState()
        NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: bitmap)
        let transform = NSAffineTransform()
        transform.scale(by: CGFloat(pixels) / 1024)
        transform.concat()
        let background = NSBezierPath(roundedRect: NSRect(x: 80, y: 80, width: 864, height: 864), xRadius: 190, yRadius: 190)
        NSGradient(starting: NSColor(calibratedRed: 0.12, green: 0.55, blue: 1, alpha: 1),
                   ending: NSColor(calibratedRed: 0.16, green: 0.27, blue: 0.8, alpha: 1))!.draw(in: background, angle: 90)
        NSColor.white.setStroke()
        let board = NSBezierPath(roundedRect: NSRect(x: 294, y: 235, width: 436, height: 540), xRadius: 44, yRadius: 44)
        board.lineWidth = 42
        board.stroke()
        NSColor.white.setFill()
        NSBezierPath(roundedRect: NSRect(x: 415, y: 724, width: 194, height: 85), xRadius: 24, yRadius: 24).fill()
        let arrow = NSBezierPath()
        arrow.move(to: NSPoint(x: 512, y: 355))
        arrow.line(to: NSPoint(x: 512, y: 625))
        arrow.move(to: NSPoint(x: 404, y: 517))
        arrow.line(to: NSPoint(x: 512, y: 625))
        arrow.line(to: NSPoint(x: 620, y: 517))
        arrow.lineWidth = 46
        arrow.lineCapStyle = .round
        arrow.lineJoinStyle = .round
        arrow.stroke()
        NSGraphicsContext.restoreGraphicsState()
        let suffix = scale == 2 ? "@2x" : ""
        let url = URL(fileURLWithPath: "\(output)/icon_\(size)x\(size)\(suffix).png")
        try bitmap.representation(using: .png, properties: [:])!.write(to: url)
    }
}
