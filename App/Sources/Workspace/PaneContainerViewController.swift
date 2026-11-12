import AppKit

/// An NSSplitView that lays its children out evenly the first time it gets a size.
final class PaneSplitView: NSSplitView {
    private var distributed = false

    override func layout() {
        super.layout()
        guard !distributed, arrangedSubviews.count > 1 else { return }
        let total = isVertical ? bounds.width : bounds.height
        guard total > 0 else { return }
        distributed = true
        let n = CGFloat(arrangedSubviews.count)
        let size = (total - dividerThickness * (n - 1)) / n
        for i in 0 ..< arrangedSubviews.count - 1 {
            let position = size * CGFloat(i + 1) + dividerThickness * CGFloat(i)
            setPosition(position, ofDividerAt: i)
        }
    }
}

/// Owns the pane tree of one workspace and the views that render it.
final class PaneContainerViewController: NSViewController {
    private(set) var tree: PaneTree
    private var panes: [UUID: PaneView] = [:]
    private(set) var focusedID: UUID
    var onLayoutChange: (() -> Void)?
    private var clickMonitor: Any?

    init() {
        let id = UUID()
        tree = .leaf(id)
        focusedID = id
        super.init(nibName: nil, bundle: nil)
        panes[id] = makePane(id)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError() }

    override func loadView() {
        view = NSView()
        view.wantsLayer = true
        rebuild()
    }

    override func viewDidLoad() {
        super.viewDidLoad()
        // Clicking anywhere in a pane makes it the active one. SwiftTerm's
        // view takes first responder itself; this just keeps our bookkeeping
        // in step without subclassing it.
        clickMonitor = NSEvent.addLocalMonitorForEvents(matching: [.leftMouseDown, .rightMouseDown, .otherMouseDown]) { [weak self] event in
            self?.noteClick(event)
            return event
        }
    }

    deinit {
        if let clickMonitor {
            NSEvent.removeMonitor(clickMonitor)
        }
    }

    private func noteClick(_ event: NSEvent) {
        guard let window = view.window, event.window === window else { return }
        for (id, pane) in panes where id != focusedID {
            let point = pane.convert(event.locationInWindow, from: nil)
            if pane.bounds.contains(point) {
                setFocus(id, makeFirstResponder: false)
                return
            }
        }
    }

    var paneCount: Int { tree.leaves.count }

    var focusedPane: PaneView { panes[focusedID]! }

    var liveSessionCount: Int {
        panes.values.filter { $0.content?.isLive == true }.count
    }

    /// The pane a new session should open in: the focused one if it is
    /// empty, otherwise a fresh pane split to the right of it.
    func paneForNewSession() -> PaneView {
        focusedPane.content == nil ? focusedPane : split(.horizontal)
    }

    @discardableResult
    func split(_ axis: SplitAxis) -> PaneView {
        let id = UUID()
        let pane = makePane(id)
        panes[id] = pane
        tree = tree.splitting(focusedID, axis: axis, newLeaf: id)
        rebuild()
        setFocus(id, makeFirstResponder: true)
        return pane
    }

    /// Closes a pane's session. The last remaining pane is emptied, not removed.
    func closePane(_ id: UUID) {
        guard let pane = panes[id] else { return }
        pane.setContent(nil)
        guard tree.leaves.count > 1 else {
            setFocus(id, makeFirstResponder: true)
            return
        }
        let order = tree.leaves
        let index = order.firstIndex(of: id) ?? 0
        tree = tree.removing(id) ?? .leaf(id)
        panes[id] = nil
        pane.removeFromSuperview()
        rebuild()
        let remaining = tree.leaves
        setFocus(remaining[min(index, remaining.count - 1)], makeFirstResponder: true)
    }

    func focusNeighbor(_ delta: Int) {
        let order = tree.leaves
        guard let index = order.firstIndex(of: focusedID), order.count > 1 else { return }
        setFocus(order[(index + delta + order.count) % order.count], makeFirstResponder: true)
    }

    func setFocus(_ id: UUID, makeFirstResponder: Bool) {
        guard let pane = panes[id] else { return }
        focusedID = id
        for (paneID, view) in panes {
            view.isFocused = paneID == id
        }
        if makeFirstResponder {
            pane.focus()
        }
        onLayoutChange?()
    }

    func terminateAll() {
        panes.values.forEach { $0.setContent(nil) }
    }

    private func makePane(_ id: UUID) -> PaneView {
        let pane = PaneView(id: id)
        pane.onFocusRequest = { [weak self] in self?.setFocus(id, makeFirstResponder: false) }
        pane.onCloseRequest = { [weak self] in self?.closePane(id) }
        return pane
    }

    private func rebuild() {
        view.subviews.forEach { $0.removeFromSuperview() }
        let root = build(tree)
        root.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(root)
        NSLayoutConstraint.activate([
            root.topAnchor.constraint(equalTo: view.topAnchor),
            root.leadingAnchor.constraint(equalTo: view.leadingAnchor),
            root.trailingAnchor.constraint(equalTo: view.trailingAnchor),
            root.bottomAnchor.constraint(equalTo: view.bottomAnchor),
        ])
        let multiple = tree.leaves.count > 1
        panes.values.forEach { $0.showsFocusRing = multiple }
        onLayoutChange?()
    }

    private func build(_ node: PaneTree) -> NSView {
        switch node {
        case .leaf(let id):
            let pane = panes[id]!
            pane.removeFromSuperview()
            pane.translatesAutoresizingMaskIntoConstraints = true
            return pane
        case .split(let axis, let children):
            let split = PaneSplitView()
            split.isVertical = axis == .horizontal
            split.dividerStyle = .thin
            for child in children {
                split.addArrangedSubview(build(child))
            }
            return split
        }
    }
}
