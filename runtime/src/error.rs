pub use jsonrpsee::core::Error as JsonRpseeError;

use crate::{
    metadata::{DispatchError, ErrorDetails},
    types::*,
    BTC_RELAY_MODULE, ISSUE_MODULE, REDEEM_MODULE, RELAY_MODULE,
};
use codec::Error as CodecError;
use jsonrpsee::{core::error::Error as RequestError, types::error::CallError};
use serde_json::Error as SerdeJsonError;
use std::{array::TryFromSliceError, io::Error as IoError, num::TryFromIntError};
use subxt::{sp_core::crypto::SecretStringError, BasicError};
use thiserror::Error;
use tokio::time::error::Elapsed;
use url::ParseError as UrlParseError;

pub type SubxtError = subxt::Error<DispatchError>;

#[derive(Error, Debug)]
pub enum Error {
    #[error("Could not get exchange rate info")]
    ExchangeRateInfo,
    #[error("Could not get issue id")]
    RequestIssueIDNotFound,
    #[error("Could not get redeem id")]
    RequestRedeemIDNotFound,
    #[error("Could not get replace id")]
    RequestReplaceIDNotFound,
    #[error("Could not get block")]
    BlockNotFound,
    #[error("Could not get vault")]
    VaultNotFound,
    #[error("Vault has been liquidated")]
    VaultLiquidated,
    #[error("Vault has stolen BTC")]
    VaultCommittedTheft,
    #[error("Channel closed unexpectedly")]
    ChannelClosed,
    #[error("Transaction is invalid")]
    InvalidTransaction,
    #[error("Request has timed out")]
    Timeout,
    #[error("Block is not in the relay main chain")]
    BlockNotInRelayMainChain,
    #[error("Invalid currency")]
    InvalidCurrency,
    #[error("Failed to parse keyring account")]
    KeyringAccountParsingError,
    #[error("Storage item not found")]
    StorageItemNotFound,
    #[error("Client does not support spec_version: expected {0}, got {1}")]
    InvalidSpecVersion(u32, u32),
    #[error("Failed to load credentials from file: {0}")]
    KeyLoadingFailure(#[from] KeyLoadingError),
    #[error("Error serializing: {0}")]
    Serialize(#[from] TryFromSliceError),
    #[error("Error converting: {0}")]
    Convert(#[from] TryFromIntError),
    #[error("Subxt basic error: {0}")]
    SubxtBasicError(#[from] BasicError),
    #[error("Subxt runtime error: {0}")]
    SubxtRuntimeError(#[from] SubxtError),
    #[error("Error decoding: {0}")]
    CodecError(#[from] CodecError),
    #[error("Error encoding json data: {0}")]
    SerdeJsonError(#[from] SerdeJsonError),
    #[error("Error getting json-rpsee data: {0}")]
    JsonRpseeError(#[from] JsonRpseeError),
    #[error("Timeout: {0}")]
    TimeElapsed(#[from] Elapsed),
    #[error("UrlParseError: {0}")]
    UrlParseError(#[from] UrlParseError),
}

impl Error {
    fn is_runtime_err(&self, pallet_name: &str, error_name: &str) -> bool {
        matches!(
            self,
            Error::SubxtRuntimeError(SubxtError::Runtime(runtime_error))
            if matches!(
                runtime_error.clone().inner().details(),
                Some(ErrorDetails {
                    pallet,
                    error,
                    ..
                })
                if pallet == pallet_name && error == error_name
            )
        )
    }

    pub fn is_duplicate_block(&self) -> bool {
        self.is_runtime_err(BTC_RELAY_MODULE, &format!("{:?}", BtcRelayPalletError::DuplicateBlock))
    }

    pub fn is_invalid_chain_id(&self) -> bool {
        self.is_runtime_err(BTC_RELAY_MODULE, &format!("{:?}", BtcRelayPalletError::InvalidChainID))
    }

    pub fn is_issue_completed(&self) -> bool {
        self.is_runtime_err(ISSUE_MODULE, &format!("{:?}", IssuePalletError::IssueCompleted))
    }

    pub fn is_valid_refund(&self) -> bool {
        self.is_runtime_err(RELAY_MODULE, &format!("{:?}", RelayPalletError::ValidRefundTransaction))
    }

    pub fn is_invalid_transaction(&self) -> bool {
        matches!(self,
            Error::SubxtRuntimeError(SubxtError::Rpc(RequestError::Call(CallError::Custom { code, message, .. })))
                if *code == POOL_INVALID_TX &&
                message == INVALID_TX_MESSAGE
        )
    }

    pub fn is_commit_period_expired(&self) -> bool {
        self.is_runtime_err(REDEEM_MODULE, &format!("{:?}", RedeemPalletError::CommitPeriodExpired))
    }

    pub fn is_rpc_disconnect_error(&self) -> bool {
        matches!(
            self,
            Error::SubxtRuntimeError(SubxtError::Rpc(JsonRpseeError::RestartNeeded(_)))
        )
    }

    pub fn is_rpc_error(&self) -> bool {
        matches!(self, Error::SubxtRuntimeError(SubxtError::Rpc(_)))
    }

    pub fn is_parachain_shutdown_error(&self) -> bool {
        matches!(
            self,
            Error::SubxtRuntimeError(SubxtError::Runtime(runtime_error))
            if matches!(runtime_error.clone().inner(), DispatchError::BadOrigin)
        )
    }
}

#[derive(Error, Debug)]
pub enum KeyLoadingError {
    #[error("Key not found in file")]
    KeyNotFound,
    #[error("Json parsing error: {0}")]
    JsonError(#[from] SerdeJsonError),
    #[error("Io error: {0}")]
    IoError(#[from] IoError),
    #[error("Invalid secret string: {0:?}")]
    SecretStringError(SecretStringError),
}

// https://github.com/paritytech/substrate/blob/e60597dff0aa7ffad623be2cc6edd94c7dc51edd/client/rpc-api/src/author/error.rs#L80
const BASE_ERROR: i32 = 1000;
const POOL_INVALID_TX: i32 = BASE_ERROR + 10;
const INVALID_TX_MESSAGE: &str = "Invalid Transaction";
