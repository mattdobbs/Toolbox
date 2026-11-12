import SwiftUI

enum ProviderKindChoice: String, CaseIterable, Identifiable {
    case onePassword, activeDirectory

    var id: String { rawValue }

    var label: String {
        switch self {
        case .onePassword: return "1Password"
        case .activeDirectory: return "Active Directory"
        }
    }
}

struct ProviderDraft {
    var id = ""
    var name = ""
    var folder = ""
    var kind: ProviderKindChoice = .onePassword
    var autoRefreshMinutes = 60
    var opPath = ""
    var account = ""
    var vaults = ""
    var server = ""
    var searchBase = ""
    var bindCredentialId: String?
    var adminCredentialId: String?
    var userCredentialId: String?
    var allowPlainLdap = true
    var acceptInvalidCerts = false
    var lastSyncEpoch: UInt64?
    var lastStatus: String?

    init() {}

    init(_ provider: Provider) {
        id = provider.id
        name = provider.name
        folder = provider.folder
        autoRefreshMinutes = Int(provider.autoRefreshMinutes)
        lastSyncEpoch = provider.lastSyncEpoch
        lastStatus = provider.lastStatus
        switch provider.kind {
        case .onePassword(let opPath, let account, let vaults):
            kind = .onePassword
            self.opPath = opPath ?? ""
            self.account = account ?? ""
            self.vaults = vaults.joined(separator: ", ")
        case .activeDirectory(let server, let searchBase, let bind, let admin, let user, let plain, let invalid):
            kind = .activeDirectory
            self.server = server
            self.searchBase = searchBase
            bindCredentialId = bind
            adminCredentialId = admin
            userCredentialId = user
            allowPlainLdap = plain
            acceptInvalidCerts = invalid
        }
    }

    var validationError: String? {
        if name.trimmingCharacters(in: .whitespaces).isEmpty { return "Give the provider a name." }
        if kind == .activeDirectory {
            if server.trimmingCharacters(in: .whitespaces).isEmpty { return "Enter the domain controller to query." }
            if searchBase.trimmingCharacters(in: .whitespaces).isEmpty { return "Enter the search base, e.g. DC=corp,DC=example." }
            if bindCredentialId == nil { return "Choose the credential to bind with (your regular domain account)." }
        }
        return nil
    }

    func provider() -> Provider {
        let kind: ProviderKind
        switch self.kind {
        case .onePassword:
            let list = vaults.split(separator: ",").map { $0.trimmingCharacters(in: .whitespaces) }.filter { !$0.isEmpty }
            kind = .onePassword(opPath: blankToNil(opPath), account: blankToNil(account), vaults: list)
        case .activeDirectory:
            kind = .activeDirectory(server: server.trimmingCharacters(in: .whitespaces),
                                    searchBase: searchBase.trimmingCharacters(in: .whitespaces),
                                    bindCredentialId: bindCredentialId,
                                    adminCredentialId: adminCredentialId,
                                    userCredentialId: userCredentialId,
                                    allowPlainLdap: allowPlainLdap,
                                    acceptInvalidCerts: acceptInvalidCerts)
        }
        return Provider(
            id: id,
            name: name.trimmingCharacters(in: .whitespaces),
            kind: kind,
            folder: folder.trimmingCharacters(in: .whitespaces),
            autoRefreshMinutes: UInt32(max(autoRefreshMinutes, 0)),
            lastSyncEpoch: lastSyncEpoch,
            lastStatus: lastStatus
        )
    }
}

struct ProviderEditorView: View {
    @ObservedObject var model: AppModel
    @State var draft: ProviderDraft
    let onSave: (Provider, Bool) -> Void
    let onCancel: () -> Void

    @State private var validation: String?

    var body: some View {
        VStack(spacing: 0) {
            Form {
                Section("Provider") {
                    TextField("Name", text: $draft.name, prompt: Text(draft.kind.label))
                    TextField("Folder", text: $draft.folder, prompt: Text(draft.name.isEmpty ? "Same as name" : draft.name))
                    Picker("Source", selection: $draft.kind) {
                        ForEach(ProviderKindChoice.allCases) { Text($0.label).tag($0) }
                    }
                    Stepper(value: $draft.autoRefreshMinutes, in: 0...1440, step: 15) {
                        Text(draft.autoRefreshMinutes == 0
                             ? "Refresh manually"
                             : "Refresh every \(draft.autoRefreshMinutes) min")
                    }
                    Text("Automatic refresh also runs when Toolbox starts.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                switch draft.kind {
                case .onePassword:
                    Section("1Password") {
                        TextField("op path", text: $draft.opPath, prompt: Text(detectOpPath() ?? "/opt/homebrew/bin/op"))
                        TextField("Account", text: $draft.account, prompt: Text("All accounts"))
                        TextField("Vaults", text: $draft.vaults, prompt: Text("All vaults; separate names with commas"))
                        Text("Items become credentials, grouped by vault. Uses the desktop app integration: turn on “Integrate with 1Password CLI” under 1Password ▸ Settings ▸ Developer, then the first sync asks for Touch ID.")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                case .activeDirectory:
                    Section("Active Directory") {
                        TextField("Domain controller", text: $draft.server, prompt: Text("dc01.corp.example"))
                        TextField("Search base", text: $draft.searchBase, prompt: Text("DC=corp,DC=example"))
                        credentialPicker("Bind as", selection: $draft.bindCredentialId)
                        credentialPicker("Domain controllers use", selection: $draft.adminCredentialId)
                        credentialPicker("Other computers use", selection: $draft.userCredentialId)
                        Toggle("Fall back to unencrypted LDAP if LDAPS fails", isOn: $draft.allowPlainLdap)
                        Toggle("Accept untrusted certificates", isOn: $draft.acceptInvalidCerts)
                        Text("Computers become connections: Windows → RDP, macOS → VNC, Linux → SSH. OUs become folders.")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                }
            }
            .formStyle(.grouped)

            if let validation {
                Text(validation)
                    .font(.caption)
                    .foregroundStyle(.red)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.horizontal, 20)
            }

            HStack {
                Spacer()
                Button("Cancel", action: onCancel).keyboardShortcut(.cancelAction)
                Button("Save") { save(syncNow: false) }
                Button("Save and Sync") { save(syncNow: true) }
                    .keyboardShortcut(.defaultAction)
                    .buttonStyle(.borderedProminent)
            }
            .padding(16)
        }
        .frame(width: 560, height: 600)
    }

    private func credentialPicker(_ title: String, selection: Binding<String?>) -> some View {
        Picker(title, selection: selection) {
            Text("None").tag(String?.none)
            ForEach(model.credentials) { credential in
                Text(credential.username.isEmpty ? credential.name : "\(credential.name) (\(credential.username))")
                    .tag(Optional(credential.id))
            }
        }
    }

    private func save(syncNow: Bool) {
        if let problem = draft.validationError {
            validation = problem
            return
        }
        onSave(draft.provider(), syncNow)
    }
}
