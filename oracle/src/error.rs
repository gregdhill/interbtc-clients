#![allow(clippy::enum_variant_names)]

use reqwest::Error as ReqwestError;
use runtime::{Error as RuntimeError, SubxtError};
use std::num::ParseIntError;
use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
    #[error("Shutdown")]
    Shutdown,
    #[error("Invalid response")]
    InvalidResponse,
    #[error("Invalid exchange rate")]
    InvalidExchangeRate,
    #[error("Invalid fee estimate")]
    InvalidFeeEstimate,
    #[error("Invalid arguments. Either provide explicit exchange rates (e.g. KSM=1) or provide a coingecko url")]
    InvalidArguments,
    #[error("Invalid CoinGecko Id")]
    InvalidCoinGeckoId,

    #[error("ReqwestError: {0}")]
    ReqwestError(#[from] ReqwestError),
    #[error("RuntimeError: {0}")]
    RuntimeError(#[from] RuntimeError),
    #[error("SubxtError: {0}")]
    SubxtError(#[from] SubxtError),
    #[error("ParseIntError: {0}")]
    ParseIntError(#[from] ParseIntError),
}
