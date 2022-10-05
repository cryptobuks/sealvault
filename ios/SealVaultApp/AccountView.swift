// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

import SwiftUI

@MainActor
struct AccountView: View {
    @EnvironmentObject private var model: GlobalModel
    @ObservedObject var account: Account

    var body: some View {
        ScrollViewReader { _ in
            List {
                Section {
                    ForEach(account.walletList) { wallet in
                        NavigationLink {
                            AddressView(title: "Wallet", account: account, address: wallet)

                        } label: {
                            WalletRow(address: wallet)
                        }
                    }
                } header: {
                    Text("Wallets")
                }
                Section {
                    ForEach(account.dappList) { dapp in
                        ForEach(dapp.addressList) { dappAddress in
                            NavigationLink {
                                AddressView(title: "Dapp", account: account, address: dappAddress)
                            } label: {
                                DappRow(dapp: dapp).accessibilityIdentifier(dapp.displayName)
                            }
                        }
                    }
                } header: {
                    Text("Dapps")
                }
            }
            .refreshable(action: {
                await model.refreshAccounts()
            })
            .accessibilityRotor("Dapps", entries: account.dappList, entryLabel: \.displayName)
        }
        .navigationTitle(Text(account.displayName))
        .navigationBarTitleDisplayMode(.inline)
        .toolbar {
            ToolbarItem(placement: .navigationBarTrailing) {
                AccountImageCircle(account: account)
            }
        }
        .task {
            await self.model.refreshAccounts()
        }
    }
}

struct AccountView_Previews: PreviewProvider {
    static var previews: some View {
        let model = GlobalModel.buildForPreview()
        return AccountView(account: model.activeAccount)
    }
}
