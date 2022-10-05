// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import Foundation
import SwiftUI

@MainActor
class Address: Identifiable, ObservableObject {
    let core: AppCoreProtocol

    let id: String
    let checksumAddress: String
    @Published var blockchainExplorerLink: URL?
    @Published var chainDisplayName: String
    @Published var chainIcon: UIImage

    @Published var nativeToken: Token
    @Published var fungibleTokens: [String: Token]
    @Published var loading: Bool = false

    var fungibleTokenList: [Token] {
        self.fungibleTokens.values.sorted(by: {$0.symbol < $1.symbol})
    }

    required init(_ core: AppCoreProtocol, id: String, checksumAddress: String, blockchainExplorerLink: URL?,
                  chainDisplayName: String, chainIcon: UIImage, nativeToken: Token) {
        self.core = core
        self.id = id
        self.checksumAddress = checksumAddress
        self.blockchainExplorerLink = blockchainExplorerLink
        self.chainDisplayName = chainDisplayName
        self.chainIcon = chainIcon
        self.nativeToken = nativeToken
        self.fungibleTokens = Dictionary()
    }

    static func fromCore(_ core: AppCoreProtocol, _ address: CoreAddress) -> Self {
        let chainIcon = Self.convertIcon(address.chainIcon)
        let url = URL(string: address.blockchainExplorerLink)
        let nativeToken = Token.fromCore(address.nativeToken)
        return Self(
            core, id: address.id, checksumAddress: address.checksumAddress, blockchainExplorerLink: url,
            chainDisplayName: address.chainDisplayName, chainIcon: chainIcon, nativeToken: nativeToken
        )
    }

    static func convertIcon(_ icon: [UInt8]) -> UIImage {
        return UIImage(data: Data(icon)) ?? UIImage(systemName: "diamond")!
    }

    func updateFromCore(_ address: CoreAddress) {
        withAnimation {
            assert(self.id == address.id, "id mismatch when updating address from core")
            assert(
                self.checksumAddress == address.checksumAddress,
                "checksum address mismatch when updating address from core"
            )
            // These values may become user configurable at some point
            self.blockchainExplorerLink = URL(string: address.blockchainExplorerLink)
            self.chainDisplayName = address.chainDisplayName
            self.chainIcon = Self.convertIcon(address.chainIcon)
            self.nativeToken.updateFromCore(address.nativeToken)
        }
    }

    func updateFungibleTokens(_ coreTokens: [CoreToken]) {
        let newIds = Set(coreTokens.map {$0.id})
        let oldIds = Set(self.fungibleTokens.keys)
        let toRemoveIds = oldIds.subtracting(newIds)
        for id in toRemoveIds {
            self.fungibleTokens.removeValue(forKey: id)
        }
        for coreToken in coreTokens {
            if let token = self.fungibleTokens[coreToken.id] {
                token.updateFromCore(coreToken)
            } else {
                self.fungibleTokens[coreToken.id] = Token.fromCore(coreToken)
            }
        }
    }

    private func fetchhNativeToken() async -> CoreToken? {
        return await dispatchBackground(.userInteractive) {
            do {
                return try self.core.nativeTokenForAddress(addressId: self.id)
            } catch {
                print("Failed to fetch native token for address id \(self.id)")
                return nil
            }
        }
    }

    private func fetchFungibleTokens() async -> [CoreToken]? {
        return await dispatchBackground(.userInteractive) {
            do {
                return try self.core.fungibleTokensForAddress(addressId: self.id)
            } catch {
                print("Failed to fetch fungible tokens for address id \(self.id)")
                return nil
            }
        }
    }

    func refreshTokens() async {
        self.loading = true
        defer { self.loading = false }
        async let native = self.fetchhNativeToken()
        async let fungibles = self.fetchFungibleTokens()
        // Execute concurrently
        let (nativeToken, fungibleTokens) = await (native, fungibles)
        if let nativeToken = nativeToken {
            self.nativeToken.updateFromCore(nativeToken)
        }
        if let fungibleTokens = fungibleTokens {
            self.updateFungibleTokens(fungibleTokens)
        }
    }

}

// MARK: - Hashable

extension Address: Equatable, Hashable {

    static func == (lhs: Address, rhs: Address) -> Bool {
        return lhs.id == rhs.id
    }

    func hash(into hasher: inout Hasher) {
        hasher.combine(id)
    }

}

// MARK: - display

extension Address {
    var addressDisplay: String {
        "\(checksumAddress.prefix(5))...\(checksumAddress.suffix(3))"
    }

    var image: Image {
        Image(uiImage: chainIcon)
    }
}

// MARK: - preview

#if DEBUG
extension Address {
    static func ethereumWallet() -> Self {
        Self.ethereum(checksumAddress: "0xb3f5354C4c4Ca1E9314302CcFcaDc9de5da53AdA")
    }

    static func polygonWallet() -> Self {
        Self.polygon(checksumAddress: "0xb3f5354C4c4Ca1E9314302CcFcaDc9de5da53AdA")
    }

    static func ethereumDapp() -> Self {
        Self.ethereum(checksumAddress: "0x696e931B0d3112FebAA9401A89C2658f96C725f2")
    }

    static func polygonDapp() -> Self {
        Self.polygon(checksumAddress: "0x696e931B0d3112FebAA9401A89C2658f96C725f2")
    }

    static func ethereum(checksumAddress: String) -> Self {
        let nativeToken = Token.eth()
        let icon = UIImage(named: "eth")!
        let explorer = URL(string: "https://etherscan.io/address/\(checksumAddress)")!
        let id = "eth-\(checksumAddress)"
        return Self(
            PreviewAppCore(), id: id, checksumAddress: "0xb3f5354C4c4Ca1E9314302CcFcaDc9de5da53AdA",
            blockchainExplorerLink: explorer, chainDisplayName: "Ethereum", chainIcon: icon,
            nativeToken: nativeToken
        )
    }

    static func polygon(checksumAddress: String) -> Self {
        let nativeToken = Token.matic()
        let icon = UIImage(named: "matic")!
        let explorer = URL(string: "https://polygonscan.com/address/\(checksumAddress)")!
        let id = "polygon-pos-\(checksumAddress)"
        return Self(
            PreviewAppCore(), id: id, checksumAddress: checksumAddress, blockchainExplorerLink: explorer,
            chainDisplayName: "Polygon PoS", chainIcon: icon, nativeToken: nativeToken
        )
    }
}
#endif
