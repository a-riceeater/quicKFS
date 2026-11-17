// SPDX-License-Identifier: Apache-2.0
import Foundation
import Security

/// Server passwords, stored as generic Keychain items keyed by profile id.
///
/// Keychain calls can block for as long as macOS shows an access prompt, so
/// the app only uses the async entry points, which run every operation in
/// order on one background queue. A read therefore always observes earlier
/// writes, and the main thread never waits on the Keychain.
enum Keychain {
    private static let service = "org.quickfs.client"
    private static let queue = DispatchQueue(label: "org.quickfs.keychain", qos: .userInitiated)

    static func load(for profile: UUID) async -> String? {
        await withCheckedContinuation { continuation in
            queue.async { continuation.resume(returning: password(for: profile)) }
        }
    }

    static func store(_ password: String, for profile: UUID) {
        queue.async { setPassword(password, for: profile) }
    }

    static func remove(for profile: UUID) {
        queue.async { removePassword(for: profile) }
    }

    private static func password(for profile: UUID) -> String? {
        var query = baseQuery(profile)
        query[kSecReturnData as String] = true
        query[kSecMatchLimit as String] = kSecMatchLimitOne
        var result: AnyObject?
        guard SecItemCopyMatching(query as CFDictionary, &result) == errSecSuccess,
            let data = result as? Data
        else { return nil }
        return String(data: data, encoding: .utf8)
    }

    @discardableResult
    private static func setPassword(_ password: String, for profile: UUID) -> Bool {
        let data = Data(password.utf8)
        let update = [kSecValueData as String: data] as CFDictionary
        let status = SecItemUpdate(baseQuery(profile) as CFDictionary, update)
        if status == errSecSuccess { return true }
        guard status == errSecItemNotFound else { return false }
        var item = baseQuery(profile)
        item[kSecValueData as String] = data
        item[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly
        item[kSecAttrLabel as String] = "quicKFS server password"
        return SecItemAdd(item as CFDictionary, nil) == errSecSuccess
    }

    private static func removePassword(for profile: UUID) {
        SecItemDelete(baseQuery(profile) as CFDictionary)
    }

    private static func baseQuery(_ profile: UUID) -> [String: Any] {
        [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: profile.uuidString,
        ]
    }
}
