import AppKit
import SwiftUI

/// One workspace: a window (shown as a native tab) holding the connection
/// sidebar and a pane layout.
final class WorkspaceWindowController: NSWindowController, NSWindowDelegate, NSToolbarDelegate,
    NSMenuItemValidation, NSToolbarItemValidation {
    static var counter = 0
    static let tabbingIdentifier = NSWindow.TabbingIdentifier("com.dobbs.Toolbox.workspace")

    let paneVC = PaneContainerViewController()
    private let splitVC = NSSplitViewController()
    private var editorSheet: NSWindow?

    init(name: String) {
        let window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 1280, height: 820),
            styleMask: [.titled, .closable, .miniaturizable, .resizable],
            backing: .buffered,
            defer: false
        )
        window.title = name
        window.tabbingMode = .preferred
        window.tabbingIdentifier = Self.tabbingIdentifier
        window.toolbarStyle = .unified
        window.minSize = NSSize(width: 720, height: 420)
        window.center()
        super.init(window: window)
        window.delegate = self

        let sidebar = NSHostingController(rootView: SidebarView(
            model: AppModel.shared,
            actions: SidebarActions(
                openConnection: { [weak self] in self?.open($0) },
                editConnection: { [weak self] in self?.edit($0) },
                deleteConnection: { [weak self] in self?.delete($0) },
                editCredential: { [weak self] in self?.editCredential($0) },
                deleteCredential: { [weak self] in self?.deleteCredential($0) },
                editProvider: { [weak self] in self?.editProvider($0) },
                deleteProvider: { [weak self] in self?.deleteProvider($0) },
                syncProvider: { [weak self] in self?.syncProvider($0) }
            )
        ))
        let sidebarItem = NSSplitViewItem(sidebarWithViewController: sidebar)
        sidebarItem.minimumThickness = 220
        sidebarItem.maximumThickness = 420
        sidebarItem.canCollapse = true
        let content = NSSplitViewItem(viewController: paneVC)
        content.minimumThickness = 420
        splitVC.addSplitViewItem(sidebarItem)
        splitVC.addSplitViewItem(content)
        window.contentViewController = splitVC
        // Assigning a content view controller shrinks the window to its fitting
        // size; restore the intended size afterwards.
        window.setContentSize(NSSize(width: 1280, height: 820))
        window.center()

        let toolbar = NSToolbar(identifier: "workspace")
        toolbar.delegate = self
        toolbar.displayMode = .iconOnly
        toolbar.allowsUserCustomization = false
        window.toolbar = toolbar

        paneVC.onLayoutChange = { [weak self] in self?.window?.toolbar?.validateVisibleItems() }
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError() }

    // MARK: Connections

    func open(_ connection: Connection) {
        let pane = paneVC.paneForNewSession()
        switch connection.kind {
        case .ssh:
            let session = TerminalSessionView(connection: connection)
            pane.setContent(session)
            paneVC.setFocus(pane.id, makeFirstResponder: true)
            Task { @MainActor in
                do {
                    guard let resolved = try await CredentialResolver.sshAuth(for: connection) else {
                        session.cancelled()
                        return
                    }
                    session.start(username: resolved.username, auth: resolved.auth)
                } catch {
                    session.failed(describe(error))
                }
            }
        case .rdp:
            let session = RdpSessionView(connection: connection)
            pane.setContent(session)
            paneVC.setFocus(pane.id, makeFirstResponder: true)
            Task { @MainActor in
                do {
                    guard let resolved = try await CredentialResolver.passwordAuth(for: connection) else {
                        session.cancelled()
                        return
                    }
                    session.start(username: resolved.username, password: resolved.password, domain: resolved.domain)
                } catch {
                    session.failed(describe(error))
                }
            }
        case .vnc:
            let session = VncSessionView(connection: connection)
            pane.setContent(session)
            paneVC.setFocus(pane.id, makeFirstResponder: true)
            Task { @MainActor in
                do {
                    guard let resolved = try await CredentialResolver.passwordAuth(for: connection) else {
                        session.cancelled()
                        return
                    }
                    session.start(username: resolved.username, password: resolved.password)
                } catch {
                    session.failed(describe(error))
                }
            }
        }
    }

    private func presentSheet<Content: View>(_ content: Content) {
        guard editorSheet == nil else { return }
        let sheet = NSWindow(contentViewController: NSHostingController(rootView: content))
        sheet.styleMask = [.titled]
        editorSheet = sheet
        window?.beginSheet(sheet)
    }

    func edit(_ connection: Connection?) {
        guard editorSheet == nil else { return }
        let editor = ConnectionEditorView(
            model: AppModel.shared,
            draft: connection.map(ConnectionDraft.init) ?? ConnectionDraft(),
            onSave: { [weak self] result in
                guard let self else { return }
                do {
                    try AppModel.shared.save(result)
                    self.endEditor()
                } catch {
                    Prompts.error(title: "Couldn't save the connection", message: describe(error))
                }
            },
            onCancel: { [weak self] in self?.endEditor() }
        )
        presentSheet(editor)
    }

    // MARK: Credentials

    func editCredential(_ credential: Credential?) {
        let editor = CredentialEditorView(
            draft: credential.map(CredentialDraft.init) ?? CredentialDraft(),
            onSave: { [weak self] record, secret in
                guard let self else { return }
                Task { @MainActor in
                    do {
                        try await AppModel.shared.saveCredential(record, secret: secret)
                        self.endEditor()
                    } catch {
                        Prompts.error(title: "Couldn't save the credential", message: describe(error))
                    }
                }
            },
            onCancel: { [weak self] in self?.endEditor() }
        )
        presentSheet(editor)
    }

    func deleteCredential(_ credential: Credential) {
        let users = AppModel.shared.store.connectionsUsingCredential(id: credential.id)
        var message = "The credential is removed from Toolbox."
        if case .vault = credential.kind { message = "The stored secret is deleted from the vault." }
        if !users.isEmpty {
            message += " \(users.count) connection\(users.count == 1 ? "" : "s") still reference it and will ask for a password instead."
        }
        guard Prompts.confirm(title: "Delete “\(credential.name)”?", message: message, ok: "Delete") else { return }
        do {
            try AppModel.shared.deleteCredential(credential)
        } catch {
            Prompts.error(title: "Couldn't delete the credential", message: describe(error))
        }
    }

    // MARK: Providers

    func editProvider(_ provider: Provider?) {
        let editor = ProviderEditorView(
            model: AppModel.shared,
            draft: provider.map(ProviderDraft.init) ?? ProviderDraft(),
            onSave: { [weak self] record, syncNow in
                guard let self else { return }
                do {
                    let stored = try AppModel.shared.saveProvider(record)
                    self.endEditor()
                    if syncNow {
                        self.syncProvider(stored)
                    }
                } catch {
                    Prompts.error(title: "Couldn't save the provider", message: describe(error))
                }
            },
            onCancel: { [weak self] in self?.endEditor() }
        )
        presentSheet(editor)
    }

    func deleteProvider(_ provider: Provider) {
        guard Prompts.confirm(title: "Delete “\(provider.name)”?",
                              message: "Everything it generated is removed from the sidebar too.",
                              ok: "Delete") else { return }
        do {
            try AppModel.shared.deleteProvider(provider)
        } catch {
            Prompts.error(title: "Couldn't delete the provider", message: describe(error))
        }
    }

    func syncProvider(_ provider: Provider) {
        Task { @MainActor in
            await ProviderSync.shared.sync(provider, interactive: true)
        }
    }

    private func endEditor() {
        guard let sheet = editorSheet else { return }
        window?.endSheet(sheet)
        editorSheet = nil
    }

    func delete(_ connection: Connection) {
        guard Prompts.confirm(title: "Delete “\(connection.name)”?",
                              message: "This removes the connection from the sidebar. Open sessions are not affected.",
                              ok: "Delete") else { return }
        do {
            try AppModel.shared.delete(connection)
        } catch {
            Prompts.error(title: "Couldn't delete the connection", message: describe(error))
        }
    }

    // MARK: Actions (menu + toolbar)

    @objc func newConnection(_ sender: Any?) { edit(nil) }
    @objc func newCredential(_ sender: Any?) { editCredential(nil) }
    @objc func newProvider(_ sender: Any?) { editProvider(nil) }
    @objc func splitRight(_ sender: Any?) { paneVC.split(.horizontal) }
    @objc func splitDown(_ sender: Any?) { paneVC.split(.vertical) }
    @objc func focusNextPane(_ sender: Any?) { paneVC.focusNeighbor(1) }
    @objc func focusPreviousPane(_ sender: Any?) { paneVC.focusNeighbor(-1) }

    @objc func closePane(_ sender: Any?) {
        let pane = paneVC.focusedPane
        if pane.content?.isLive == true,
           !Prompts.confirm(title: "Close pane?", message: "The session will be disconnected.", ok: "Close") {
            return
        }
        paneVC.closePane(pane.id)
    }

    @objc func renameWorkspace(_ sender: Any?) {
        guard let window else { return }
        if let name = Prompts.askText(title: "Rename Workspace", message: "", initial: window.title, ok: "Rename"),
           !name.trimmingCharacters(in: .whitespaces).isEmpty {
            window.title = name
        }
    }

    private var canClosePane: Bool {
        paneVC.focusedPane.content != nil || paneVC.paneCount > 1
    }

    func validateMenuItem(_ menuItem: NSMenuItem) -> Bool {
        switch menuItem.action {
        case #selector(closePane(_:)): return canClosePane
        case #selector(focusNextPane(_:)), #selector(focusPreviousPane(_:)): return paneVC.paneCount > 1
        default: return true
        }
    }

    func validateToolbarItem(_ item: NSToolbarItem) -> Bool {
        item.itemIdentifier == Self.closePaneItem ? canClosePane : true
    }

    // MARK: NSWindowDelegate

    func windowShouldClose(_ sender: NSWindow) -> Bool {
        let live = paneVC.liveSessionCount
        guard live > 0 else { return true }
        return Prompts.confirm(title: "Close “\(sender.title)”?",
                               message: "\(live) active session\(live == 1 ? "" : "s") will be disconnected.",
                               ok: "Close")
    }

    func windowWillClose(_ notification: Notification) {
        paneVC.terminateAll()
        (NSApp.delegate as? AppDelegate)?.windowControllerDidClose(self)
    }

    // MARK: NSToolbarDelegate

    private static let newConnectionItem = NSToolbarItem.Identifier("newConnection")
    private static let splitRightItem = NSToolbarItem.Identifier("splitRight")
    private static let splitDownItem = NSToolbarItem.Identifier("splitDown")
    private static let closePaneItem = NSToolbarItem.Identifier("closePane")

    func toolbarDefaultItemIdentifiers(_ toolbar: NSToolbar) -> [NSToolbarItem.Identifier] {
        [.toggleSidebar, .sidebarTrackingSeparator, Self.newConnectionItem, .flexibleSpace,
         Self.splitRightItem, Self.splitDownItem, Self.closePaneItem]
    }

    func toolbarAllowedItemIdentifiers(_ toolbar: NSToolbar) -> [NSToolbarItem.Identifier] {
        toolbarDefaultItemIdentifiers(toolbar)
    }

    func toolbar(_ toolbar: NSToolbar, itemForItemIdentifier id: NSToolbarItem.Identifier,
                 willBeInsertedIntoToolbar flag: Bool) -> NSToolbarItem? {
        switch id {
        case Self.newConnectionItem:
            return toolbarItem(id, symbol: "plus", label: "New Connection", action: #selector(newConnection(_:)))
        case Self.splitRightItem:
            return toolbarItem(id, symbol: "rectangle.split.2x1", label: "Split Right", action: #selector(splitRight(_:)))
        case Self.splitDownItem:
            return toolbarItem(id, symbol: "rectangle.split.1x2", label: "Split Down", action: #selector(splitDown(_:)))
        case Self.closePaneItem:
            return toolbarItem(id, symbol: "xmark.rectangle", label: "Close Pane", action: #selector(closePane(_:)))
        default:
            return nil
        }
    }

    private func toolbarItem(_ id: NSToolbarItem.Identifier, symbol: String, label: String, action: Selector) -> NSToolbarItem {
        let item = NSToolbarItem(itemIdentifier: id)
        item.image = NSImage(systemSymbolName: symbol, accessibilityDescription: label)
        item.label = label
        item.toolTip = label
        item.target = self
        item.action = action
        item.isBordered = true
        return item
    }
}
