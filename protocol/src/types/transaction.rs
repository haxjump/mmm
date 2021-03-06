use bytes::Bytes;
use muta_codec_derive::RlpFixedCodec;
use serde::{Deserialize, Serialize};

use crate::fixed_codec::{FixedCodec, FixedCodecError};
use crate::types::primitive::{Address, Hash, JsonString};
use crate::ProtocolResult;

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct RawTransaction {
    pub chain_id: Hash,
    pub cycles_price: u64,
    pub cycles_limit: u64,
    pub nonce: Hash,
    pub request: TransactionRequest,
    pub timeout: u64,
    pub sender: Address,
}

#[derive(RlpFixedCodec, Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct TransactionRequest {
    pub method: String,
    pub service_name: String,
    pub payload: JsonString,
}

#[derive(RlpFixedCodec, Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct SignedTransaction {
    pub raw: RawTransaction,
    pub tx_hash: Hash,
    pub pubkey: Bytes,
    pub signature: Bytes,
}
