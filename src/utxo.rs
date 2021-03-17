use std::io;
use bitcoin::consensus::encode::{self, Decodable, Encodable};
use bitcoin_utxo::utxo::UtxoState;
use bitcoin::{Script, BlockHeader, Transaction};

#[derive(Debug, Clone)]
pub struct FilterCoin {
    pub script: Script,
}

impl UtxoState for FilterCoin {
    fn new_utxo(_height: u32, _header: &BlockHeader, tx: &Transaction, vout: u32) -> Self {
        FilterCoin {
            script: tx.output[vout as usize].script_pubkey.clone(),
        }
    }
}

impl Encodable for FilterCoin {
    fn consensus_encode<W: io::Write>(&self, writer: W) -> Result<usize, io::Error> {
        let len = self.script.consensus_encode(writer)?;
        Ok(len)
    }
}
impl Decodable for FilterCoin {
    fn consensus_decode<D: io::Read>(mut d: D) -> Result<Self, encode::Error> {
        Ok(FilterCoin {
            script: Decodable::consensus_decode(&mut d)?,
        })
    }
}