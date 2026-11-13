import AppKit
import Combine

/// Connections, credentials and providers, backed by the Rust store on disk.
@MainActor
final class AppModel: ObservableObject {
    static let shared = AppModel()

    @Published private(set) var connections: [Connection] = []
    @Published private(set) var credentials: [Credential] = []
    @Published private(set) var providers: [Provider] = []
    /// Folder paths (`ACME/Jump Boxes`), including empty ones.
    @Published private(set) var folders: [String] = []

    let store: ConnectionStore
    let storeURL: URL

    static let supportDirectory: URL = {
        let dir = FileManager.default
            .urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("Toolbox", isDirectory: true)
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir
    }()

    static var vaultURL: URL {
        if let override = ProcessInfo.processInfo.environment["TOOLBOX_VAULT_PATH"], !override.isEmpty {
            return URL(fileURLWithPath: override)
        }
        return supportDirectory.appendingPathComponent("vault.json")
    }

    private init() {
        if let override = ProcessInfo.processInfo.environment["TOOLBOX_STORE_PATH"], !override.isEmpty {
            storeURL = URL(fileURLWithPath: override)
        } else {
            storeURL = Self.supportDirectory.appendingPathComponent("connections.json")
        }
        store = Self.openStore(at: storeURL)
        reload()
    }

    private static func openStore(at url: URL) -> ConnectionStore {
        do {
            return try ConnectionStore.open(path: url.path)
        } catch {
            // Keep the unreadable file for inspection and start with an empty one.
            let stamp = Int(Date().timeIntervalSince1970)
            let broken = url.deletingPathExtension().appendingPathExtension("broken-\(stamp).json")
            try? FileManager.default.moveItem(at: url, to: broken)
            NSLog("connections.json could not be read (\(error)); moved to \(broken.path)")
            if let fresh = try? ConnectionStore.open(path: url.path) {
                return fresh
            }
            // Last resort: an in-memory-like store in the temp directory so the app still opens.
            let temp = FileManager.default.temporaryDirectory.appendingPathComponent("toolbox-connections.json")
            Prompts.error(title: "Connections can't be saved",
                          message: "The store at \(url.path) is unusable; working from a temporary copy until it's fixed.")
            return try! ConnectionStore.open(path: temp.path)
        }
    }

    func reload() {
        connections = store.list()
        credentials = store.listCredentials()
        providers = store.listProviders()
        folders = store.listFolders()
    }

    // MARK: Folders

    @discardableResult
    func addFolder(_ path: String) throws -> String {
        let created = try store.addFolder(path: path)
        reload()
        return created
    }

    func renameFolder(_ path: String, to newPath: String) throws {
        _ = try store.renameFolder(from: path, to: newPath)
        reload()
        ProviderSync.shared.scheduleAutoRefresh()
    }

    func deleteFolder(_ path: String) throws {
        try store.deleteFolder(path: path)
        reload()
    }

    func moveConnection(_ connection: Connection, to folder: String?) throws {
        try store.moveConnection(id: connection.id, folder: folder)
        reload()
    }

    func moveCredential(_ credential: Credential, to folder: String?) throws {
        try store.moveCredential(id: credential.id, folder: folder)
        reload()
    }

    func moveProvider(_ provider: Provider, to parent: String?) throws {
        try store.moveProvider(id: provider.id, parent: parent)
        reload()
        ProviderSync.shared.scheduleAutoRefresh()
    }

    // MARK: Connections

    @discardableResult
    func save(_ connection: Connection) throws -> Connection {
        let stored = try store.upsert(connection: connection)
        reload()
        return stored
    }

    func delete(_ connection: Connection) throws {
        _ = try store.delete(id: connection.id)
        reload()
    }

    // MARK: Credentials

    func credential(_ id: String) -> Credential? {
        credentials.first { $0.id == id }
    }

    /// Saves the record and, for vault credentials, the secret alongside it.
    /// Blank secret fields keep whatever was stored before.
    @discardableResult
    func saveCredential(_ credential: Credential, secret: Secret?) async throws -> Credential {
        var record = credential
        var vault: Vault?
        var merged: Secret?
        if case .vault = credential.kind {
            let unlocked = try await VaultManager.shared.unlockedVault()
            let existing = record.id.isEmpty ? Secret(password: nil, privateKey: nil, passphrase: nil)
                : ((try? unlocked.getSecret(id: record.id)) ?? Secret(password: nil, privateKey: nil, passphrase: nil))
            let incoming = secret ?? Secret(password: nil, privateKey: nil, passphrase: nil)
            let result = Secret(
                password: incoming.password.nonEmpty ?? existing.password,
                privateKey: incoming.privateKey.nonEmpty ?? existing.privateKey,
                passphrase: incoming.passphrase.nonEmpty ?? existing.passphrase
            )
            if result.password.nonEmpty == nil, result.privateKey.nonEmpty == nil {
                throw ResolveError.emptyCredential(record.name)
            }
            record.hasPrivateKey = result.privateKey.nonEmpty != nil
            vault = unlocked
            merged = result
        }
        let stored = try store.upsertCredential(credential: record)
        if let vault, let merged {
            try vault.setSecret(id: stored.id, secret: merged)
        }
        reload()
        return stored
    }

    func deleteCredential(_ credential: Credential) throws {
        _ = try store.deleteCredential(id: credential.id)
        if case .vault = credential.kind, VaultManager.shared.vault.isUnlocked() {
            _ = try? VaultManager.shared.vault.deleteSecret(id: credential.id)
        }
        reload()
    }

    // MARK: Providers

    func provider(_ id: String) -> Provider? {
        providers.first { $0.id == id }
    }

    @discardableResult
    func saveProvider(_ provider: Provider) throws -> Provider {
        let stored = try store.upsertProvider(provider: provider)
        reload()
        ProviderSync.shared.scheduleAutoRefresh()
        return stored
    }

    func deleteProvider(_ provider: Provider) throws {
        _ = try store.deleteProvider(id: provider.id)
        reload()
        ProviderSync.shared.scheduleAutoRefresh()
    }
}

extension Connection: Identifiable {}
extension Credential: Identifiable {}
extension Provider: Identifiable {}

extension Optional where Wrapped == String {
    /// `nil` for nil, empty or whitespace-only strings.
    var nonEmpty: String? {
        guard let value = self, !value.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty else { return nil }
        return value
    }
}

extension ConnectionKind {
    var label: String {
        switch self {
        case .ssh: return "SSH"
        case .rdp: return "RDP"
        case .vnc: return "VNC"
        }
    }

    var defaultPort: Int {
        switch self {
        case .ssh: return 22
        case .rdp: return 3389
        case .vnc: return 5900
        }
    }

    var symbol: String {
        switch self {
        case .ssh: return "terminal"
        case .rdp: return "pc"
        case .vnc: return "display"
        }
    }
}
