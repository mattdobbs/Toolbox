import AppKit

@MainActor
final class AppDelegate: NSObject, NSApplicationDelegate, NSMenuItemValidation, NSMenuDelegate {
    private var controllers: [WorkspaceWindowController] = []

    var workspaces: [WorkspaceWindowController] { controllers }

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

    /// `Toolbox --connect <name or id>` opens that connection on launch;
    /// `--files <name or id>` opens its SFTP browser.
    private func openConnectionFromArguments(in controller: WorkspaceWindowController) {
        let args = CommandLine.arguments
        for (index, flag) in args.enumerated() where index + 1 < args.count {
            let wanted = args[index + 1]
            let action: ((Connection) -> Void)?
            switch flag {
            case "--connect": action = { controller.open($0) }
            case "--files": action = { controller.openFiles($0) }
            default: action = nil
            }
            guard let action else { continue }
            guard let connection = AppModel.shared.connections.first(where: { $0.id == wanted || $0.name == wanted }) else {
                NSLog("\(flag): no connection named \(wanted)")
                continue
            }
            action(connection)
        }
        // Testing aid: move the focused pane into a fresh workspace tab.
        if args.contains("--demo-move") {
            controller.movePaneToNewWorkspace(nil)
        }
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
        // Workspaces are tabs; keep the bar visible so the "+" is always there.
        if let window = controller.window, let group = window.tabGroup, !group.isTabBarVisible {
            window.toggleTabBar(nil)
        }
        return controller
    }

    /// Fills the "Move Pane to Workspace" submenu with the other workspaces.
    func menuNeedsUpdate(_ menu: NSMenu) {
        menu.removeAllItems()
        let current = (NSApp.keyWindow?.windowController as? WorkspaceWindowController)
            ?? (NSApp.mainWindow?.windowController as? WorkspaceWindowController)
        for workspace in controllers where workspace !== current {
            let item = NSMenuItem(title: workspace.window?.title ?? "Workspace",
                                  action: #selector(WorkspaceWindowController.movePaneToWorkspace(_:)),
                                  keyEquivalent: "")
            item.representedObject = workspace
            menu.addItem(item)
        }
        if menu.items.isEmpty {
            let none = NSMenuItem(title: "No Other Workspaces", action: nil, keyEquivalent: "")
            none.isEnabled = false
            menu.addItem(none)
        }
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
