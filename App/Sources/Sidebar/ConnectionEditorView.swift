import AppKit
import SwiftUI

enum AuthMethod: String, CaseIterable, Identifiable {
    case credential, agent, keyFile, password

    var id: String { rawValue }

    var label: String {
        switch self {
        case .credential: return "Credential"
        case .agent: return "SSH agent"
        case .keyFile: return "Private key file"
        case .password: return "Ask for a password"
        }
    }

    static func available(for kind: ConnectionKind) -> [AuthMethod] {
        kind == .ssh ? allCases : [.credential, .password]
    }
}

/// Editable copy of a `Connection` for the form.
struct ConnectionDraft {
    var id = ""
    var name = ""
    var kind: ConnectionKind = .ssh
    var host = ""
    var port = 22
    var username = NSUserName()
    var folder = ""
    var authMethod: AuthMethod = .agent
    var credentialId: String?
    var keyPath = ""
    var agentSocket = ""
    var tags: [String] = []
    var source: String?

    static let onePasswordAgentSocket = "~/Library/Group Containers/2BUA8C4S2C.com.1password/t/agent.sock"

    init() {}

    init(_ connection: Connection) {
        id = connection.id
        name = connection.name
        kind = connection.kind
        host = connection.host
        port = Int(connection.port)
        username = connection.username
        folder = connection.folder ?? ""
        tags = connection.tags
        source = connection.source
        switch connection.auth {
        case .password:
            authMethod = .password
        case .keyFile(let path):
            authMethod = .keyFile
            keyPath = path
        case .agent(let socketPath):
            authMethod = .agent
            agentSocket = socketPath ?? ""
        case .credential(let credentialId):
            authMethod = .credential
            self.credentialId = credentialId
        }
    }

    var validationError: String? {
        if name.trimmingCharacters(in: .whitespaces).isEmpty { return "Give the connection a name." }
        if host.trimmingCharacters(in: .whitespaces).isEmpty { return "Host is required." }
        if !(1...65535).contains(port) { return "Port must be between 1 and 65535." }
        switch authMethod {
        case .credential where credentialId == nil:
            return "Choose a credential."
        case .keyFile where keyPath.trimmingCharacters(in: .whitespaces).isEmpty:
            return "Choose a private key file."
        case .agent, .keyFile, .password:
            if username.trimmingCharacters(in: .whitespaces).isEmpty { return "Username is required." }
        default:
            break
        }
        return nil
    }

    func connection() -> Connection {
        let auth: AuthKind
        switch authMethod {
        case .password:
            auth = .password
        case .keyFile:
            auth = .keyFile(path: keyPath.trimmingCharacters(in: .whitespaces))
        case .agent:
            auth = .agent(socketPath: blankToNil(agentSocket))
        case .credential:
            auth = .credential(credentialId: credentialId ?? "")
        }
        return Connection(
            id: id,
            name: name.trimmingCharacters(in: .whitespaces),
            kind: kind,
            host: host.trimmingCharacters(in: .whitespaces),
            port: UInt16(clamping: port),
            username: username.trimmingCharacters(in: .whitespaces),
            auth: auth,
            folder: blankToNil(folder),
            tags: tags,
            source: source
        )
    }
}

struct ConnectionEditorView: View {
    @ObservedObject var model: AppModel
    @State var draft: ConnectionDraft
    let onSave: (Connection) -> Void
    let onCancel: () -> Void

    @State private var validation: String?

    var body: some View {
        VStack(spacing: 0) {
            Form {
                Section("Connection") {
                    Picker("Type", selection: $draft.kind) {
                        ForEach([ConnectionKind.ssh, .rdp, .vnc], id: \.self) { Text($0.label).tag($0) }
                    }
                    .pickerStyle(.segmented)
                    TextField("Name", text: $draft.name, prompt: Text("nutanix1"))
                    TextField("Host", text: $draft.host, prompt: Text("hostname or IP"))
                    TextField("Port", value: $draft.port, format: .number.grouping(.never))
                    TextField("Username", text: $draft.username,
                              prompt: Text(draft.authMethod == .credential ? "Blank uses the credential's" : ""))
                    TextField("Folder", text: $draft.folder, prompt: Text("Optional; use / for subfolders"))
                    if draft.kind == .rdp {
                        Text("The desktop follows the pane size. Use DOMAIN\\user or user@domain for domain accounts.")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                }
                Section("Authentication") {
                    Picker("Method", selection: $draft.authMethod) {
                        ForEach(AuthMethod.available(for: draft.kind)) { method in
                            Text(method.label).tag(method)
                        }
                    }
                    switch draft.authMethod {
                    case .credential:
                        Picker("Credential", selection: $draft.credentialId) {
                            Text("Choose…").tag(String?.none)
                            ForEach(model.credentials) { credential in
                                Text(credential.username.isEmpty ? credential.name : "\(credential.name) (\(credential.username))")
                                    .tag(Optional(credential.id))
                            }
                        }
                        if model.credentials.isEmpty {
                            Text("No credentials yet. Add one under the key tab in the sidebar.")
                                .font(.caption)
                                .foregroundStyle(.secondary)
                        }
                    case .password:
                        Text("You'll be asked for the password every time.")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    case .keyFile:
                        HStack {
                            TextField("Private key", text: $draft.keyPath, prompt: Text("~/.ssh/id_ed25519"))
                            Button("Choose…", action: chooseKey)
                        }
                        Text("An encrypted key prompts for its passphrase on connect.")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    case .agent:
                        TextField("Agent socket", text: $draft.agentSocket, prompt: Text("Blank uses SSH_AUTH_SOCK"))
                        Button("Use 1Password's SSH agent") {
                            draft.agentSocket = ConnectionDraft.onePasswordAgentSocket
                        }
                    }
                    if draft.source != nil {
                        Text("Generated by a provider. The next sync replaces your edits.")
                            .font(.caption)
                            .foregroundStyle(.orange)
                    }
                }
            }
            .formStyle(.grouped)
            .onChange(of: draft.kind) { old, new in
                if draft.port == old.defaultPort { draft.port = new.defaultPort }
                if !AuthMethod.available(for: new).contains(draft.authMethod) { draft.authMethod = .credential }
            }

            if let validation {
                Text(validation)
                    .font(.caption)
                    .foregroundStyle(.red)
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(.horizontal, 20)
            }

            HStack {
                Spacer()
                Button("Cancel", action: onCancel)
                    .keyboardShortcut(.cancelAction)
                Button(draft.id.isEmpty ? "Add" : "Save", action: save)
                    .keyboardShortcut(.defaultAction)
                    .buttonStyle(.borderedProminent)
            }
            .padding(16)
        }
        .frame(width: 520, height: 580)
    }

    private func save() {
        if let problem = draft.validationError {
            validation = problem
            return
        }
        onSave(draft.connection())
    }

    private func chooseKey() {
        let panel = NSOpenPanel()
        panel.canChooseFiles = true
        panel.canChooseDirectories = false
        panel.allowsMultipleSelection = false
        panel.showsHiddenFiles = true
        panel.directoryURL = URL(fileURLWithPath: NSHomeDirectory()).appendingPathComponent(".ssh")
        panel.message = "Choose a private key"
        if panel.runModal() == .OK, let url = panel.url {
            draft.keyPath = url.path
        }
    }
}
