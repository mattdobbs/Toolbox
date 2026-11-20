import AppKit

/// Testing aid behind `--snapshot <dir>`: pictures of the app's windows,
/// drawn by the app itself. Unlike a screenshot it needs no Screen Recording
/// permission and works with the screen locked. Remote desktop panes are
/// saved separately, straight from the surface they display, because a
/// layer showing a surface does not draw into a bitmap.
@MainActor
enum Snapshot {
    static func write(to directory: URL) {
        try? FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        var desktops = 0
        var report: [String] = []
        defer { save(report.joined(separator: "\n").data(using: .utf8), as: "snapshot.txt", in: directory) }
        func note(_ line: String) {
            NSLog("snapshot: \(line)")
            report.append(line)
        }
        for (index, window) in NSApp.windows.enumerated() where window.isVisible {
            guard let view = window.contentView?.superview ?? window.contentView,
                  let bitmap = view.bitmapImageRepForCachingDisplay(in: view.bounds) else { continue }
            view.cacheDisplay(in: view.bounds, to: bitmap)
            let name = "window-\(index).png"
            save(bitmap.representation(using: .png, properties: [:]), as: name, in: directory)
            note("\(name) \(Int(view.bounds.width))x\(Int(view.bounds.height)) \(type(of: window)) title=\(window.title)")
            for pane in remoteDesktops(in: view) {
                desktops += 1
                let name = "desktop-\(desktops).png"
                let image = pane.displayedImage()
                if let image {
                    save(NSBitmapImageRep(cgImage: image).representation(using: .png, properties: [:]), as: name, in: directory)
                }
                note("\(name) \(pane.snapshotDescription) saved=\(image != nil)")
            }
        }
    }

    private static func save(_ data: Data?, as name: String, in directory: URL) {
        guard let data else { return }
        try? data.write(to: directory.appendingPathComponent(name))
    }

    private static func remoteDesktops(in view: NSView) -> [RdpSessionView] {
        var found: [RdpSessionView] = []
        if let pane = view as? RdpSessionView { found.append(pane) }
        for child in view.subviews { found += remoteDesktops(in: child) }
        return found
    }
}
