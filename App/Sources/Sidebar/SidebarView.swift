import SwiftUI

enum SidebarSection: String, CaseIterable, Identifiable {
    case connections, credentials, providers

    var id: String { rawValue }

    var label: String {
        switch self {
        case .connections: return "Connections"
        case .credentials: return "Credentials"
        case .providers: return "Providers"
        }
    }

    var symbol: String {
        switch self {
        case .connections: return "network"
        case .credentials: return "key.fill"
        case .providers: return "arrow.triangle.2.circlepath"
        }
    }
}

/// What the sidebar can ask the window to do.
struct SidebarActions {
    var openConnection: (Connection) -> Void
    var editConnection: (Connection?) -> Void
    var deleteConnection: (Connection) -> Void
    var editCredential: (Credential?) -> Void
    var deleteCredential: (Credential) -> Void
    var editProvider: (Provider?) -> Void
    var deleteProvider: (Provider) -> Void
    var syncProvider: (Provider) -> Void
}

private struct FolderGroup<Item: Identifiable>: Identifiable {
    let name: String
    let items: [Item]
    var id: String { name }
}

private let ungroupedName = "General"

private func grouped<Item: Identifiable>(_ items: [Item], folder: (Item) -> String?) -> [FolderGroup<Item>] {
    let byFolder = Dictionary(grouping: items) { item -> String in
        let name = folder(item)?.trimmingCharacters(in: .whitespaces) ?? ""
        return name.isEmpty ? ungroupedName : name
    }
    return byFolder.keys
        .sorted { a, b in
            if a == ungroupedName { return false }
            if b == ungroupedName { return true }
            return a.localizedCaseInsensitiveCompare(b) == .orderedAscending
        }
        .map { FolderGroup(name: $0, items: byFolder[$0]!) }
}

struct SidebarView: View {
    @ObservedObject var model: AppModel
    @ObservedObject var sync = ProviderSync.shared
    let actions: SidebarActions

    @State private var section: SidebarSection = .connections
    @State private var filter = ""
    @State private var selection: String?

    private var query: String { filter.trimmingCharacters(in: .whitespaces).lowercased() }

    private func matches(_ fields: [String?]) -> Bool {
        query.isEmpty || fields.contains { $0?.lowercased().contains(query) == true }
    }

    var body: some View {
        VStack(spacing: 0) {
            Picker("Section", selection: $section) {
                ForEach(SidebarSection.allCases) { s in
                    Image(systemName: s.symbol).help(s.label).tag(s)
                }
            }
            .pickerStyle(.segmented)
            .labelsHidden()
            .padding(.horizontal, 10)
            .padding(.top, 8)
            .padding(.bottom, 4)

            HStack(spacing: 6) {
                Image(systemName: "magnifyingglass").foregroundStyle(.secondary)
                TextField("Filter \(section.label.lowercased())", text: $filter)
                    .textFieldStyle(.plain)
            }
            .padding(.horizontal, 12)
            .padding(.vertical, 6)
            Divider()

            Group {
                switch section {
                case .connections: connectionList
                case .credentials: credentialList
                case .providers: providerList
                }
            }

            Divider()
            footer
        }
        .frame(minWidth: 240)
        .onChange(of: section) { _, _ in selection = nil }
    }

    // MARK: Connections

    private var connectionList: some View {
        let visible = model.connections.filter { matches([$0.name, $0.host, $0.folder, $0.username]) }
        return Group {
            if model.connections.isEmpty {
                emptyState(symbol: "network", title: "No connections yet",
                           hint: "Add one, or let a provider fill this in from Active Directory.",
                           button: "New Connection…") { actions.editConnection(nil) }
            } else {
                List(selection: $selection) {
                    ForEach(grouped(visible, folder: \.folder)) { group in
                        Section(group.name) {
                            ForEach(group.items) { connection in
                                ConnectionRow(connection: connection)
                                    .tag(connection.id)
                                    .contentShape(Rectangle())
                                    .simultaneousGesture(TapGesture(count: 2).onEnded { actions.openConnection(connection) })
                                    .contextMenu {
                                        Button("Connect") { actions.openConnection(connection) }
                                        Button("Edit…") { actions.editConnection(connection) }
                                        Divider()
                                        Button("Delete", role: .destructive) { actions.deleteConnection(connection) }
                                    }
                            }
                        }
                    }
                }
                .listStyle(.sidebar)
                .onKeyPress(.return) {
                    guard let id = selection, let c = model.connections.first(where: { $0.id == id }) else { return .ignored }
                    actions.openConnection(c)
                    return .handled
                }
            }
        }
    }

    // MARK: Credentials

    private var credentialList: some View {
        let visible = model.credentials.filter { matches([$0.name, $0.username, $0.folder]) }
        return Group {
            if model.credentials.isEmpty {
                emptyState(symbol: "key.fill", title: "No credentials yet",
                           hint: "A credential is a username with a password or key, kept in the local vault or fetched from 1Password.",
                           button: "New Credential…") { actions.editCredential(nil) }
            } else {
                List(selection: $selection) {
                    ForEach(grouped(visible, folder: \.folder)) { group in
                        Section(group.name) {
                            ForEach(group.items) { credential in
                                CredentialRow(credential: credential)
                                    .tag(credential.id)
                                    .contentShape(Rectangle())
                                    .simultaneousGesture(TapGesture(count: 2).onEnded { actions.editCredential(credential) })
                                    .contextMenu {
                                        Button("Edit…") { actions.editCredential(credential) }
                                        Divider()
                                        Button("Delete", role: .destructive) { actions.deleteCredential(credential) }
                                    }
                            }
                        }
                    }
                }
                .listStyle(.sidebar)
            }
        }
    }

    // MARK: Providers

    private var providerList: some View {
        let visible = model.providers.filter { matches([$0.name, $0.folder]) }
        return Group {
            if model.providers.isEmpty {
                emptyState(symbol: "arrow.triangle.2.circlepath", title: "No providers yet",
                           hint: "Providers fill the sidebar for you: 1Password vaults become credentials, Active Directory computers become connections.",
                           button: "New Provider…") { actions.editProvider(nil) }
            } else {
                List(selection: $selection) {
                    ForEach(visible) { provider in
                        ProviderRow(provider: provider, syncing: sync.inProgress.contains(provider.id))
                            .tag(provider.id)
                            .contentShape(Rectangle())
                            .simultaneousGesture(TapGesture(count: 2).onEnded { actions.syncProvider(provider) })
                            .contextMenu {
                                Button("Sync Now") { actions.syncProvider(provider) }
                                Button("Edit…") { actions.editProvider(provider) }
                                Divider()
                                Button("Delete", role: .destructive) { actions.deleteProvider(provider) }
                            }
                    }
                }
                .listStyle(.sidebar)
            }
        }
    }

    // MARK: Chrome

    private var footer: some View {
        HStack {
            Button {
                switch section {
                case .connections: actions.editConnection(nil)
                case .credentials: actions.editCredential(nil)
                case .providers: actions.editProvider(nil)
                }
            } label: {
                Label("New \(section.label.dropLast())", systemImage: "plus")
            }
            .buttonStyle(.borderless)
            Spacer()
        }
        .padding(10)
    }

    private func emptyState(symbol: String, title: String, hint: String, button: String,
                            action: @escaping () -> Void) -> some View {
        VStack(spacing: 10) {
            Image(systemName: symbol)
                .font(.system(size: 30))
                .foregroundStyle(.tertiary)
            Text(title).foregroundStyle(.secondary)
            Text(hint)
                .font(.caption)
                .foregroundStyle(.tertiary)
                .multilineTextAlignment(.center)
                .padding(.horizontal, 16)
            Button(button, action: action)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

struct ConnectionRow: View {
    let connection: Connection

    var body: some View {
        HStack(spacing: 8) {
            Image(systemName: connection.kind.symbol)
                .foregroundStyle(.secondary)
                .frame(width: 16)
            VStack(alignment: .leading, spacing: 1) {
                Text(connection.name).lineLimit(1)
                Text(subtitle)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
            Spacer(minLength: 0)
            if connection.source != nil {
                Image(systemName: "arrow.triangle.2.circlepath")
                    .font(.caption2)
                    .foregroundStyle(.tertiary)
                    .help("Generated by a provider")
            }
        }
        .padding(.vertical, 2)
    }

    private var subtitle: String {
        let port = Int(connection.port) == connection.kind.defaultPort ? "" : ":\(connection.port)"
        let user = connection.username.isEmpty ? "" : "\(connection.username)@"
        return "\(user)\(connection.host)\(port)"
    }
}

struct CredentialRow: View {
    let credential: Credential

    var body: some View {
        HStack(spacing: 8) {
            Image(systemName: symbol)
                .foregroundStyle(.secondary)
                .frame(width: 16)
            VStack(alignment: .leading, spacing: 1) {
                Text(credential.name).lineLimit(1)
                Text(subtitle)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
            Spacer(minLength: 0)
            if credential.source != nil {
                Image(systemName: "arrow.triangle.2.circlepath")
                    .font(.caption2)
                    .foregroundStyle(.tertiary)
                    .help("Generated by a provider")
            }
        }
        .padding(.vertical, 2)
    }

    private var symbol: String {
        switch credential.kind {
        case .vault: return "lock.fill"
        case .onePassword: return "shield.lefthalf.filled"
        case .prompt: return "key"
        }
    }

    private var subtitle: String {
        var parts: [String] = []
        if !credential.username.isEmpty { parts.append(credential.username) }
        switch credential.kind {
        case .vault: parts.append("Vault")
        case .onePassword: parts.append("1Password")
        case .prompt: parts.append("Asks each time")
        }
        if credential.hasPrivateKey { parts.append("SSH key") }
        return parts.joined(separator: " · ")
    }
}

struct ProviderRow: View {
    let provider: Provider
    let syncing: Bool

    var body: some View {
        HStack(spacing: 8) {
            Image(systemName: symbol)
                .foregroundStyle(.secondary)
                .frame(width: 16)
            VStack(alignment: .leading, spacing: 1) {
                Text(provider.name).lineLimit(1)
                Text(subtitle)
                    .font(.caption)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
            }
            Spacer(minLength: 0)
            if syncing {
                ProgressView().controlSize(.small)
            }
        }
        .padding(.vertical, 2)
    }

    private var symbol: String {
        switch provider.kind {
        case .onePassword: return "shield.lefthalf.filled"
        case .activeDirectory: return "building.2"
        }
    }

    private var subtitle: String {
        guard let epoch = provider.lastSyncEpoch else {
            return provider.lastStatus ?? "Never synced"
        }
        let when = RelativeDateTimeFormatter().localizedString(for: Date(timeIntervalSince1970: TimeInterval(epoch)),
                                                               relativeTo: Date())
        return [provider.lastStatus, when].compactMap { $0 }.joined(separator: " · ")
    }
}
