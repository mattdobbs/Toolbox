import AppKit
import Combine

/// Connections, credentials and providers, backed by the Rust store on disk.
@MainActor
final class AppModel: ObservableObject {
    static let shared = AppModel()

    @Published private(set) var connections: [Connection] = []
    @Published private(set) var credentials: [Credential] = []
    @Published private(set) var providers: [Provider] = []

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
            return try! ConnectionStore.open(path: url.path)
        }
    }

    func reload() {
        connections = store.list()
        credentials = store.listCredentials()
        providers = store.listProviders()
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
