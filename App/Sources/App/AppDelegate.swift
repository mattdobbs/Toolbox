import AppKit

@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate, NSMenuItemValidation {
    private var controllers: [WorkspaceWindowController] = []

    func applicationDidFinishLaunching(_ notification: Notification) {
        MainMenu.install()
        _ = AppModel.shared
        let controller = newWorkspaceWindow(asTab: false)
        NSApp.activate(ignoringOtherApps: true)
        Task { @MainActor in
            // Providers with auto-refresh run at launch, then on their timers.
            await ProviderSync.shared.syncAll(automatic: true)
            ProviderSync.shared.scheduleAutoRefresh()
            self.openConnectionFromArguments(in: controller)
        }
    }

    @objc func syncProviders(_ sender: Any?) {
        Task { @MainActor in
            await ProviderSync.shared.syncAll(automatic: false)
        }
    }

    @objc func lockVault(_ sender: Any?) {
        VaultManager.shared.lock()
    }

    /// `Toolbox --connect <name or id>` opens that connection on launch.
    private func openConnectionFromArguments(in controller: WorkspaceWindowController) {
        let args = CommandLine.arguments
        guard let flag = args.firstIndex(of: "--connect"), flag + 1 < args.count else { return }
        let wanted = args[flag + 1]
        guard let connection = AppModel.shared.connections.first(where: { $0.id == wanted || $0.name == wanted }) else {
            NSLog("--connect: no connection named \(wanted)")
            return
        }
        controller.open(connection)
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        false
    }

    func applicationShouldHandleReopen(_ sender: NSApplication, hasVisibleWindows: Bool) -> Bool {
        if !hasVisibleWindows {
            newWorkspaceWindow(asTab: false)
        }
        return true
    }

    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        let live = controllers.reduce(0) { $0 + $1.paneVC.liveSessionCount }
        if live > 0,
           !Prompts.confirm(title: "Quit Toolbox?",
                            message: "\(live) active session\(live == 1 ? "" : "s") will be disconnected.",
                            ok: "Quit") {
            return .terminateCancel
        }
        controllers.forEach { $0.paneVC.terminateAll() }
        return .terminateNow
    }

    // MARK: Workspaces

    @objc func newWorkspace(_ sender: Any?) {
        newWorkspaceWindow(asTab: true)
    }

    /// AppKit sends this when the "+" button in the native tab bar is clicked.
    @objc func newWindowForTab(_ sender: Any?) {
        newWorkspaceWindow(asTab: true)
    }

    @discardableResult
    func newWorkspaceWindow(asTab: Bool) -> WorkspaceWindowController {
        WorkspaceWindowController.counter += 1
        let controller = WorkspaceWindowController(name: "Workspace \(WorkspaceWindowController.counter)")
        controllers.append(controller)
        if asTab,
           let key = NSApp.keyWindow ?? NSApp.mainWindow,
           let window = controller.window,
           key.tabbingIdentifier == window.tabbingIdentifier {
            key.addTabbedWindow(window, ordered: .above)
        }
        controller.showWindow(nil)
        controller.window?.makeKeyAndOrderFront(nil)
        return controller
    }

    func windowControllerDidClose(_ controller: WorkspaceWindowController) {
        controllers.removeAll { $0 === controller }
    }

    // MARK: App menu

    @objc func showAbout(_ sender: Any?) {
        NSApp.orderFrontStandardAboutPanel(options: [
            .applicationName: "Toolbox",
            .version: "core \(coreVersion())",
        ])
    }

    // MARK: View menu

    @objc func selectTheme(_ sender: NSMenuItem) {
        guard let id = sender.representedObject as? String,
              let theme = TerminalTheme.all.first(where: { $0.id == id }) else { return }
        ThemeStore.shared.current = theme
    }

    @objc func increaseFontSize(_ sender: Any?) { ThemeStore.shared.fontSize += 1 }
    @objc func decreaseFontSize(_ sender: Any?) { ThemeStore.shared.fontSize -= 1 }
    @objc func resetFontSize(_ sender: Any?) { ThemeStore.shared.fontSize = ThemeStore.defaultFontSize }

    func validateMenuItem(_ menuItem: NSMenuItem) -> Bool {
        if menuItem.action == #selector(selectTheme(_:)) {
            let id = menuItem.representedObject as? String
            menuItem.state = id == ThemeStore.shared.current.id ? .on : .off
        }
        return true
    }
}
