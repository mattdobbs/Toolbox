import AppKit

/// Something that can live in a pane: today a terminal, later an RDP or VNC surface.
@MainActor
protocol PaneContent: AnyObject {
    var view: NSView { get }
    var title: String { get }
    var isLive: Bool { get }
    var onTitleChange: ((String) -> Void)? { get set }
    var onStateChange: (() -> Void)? { get set }
    var onFocus: (() -> Void)? { get set }
    func focus()
    func terminate()
}

/// One cell of the workspace layout: a header strip plus the session view,
/// or a placeholder when empty.
final class PaneView: NSView {
    let id: UUID
    private(set) var content: PaneContent?
    var onFocusRequest: (() -> Void)?
    var onCloseRequest: (() -> Void)?

    var isFocused = false {
        didSet { updateBorder() }
    }

    /// Whether to draw the focus border at all (pointless with a single pane).
    var showsFocusRing = false {
        didSet { updateBorder() }
    }

    private let header = NSView()
    private let statusDot = NSView()
    private let titleLabel = NSTextField(labelWithString: "")
    private let placeholder = NSTextField(wrappingLabelWithString: "No session\n\nDouble-click a connection in the sidebar,\nor press ⌘N to create one.")
    private var contentConstraints: [NSLayoutConstraint] = []

    init(id: UUID) {
        self.id = id
        super.init(frame: .zero)
        wantsLayer = true
        layer?.borderWidth = 1
        buildHeader()
        buildPlaceholder()
        updateBorder()
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError() }

    func setContent(_ newContent: PaneContent?) {
        content?.terminate()
        content?.view.removeFromSuperview()
        NSLayoutConstraint.deactivate(contentConstraints)
        contentConstraints = []
        content = newContent

        guard let newContent else {
            header.isHidden = true
            placeholder.isHidden = false
            titleLabel.stringValue = ""
            return
        }

        header.isHidden = false
        placeholder.isHidden = true
        titleLabel.stringValue = newContent.title
        newContent.onTitleChange = { [weak self] title in self?.titleLabel.stringValue = title }
        newContent.onStateChange = { [weak self] in self?.refreshStatus() }
        newContent.onFocus = { [weak self] in
            guard let self, !self.isFocused else { return }
            self.onFocusRequest?()
        }

        let view = newContent.view
        view.translatesAutoresizingMaskIntoConstraints = false
        addSubview(view)
        contentConstraints = [
            view.topAnchor.constraint(equalTo: header.bottomAnchor),
            view.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 1),
            view.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -1),
            view.bottomAnchor.constraint(equalTo: bottomAnchor, constant: -1),
        ]
        NSLayoutConstraint.activate(contentConstraints)
        refreshStatus()
    }

    func focus() {
        if let content {
            content.focus()
        } else {
            window?.makeFirstResponder(self)
        }
    }

    override var acceptsFirstResponder: Bool { true }

    override func becomeFirstResponder() -> Bool {
        if !isFocused { onFocusRequest?() }
        return true
    }

    override func mouseDown(with event: NSEvent) {
        if !isFocused { onFocusRequest?() }
        super.mouseDown(with: event)
    }

    private func refreshStatus() {
        let live = content?.isLive ?? false
        statusDot.layer?.backgroundColor = (live ? NSColor.systemGreen : NSColor.systemGray).cgColor
    }

    private func updateBorder() {
        let color: NSColor = (isFocused && showsFocusRing)
            ? NSColor.controlAccentColor.withAlphaComponent(0.8)
            : NSColor.separatorColor.withAlphaComponent(0.6)
        layer?.borderColor = color.cgColor
    }

    private func buildHeader() {
        header.translatesAutoresizingMaskIntoConstraints = false
        header.wantsLayer = true
        header.layer?.backgroundColor = NSColor.windowBackgroundColor.cgColor
        header.isHidden = true
        addSubview(header)

        statusDot.translatesAutoresizingMaskIntoConstraints = false
        statusDot.wantsLayer = true
        statusDot.layer?.cornerRadius = 4
        statusDot.layer?.backgroundColor = NSColor.systemGray.cgColor

        titleLabel.font = NSFont.systemFont(ofSize: 11, weight: .medium)
        titleLabel.textColor = .secondaryLabelColor
        titleLabel.lineBreakMode = .byTruncatingTail
        titleLabel.translatesAutoresizingMaskIntoConstraints = false

        let close = NSButton(image: NSImage(systemSymbolName: "xmark", accessibilityDescription: "Close pane")!,
                             target: self, action: #selector(closeTapped))
        close.isBordered = false
        close.imageScaling = .scaleProportionallyDown
        close.contentTintColor = .secondaryLabelColor
        close.toolTip = "Close pane"
        close.translatesAutoresizingMaskIntoConstraints = false

        header.addSubview(statusDot)
        header.addSubview(titleLabel)
        header.addSubview(close)

        NSLayoutConstraint.activate([
            header.topAnchor.constraint(equalTo: topAnchor, constant: 1),
            header.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 1),
            header.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -1),
            header.heightAnchor.constraint(equalToConstant: 24),

            statusDot.leadingAnchor.constraint(equalTo: header.leadingAnchor, constant: 8),
            statusDot.centerYAnchor.constraint(equalTo: header.centerYAnchor),
            statusDot.widthAnchor.constraint(equalToConstant: 8),
            statusDot.heightAnchor.constraint(equalToConstant: 8),

            titleLabel.leadingAnchor.constraint(equalTo: statusDot.trailingAnchor, constant: 6),
            titleLabel.centerYAnchor.constraint(equalTo: header.centerYAnchor),
            titleLabel.trailingAnchor.constraint(lessThanOrEqualTo: close.leadingAnchor, constant: -6),

            close.trailingAnchor.constraint(equalTo: header.trailingAnchor, constant: -6),
            close.centerYAnchor.constraint(equalTo: header.centerYAnchor),
            close.widthAnchor.constraint(equalToConstant: 18),
            close.heightAnchor.constraint(equalToConstant: 18),
        ])
    }

    private func buildPlaceholder() {
        placeholder.alignment = .center
        placeholder.textColor = .tertiaryLabelColor
        placeholder.font = NSFont.systemFont(ofSize: 13)
        placeholder.translatesAutoresizingMaskIntoConstraints = false
        addSubview(placeholder)
        NSLayoutConstraint.activate([
            placeholder.centerXAnchor.constraint(equalTo: centerXAnchor),
            placeholder.centerYAnchor.constraint(equalTo: centerYAnchor),
            placeholder.widthAnchor.constraint(lessThanOrEqualTo: widthAnchor, constant: -40),
        ])
    }

    @objc private func closeTapped() {
        onCloseRequest?()
    }
}
