extern crate test;

mod adapter;
mod storage;

use rand::random;

use protocol::traits::ServiceResponse;
use protocol::types::{
    Block, BlockHeader, Hash, Proof, RawTransaction, Receipt, ReceiptResponse,
    SignedTransaction, TransactionRequest,
};
use protocol::Bytes;

const ADDRESS_STR: &str = "muta14e0lmgck835vm2dfm0w3ckv6svmez8fdgdl705";

fn mock_signed_tx(tx_hash: Hash) -> SignedTransaction {
    let nonce = Hash::digest(Bytes::from("XXXX"));

    let request = TransactionRequest {
        service_name: "test".to_owned(),
        method: "test".to_owned(),
        payload: "test".to_owned(),
    };

    let raw = RawTransaction {
        chain_id: nonce.clone(),
        nonce,
        timeout: 10,
        cycles_limit: 10,
        cycles_price: 1,
        request,
        sender: ADDRESS_STR.parse().unwrap(),
    };

    SignedTransaction {
        raw,
        tx_hash,
        pubkey: Default::default(),
        signature: Default::default(),
    }
}

fn mock_receipt(tx_hash: Hash) -> Receipt {
    let nonce = Hash::digest(Bytes::from("XXXX"));

    let response = ReceiptResponse {
        service_name: "test".to_owned(),
        method: "test".to_owned(),
        response: ServiceResponse::<String> {
            code: 0,
            succeed_data: "ok".to_owned(),
            error_message: "".to_owned(),
        },
    };
    Receipt {
        state_root: nonce,
        height: 10,
        tx_hash,
        cycles_used: 10,
        events: vec![],
        response,
    }
}

fn mock_block(height: u64, block_hash: Hash) -> Block {
    let nonce = Hash::digest(Bytes::from("XXXX"));
    let header = BlockHeader {
        chain_id: nonce.clone(),
        height,
        exec_height: height - 1,
        prev_hash: nonce.clone(),
        timestamp: 1000,
        order_root: nonce.clone(),
        order_signed_transactions_hash: nonce.clone(),
        confirm_root: Vec::new(),
        state_root: nonce,
        receipt_root: Vec::new(),
        cycles_used: vec![999_999],
        proposer: ADDRESS_STR.parse().unwrap(),
        proof: mock_proof(block_hash),
        validator_version: 1,
        validators: Vec::new(),
    };

    Block {
        header,
        ordered_tx_hashes: Vec::new(),
    }
}

fn mock_proof(block_hash: Hash) -> Proof {
    Proof {
        height: 0,
        round: 0,
        block_hash,
        signature: Default::default(),
        bitmap: Default::default(),
    }
}

fn get_random_bytes(len: usize) -> Bytes {
    let vec: Vec<u8> = (0..len).map(|_| random::<u8>()).collect();
    Bytes::from(vec)
}
