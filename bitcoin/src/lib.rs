pub mod cli;

mod addr;
mod electrs;
mod error;
mod iter;

use async_trait::async_trait;
use backoff::{backoff::Backoff, future::retry, ExponentialBackoff};
use bitcoincore_rpc::bitcoin::consensus::encode::serialize_hex;
pub use bitcoincore_rpc::{
    bitcoin::{
        blockdata::{opcodes::all as opcodes, script::Builder},
        consensus::encode::{deserialize, serialize},
        hash_types::BlockHash,
        hashes::{hex::ToHex, sha256, Hash},
        secp256k1,
        secp256k1::{constants::PUBLIC_KEY_SIZE, SecretKey},
        util::{address::Payload, key, merkleblock::PartialMerkleTree, psbt::serialize::Serialize, uint::Uint256},
        Address, Amount, Block, BlockHeader, Network, OutPoint, PrivateKey, PubkeyHash, PublicKey, Script, ScriptHash,
        SignedAmount, Transaction, TxIn, TxMerkleNode, TxOut, Txid, WPubkeyHash, WScriptHash,
    },
    bitcoincore_rpc_json::{
        CreateRawTransactionInput, FundRawTransactionOptions, GetBlockchainInfoResult, GetTransactionResult,
        GetTransactionResultDetailCategory, WalletTxInfo,
    },
    json::{self, AddressType, GetBlockResult},
    jsonrpc::{error::RpcError, Error as JsonRpcError},
    Auth, Client, Error as BitcoinError, RpcApi,
};
use electrs::{get_address_tx_history_full, get_tx_hex, get_tx_merkle_block_proof};
pub use error::{BitcoinRpcError, ConversionError, Error};
use esplora_btc_api::apis::configuration::Configuration as ElectrsConfiguration;
pub use iter::{reverse_stream_transactions, stream_blocks, stream_in_chain_transactions};
use log::{info, trace};
use serde_json::error::Category as SerdeJsonCategory;
use sp_core::H256;
use std::{convert::TryInto, future::Future, sync::Arc, time::Duration};
use tokio::{
    sync::{Mutex, OwnedMutexGuard},
    time::{sleep, timeout},
};

#[macro_use]
extern crate num_derive;

/// Average time to mine a Bitcoin block.
pub const BLOCK_INTERVAL: Duration = Duration::from_secs(600); // 10 minutes
pub const DEFAULT_MAX_TX_COUNT: usize = 100_000_000;
/// the bitcoin core version.
/// See https://github.com/bitcoin/bitcoin/blob/833add0f48b0fad84d7b8cf9373a349e7aef20b4/src/rpc/net.cpp#L627
/// and https://github.com/bitcoin/bitcoin/blob/833add0f48b0fad84d7b8cf9373a349e7aef20b4/src/clientversion.h#L33-L37
pub const BITCOIN_CORE_VERSION_23: usize = 230_000;
const NOT_IN_MEMPOOL_ERROR_CODE: i32 = BitcoinRpcError::RpcInvalidAddressOrKey as i32;

// Time to sleep before retry on startup.
const RETRY_DURATION: Duration = Duration::from_millis(1000);

// The default initial interval value (1 second).
const INITIAL_INTERVAL: Duration = Duration::from_millis(1000);

// The default maximum elapsed time (24 hours).
const MAX_ELAPSED_TIME: Duration = Duration::from_secs(24 * 60 * 60);

// The default maximum back off time (5 minutes).
const MAX_INTERVAL: Duration = Duration::from_secs(5 * 60);

// The default multiplier value (delay doubles every time).
const MULTIPLIER: f64 = 2.0;

// Random value between 25% below and 25% above the ideal delay.
const RANDOMIZATION_FACTOR: f64 = 0.25;

const DERIVATION_KEY_LABEL: &str = "derivation-key";
const DEPOSIT_LABEL: &str = "deposit";

const ELECTRS_TESTNET_URL: &str = "https://btc-testnet.interlay.io";
const ELECTRS_MAINNET_URL: &str = "https://btc-mainnet.interlay.io";
const ELECTRS_LOCALHOST_URL: &str = "http://localhost:3002";

fn get_exponential_backoff() -> ExponentialBackoff {
    ExponentialBackoff {
        current_interval: INITIAL_INTERVAL,
        initial_interval: INITIAL_INTERVAL,
        max_elapsed_time: Some(MAX_ELAPSED_TIME),
        max_interval: MAX_INTERVAL,
        multiplier: MULTIPLIER,
        randomization_factor: RANDOMIZATION_FACTOR,
        ..Default::default()
    }
}

#[derive(PartialEq, PartialOrd, Clone, Copy, Debug)]
pub struct SatPerVbyte(pub u64);

#[derive(Debug, Clone)]
pub struct TransactionMetadata {
    pub txid: Txid,
    pub proof: Vec<u8>,
    pub raw_tx: Vec<u8>,
    pub block_height: u32,
    pub block_hash: BlockHash,
    pub fee: Option<SignedAmount>,
}

#[async_trait]
pub trait BitcoinCoreApi {
    fn network(&self) -> Network;

    async fn wait_for_block(&self, height: u32, num_confirmations: u32) -> Result<Block, Error>;

    async fn get_block_count(&self) -> Result<u64, Error>;

    fn get_balance(&self, min_confirmations: Option<u32>) -> Result<Amount, Error>;

    fn list_transactions(&self, max_count: Option<usize>) -> Result<Vec<json::ListTransactionResult>, Error>;

    async fn get_raw_tx(&self, txid: &Txid, block_hash: &BlockHash) -> Result<Vec<u8>, Error>;

    async fn get_transaction(&self, txid: &Txid, block_hash: Option<BlockHash>) -> Result<Transaction, Error>;

    async fn get_proof(&self, txid: Txid, block_hash: &BlockHash) -> Result<Vec<u8>, Error>;

    async fn get_block_hash(&self, height: u32) -> Result<BlockHash, Error>;

    async fn get_new_address(&self) -> Result<Address, Error>;

    async fn get_new_public_key(&self) -> Result<PublicKey, Error>;

    fn dump_derivation_key<P: Into<[u8; PUBLIC_KEY_SIZE]> + Send + Sync + 'static>(
        &self,
        public_key: P,
    ) -> Result<PrivateKey, Error>;

    fn import_derivation_key(&self, private_key: &PrivateKey) -> Result<(), Error>;

    async fn add_new_deposit_key<P: Into<[u8; PUBLIC_KEY_SIZE]> + Send + Sync + 'static>(
        &self,
        public_key: P,
        secret_key: Vec<u8>,
    ) -> Result<(), Error>;

    async fn get_best_block_hash(&self) -> Result<BlockHash, Error>;

    async fn get_pruned_height(&self) -> Result<u64, Error>;

    async fn get_block(&self, hash: &BlockHash) -> Result<Block, Error>;

    async fn get_block_header(&self, hash: &BlockHash) -> Result<BlockHeader, Error>;

    async fn get_mempool_transactions<'a>(
        &'a self,
    ) -> Result<Box<dyn Iterator<Item = Result<Transaction, Error>> + Send + 'a>, Error>;

    async fn wait_for_transaction_metadata(
        &self,
        txid: Txid,
        num_confirmations: u32,
    ) -> Result<TransactionMetadata, Error>;

    async fn bump_fee(&self, txid: &Txid, address: Address, fee_rate: SatPerVbyte) -> Result<Txid, Error>;

    async fn create_and_send_transaction(
        &self,
        address: Address,
        sat: u64,
        fee_rate: SatPerVbyte,
        request_id: Option<H256>,
    ) -> Result<Txid, Error>;

    async fn send_to_address(
        &self,
        address: Address,
        sat: u64,
        request_id: Option<H256>,
        fee_rate: SatPerVbyte,
        num_confirmations: u32,
    ) -> Result<TransactionMetadata, Error>;

    async fn create_or_load_wallet(&self) -> Result<(), Error>;

    async fn rescan_blockchain(&self, start_height: usize, end_height: usize) -> Result<(), Error>;

    async fn rescan_electrs_for_addresses(&self, addresses: Vec<Address>) -> Result<(), Error>;

    fn get_utxo_count(&self) -> Result<usize, Error>;

    fn is_in_mempool(&self, txid: Txid) -> Result<bool, Error>;

    fn fee_rate(&self, txid: Txid) -> Result<SatPerVbyte, Error>;
}

struct LockedTransaction {
    transaction: Transaction,
    recipient: String,
    _lock: Option<OwnedMutexGuard<()>>,
}

impl LockedTransaction {
    pub fn new(transaction: Transaction, recipient: String, lock: Option<OwnedMutexGuard<()>>) -> Self {
        LockedTransaction {
            transaction,
            recipient,
            _lock: lock,
        }
    }
}

fn parse_bitcoin_network(src: &str) -> Result<Network, Error> {
    match src {
        "main" => Ok(Network::Bitcoin),
        "test" => Ok(Network::Testnet),
        "regtest" => Ok(Network::Regtest),
        _ => Err(Error::InvalidBitcoinNetwork),
    }
}

struct ConnectionInfo {
    chain: String,
    version: usize,
}

fn get_info(rpc: &Client) -> Result<ConnectionInfo, Error> {
    let blockchain_info = rpc.get_blockchain_info()?;
    let network_info = rpc.get_network_info()?;
    Ok(ConnectionInfo {
        chain: blockchain_info.chain,
        version: network_info.version,
    })
}

/// Connect to a bitcoin-core full node or timeout.
async fn connect(rpc: &Client, connection_timeout: Duration) -> Result<Network, Error> {
    info!("Connecting to bitcoin-core...");
    timeout(connection_timeout, async move {
        loop {
            match get_info(rpc) {
                Err(err)
                    if err.is_transport_error() =>
                {
                    trace!("A transport error occurred while attempting to communicate with bitcoin-core. Typically this indicates a failure to connect");
                    sleep(RETRY_DURATION).await;
                    continue;
                }
                Err(Error::BitcoinError(BitcoinError::JsonRpc(JsonRpcError::Rpc(err))))
                    if BitcoinRpcError::from(err.clone()) == BitcoinRpcError::RpcInWarmup =>
                {
                    // may be loading block index or verifying wallet
                    trace!("bitcoin-core still in warm up");
                    sleep(RETRY_DURATION).await;
                    continue;
                }
                Err(Error::BitcoinError(BitcoinError::JsonRpc(JsonRpcError::Json(err)))) if err.classify() == SerdeJsonCategory::Syntax => {
                    // invalid response, can happen if server is in shutdown
                    trace!("bitcoin-core gave an invalid response: {}", err);
                    sleep(RETRY_DURATION).await;
                    continue;
                }
                Ok(ConnectionInfo{chain, version}) => {
                    info!("Connected to {}", chain);
                    info!("Bitcoin version {}", version);

                    if version >= BITCOIN_CORE_VERSION_23 {
                        return Err(Error::IncompatibleVersion(version))
                    }

                    return parse_bitcoin_network(&chain);
                }
                Err(err) => return Err(err),
            }
        }
    })
    .await?
}

pub struct BitcoinCoreBuilder {
    url: String,
    auth: Auth,
    wallet_name: Option<String>,
    electrs_url: Option<String>,
}

impl BitcoinCoreBuilder {
    pub fn new(url: String) -> Self {
        Self {
            url,
            auth: Auth::None,
            wallet_name: None,
            electrs_url: None,
        }
    }

    pub fn set_auth(mut self, auth: Auth) -> Self {
        self.auth = auth;
        self
    }

    pub fn set_wallet_name(mut self, wallet_name: Option<String>) -> Self {
        self.wallet_name = wallet_name;
        self
    }

    pub fn set_electrs_url(mut self, electrs_url: Option<String>) -> Self {
        self.electrs_url = electrs_url;
        self
    }

    fn new_client(&self) -> Result<Client, Error> {
        let url = match self.wallet_name {
            Some(ref x) => format!("{}/wallet/{}", self.url, x),
            None => self.url.clone(),
        };
        Ok(Client::new(&url, self.auth.clone())?)
    }

    pub fn build_with_network(self, network: Network) -> Result<BitcoinCore, Error> {
        Ok(BitcoinCore::new(
            self.new_client()?,
            self.wallet_name,
            network,
            self.electrs_url,
        ))
    }

    pub async fn build_and_connect(self, connection_timeout: Duration) -> Result<BitcoinCore, Error> {
        let client = self.new_client()?;
        let network = connect(&client, connection_timeout).await?;
        Ok(BitcoinCore::new(client, self.wallet_name, network, self.electrs_url))
    }
}

#[derive(Clone)]
pub struct BitcoinCore {
    rpc: Arc<Client>,
    wallet_name: Option<String>,
    network: Network,
    transaction_creation_lock: Arc<Mutex<()>>,
    electrs_config: ElectrsConfiguration,
    #[cfg(feature = "regtest-manual-mining")]
    auto_mine: bool,
}

impl BitcoinCore {
    fn new(client: Client, wallet_name: Option<String>, network: Network, electrs_url: Option<String>) -> Self {
        BitcoinCore {
            rpc: Arc::new(client),
            wallet_name,
            network,
            transaction_creation_lock: Arc::new(Mutex::new(())),
            electrs_config: ElectrsConfiguration {
                base_path: electrs_url.unwrap_or_else(|| {
                    match network {
                        Network::Bitcoin => ELECTRS_MAINNET_URL,
                        Network::Testnet => ELECTRS_TESTNET_URL,
                        _ => ELECTRS_LOCALHOST_URL,
                    }
                    .to_owned()
                }),
                ..Default::default()
            },
            #[cfg(feature = "regtest-manual-mining")]
            auto_mine: false,
        }
    }

    #[cfg(feature = "regtest-manual-mining")]
    pub fn set_auto_mining(&mut self, enable: bool) {
        self.auto_mine = enable;
    }

    /// Wait indefinitely for the node to sync.
    pub async fn sync(&self) -> Result<(), Error> {
        info!("Waiting for bitcoin-core to sync...");
        loop {
            let info = self.rpc.get_blockchain_info()?;
            // NOTE: initial_block_download is always true on regtest
            if !info.initial_block_download || info.verification_progress.eq(&1.0) {
                info!("Synced!");
                return Ok(());
            }
            trace!("bitcoin-core not synced");
            sleep(RETRY_DURATION).await;
        }
    }

    /// Wrapper of rust_bitcoincore_rpc::create_raw_transaction_hex that accepts an optional op_return
    fn create_raw_transaction_hex(
        &self,
        address: String,
        amount: Amount,
        request_id: Option<H256>,
    ) -> Result<String, Error> {
        let mut outputs = serde_json::Map::<String, serde_json::Value>::new();
        // add the payment output
        outputs.insert(address, serde_json::Value::from(amount.as_btc()));

        if let Some(request_id) = request_id {
            // add the op_return data - bitcoind will add op_return and the length automatically
            outputs.insert("data".to_string(), serde_json::Value::from(request_id.to_hex()));
        }

        let args = [
            serde_json::to_value::<&[json::CreateRawTransactionInput]>(&[])?,
            serde_json::to_value(outputs)?,
            serde_json::to_value(0i64)?, /* locktime - default 0: see https://developer.bitcoin.org/reference/rpc/createrawtransaction.html */
            serde_json::to_value(true)?, // BIP125-replaceable, aka Replace By Fee (RBF)
        ];
        Ok(self.rpc.call("createrawtransaction", &args)?)
    }

    async fn fund_and_sign_transaction(
        &self,
        fee_rate: SatPerVbyte,
        raw_tx: &str,
        return_to_self_address: &Option<Address>,
        recipient: &str,
        auto_retry: bool,
    ) -> Result<LockedTransaction, Error> {
        self.with_wallet_inner(auto_retry, || async {
            // ensure no other fund_raw_transaction calls are made until we submitted the
            // transaction to the bitcoind. If we don't do this, the same uxto may be used
            // as input twice (i.e. double spend)
            let lock = self.transaction_creation_lock.clone().lock_owned().await;
            // FundRawTransactionOptions takes an amount per kvByte, rather than per vByte
            let fee_rate = fee_rate.0.saturating_mul(1_000);
            let funding_opts = FundRawTransactionOptions {
                fee_rate: Some(Amount::from_sat(fee_rate)),
                change_address: return_to_self_address.clone(),
                replaceable: Some(true),
                ..Default::default()
            };

            // fund the transaction: adds required inputs, and possibly a return-to-self output
            let funded_raw_tx = self.rpc.fund_raw_transaction(raw_tx, Some(&funding_opts), None)?;

            // sign the transaction
            let signed_funded_raw_tx =
                self.rpc
                    .sign_raw_transaction_with_wallet(&funded_raw_tx.transaction()?, None, None)?;

            // Make sure signing is successful
            if signed_funded_raw_tx.errors.is_some() {
                return Err(Error::TransactionSigningError);
            }

            let transaction = signed_funded_raw_tx.transaction()?;

            Ok(LockedTransaction::new(transaction, recipient.to_string(), Some(lock)))
        })
        .await
    }

    /// Creates and return a transaction; it is not submitted to the mempool. While the returned value
    /// is alive, no other transactions can be created (this is guarded by a mutex). This prevents
    /// accidental double spending.
    ///
    /// # Arguments
    /// * `address` - Bitcoin address to fund
    /// * `sat` - number of Satoshis to transfer
    /// * `fee_rate` - fee rate in sat/vbyte
    /// * `request_id` - the issue/redeem/replace id for which this transfer is being made
    async fn create_transaction(
        &self,
        address: Address,
        sat: u64,
        fee_rate: SatPerVbyte,
        request_id: Option<H256>,
    ) -> Result<LockedTransaction, Error> {
        let recipient = address.to_string();
        let raw_tx = self
            .with_wallet(|| async {
                // create raw transaction that includes the op_return (if any). If we were to add the op_return
                // after funding, the fees might be insufficient. An alternative to our own version of
                // this function would be to call create_raw_transaction (without the _hex suffix), and
                // to add the op_return afterwards. However, this function fails if no inputs are
                // specified, as is the case for us prior to calling fund_raw_transaction.
                self.create_raw_transaction_hex(recipient.clone(), Amount::from_sat(sat), request_id)
            })
            .await?;

        self.fund_and_sign_transaction(fee_rate, &raw_tx, &None, &recipient, true)
            .await
    }

    /// Submits a transaction to the mempool
    ///
    /// # Arguments
    /// * `transaction` - The transaction created by create_transaction
    async fn send_transaction(&self, transaction: LockedTransaction) -> Result<Txid, Error> {
        log::info!("Sending bitcoin to {}", transaction.recipient);

        // place the transaction into the mempool, this is fine to retry
        let txid = self
            .with_wallet(|| async { Ok(self.rpc.send_raw_transaction(&transaction.transaction)?) })
            .await?;

        #[cfg(feature = "regtest-manual-mining")]
        if self.auto_mine {
            log::debug!("Auto-mining!");

            self.rpc
                .generate_to_address(1, &self.rpc.get_new_address(None, Some(AddressType::Bech32))?)?;
        }

        Ok(txid)
    }

    #[cfg(feature = "regtest-manual-mining")]
    pub fn mine_block(&self) -> Result<BlockHash, Error> {
        Ok(self
            .rpc
            .generate_to_address(1, &self.rpc.get_new_address(None, Some(AddressType::Bech32))?)?[0])
    }

    async fn with_wallet<F, R, T>(&self, call: F) -> Result<T, Error>
    where
        F: Fn() -> R,
        R: Future<Output = Result<T, Error>>,
    {
        self.with_wallet_inner(true, call).await
    }

    /// Exactly like with_wallet, but with with opt-out of retrying wallet error
    async fn with_wallet_inner<F, R, T>(&self, retry_on_wallet_error: bool, call: F) -> Result<T, Error>
    where
        F: Fn() -> R,
        R: Future<Output = Result<T, Error>>,
    {
        let mut backoff = get_exponential_backoff();
        loop {
            let err = match call().await.map_err(Error::from) {
                Err(inner) if inner.is_wallet_not_found() => {
                    // wallet not loaded (e.g. daemon restarted)
                    self.create_or_load_wallet().await?;
                    inner
                }
                Err(inner) if retry_on_wallet_error && inner.is_wallet_error() => {
                    // fee estimation failed or other
                    inner
                }
                result => return result,
            };

            match backoff.next_backoff() {
                Some(wait) => {
                    // error occurred, sleep before retrying
                    log::warn!("{:?} - next retry in {:.3} s", err, wait.as_secs_f64());
                    tokio::time::sleep(wait).await;
                }
                None => break Err(Error::ConnectionRefused),
            }
        }
    }

    pub async fn wallet_has_public_key<P>(&self, public_key: P) -> Result<bool, Error>
    where
        P: Into<[u8; PUBLIC_KEY_SIZE]> + From<[u8; PUBLIC_KEY_SIZE]> + Clone + PartialEq + Send + Sync + 'static,
    {
        self.with_wallet(|| async {
            let address = Address::p2wpkh(&PublicKey::from_slice(&public_key.clone().into())?, self.network)
                .map_err(ConversionError::from)?;
            let address_info = self.rpc.get_address_info(&address)?;
            let wallet_pubkey = address_info.pubkey.ok_or(Error::MissingPublicKey)?;
            Ok(P::from(wallet_pubkey.key.serialize()) == public_key)
        })
        .await
    }

    pub async fn import_private_key(&self, privkey: PrivateKey) -> Result<(), Error> {
        self.with_wallet(|| async { Ok(self.rpc.import_private_key(&privkey, None, None)?) })
            .await
    }
}

/// true if the given indicates that the item was not found in the mempool
fn err_not_in_mempool(err: &bitcoincore_rpc::Error) -> bool {
    matches!(
        err,
        &bitcoincore_rpc::Error::JsonRpc(JsonRpcError::Rpc(RpcError {
            code: NOT_IN_MEMPOOL_ERROR_CODE,
            ..
        }))
    )
}

#[async_trait]
impl BitcoinCoreApi for BitcoinCore {
    fn network(&self) -> Network {
        self.network
    }

    /// Wait for a specified height to return a `BlockHash` or
    /// exit on error.
    ///
    /// # Arguments
    /// * `height` - block height to fetch
    /// * `num_confirmations` - minimum for a block to be accepted
    async fn wait_for_block(&self, height: u32, num_confirmations: u32) -> Result<Block, Error> {
        loop {
            match self.rpc.get_block_hash(height.into()) {
                Ok(hash) => {
                    let info = self.rpc.get_block_info(&hash)?;
                    if info.confirmations >= num_confirmations as i32 {
                        return Ok(self.rpc.get_block(&hash)?);
                    } else {
                        sleep(RETRY_DURATION).await;
                        continue;
                    }
                }
                Err(BitcoinError::JsonRpc(JsonRpcError::Rpc(err)))
                    if BitcoinRpcError::from(err.clone()) == BitcoinRpcError::RpcInvalidParameter =>
                {
                    // block does not exist yet
                    sleep(RETRY_DURATION).await;
                    continue;
                }
                Err(err) => return Err(err.into()),
            }
        }
    }

    /// Get the tip of the main chain as reported by Bitcoin core.
    async fn get_block_count(&self) -> Result<u64, Error> {
        Ok(self.rpc.get_block_count()?)
    }

    /// Get wallet balance.
    fn get_balance(&self, min_confirmations: Option<u32>) -> Result<Amount, Error> {
        Ok(self
            .rpc
            .get_balance(min_confirmations.map(|x| x.try_into().unwrap_or_default()), None)?)
    }

    /// List the transaction in the wallet. `max_count` sets a limit on the amount of transactions returned.
    /// If none is provided, [`DEFAULT_MAX_TX_COUNT`] is used, which is an arbitrarily picked big number to
    /// effectively return all transactions.
    fn list_transactions(&self, max_count: Option<usize>) -> Result<Vec<json::ListTransactionResult>, Error> {
        // If no `max_count` is specified to the rpc call, bitcoin core only returns 10 items.
        Ok(self
            .rpc
            .list_transactions(None, max_count.or(Some(DEFAULT_MAX_TX_COUNT)), None, None)?)
    }

    /// Get the raw transaction identified by `Txid` and stored
    /// in the specified block.
    ///
    /// # Arguments
    /// * `txid` - transaction ID
    /// * `block_hash` - hash of the block tx is stored in
    async fn get_raw_tx(&self, txid: &Txid, block_hash: &BlockHash) -> Result<Vec<u8>, Error> {
        Ok(serialize(&self.rpc.get_raw_transaction(txid, Some(block_hash))?))
    }

    /// Get the raw transaction identified by `Txid` and stored
    /// in the specified block.
    ///
    /// # Arguments
    /// * `txid` - transaction ID
    /// * `block_hash` - hash of the block tx is stored in
    async fn get_transaction(&self, txid: &Txid, block_hash: Option<BlockHash>) -> Result<Transaction, Error> {
        Ok(self.rpc.get_raw_transaction(txid, block_hash.as_ref())?)
    }

    /// Get the merkle proof which can be used to validate transaction inclusion.
    ///
    /// # Arguments
    /// * `txid` - transaction ID
    /// * `block_hash` - hash of the block tx is stored in
    async fn get_proof(&self, txid: Txid, block_hash: &BlockHash) -> Result<Vec<u8>, Error> {
        Ok(self.rpc.get_tx_out_proof(&[txid], Some(block_hash))?)
    }

    /// Get the block hash for a given height.
    ///
    /// # Arguments
    /// * `height` - block height
    async fn get_block_hash(&self, height: u32) -> Result<BlockHash, Error> {
        match self.rpc.get_block_hash(height.into()) {
            Ok(block_hash) => Ok(block_hash),
            Err(BitcoinError::JsonRpc(JsonRpcError::Rpc(err)))
                if BitcoinRpcError::from(err.clone()) == BitcoinRpcError::RpcInvalidParameter =>
            {
                // block does not exist yet
                Err(Error::InvalidBitcoinHeight)
            }
            Err(err) => return Err(err.into()),
        }
    }

    /// Gets a new address from the wallet
    async fn get_new_address(&self) -> Result<Address, Error> {
        Ok(self.rpc.get_new_address(None, Some(AddressType::Bech32))?)
    }

    /// Gets a new public key for an address in the wallet
    async fn get_new_public_key(&self) -> Result<PublicKey, Error> {
        let address = self
            .rpc
            .get_new_address(Some(DERIVATION_KEY_LABEL), Some(AddressType::Bech32))?;
        let address_info = self.rpc.get_address_info(&address)?;
        let public_key = address_info.pubkey.ok_or(Error::MissingPublicKey)?;
        Ok(public_key)
    }

    fn dump_derivation_key<P: Into<[u8; PUBLIC_KEY_SIZE]> + Send + Sync + 'static>(
        &self,
        public_key: P,
    ) -> Result<PrivateKey, Error> {
        let address = Address::p2wpkh(&PublicKey::from_slice(&public_key.into())?, self.network)
            .map_err(ConversionError::from)?;
        Ok(self.rpc.dump_private_key(&address)?)
    }

    fn import_derivation_key(&self, private_key: &PrivateKey) -> Result<(), Error> {
        Ok(self
            .rpc
            .import_private_key(private_key, Some(DERIVATION_KEY_LABEL), Some(false))?)
    }

    /// Derive and import the private key for the master public key and public secret
    async fn add_new_deposit_key<P: Into<[u8; PUBLIC_KEY_SIZE]> + Send + Sync + 'static>(
        &self,
        public_key: P,
        secret_key: Vec<u8>,
    ) -> Result<(), Error> {
        let address = Address::p2wpkh(&PublicKey::from_slice(&public_key.into())?, self.network)
            .map_err(ConversionError::from)?;
        let private_key = self.rpc.dump_private_key(&address)?;
        let deposit_secret_key =
            addr::calculate_deposit_secret_key(private_key.key, SecretKey::from_slice(&secret_key)?)?;
        self.rpc.import_private_key(
            &PrivateKey {
                compressed: private_key.compressed,
                network: self.network,
                key: deposit_secret_key,
            },
            Some(DEPOSIT_LABEL),
            // rescan true by default
            Some(false),
        )?;
        Ok(())
    }

    async fn get_best_block_hash(&self) -> Result<BlockHash, Error> {
        Ok(self.rpc.get_best_block_hash()?)
    }

    async fn get_pruned_height(&self) -> Result<u64, Error> {
        Ok(self.rpc.get_blockchain_info()?.prune_height.unwrap_or(0))
    }

    async fn get_block(&self, hash: &BlockHash) -> Result<Block, Error> {
        Ok(self.rpc.get_block(hash)?)
    }

    async fn get_block_header(&self, hash: &BlockHash) -> Result<BlockHeader, Error> {
        Ok(self.rpc.get_block_header(hash)?)
    }

    /// Get the transactions that are currently in the mempool. Since `impl trait` is not
    /// allowed within trait method, we have to use trait objects.
    async fn get_mempool_transactions<'a>(
        &'a self,
    ) -> Result<Box<dyn Iterator<Item = Result<Transaction, Error>> + Send + 'a>, Error> {
        // get txids from the mempool
        let txids = self.rpc.get_raw_mempool()?;
        // map txid to the actual Transaction structs
        let iterator = txids.into_iter().filter_map(move |txid| {
            match self.rpc.get_raw_transaction(&txid, None) {
                Ok(x) => Some(Ok(x)),
                Err(e) if err_not_in_mempool(&e) => None, // not in mempool anymore, so filter out
                Err(e) => Some(Err(e.into())),            // unknown error, propagate to user
            }
        });
        Ok(Box::new(iterator))
    }

    /// Waits for the required number of confirmations, and collects data about the
    /// transaction
    ///
    /// # Arguments
    /// * `txid` - transaction ID
    /// * `num_confirmations` - how many confirmations we need to wait for
    async fn wait_for_transaction_metadata(
        &self,
        txid: Txid,
        num_confirmations: u32,
    ) -> Result<TransactionMetadata, Error> {
        let (block_height, block_hash, fee) = retry(get_exponential_backoff(), || async {
            Ok(match self.rpc.get_transaction(&txid, None) {
                Ok(GetTransactionResult {
                    info:
                        WalletTxInfo {
                            confirmations,
                            blockhash: Some(hash),
                            blockheight: Some(height),
                            ..
                        },
                    fee,
                    ..
                }) if confirmations >= 0 && confirmations as u32 >= num_confirmations => Ok((height, hash, fee)),
                Ok(_) => Err(Error::ConfirmationError),
                Err(e) => Err(e.into()),
            }?)
        })
        .await?;

        let proof = retry(get_exponential_backoff(), || async {
            Ok(self.get_proof(txid, &block_hash).await?)
        })
        .await?;

        let raw_tx = retry(get_exponential_backoff(), || async {
            Ok(self.get_raw_tx(&txid, &block_hash).await?)
        })
        .await?;

        Ok(TransactionMetadata {
            txid,
            proof,
            raw_tx,
            block_height,
            block_hash,
            fee,
        })
    }

    async fn bump_fee(&self, txid: &Txid, address: Address, fee_rate: SatPerVbyte) -> Result<Txid, Error> {
        let (raw_tx, return_to_self_address) = self
            .with_wallet_inner(false, || async {
                let mut existing_transaction = self.rpc.get_raw_transaction(txid, None)?;

                let return_to_self = existing_transaction
                    .extract_return_to_self_address(&address.payload)?
                    .map(|(idx, payload)| {
                        existing_transaction.output.remove(idx);
                        Address {
                            payload,
                            network: self.network(),
                        }
                    });

                let raw_tx = serialize_hex(&existing_transaction);
                Ok((raw_tx, return_to_self))
            })
            .await?;

        let recipient = address.to_string();
        let tx = self
            .fund_and_sign_transaction(fee_rate, &raw_tx, &return_to_self_address, &recipient, false)
            .await?;

        let txid = self
            .with_wallet_inner(false, || async { Ok(self.rpc.send_raw_transaction(&tx.transaction)?) })
            .await?;

        #[cfg(feature = "regtest-manual-mining")]
        if self.auto_mine {
            log::debug!("Auto-mining!");

            self.rpc
                .generate_to_address(1, &self.rpc.get_new_address(None, Some(AddressType::Bech32))?)?;
        }

        Ok(txid)
    }

    /// Send an amount of Bitcoin to an address, but only submit the transaction
    /// to the mempool; this method does not wait until the block is included in
    /// the blockchain.
    ///
    /// # Arguments
    /// * `address` - Bitcoin address to fund
    /// * `sat` - number of Satoshis to transfer
    /// * `fee_rate` - fee rate in sat/vbyte
    /// * `request_id` - the issue/redeem/replace id for which this transfer is being made
    async fn create_and_send_transaction(
        &self,
        address: Address,
        sat: u64,
        fee_rate: SatPerVbyte,
        request_id: Option<H256>,
    ) -> Result<Txid, Error> {
        let tx = self.create_transaction(address, sat, fee_rate, request_id).await?;
        let txid = self.send_transaction(tx).await?;
        Ok(txid)
    }

    /// Send an amount of Bitcoin to an address and wait until it is included
    /// in the blockchain with the requested number of confirmations.
    ///
    /// # Arguments
    /// * `address` - Bitcoin address to fund
    /// * `sat` - number of Satoshis to transfer
    /// * `request_id` - the issue/redeem/replace id for which this transfer is being made
    /// * `fee_rate` - fee rate in sat/vbyte
    /// * `num_confirmations` - how many confirmations we need to wait for
    async fn send_to_address(
        &self,
        address: Address,
        sat: u64,
        request_id: Option<H256>,
        fee_rate: SatPerVbyte,
        num_confirmations: u32,
    ) -> Result<TransactionMetadata, Error> {
        let txid = self
            .create_and_send_transaction(address, sat, fee_rate, request_id)
            .await?;

        Ok(self.wait_for_transaction_metadata(txid, num_confirmations).await?)
    }

    /// Create or load a wallet on Bitcoin Core.
    async fn create_or_load_wallet(&self) -> Result<(), Error> {
        let wallet_name = if let Some(ref wallet_name) = self.wallet_name {
            wallet_name
        } else {
            return Err(Error::WalletNotFound);
        };

        // NOTE: bitcoincore-rpc does not expose listwalletdir
        if self.rpc.list_wallets()?.contains(wallet_name) || self.rpc.load_wallet(wallet_name).is_ok() {
            // wallet already loaded
            return Ok(());
        }
        // wallet does not exist, create
        self.rpc.create_wallet(wallet_name, None, None, None, None)?;
        Ok(())
    }

    async fn rescan_blockchain(&self, start_height: usize, end_height: usize) -> Result<(), Error> {
        self.rpc.rescan_blockchain(Some(start_height), Some(end_height))?;
        Ok(())
    }

    async fn rescan_electrs_for_addresses(&self, addresses: Vec<Address>) -> Result<(), Error> {
        for address in addresses.into_iter() {
            let address = address.to_string();
            let all_transactions = get_address_tx_history_full(&self.electrs_config.base_path, &address).await?;
            // filter to only import
            // a) payments in the blockchain (not in mempool), and
            // b) payments TO the address (as bitcoin core will already know about transactions spending FROM it)
            let confirmed_payments_to = all_transactions.into_iter().filter(|tx| {
                if let Some(status) = &tx.status {
                    if !status.confirmed {
                        return false;
                    }
                };
                tx.vout
                    .as_ref()
                    .unwrap_or(&vec![])
                    .iter()
                    .any(|output| matches!(&output.scriptpubkey_address, Some(addr) if addr == &address))
            });
            for transaction in confirmed_payments_to {
                let rawtx = get_tx_hex(&self.electrs_config.base_path, &transaction.txid).await?;
                let merkle_proof = get_tx_merkle_block_proof(&self.electrs_config.base_path, &transaction.txid).await?;
                self.rpc.call(
                    "importprunedfunds",
                    &[serde_json::to_value(rawtx)?, serde_json::to_value(merkle_proof)?],
                )?;
            }
        }
        Ok(())
    }

    /// Get the number of unspent transaction outputs.
    fn get_utxo_count(&self) -> Result<usize, Error> {
        Ok(self.rpc.list_unspent(None, None, None, None, None)?.len())
    }

    fn is_in_mempool(&self, txid: Txid) -> Result<bool, Error> {
        let get_tx_result = self.rpc.get_transaction(&txid, None)?;
        Ok(get_tx_result.info.confirmations == 0)
    }

    fn fee_rate(&self, txid: Txid) -> Result<SatPerVbyte, Error> {
        // unfortunately we need both of these rpc results. The result of the second call
        // is not a parsed tx, but rather a GetTransactionResult.
        let tx = self.rpc.get_raw_transaction(&txid, None)?;
        let get_tx_result = self.rpc.get_transaction(&txid, None)?;

        // to get from weight to vsize we divide by 4, but round up by first adding 3
        // Note that we can not rely on tx.get_size() since it doesn't 'discount' witness bytes
        let vsize = tx
            .get_weight()
            .checked_add(3)
            .ok_or(Error::ArithmeticError)?
            .checked_div(4)
            .ok_or(Error::ArithmeticError)?
            .try_into()?;

        let fee = get_tx_result
            .fee
            .ok_or(Error::MissingBitcoinFeeInfo)?
            .as_sat()
            .checked_abs()
            .ok_or(Error::ArithmeticError)?;

        log::debug!("fee: {fee}, size: {vsize}");

        let fee_rate = fee.checked_div(vsize).ok_or(Error::ArithmeticError)?;
        Ok(SatPerVbyte(fee_rate.try_into()?))
    }
}

/// Extension trait for transaction, adding methods to help to match the Transaction to Replace/Redeem requests
pub trait TransactionExt {
    fn get_op_return(&self) -> Option<H256>;
    fn get_op_return_bytes(&self) -> Option<[u8; 34]>;
    fn get_payment_amount_to(&self, dest: Payload) -> Option<u64>;
    fn extract_output_addresses(&self) -> Vec<Payload>;
    fn extract_indexed_output_addresses(&self) -> Vec<(usize, Payload)>;
    fn extract_return_to_self_address(&self, destination: &Payload) -> Result<Option<(usize, Payload)>, Error>;
}

impl TransactionExt for Transaction {
    /// Extract the hash from the OP_RETURN uxto, if present
    fn get_op_return(&self) -> Option<H256> {
        self.get_op_return_bytes().map(|x| H256::from_slice(&x[2..]))
    }

    /// Extract the bytes of the OP_RETURN uxto, if present
    fn get_op_return_bytes(&self) -> Option<[u8; 34]> {
        // we only consider the first three items because the parachain only checks the first 3 positions
        self.output.iter().take(3).find_map(|x| {
            // check that the length is 34 bytes
            let arr: [u8; 34] = x.script_pubkey.to_bytes().as_slice().try_into().ok()?;
            // check that it starts with op_return (0x6a), then 32 as the length indicator
            match arr {
                [0x6a, 32, ..] => Some(arr),
                _ => None,
            }
        })
    }

    /// Get the amount of btc that self sent to `dest`, if any
    fn get_payment_amount_to(&self, dest: Payload) -> Option<u64> {
        self.output.iter().find_map(|uxto| {
            let payload = Payload::from_script(&uxto.script_pubkey)?;
            if payload == dest {
                Some(uxto.value)
            } else {
                None
            }
        })
    }

    /// return the addresses that are used as outputs with non-zero value in this transaction
    fn extract_output_addresses(&self) -> Vec<Payload> {
        self.extract_indexed_output_addresses()
            .into_iter()
            .map(|(_idx, val)| val)
            .collect()
    }

    /// return the addresses that are used as outputs with non-zero value in this transaction,
    /// together with their index
    fn extract_indexed_output_addresses(&self) -> Vec<(usize, Payload)> {
        self.output
            .iter()
            .enumerate()
            .filter(|(_, x)| x.value > 0)
            .filter_map(|(idx, tx_out)| Some((idx, Payload::from_script(&tx_out.script_pubkey)?)))
            .collect()
    }

    /// return index and address of the return-to-self (or None if it does not exist)
    fn extract_return_to_self_address(&self, destination: &Payload) -> Result<Option<(usize, Payload)>, Error> {
        let mut return_to_self_addresses = self
            .extract_indexed_output_addresses()
            .into_iter()
            .filter(|(_idx, x)| x != destination)
            .collect::<Vec<_>>();

        // register return-to-self address if it exists
        match return_to_self_addresses.len() {
            0 => Ok(None),                                     // no return-to-self
            1 => Ok(Some(return_to_self_addresses.remove(0))), // one return-to-self address
            _ => Err(Error::TooManyReturnToSelfAddresses),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use bitcoincore_rpc::bitcoin::hashes::hex::FromHex;

    async fn test_electrs(url: &str, script_hex: &str, expected_txid: &str) {
        let config = ElectrsConfiguration {
            base_path: url.to_owned(),
            ..Default::default()
        };

        let script_bytes = Vec::from_hex(script_hex).unwrap();
        let script_hash = bitcoincore_rpc::bitcoin::hashes::sha256::Hash::hash(&script_bytes);

        let txs = esplora_btc_api::apis::scripthash_api::get_txs_by_scripthash(&config, &hex::encode(script_hash))
            .await
            .unwrap();
        assert!(txs.iter().any(|tx| { &tx.txid == expected_txid }));
    }

    #[tokio::test(flavor = "multi_thread")]
    #[ignore] // disabled until mainnet electrs is up and running
    async fn test_find_esplora_mainnet() {
        let script_hex = "6a24aa21a9ed932d00baa7d428106db4f785d398d60d0b9c1369c38448717db4a8f36d2512e3";
        let expected_txid = "d734d56c70ee7ac67d31a22f4b9a781619c5cff1803942b52036cd7eab1692e7";
        test_electrs(ELECTRS_MAINNET_URL, script_hex, expected_txid).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_find_esplora_testnet() {
        let script_hex = "6a208b26f7cf49e1ad4d9f81d237933da8810644a85ac25b3c22a6a2324e1ba02efc";
        let expected_txid = "ec736ccba2cb7d1a97145a7e98d32f8eec362cd140e917ce40842a492f43b49b";
        test_electrs(ELECTRS_TESTNET_URL, script_hex, expected_txid).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_find_esplora_testnet2() {
        let script_hex = "6a4c5054325b0f43c54432b8df76322d225c9759359f73b283e108441862c2ee6fe4a021f6825bee72311ec0f53dd7197d0e325dca9a45aa3af296294b42c667b6db214a5174001fe7f40004001f7a07000b02";
        let expected_txid = "ddfaa4f63b9cbdf72299b91074fbff13b02816f2a29109b2fecfd912a7476807";
        test_electrs(ELECTRS_TESTNET_URL, script_hex, expected_txid).await;
    }

    #[test]
    fn test_op_return_hashing() {
        let raw = Vec::from_hex("6a208703723a787b0f989110b49fd5e1cf1c2571525d564bf384b5aa9e340c9ad8bd").unwrap();
        let script_hash = bitcoincore_rpc::bitcoin::hashes::sha256::Hash::hash(&raw);

        let expected = "6ed3928fdcf7375b9622746eb46f8e97a2832a0c43000e3d86774fecb74ee67e";
        let expected = bitcoincore_rpc::bitcoin::hashes::sha256::Hash::from_hex(expected).unwrap();

        assert_eq!(expected, script_hash);
    }
}
