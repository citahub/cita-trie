use std::hash;

use crate::errors::RLPCodecError;
use rlp::{Prototype, Rlp, RlpStream};
use sha3::{Digest, Sha3_256};

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum DataType<'a> {
    Empty,
    Pair(&'a [u8], &'a [u8]),
    Values(&'a [Vec<u8>]),
    Hash(&'a [u8]),
}

pub trait NodeCodec: Sized + Clone {
    type Error: ::std::error::Error;

    const HASH_LENGTH: usize;

    type Hash: AsRef<[u8]>
        + AsMut<[u8]>
        + Default
        + PartialEq
        + Eq
        + hash::Hash
        + Send
        + Sync
        + Clone;

    fn decode<F, T>(&self, data: &[u8], f: F) -> Result<T, Self::Error>
    where
        F: Fn(DataType) -> Result<T, Self::Error>;

    fn encode_empty(&self) -> Vec<u8>;
    fn encode_pair(&self, key: &[u8], value: &[u8]) -> Vec<u8>;
    fn encode_values(&self, values: &[Vec<u8>]) -> Vec<u8>;
    fn encode_raw(&self, raw: &[u8]) -> Vec<u8>;

    fn decode_hash(&self, data: &[u8], is_hash: bool) -> Self::Hash;
}

#[derive(Default, Debug, Clone)]
pub struct RLPNodeCodec {}

impl NodeCodec for RLPNodeCodec {
    type Error = RLPCodecError;

    const HASH_LENGTH: usize = 32;

    type Hash = [u8; 32];

    fn decode<F, T>(&self, data: &[u8], f: F) -> Result<T, Self::Error>
    where
        F: Fn(DataType) -> Result<T, Self::Error>,
    {
        let r = Rlp::new(data);
        match r.prototype()? {
            Prototype::Data(0) => Ok(f(DataType::Empty)?),
            Prototype::List(2) => {
                let key = r.at(0)?.data()?;
                let rlp_data = r.at(1)?;
                // TODO: if “is_data == true”, the value of the leaf node
                // This is not a good implementation
                // the details of MPT should not be exposed to the user.
                let value = if rlp_data.is_data() {
                    rlp_data.data()?
                } else {
                    rlp_data.as_raw()
                };

                Ok(f(DataType::Pair(&key, &value))?)
            }
            Prototype::List(17) => {
                let mut values = vec![];
                for i in 0..16 {
                    values.push(r.at(i)?.as_raw().to_vec());
                }

                // The last element is a value node.
                let value_rlp = r.at(16)?;
                if value_rlp.is_empty() {
                    values.push(self.encode_empty());
                } else {
                    values.push(value_rlp.data()?.to_vec());
                }
                Ok(f(DataType::Values(&values))?)
            }
            Prototype::Data(Self::HASH_LENGTH) => Ok(f(DataType::Hash(r.data()?))?),
            _ => panic!("invalid data"),
        }
    }

    fn encode_empty(&self) -> Vec<u8> {
        let mut stream = RlpStream::new();
        stream.append_empty_data();
        stream.out()
    }

    fn encode_pair(&self, key: &[u8], value: &[u8]) -> Vec<u8> {
        let mut stream = RlpStream::new_list(2);
        stream.append_raw(key, 1);
        stream.append_raw(value, 1);
        stream.out()
    }

    fn encode_values(&self, values: &[Vec<u8>]) -> Vec<u8> {
        let mut stream = RlpStream::new_list(values.len());
        for data in values {
            stream.append_raw(data, 1);
        }
        stream.out()
    }

    fn encode_raw(&self, raw: &[u8]) -> Vec<u8> {
        let mut stream = RlpStream::new();
        stream.append(&raw);
        stream.out()
    }

    fn decode_hash(&self, data: &[u8], is_hash: bool) -> Self::Hash {
        let mut out = [0u8; Self::HASH_LENGTH];
        if is_hash {
            out.copy_from_slice(data);
        } else {
            out.copy_from_slice(&Sha3_256::digest(data));
        }
        out
    }
}
