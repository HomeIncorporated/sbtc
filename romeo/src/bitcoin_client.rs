//! RPC Bitcoin client

use std::{
	fmt::Debug,
	sync::{Arc, Mutex},
	time::Duration,
};

use anyhow::anyhow;
use bdk::{
	bitcoin::{Block, PrivateKey, Script, Transaction, Txid},
	bitcoincore_rpc::{self, Auth, Client as RPCClient, RpcApi},
	blockchain::{ElectrumBlockchain, GetHeight, WalletSync},
	database::MemoryDatabase,
	template::P2TR,
	SignOptions, SyncOptions, Wallet,
};
use derivative::Derivative;
use sbtc_core::operations::op_return::utils::reorder_outputs;
use stacks_core::wallet::BitcoinCredentials;
use tokio::{task::spawn_blocking, time::sleep};
use tracing::trace;
use url::Url;

use crate::event::TransactionStatus;

const BLOCK_POLLING_INTERVAL: Duration = Duration::from_secs(5);

/// [Client]
pub type BitcoinClient = Client<ElectrumBlockchain>;

/// Bitcoin RPC client
/// unless testing use [ElectrumBlockchain] for `ElectrumClient`.
#[derive(Derivative, Debug)]
#[derivative(Clone)]
pub struct Client<ElectrumClient = ElectrumBlockchain> {
	bitcoin_url: Url,
	#[derivative(Clone(bound = ""))]
	blockchain: Arc<ElectrumClient>,
	// required for fulfillment txs
	wallet: Arc<Mutex<Wallet<MemoryDatabase>>>,
}

impl<B> Client<B> {
	/// Create a new RPC client
	pub fn new(
		bitcoin_url: Url,
		electrum_blockchain: B,
		credentials: BitcoinCredentials,
	) -> anyhow::Result<Self> {
		let network = credentials.network();
		let p2tr_private_key = PrivateKey::new(
			credentials.private_key_p2tr(),
			credentials.network(),
		);

		let blockchain = electrum_blockchain;

		let wallet = Wallet::new(
			P2TR(p2tr_private_key),
			Some(P2TR(p2tr_private_key)),
			network,
			MemoryDatabase::default(),
		)?;

		if bitcoin_url.username().is_empty() {
			return Err(anyhow::anyhow!("Username in {bitcoin_url} is empty"));
		}

		if bitcoin_url.password().is_none() {
			return Err(anyhow::anyhow!("Password in {bitcoin_url} is empty"));
		}

		Ok(Self {
			bitcoin_url,
			blockchain: Arc::new(blockchain),
			wallet: Arc::new(Mutex::new(wallet)),
		})
	}
}

impl<B> Client<B> {
	/// Create a new RPC client
	async fn execute<F, T>(
		&self,
		f: F,
	) -> anyhow::Result<bitcoincore_rpc::Result<T>>
	where
		F: FnOnce(RPCClient) -> bitcoincore_rpc::Result<T> + Send + 'static,
		T: Send + 'static,
	{
		let mut url = self.bitcoin_url.clone();

		let username = url.username().to_string();
		let password = url.password().unwrap_or_default().to_string();

		url.set_username("").unwrap();
		url.set_password(None).unwrap();

		let client =
			RPCClient::new(url.as_ref(), Auth::UserPass(username, password))?;

		Ok(spawn_blocking(move || f(client)).await?)
	}

	/// Broadcast a transaction
	pub async fn broadcast(&self, tx: Transaction) -> anyhow::Result<()> {
		self.execute(move |client| client.send_raw_transaction(&tx))
			.await??;

		Ok(())
	}

	/// Get transaction status
	pub async fn get_tx_status(
		&self,
		txid: Txid,
	) -> anyhow::Result<TransactionStatus> {
		let is_confirmed = self
			.execute(move |client| client.get_raw_transaction_info(&txid, None))
			.await?
			.ok()
			.and_then(|tx| tx.confirmations)
			.map(|confirmations| confirmations > 0)
			.unwrap_or_default();

		let in_mempool = self
			.execute(move |client| client.get_mempool_entry(&txid))
			.await?
			.is_ok();

		let res = match (is_confirmed, in_mempool) {
			(true, false) => TransactionStatus::Confirmed,
			(false, true) => TransactionStatus::Broadcasted,
			(false, false) => TransactionStatus::Rejected,
			(true, true) => {
				panic!("Transaction cannot be both confirmed and pending")
			}
		};

		tracing::debug!("BTC TX {} IS {:?}", txid, res);

		Ok(res)
	}

	/// Get block
	pub async fn get_block(
		&self,
		block_height: u32,
	) -> anyhow::Result<(u32, Block)> {
		let block_hash = loop {
			let res = self
				.execute(move |client| {
					client.get_block_hash(block_height as u64)
				})
				.await?;

			match res {
				Ok(hash) => {
					trace!(
						"Got Bitcoin block hash at height {}: {}",
						block_height,
						hash
					);
					break hash;
				}
				Err(bitcoincore_rpc::Error::JsonRpc(
					bitcoincore_rpc::jsonrpc::Error::Rpc(err),
				)) => {
					if err.code == -8 {
						trace!("Bitcoin block not found, retrying...");
					} else {
						Err(anyhow!(
							"Error fetching Bitcoin block: {:?}",
							err
						))?;
					}
				}
				Err(bitcoincore_rpc::Error::JsonRpc(
					bitcoincore_rpc::jsonrpc::Error::Transport(_),
				)) => {
					trace!("Bitcoin client connection error, retrying...");
				}
				Err(err) => {
					Err(anyhow!("Error fetching Bitcoin block: {:?}", err))?
				}
			};

			sleep(BLOCK_POLLING_INTERVAL).await;
		};

		let block = self
			.execute(move |client| client.get_block(&block_hash))
			.await??;

		Ok((block_height, block))
	}

	/// Get current block height
	pub async fn get_height(&self) -> anyhow::Result<u32> {
		let info = self
			.execute(|client| client.get_blockchain_info())
			.await??;

		Ok(info.blocks as u32)
	}
}

impl<B: WalletSync + GetHeight + Sync + 'static> Client<B>
where
	Arc<B>: Send,
{
	/// Sign and broadcast a transaction
	pub async fn sign_and_broadcast(
		&self,
		outputs: Vec<(Script, u64)>,
	) -> anyhow::Result<Txid> {
		sleep(Duration::from_secs(3)).await;

		let blockchain = self.blockchain.clone();
		let wallet = self.wallet.clone();

		let tx: Transaction =
			spawn_blocking::<_, anyhow::Result<Transaction>>(move || {
				let wallet = wallet
					.lock()
					.map_err(|_| anyhow!("Cannot get wallet read lock"))?;

				wallet.sync(&blockchain, SyncOptions::default())?;

				let mut tx_builder = wallet.build_tx();

				for (script, amount) in outputs.clone() {
					tx_builder.add_recipient(script, amount);
				}

				let (mut partial_tx, _) = tx_builder.finish()?;

				partial_tx.unsigned_tx.output =
					reorder_outputs(partial_tx.unsigned_tx.output, outputs);

				wallet.sign(&mut partial_tx, SignOptions::default())?;

				Ok(partial_tx.extract_tx())
			})
			.await??;

		let txid: Txid = self
			.execute(move |client| client.send_raw_transaction(&tx))
			.await??;

		Ok(txid)
	}
}

#[cfg(test)]
mod tests {
	use std::path::Path;

	use bdk::{
		bitcoin::Network as BitcoinNetwork,
		blockchain::{ConfigurableBlockchain, ElectrumBlockchainConfig},
	};
	use blockstack_lib::vm::ContractName;
	use stacks_core::{wallet::Wallet, Network};

	use super::*;
	use crate::{config::Config, test::MNEMONIC};

	#[test]
	// test that wallet returns correct address
	fn test_wallet_address() {
		let wallet = Wallet::new(MNEMONIC[0]).unwrap();

		let stacks_network = Network::Testnet;
		let stacks_credentials = wallet.credentials(stacks_network, 0).unwrap();
		let bitcoin_credentials = wallet
			.bitcoin_credentials(BitcoinNetwork::Testnet, 0)
			.unwrap();

		let conf = Config {
			state_directory: Path::new("/tmp/romeo").to_path_buf(),
			bitcoin_credentials,
			bitcoin_node_url: "http://user:pwd@localhost:18443"
				.parse()
				.unwrap(),
			electrum_node_url: "ssl://blockstream.info:993".parse().unwrap(),
			bitcoin_network: "testnet".parse().unwrap(),
			contract_name: ContractName::from("asset"),
			stacks_node_url: "http://localhost:20443".parse().unwrap(),
			stacks_credentials,
			stacks_network,
			hiro_api_key: None,
			strict: true,
		};

		let electrum_blockchain =
			ElectrumBlockchain::from_config(&ElectrumBlockchainConfig {
				url: conf.electrum_node_url.to_string(),
				socks5: None,
				retry: 3,
				timeout: Some(10),
				stop_gap: 10,
				validate_domain: false,
			})
			.unwrap();

		let client = Client::new(
			conf.bitcoin_node_url.clone(),
			electrum_blockchain,
			conf.bitcoin_credentials.clone(),
		)
		.unwrap();

		let client_sbtc_wallet = client
			.wallet
			.clone()
			.lock()
			.unwrap()
			.get_address(bdk::wallet::AddressIndex::Peek(0))
			.unwrap();

		// expect sbtc wallet to be p2tr of mnemonic
		let expected_sbtc_wallet =
			"tb1pte5zmd7qzj4hdu45lh9mmdm0nwq3z35pwnxmzkwld6y0a8g83nnq6ts2d4";
		// expect sbtc_wallet equals and config sbtc wallet address to be the
		// p2tr address
		assert_eq!(client_sbtc_wallet.to_string(), expected_sbtc_wallet);
		assert_eq!(
			conf.sbtc_wallet_address().to_string(),
			expected_sbtc_wallet
		);
	}

	fn client<const WALLET_INDEX: usize>(
		url: &str,
	) -> anyhow::Result<Client<()>> {
		let wallet = Wallet::new(MNEMONIC[WALLET_INDEX]).unwrap();
		let credentials = wallet
			.bitcoin_credentials(BitcoinNetwork::Testnet, 0)
			.unwrap();

		Client::new(url.parse().unwrap(), (), credentials)
	}

	#[test]
	fn no_password() {
		let broken_client =
			|url: &str| client::<0>(url).expect_err("missing password");

		let err_string = "Password in http://user@host/ is empty";
		assert_eq!(broken_client("http://user:@host").to_string(), err_string,);
		assert_eq!(broken_client("http://user@host").to_string(), err_string,);
		let err_string = "Username in http://host/ is empty";
		assert_eq!(broken_client("http://@host").to_string(), err_string);
		assert_eq!(broken_client("http://host").to_string(), err_string,);
	}
}
