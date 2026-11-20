import SwiftUI

/// Chooses a credential from a searchable list, grouped by where the secret
/// lives. A 1Password provider can contribute hundreds of items, so typing
/// narrows the list to the names, usernames and folders that match.
struct CredentialPicker: View {
    let title: String
    let credentials: [Credential]
    @Binding var selection: String?
    var noneLabel = "Choose…"

    @State private var isOpen = false

    var body: some View {
        LabeledContent(title) {
            Button {
                isOpen = true
            } label: {
                HStack(spacing: 6) {
                    Text(currentLabel)
                        .lineLimit(1)
                        .truncationMode(.middle)
                    Image(systemName: "chevron.up.chevron.down")
                        .font(.caption2)
                        .foregroundStyle(.secondary)
                }
            }
            .popover(isPresented: $isOpen, arrowEdge: .bottom) {
                CredentialSearchList(credentials: credentials, selection: $selection, noneLabel: noneLabel,
                                     initialQuery: Self.demoSearch ?? "") {
                    isOpen = false
                }
            }
            .onAppear {
                if Self.demoSearch != nil { isOpen = true }
            }
        }
    }

    /// Testing aid: `-demoCredentialSearch <text>` on the command line opens
    /// each picker with that search already typed.
    private static let demoSearch = UserDefaults.standard.string(forKey: "demoCredentialSearch")

    private var currentLabel: String {
        guard let selection else { return noneLabel }
        guard let credential = credentials.first(where: { $0.id == selection }) else { return "Missing credential" }
        return Self.label(credential)
    }

    /// "Name (username)", or whichever of the two exists, so items without
    /// a title still read as something.
    static func label(_ credential: Credential) -> String {
        let name = credential.name.trimmingCharacters(in: .whitespaces)
        let user = credential.username.trimmingCharacters(in: .whitespaces)
        switch (name.isEmpty, user.isEmpty) {
        case (false, false): return name == user ? name : "\(name) (\(user))"
        case (false, true): return name
        case (true, false): return user
        case (true, true): return "Untitled credential"
        }
    }

    /// The credentials whose name, username or folder contain every word of
    /// `query`, ignoring case. An empty query matches everything.
    static func matches(_ credentials: [Credential], query: String) -> [Credential] {
        let words = query.lowercased().split(whereSeparator: \.isWhitespace)
        guard !words.isEmpty else { return credentials }
        return credentials.filter { credential in
            let haystack = [credential.name, credential.username, credential.folder ?? ""]
                .joined(separator: "\n").lowercased()
            return words.allSatisfy { haystack.contains($0) }
        }
    }
}

/// The popover behind `CredentialPicker`: a search field over the grouped
/// list. Return picks the highlighted row, the arrow keys move it.
private struct CredentialSearchList: View {
    let credentials: [Credential]
    @Binding var selection: String?
    let noneLabel: String
    let onPick: () -> Void

    @State private var query: String

    init(credentials: [Credential], selection: Binding<String?>, noneLabel: String, initialQuery: String,
         onPick: @escaping () -> Void) {
        self.credentials = credentials
        self._selection = selection
        self.noneLabel = noneLabel
        self.onPick = onPick
        self._query = State(initialValue: initialQuery)
    }

    @State private var highlighted: String?
    /// Set by the keyboard only: following the mouse would scroll the list
    /// out from under it.
    @State private var scrollTarget: String?
    @FocusState private var searchFocused: Bool

    private struct Group: Identifiable {
        let id: String
        let items: [Credential]
    }

    private var groups: [Group] {
        let matching = CredentialPicker.matches(credentials, query: query)
        func group(_ name: String, _ belongs: (Credential) -> Bool) -> Group {
            Group(id: name, items: matching.filter(belongs).sorted {
                CredentialPicker.label($0).localizedCaseInsensitiveCompare(CredentialPicker.label($1)) == .orderedAscending
            })
        }
        return [
            group("Vault") { if case .vault = $0.kind { return true }; return false },
            group("1Password") { if case .onePassword = $0.kind { return true }; return false },
            group("Ask each time") { if case .prompt = $0.kind { return true }; return false },
        ].filter { !$0.items.isEmpty }
    }

    /// Row ids in display order; the empty string stands for "none".
    private var order: [String] {
        (query.isEmpty ? [""] : []) + groups.flatMap { $0.items.map(\.id) }
    }

    var body: some View {
        let groups = groups
        VStack(spacing: 0) {
            HStack(spacing: 6) {
                Image(systemName: "magnifyingglass").foregroundStyle(.secondary)
                TextField("Search credentials", text: $query)
                    .textFieldStyle(.plain)
                    .multilineTextAlignment(.leading)
                    .focused($searchFocused)
                    .onSubmit { pick(highlighted ?? order.first) }
                if !query.isEmpty {
                    Button {
                        query = ""
                    } label: {
                        Image(systemName: "xmark.circle.fill").foregroundStyle(.secondary)
                    }
                    .buttonStyle(.plain)
                }
            }
            .padding(10)
            Divider()
            ScrollViewReader { proxy in
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 0, pinnedViews: [.sectionHeaders]) {
                        if query.isEmpty {
                            row(id: "", label: noneLabel, detail: nil, secondary: true)
                        }
                        ForEach(groups) { group in
                            Section {
                                ForEach(group.items) { credential in
                                    row(id: credential.id, label: CredentialPicker.label(credential),
                                        detail: credential.folder, secondary: false)
                                }
                            } header: {
                                Text(group.id)
                                    .font(.caption.weight(.semibold))
                                    .foregroundStyle(.secondary)
                                    .padding(.horizontal, 12)
                                    .padding(.vertical, 4)
                                    .frame(maxWidth: .infinity, alignment: .leading)
                                    .background(.bar)
                            }
                        }
                        if groups.isEmpty && !query.isEmpty {
                            Text("No credentials match “\(query)”.")
                                .font(.callout)
                                .foregroundStyle(.secondary)
                                .frame(maxWidth: .infinity)
                                .padding(24)
                        }
                    }
                }
                .onChange(of: scrollTarget) { _, id in
                    if let id { proxy.scrollTo(id, anchor: .center) }
                }
            }
        }
        .frame(width: 460, height: 360)
        .onAppear {
            highlighted = query.isEmpty ? selection : order.first
            scrollTarget = highlighted
            searchFocused = true
        }
        .onChange(of: query) { _, _ in
            // Keep the highlight on something visible; the best match is first.
            highlighted = query.isEmpty ? selection : order.first
            scrollTarget = highlighted
        }
        .onKeyPress(.downArrow) { move(1) }
        .onKeyPress(.upArrow) { move(-1) }
    }

    private func row(id: String, label: String, detail: String?, secondary: Bool) -> some View {
        let isSelected = (selection ?? "") == id
        let isHighlighted = (highlighted ?? "\u{0}") == id
        return Button {
            pick(id)
        } label: {
            HStack(spacing: 6) {
                Image(systemName: "checkmark")
                    .font(.caption.weight(.semibold))
                    .opacity(isSelected ? 1 : 0)
                Text(label)
                    .lineLimit(1)
                    .truncationMode(.middle)
                    .foregroundStyle(secondary && !isHighlighted ? AnyShapeStyle(.secondary) : AnyShapeStyle(.primary))
                    .layoutPriority(1)
                Spacer(minLength: 8)
                if let detail, !detail.isEmpty {
                    Text(detail)
                        .font(.caption)
                        .lineLimit(1)
                        .truncationMode(.head)
                        .foregroundStyle(isHighlighted ? AnyShapeStyle(.primary) : AnyShapeStyle(.tertiary))
                }
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 5)
            .contentShape(Rectangle())
            .background(isHighlighted ? Color.accentColor.opacity(0.85) : .clear)
            .foregroundStyle(isHighlighted ? Color.white : Color.primary)
        }
        .buttonStyle(.plain)
        .id(id)
        .onHover { inside in
            if inside { highlighted = id }
        }
    }

    private func move(_ delta: Int) -> KeyPress.Result {
        let order = order
        guard !order.isEmpty else { return .handled }
        let current = highlighted.flatMap { order.firstIndex(of: $0) }
        let next = current.map { min(max($0 + delta, 0), order.count - 1) } ?? (delta > 0 ? 0 : order.count - 1)
        highlighted = order[next]
        scrollTarget = order[next]
        return .handled
    }

    private func pick(_ id: String?) {
        guard let id else { return }
        selection = id.isEmpty ? nil : id
        onPick()
    }
}
