//! The one setting that picks a deployment's network, and what each network fixes: its Zcash
//! network, the Ethereum chains it settles on, Railgun's contracts and the token there, and where
//! wallets sync. Every service checks its chain, contract and settings against it as it starts.

use std::fmt;
use std::str::FromStr;

use alloy_primitives::{Address, address};
use serde::{Deserialize, Serialize};
use zcash_protocol::consensus::{self, NetworkType, NetworkUpgrade, Parameters};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Network {
    Testnet,
    Mainnet,
}

/// Railgun's contracts on a chain, as Railgun's own `NETWORK_CONFIG` names them
/// (@railgun-community/shared-models 8.2.0).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Railgun {
    pub proxy: Address,
    pub relay_adapt: Address,
}

struct Chain {
    id: u64,
    railgun: OnChain,
    /// The token swaps there pay out, where the network fixes it.
    token: Option<Address>,
}

enum OnChain {
    Railgun(Railgun),
    /// No Railgun: swaps there pay accounts.
    None,
    /// A local chain, with whatever stand-ins its tests deploy.
    Any,
}

const TESTNET: &[Chain] = &[
    Chain {
        id: 11_155_111,
        railgun: OnChain::Railgun(Railgun {
            proxy: address!("eCFCf3b4eC647c4Ca6D49108b311b7a7C9543fea"),
            relay_adapt: address!("7e3d929EbD5bDC84d02Bd3205c777578f33A214D"),
        }),
        token: None,
    },
    // Base Sepolia, where the live suite's account payouts run.
    Chain {
        id: 84_532,
        railgun: OnChain::None,
        token: None,
    },
    // anvil.
    Chain {
        id: 31_337,
        railgun: OnChain::Any,
        token: None,
    },
];

const MAINNET: &[Chain] = &[Chain {
    id: 1,
    railgun: OnChain::Railgun(Railgun {
        proxy: address!("FA7093CDD9EE6932B4eb2c9e1cde7CE00B1FA4b9"),
        relay_adapt: address!("Ac9f360Ae85469B27aEDdEaFC579Ef2d052aD405"),
    }),
    // USDC.
    token: Some(address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48")),
}];

/// Why a deployment is not on the network its config names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mismatch {
    Chain(Network, u64),
    Railgun(Network, Address),
    Token(Network, Address),
}

impl Network {
    pub fn name(self) -> &'static str {
        match self {
            Network::Testnet => "testnet",
            Network::Mainnet => "mainnet",
        }
    }

    pub fn zcash(self) -> consensus::Network {
        match self {
            Network::Testnet => consensus::Network::TestNetwork,
            Network::Mainnet => consensus::Network::MainNetwork,
        }
    }

    pub fn zcash_type(self) -> NetworkType {
        self.zcash().network_type()
    }

    /// Whether the Zcash crates this build uses schedule NU6.3 here, which brought the Ironwood
    /// pool swaps use.
    pub fn ironwood(self) -> bool {
        self.zcash()
            .activation_height(NetworkUpgrade::Nu6_3)
            .is_some()
    }

    /// The lightwalletd wallets sync from unless told otherwise.
    pub fn lightwalletd(self) -> &'static str {
        match self {
            Network::Testnet => "https://testnet.zec.rocks:443",
            Network::Mainnet => "https://zec.rocks:443",
        }
    }

    /// Checks a deployment on `chain_id` is one of this network's: the chain is, its contract pays
    /// into this network's Railgun there or into none (`railgun` zero), and it pays out the token
    /// this network fixes there, if any.
    pub fn check(self, chain_id: u64, railgun: Address, token: Address) -> Result<(), Mismatch> {
        let chain = self
            .chain(chain_id)
            .ok_or(Mismatch::Chain(self, chain_id))?;
        let ours = match chain.railgun {
            OnChain::Railgun(ours) => railgun == ours.proxy,
            OnChain::None => false,
            OnChain::Any => true,
        };
        if !railgun.is_zero() && !ours {
            return Err(Mismatch::Railgun(self, railgun));
        }
        if chain.token.is_some_and(|fixed| fixed != token) {
            return Err(Mismatch::Token(self, token));
        }
        Ok(())
    }

    /// Railgun's own Relay Adapt on `chain_id`, if this network has Railgun's deployment there.
    pub fn relay_adapt(self, chain_id: u64) -> Option<Address> {
        match self.chain(chain_id)?.railgun {
            OnChain::Railgun(railgun) => Some(railgun.relay_adapt),
            OnChain::None | OnChain::Any => None,
        }
    }

    fn chain(self, chain_id: u64) -> Option<&'static Chain> {
        let chains = match self {
            Network::Testnet => TESTNET,
            Network::Mainnet => MAINNET,
        };
        chains.iter().find(|chain| chain.id == chain_id)
    }
}

impl fmt::Display for Network {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for Network {
    type Err = String;

    fn from_str(name: &str) -> Result<Self, Self::Err> {
        match name {
            "testnet" => Ok(Network::Testnet),
            "mainnet" => Ok(Network::Mainnet),
            _ => Err(format!("{name} is neither testnet nor mainnet")),
        }
    }
}

impl fmt::Display for Mismatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Mismatch::Chain(network, chain_id) => {
                write!(
                    f,
                    "the EVM RPC is on chain {chain_id}, not a {network} chain"
                )
            }
            Mismatch::Railgun(network, railgun) => write!(
                f,
                "the contract pays into a Railgun at {railgun}, not {network}'s"
            ),
            Mismatch::Token(network, token) => {
                write!(
                    f,
                    "the token {token} is not the one {network} swaps pay out"
                )
            }
        }
    }
}

impl std::error::Error for Mismatch {}

#[cfg(test)]
mod tests {
    use super::*;

    const SEPOLIA_RAILGUN: Address = address!("eCFCf3b4eC647c4Ca6D49108b311b7a7C9543fea");
    const MAINNET_RAILGUN: Address = address!("FA7093CDD9EE6932B4eb2c9e1cde7CE00B1FA4b9");
    const USDC: Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");

    #[test]
    fn each_network_takes_its_own_chains_railgun_and_token_alone() {
        let any = Address::repeat_byte(7);
        assert_eq!(
            Network::Testnet.check(11_155_111, SEPOLIA_RAILGUN, any),
            Ok(())
        );
        assert_eq!(Network::Testnet.check(84_532, Address::ZERO, any), Ok(()));
        assert_eq!(Network::Testnet.check(31_337, any, any), Ok(()));
        assert_eq!(Network::Mainnet.check(1, MAINNET_RAILGUN, USDC), Ok(()));
        assert_eq!(Network::Mainnet.check(1, Address::ZERO, USDC), Ok(()));

        assert_eq!(
            Network::Testnet.check(1, MAINNET_RAILGUN, USDC),
            Err(Mismatch::Chain(Network::Testnet, 1))
        );
        assert_eq!(
            Network::Mainnet.check(11_155_111, SEPOLIA_RAILGUN, any),
            Err(Mismatch::Chain(Network::Mainnet, 11_155_111))
        );
        assert_eq!(
            Network::Mainnet.check(31_337, any, USDC),
            Err(Mismatch::Chain(Network::Mainnet, 31_337))
        );
        assert_eq!(
            Network::Testnet.check(11_155_111, MAINNET_RAILGUN, any),
            Err(Mismatch::Railgun(Network::Testnet, MAINNET_RAILGUN))
        );
        assert_eq!(
            Network::Testnet.check(84_532, SEPOLIA_RAILGUN, any),
            Err(Mismatch::Railgun(Network::Testnet, SEPOLIA_RAILGUN))
        );
        assert_eq!(
            Network::Mainnet.check(1, any, USDC),
            Err(Mismatch::Railgun(Network::Mainnet, any))
        );
        assert_eq!(
            Network::Mainnet.check(1, MAINNET_RAILGUN, any),
            Err(Mismatch::Token(Network::Mainnet, any))
        );
    }

    #[test]
    fn relay_adapt_is_railguns_own_where_the_network_has_railgun() {
        assert_eq!(
            Network::Testnet.relay_adapt(11_155_111),
            Some(address!("7e3d929EbD5bDC84d02Bd3205c777578f33A214D"))
        );
        assert_eq!(
            Network::Mainnet.relay_adapt(1),
            Some(address!("Ac9f360Ae85469B27aEDdEaFC579Ef2d052aD405"))
        );
        assert_eq!(Network::Testnet.relay_adapt(84_532), None);
        assert_eq!(Network::Testnet.relay_adapt(31_337), None);
        assert_eq!(Network::Mainnet.relay_adapt(11_155_111), None);
    }

    #[test]
    fn names_read_back_and_ironwood_is_scheduled_on_both() {
        for network in [Network::Testnet, Network::Mainnet] {
            assert_eq!(network.name().parse(), Ok(network));
            assert!(network.ironwood());
        }
        assert!("main".parse::<Network>().is_err());
    }
}
