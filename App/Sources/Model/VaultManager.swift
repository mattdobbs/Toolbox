import Foundation
import LocalAuthentication
import Security

enum VaultError: LocalizedError {
    case cancelled
    case unavailable(String)
    case keychain(OSStatus)

    var errorDescription: String? {
        switch self {
        case .cancelled:
            return "Vault unlock was cancelled."
        case .unavailable(let reason):
            return "The vault can't be unlocked on this Mac: \(reason)"
        case .keychain(let status):
            let text = SecCopyErrorMessageString(status, nil) as String? ?? "status \(status)"
            return "Keychain error: \(text)"
        }
    }
}

/// Owns the vault's key. The key is random, lives in the login Keychain,
/// and is handed to the core once per launch after Touch ID (or the login
/// password) confirms it's you.
@MainActor
final class VaultManager {
    static let shared = VaultManager()

    let vault: Vault
    private static let service = "com.dobbs.Toolbox.vault-key"
    private static let account = "default"
    private var unlocking: Task<Void, Error>?

    var requiresAuthentication: Bool {
        get { UserDefaults.standard.object(forKey: "vaultRequiresAuth") as? Bool ?? true }
        set { UserDefaults.standard.set(newValue, forKey: "vaultRequiresAuth") }
    }

    private init() {
        vault = Self.openVault(at: AppModel.vaultURL)
    }

    private static func openVault(at url: URL) -> Vault {
        do {
            return try Vault.open(path: url.path)
        } catch {
            let stamp = Int(Date().timeIntervalSince1970)
            let broken = url.deletingPathExtension().appendingPathExtension("broken-\(stamp).json")
            try? FileManager.default.moveItem(at: url, to: broken)
            NSLog("vault.json could not be read (\(error)); moved to \(broken.path)")
            if let fresh = try? Vault.open(path: url.path) {
                return fresh
            }
            let temp = FileManager.default.temporaryDirectory.appendingPathComponent("toolbox-vault.json")
            return try! Vault.open(path: temp.path)
        }
    }

    /// The vault, unlocked. Prompts at most once even if called concurrently.
    func unlockedVault() async throws -> Vault {
        if vault.isUnlocked() { return vault }
        if let unlocking {
            try await unlocking.value
            return vault
        }
        let task = Task { @MainActor in
            defer { self.unlocking = nil }
            if self.requiresAuthentication {
                try await self.authenticate()
            }
            let key = try self.loadOrCreateKey()
            try self.vault.unlock(key: key)
        }
        unlocking = task
        try await task.value
        return vault
    }

    func lock() {
        vault.lock()
    }

    private func authenticate() async throws {
        let context = LAContext()
        context.localizedCancelTitle = "Cancel"
        var unavailable: NSError?
        guard context.canEvaluatePolicy(.deviceOwnerAuthentication, error: &unavailable) else {
            // Refuse rather than silently skipping the owner check.
            throw VaultError.unavailable(unavailable?.localizedDescription ?? "no login password or Touch ID is set up")
        }
        let ok = try await context.evaluatePolicy(.deviceOwnerAuthentication,
                                                  localizedReason: "unlock the Toolbox vault")
        if !ok { throw VaultError.cancelled }
    }

    private func loadOrCreateKey() throws -> Data {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: Self.service,
            kSecAttrAccount as String: Self.account,
            kSecReturnData as String: true,
            kSecMatchLimit as String: kSecMatchLimitOne,
        ]
        var item: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &item)
        if status == errSecSuccess, let data = item as? Data, data.count == 32 {
            return data
        }
        guard status == errSecItemNotFound || status == errSecSuccess else {
            throw VaultError.keychain(status)
        }
        let key = generateVaultKey()
        let add: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: Self.service,
            kSecAttrAccount as String: Self.account,
            kSecAttrLabel as String: "Toolbox vault key",
            kSecAttrAccessible as String: kSecAttrAccessibleWhenUnlockedThisDeviceOnly,
            kSecValueData as String: key,
        ]
        let addStatus = SecItemAdd(add as CFDictionary, nil)
        guard addStatus == errSecSuccess else { throw VaultError.keychain(addStatus) }
        return key
    }
}
