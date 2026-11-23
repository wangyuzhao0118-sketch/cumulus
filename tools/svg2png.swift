// 把 SVG 渲染成指定尺寸的 PNG（用 macOS 自带的 NSImage 解析 SVG）。用法：svg2png 输入.svg 输出.png 尺寸
import AppKit
let args = CommandLine.arguments
let size = Int(args[3]) ?? 1024
guard let img = NSImage(contentsOfFile: args[1]) else { print("load failed"); exit(1) }
let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: size, pixelsHigh: size, bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
NSGraphicsContext.saveGraphicsState()
NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
img.draw(in: NSRect(x: 0, y: 0, width: size, height: size))
NSGraphicsContext.restoreGraphicsState()
try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: args[2]))
