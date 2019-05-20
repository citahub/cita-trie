use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::codec::{DataType, NodeCodec};
use crate::db::{MemoryDB, DB};
use crate::errors::TrieError;
use crate::nibbles::Nibbles;
use crate::node::{BranchNode, ExtensionNode, HashNode, LeafNode, Node};

pub type TrieResult<T, C, D> = Result<T, TrieError<C, D>>;

pub trait Trie<C: NodeCodec, D: DB> {
    /// Returns the value for key stored in the trie.
    fn get(&self, key: &[u8]) -> TrieResult<Option<Vec<u8>>, C, D>;

    /// Checks that the key is present in the trie
    fn contains(&self, key: &[u8]) -> TrieResult<bool, C, D>;

    /// Inserts value into trie and modifies it if it exists
    fn insert(&mut self, key: &[u8], value: &[u8]) -> TrieResult<(), C, D>;

    /// Removes any existing value for key from the trie.
    fn remove(&mut self, key: &[u8]) -> TrieResult<bool, C, D>;

    /// Saves all the nodes in the db, clears the cache data, recalculates the root.
    /// Returns the root hash of the trie.
    fn root(&mut self) -> TrieResult<C::Hash, C, D>;

    /// Prove constructs a merkle proof for key. The result contains all encoded nodes
    /// on the path to the value at key. The value itself is also included in the last
    /// node and can be retrieved by verifying the proof.
    ///
    /// If the trie does not contain a value for key, the returned proof contains all
    /// nodes of the longest existing prefix of the key (at least the root node), ending
    /// with the node that proves the absence of the key.
    fn get_proof(&self, key: &[u8]) -> TrieResult<Vec<Vec<u8>>, C, D>;

    /// return value if key exists, None if key not exist, Error if proof is wrong
    fn verify_proof(
        &self,
        root_hash: C::Hash,
        key: &[u8],
        proof: Vec<Vec<u8>>,
    ) -> TrieResult<Option<Vec<u8>>, C, D>;
}

#[derive(Debug)]
pub struct PatriciaTrie<C, D>
where
    C: NodeCodec,
    D: DB,
{
    root: Node,
    db: Arc<D>,
    codec: C,

    root_hash: C::Hash,
    cache: RefCell<HashMap<C::Hash, Vec<u8>>>,

    passing_keys: HashSet<C::Hash>,
    gen_keys: RefCell<HashSet<C::Hash>>,
}

impl<C, D> Trie<C, D> for PatriciaTrie<C, D>
where
    C: NodeCodec,
    D: DB,
{
    fn get(&self, key: &[u8]) -> TrieResult<Option<Vec<u8>>, C, D> {
        self.get_at(&self.root, &Nibbles::from_raw(key, true))
    }

    fn contains(&self, key: &[u8]) -> TrieResult<bool, C, D> {
        Ok(self
            .get_at(&self.root, &Nibbles::from_raw(key, true))?
            .map_or(false, |_| true))
    }

    fn insert(&mut self, key: &[u8], value: &[u8]) -> TrieResult<(), C, D> {
        if value.is_empty() {
            self.remove(key)?;
            return Ok(());
        }
        let root = self.root.clone();
        self.root = self.insert_at(root, Nibbles::from_raw(key, true), value.to_vec())?;
        Ok(())
    }

    fn remove(&mut self, key: &[u8]) -> TrieResult<bool, C, D> {
        let (n, removed) = self.delete_at(self.root.clone(), &Nibbles::from_raw(key, true))?;
        self.root = n;
        Ok(removed)
    }

    fn root(&mut self) -> TrieResult<C::Hash, C, D> {
        self.commit()
    }

    fn get_proof(&self, key: &[u8]) -> TrieResult<Vec<Vec<u8>>, C, D> {
        let mut path = self.get_path_at(&self.root, &Nibbles::from_raw(key, true))?;
        match self.root {
            Node::Empty => {}
            _ => path.push(self.root.clone()),
        }
        Ok(path.iter().rev().map(|n| self.encode_node_raw(n)).collect())
    }

    fn verify_proof(
        &self,
        root_hash: C::Hash,
        key: &[u8],
        proof: Vec<Vec<u8>>,
    ) -> TrieResult<Option<Vec<u8>>, C, D> {
        let memdb = Arc::new(MemoryDB::new(true));
        for node_encoded in proof.iter() {
            let hash = self.codec.decode_hash(node_encoded, false);
            if hash == root_hash || node_encoded.len() >= C::HASH_LENGTH {
                memdb
                    .insert(hash.as_ref().to_vec(), node_encoded.to_vec())
                    .unwrap();
            }
        }
        let trie = PatriciaTrie::from(memdb, self.codec.clone(), &root_hash)
            .or(Err(TrieError::InvalidProof))?;
        let value = trie.get(key).or(Err(TrieError::InvalidProof))?;
        Ok(value)
    }
}

impl<C, D> PatriciaTrie<C, D>
where
    C: NodeCodec,
    D: DB,
{
    pub fn new(db: Arc<D>, codec: C) -> Self {
        let empty_root_hash = codec.decode_hash(&codec.encode_empty(), false);

        PatriciaTrie {
            root: Node::Empty,
            db,
            codec,

            root_hash: empty_root_hash.clone(),
            cache: RefCell::new(HashMap::new()),
            passing_keys: HashSet::new(),
            gen_keys: RefCell::new(HashSet::new()),
        }
    }

    pub fn from(db: Arc<D>, codec: C, root: &C::Hash) -> TrieResult<Self, C, D> {
        match db.get(root.as_ref()).map_err(TrieError::DB)? {
            Some(data) => {
                let mut trie = PatriciaTrie {
                    root: Node::Empty,
                    db,
                    codec,

                    root_hash: root.clone(),
                    cache: RefCell::new(HashMap::new()),
                    passing_keys: HashSet::new(),
                    gen_keys: RefCell::new(HashSet::new()),
                };

                trie.root = trie.decode_node(&data).map_err(TrieError::NodeCodec)?;
                Ok(trie)
            }
            None => Err(TrieError::InvalidStateRoot),
        }
    }

    fn get_at<'a>(&self, n: &'a Node, partial: &Nibbles) -> TrieResult<Option<Vec<u8>>, C, D> {
        match n {
            Node::Empty => Ok(None),
            Node::Leaf(ref leaf) => {
                if partial == leaf.get_key() {
                    Ok(Some(leaf.get_value().to_vec()))
                } else {
                    Ok(None)
                }
            }
            Node::Branch(ref branch) => {
                if partial.is_empty() || partial.at(0) == 16 {
                    Ok(branch.get_value().and_then(|v| Some(v.to_vec())))
                } else {
                    let index = partial.at(0) as usize;
                    let node = branch.at_children(index);
                    self.get_at(node, &partial.slice(1, partial.len()))
                }
            }
            Node::Extension(extension) => {
                let prefix = extension.get_prefix();
                let match_len = partial.common_prefix(prefix);
                if match_len == prefix.len() {
                    self.get_at(
                        extension.get_node(),
                        &partial.slice(match_len, partial.len()),
                    )
                } else {
                    Ok(None)
                }
            }
            Node::Hash(hash) => {
                let n = self.get_node_from_hash(hash.get_hash())?;
                self.get_at(&n, partial)
            }
        }
    }

    // Get nodes path along the key, only the nodes whose encode length is greater than
    // hash length are added.
    // For embedded nodes whose data are already contained in their parent node, we don't need to
    // add them in the path.
    // In the code below, we only add the nodes get by `get_node_from_hash`, because they contains
    // all data stored in db, including nodes whose encoded data is less than hash length.
    fn get_path_at(&self, n: &Node, partial: &Nibbles) -> TrieResult<Vec<Node>, C, D> {
        match n {
            Node::Empty | Node::Leaf(_) => Ok(vec![]),
            Node::Branch(ref branch) => {
                if partial.is_empty() || partial.at(0) == 16 {
                    Ok(vec![])
                } else {
                    let index = partial.at(0) as usize;
                    let node = branch.at_children(index);
                    let res = self.get_path_at(node, &partial.slice(1, partial.len()))?;
                    Ok(res)
                }
            }
            Node::Extension(extension) => {
                let prefix = extension.get_prefix();
                let match_len = partial.common_prefix(prefix);
                if match_len == prefix.len() {
                    let res = self.get_path_at(
                        extension.get_node(),
                        &partial.slice(match_len, partial.len()),
                    )?;
                    Ok(res)
                } else {
                    Ok(vec![])
                }
            }
            Node::Hash(hash) => {
                let n = self.get_node_from_hash(hash.get_hash())?;
                let mut rest = self.get_path_at(&n, partial)?;
                rest.push(n);
                Ok(rest)
            }
        }
    }

    fn delete_at(&mut self, n: Node, partial: &Nibbles) -> TrieResult<(Node, bool), C, D> {
        let (new_n, deleted) = match n {
            Node::Empty => Ok((Node::Empty, false)),
            Node::Leaf(leaf) => {
                if leaf.get_key() == partial {
                    Ok((Node::Empty, true))
                } else {
                    Ok((leaf.into_node(), false))
                }
            }
            Node::Branch(mut branch) => {
                if partial.at(0) == 16 {
                    branch.set_value(None);
                    Ok((branch.into_node(), true))
                } else {
                    let index = partial.at(0) as usize;
                    let node = branch.at_children(index);

                    let (new_n, deleted) =
                        self.delete_at(node.clone(), &partial.slice(1, partial.len()))?;
                    if deleted {
                        branch.insert(index, new_n);
                    }

                    Ok((branch.into_node(), deleted))
                }
            }
            Node::Extension(mut extension) => {
                let prefix = extension.get_prefix();
                let match_len = partial.common_prefix(prefix);
                if match_len == prefix.len() {
                    let (new_n, deleted) = self.delete_at(
                        extension.get_node().clone(),
                        &partial.slice(match_len, partial.len()),
                    )?;

                    if deleted {
                        extension.set_node(new_n);
                    }
                    Ok((extension.into_node(), deleted))
                } else {
                    Ok((extension.into_node(), false))
                }
            }
            Node::Hash(hash) => {
                self.passing_keys
                    .insert(self.codec.decode_hash(hash.get_hash(), true));
                self.delete_at(self.get_node_from_hash(hash.get_hash())?, partial)
            }
        }?;

        Ok((self.degenerate(new_n)?, deleted))
    }

    fn insert_at(&mut self, n: Node, partial: Nibbles, value: Vec<u8>) -> TrieResult<Node, C, D> {
        match n {
            Node::Empty => Ok(LeafNode::new(partial, value).into_node()),
            Node::Leaf(mut leaf) => {
                let old_partial = leaf.get_key();
                let match_index = partial.common_prefix(old_partial);
                if match_index == old_partial.len() {
                    // replace leaf value
                    leaf.value = value;
                    return Ok(leaf.into_node());
                }

                // create branch node
                let mut branch = BranchNode::new();
                let n =
                    LeafNode::new(partial.slice(match_index + 1, partial.len()), value).into_node();
                branch.insert(partial.at(match_index) as usize, n);

                let n = LeafNode::new(
                    old_partial.slice(match_index + 1, old_partial.len()),
                    leaf.get_value().to_vec(),
                )
                .into_node();
                branch.insert(old_partial.at(match_index) as usize, n);

                if match_index == 0 {
                    return Ok(branch.into_node());
                }

                // if include a common prefix
                Ok(
                    ExtensionNode::new(partial.slice(0, match_index), branch.into_node())
                        .into_node(),
                )
            }
            Node::Branch(mut branch) => {
                if partial.at(0) == 16 {
                    branch.set_value(Some(value));
                    Ok(branch.into_node())
                } else {
                    let index = partial.at(0) as usize;
                    let child = branch.child_mut(index);
                    let new_n =
                        self.insert_at(child.take(), partial.slice(1, partial.len()), value)?;
                    child.swap(new_n);
                    Ok(branch.into_node())
                }
            }
            Node::Extension(extension) => {
                let prefix = extension.prefix;
                let sub_node = extension.node;
                let match_index = partial.common_prefix(&prefix);

                if match_index == 0 {
                    let mut branch = BranchNode::new();
                    branch.insert(
                        prefix.at(0) as usize,
                        if prefix.len() == 1 {
                            *sub_node
                        } else {
                            ExtensionNode::new(prefix.slice(1, prefix.len()), *sub_node).into_node()
                        },
                    );
                    self.insert_at(branch.into_node(), partial, value)
                } else if match_index == prefix.len() {
                    let new_node = self.insert_at(
                        *sub_node,
                        partial.slice(match_index, partial.len()),
                        value,
                    )?;

                    Ok(ExtensionNode {
                        prefix,
                        node: Box::new(new_node),
                    }
                    .into_node())
                } else {
                    let new_ext = ExtensionNode {
                        prefix: prefix.slice(match_index, prefix.len()),
                        node: sub_node,
                    };

                    let new_n = self.insert_at(
                        new_ext.into_node(),
                        partial.slice(match_index, partial.len()),
                        value,
                    )?;

                    Ok(ExtensionNode {
                        prefix: prefix.slice(0, match_index),
                        node: Box::new(new_n),
                    }
                    .into_node())
                }
            }
            Node::Hash(hash) => {
                self.passing_keys
                    .insert(self.codec.decode_hash(hash.get_hash(), true));
                let n = self.get_node_from_hash(hash.get_hash())?;
                self.insert_at(n, partial, value)
            }
        }
    }

    fn degenerate(&mut self, n: Node) -> TrieResult<Node, C, D> {
        let new_n = match n {
            Node::Branch(branch) => {
                let mut used_indexs = vec![];
                for index in 0..16 {
                    match branch.at_children(index) {
                        Node::Empty => continue,
                        _ => used_indexs.push(index),
                    }
                }

                // if only a value node, transmute to leaf.
                if used_indexs.is_empty() && branch.get_value().is_some() {
                    let key = Nibbles::from_raw(&[], true);
                    LeafNode::new(key, branch.get_value().unwrap().to_vec()).into_node()

                // if only one node. make an extension.
                } else if used_indexs.len() == 1 && branch.get_value().is_none() {
                    let used_index = used_indexs[0];
                    let n = branch.at_children(used_index);

                    let new_node =
                        ExtensionNode::new(Nibbles::from_hex(vec![used_index as u8]), n.clone())
                            .into_node();
                    self.degenerate(new_node)?
                } else {
                    branch.into_node()
                }
            }
            Node::Extension(mut extension) => {
                let prefix = extension.get_prefix();

                match extension.get_node() {
                    Node::Extension(sub_ext) => {
                        let new_prefix = prefix.join(sub_ext.get_prefix());
                        let new_n =
                            ExtensionNode::new(new_prefix, sub_ext.get_node().clone()).into_node();
                        self.degenerate(new_n)?
                    }
                    Node::Leaf(leaf) => {
                        let new_prefix = prefix.join(leaf.get_key());
                        LeafNode::new(new_prefix, leaf.get_value().to_vec()).into_node()
                    }
                    // try again after recovering node from the db.
                    Node::Hash(hash) => {
                        self.passing_keys
                            .insert(self.codec.decode_hash(hash.get_hash(), true));
                        extension.set_node(self.get_node_from_hash(hash.get_hash())?);
                        self.degenerate(extension.into_node())?
                    }
                    _ => extension.into_node(),
                }
            }
            _ => n,
        };

        Ok(new_n)
    }

    fn commit(&mut self) -> TrieResult<C::Hash, C, D> {
        let encoded = self.encode_node(&self.root.clone());
        let root_hash = if encoded.len() < C::HASH_LENGTH {
            let hash = self.codec.decode_hash(&encoded, false);
            self.cache.borrow_mut().insert(hash.clone(), encoded);
            hash
        } else {
            self.codec.decode_hash(&encoded, true)
        };

        let mut keys = Vec::with_capacity(self.cache.borrow().len());
        let mut values = Vec::with_capacity(self.cache.borrow().len());
        for (k, v) in self.cache.borrow_mut().drain() {
            keys.push(k.as_ref().to_vec());
            values.push(v);
        }

        self.db.insert_batch(keys, values).map_err(TrieError::DB)?;

        let removed_keys: Vec<Vec<u8>> = self
            .passing_keys
            .iter()
            .filter(|h| !self.gen_keys.borrow().contains(&h))
            .map(|h| h.as_ref().to_vec())
            .collect();

        self.db.remove_batch(&removed_keys).map_err(TrieError::DB)?;

        self.root_hash = root_hash.clone();
        self.gen_keys.borrow_mut().clear();
        self.passing_keys.clear();
        self.root = self.get_node_from_hash(root_hash.as_ref())?;
        Ok(root_hash)
    }

    fn decode_node(&self, data: &[u8]) -> Result<Node, C::Error> {
        self.codec.decode(data, |dp| match dp {
            DataType::Empty => Ok(Node::Empty),
            DataType::Pair(key, value) => {
                let nibble = Nibbles::from_compact(key);
                if nibble.is_leaf() {
                    Ok(LeafNode::new(nibble, value.to_vec()).into_node())
                } else {
                    let n = self.try_decode_hash_node(value)?;
                    Ok(ExtensionNode::new(nibble, n).into_node())
                }
            }
            DataType::Values(values) => {
                let mut branch = BranchNode::new();
                for (index, item) in values.iter().enumerate().take(16) {
                    let n = self.try_decode_hash_node(item)?;
                    branch.insert(index, n);
                }

                if self.codec.encode_empty() == values[16] {
                    branch.set_value(None)
                } else {
                    branch.set_value(Some(values[16].to_vec()))
                }
                Ok(branch.into_node())
            }
            DataType::Hash(hash) => self.try_decode_hash_node(hash),
        })
    }

    fn encode_node(&self, n: &Node) -> Vec<u8> {
        // Returns the hash value directly to avoid double counting.
        if let Node::Hash(hash) = n {
            return hash.get_hash().to_vec();
        }
        let data = self.encode_node_raw(n);

        // Nodes smaller than 32 bytes are stored inside their parent,
        // Nodes equal to 32 bytes are returned directly
        if data.len() < C::HASH_LENGTH {
            data
        } else {
            let hash = self.codec.decode_hash(&data, false);
            self.cache.borrow_mut().insert(hash.clone(), data);

            self.gen_keys.borrow_mut().insert(hash.clone());
            Vec::from(hash.as_ref())
        }
    }

    /// encode node without final hash
    fn encode_node_raw(&self, n: &Node) -> Vec<u8> {
        match n {
            Node::Empty => self.codec.encode_empty(),
            Node::Leaf(ref leaf) => self.codec.encode_pair(
                &self.codec.encode_raw(&leaf.get_key().encode_compact()),
                &self.codec.encode_raw(leaf.get_value()),
            ),
            Node::Branch(branch) => {
                let mut values = vec![];
                for index in 0..16 {
                    let data = self.encode_node(branch.at_children(index));
                    if data.len() == C::HASH_LENGTH {
                        values.push(self.codec.encode_raw(&data));
                    } else {
                        values.push(data);
                    }
                }
                match branch.get_value() {
                    Some(v) => values.push(self.codec.encode_raw(v)),
                    None => values.push(self.codec.encode_empty()),
                }
                self.codec.encode_values(&values)
            }
            Node::Extension(extension) => {
                let key = self
                    .codec
                    .encode_raw(&extension.get_prefix().encode_compact());
                let value = self.encode_node(extension.get_node());

                let value = if value.len() == C::HASH_LENGTH {
                    self.codec.encode_raw(&value)
                } else {
                    value
                };
                self.codec.encode_pair(&key, &value)
            }
            Node::Hash(_hash) => unreachable!(),
        }
    }

    fn try_decode_hash_node(&self, data: &[u8]) -> Result<Node, C::Error> {
        if data.len() == C::HASH_LENGTH {
            Ok(HashNode::new(data).into_node())
        } else {
            self.decode_node(data)
        }
    }

    fn get_node_from_hash(&self, hash: &[u8]) -> TrieResult<Node, C, D> {
        match self.db.get(hash).map_err(TrieError::DB)? {
            Some(data) => self.decode_node(&data).map_err(TrieError::NodeCodec),
            None => Ok(Node::Empty),
        }
    }
}

#[cfg(test)]
mod tests {
    use rand::distributions::Alphanumeric;
    use rand::seq::SliceRandom;
    use rand::{thread_rng, Rng};
    use std::sync::Arc;

    use ethereum_types;

    use super::{PatriciaTrie, Trie};
    use crate::codec::{NodeCodec, RLPNodeCodec};
    use crate::db::{MemoryDB, DB};

    #[test]
    fn test_trie_insert() {
        let memdb = Arc::new(MemoryDB::new(true));
        let mut trie = PatriciaTrie::new(memdb, RLPNodeCodec::default());
        trie.insert(b"test", b"test").unwrap();
    }

    #[test]
    fn test_trie_get() {
        let memdb = Arc::new(MemoryDB::new(true));
        let mut trie = PatriciaTrie::new(memdb, RLPNodeCodec::default());
        trie.insert(b"test", b"test").unwrap();
        let v = trie.get(b"test").unwrap();

        assert_eq!(Some(b"test".to_vec()), v)
    }

    #[test]
    fn test_trie_random_insert() {
        let memdb = Arc::new(MemoryDB::new(true));
        let mut trie = PatriciaTrie::new(memdb, RLPNodeCodec::default());

        for _ in 0..1000 {
            let rand_str: String = thread_rng().sample_iter(&Alphanumeric).take(30).collect();
            let val = rand_str.as_bytes();
            trie.insert(val, val).unwrap();

            let v = trie.get(val).unwrap();
            assert_eq!(v.map(|v| v.to_vec()), Some(val.to_vec()));
        }
    }

    #[test]
    fn test_trie_contains() {
        let memdb = Arc::new(MemoryDB::new(true));
        let mut trie = PatriciaTrie::new(memdb, RLPNodeCodec::default());
        trie.insert(b"test", b"test").unwrap();
        assert_eq!(true, trie.contains(b"test").unwrap());
        assert_eq!(false, trie.contains(b"test2").unwrap());
    }

    #[test]
    fn test_trie_remove() {
        let memdb = Arc::new(MemoryDB::new(true));
        let mut trie = PatriciaTrie::new(memdb, RLPNodeCodec::default());
        trie.insert(b"test", b"test").unwrap();
        let removed = trie.remove(b"test").unwrap();
        assert_eq!(true, removed)
    }

    #[test]
    fn test_trie_random_remove() {
        let memdb = Arc::new(MemoryDB::new(true));
        let mut trie = PatriciaTrie::new(memdb, RLPNodeCodec::default());

        for _ in 0..1000 {
            let rand_str: String = thread_rng().sample_iter(&Alphanumeric).take(30).collect();
            let val = rand_str.as_bytes();
            trie.insert(val, val).unwrap();

            let removed = trie.remove(val).unwrap();
            assert_eq!(true, removed);
        }
    }

    #[test]
    fn test_trie_empty_commit() {
        let memdb = Arc::new(MemoryDB::new(true));
        let mut trie = PatriciaTrie::new(memdb, RLPNodeCodec::default());

        let codec = RLPNodeCodec::default();
        let empty_node_data = codec.decode_hash(&codec.encode_empty(), false);
        let root = trie.commit().unwrap();

        assert_eq!(hex::encode(root), hex::encode(empty_node_data))
    }

    #[test]
    fn test_trie_commit() {
        let memdb = Arc::new(MemoryDB::new(true));
        let mut trie = PatriciaTrie::new(memdb, RLPNodeCodec::default());
        trie.insert(b"test", b"test").unwrap();
        let root = trie.commit().unwrap();

        let codec = RLPNodeCodec::default();
        let empty_node_data = codec.decode_hash(&codec.encode_empty(), false);
        assert_ne!(hex::encode(root), hex::encode(empty_node_data))
    }

    #[test]
    fn test_trie_from_root() {
        let memdb = Arc::new(MemoryDB::new(true));
        let root = {
            let mut trie = PatriciaTrie::new(Arc::clone(&memdb), RLPNodeCodec::default());
            trie.insert(b"test", b"test").unwrap();
            trie.insert(b"test1", b"test").unwrap();
            trie.insert(b"test2", b"test").unwrap();
            trie.insert(b"test23", b"test").unwrap();
            trie.insert(b"test33", b"test").unwrap();
            trie.insert(b"test44", b"test").unwrap();
            trie.root().unwrap()
        };

        let mut trie =
            PatriciaTrie::from(Arc::clone(&memdb), RLPNodeCodec::default(), &root).unwrap();
        let v1 = trie.get(b"test33").unwrap();
        assert_eq!(Some(b"test".to_vec()), v1);
        let v2 = trie.get(b"test44").unwrap();
        assert_eq!(Some(b"test".to_vec()), v2);
        let root2 = trie.commit().unwrap();
        assert_eq!(hex::encode(root), hex::encode(root2));
    }

    #[test]
    fn test_trie_from_root_and_insert() {
        let memdb = Arc::new(MemoryDB::new(true));
        let root = {
            let mut trie = PatriciaTrie::new(Arc::clone(&memdb), RLPNodeCodec::default());
            trie.insert(b"test", b"test").unwrap();
            trie.insert(b"test1", b"test").unwrap();
            trie.insert(b"test2", b"test").unwrap();
            trie.insert(b"test23", b"test").unwrap();
            trie.insert(b"test33", b"test").unwrap();
            trie.insert(b"test44", b"test").unwrap();
            trie.commit().unwrap()
        };

        let mut trie =
            PatriciaTrie::from(Arc::clone(&memdb), RLPNodeCodec::default(), &root).unwrap();
        trie.insert(b"test55", b"test55").unwrap();
        trie.commit().unwrap();
        let v = trie.get(b"test55").unwrap();
        assert_eq!(Some(b"test55".to_vec()), v);
    }

    #[test]
    fn test_trie_from_root_and_delete() {
        let memdb = Arc::new(MemoryDB::new(true));
        let root = {
            let mut trie = PatriciaTrie::new(Arc::clone(&memdb), RLPNodeCodec::default());
            trie.insert(b"test", b"test").unwrap();
            trie.insert(b"test1", b"test").unwrap();
            trie.insert(b"test2", b"test").unwrap();
            trie.insert(b"test23", b"test").unwrap();
            trie.insert(b"test33", b"test").unwrap();
            trie.insert(b"test44", b"test").unwrap();
            trie.commit().unwrap()
        };

        let mut trie =
            PatriciaTrie::from(Arc::clone(&memdb), RLPNodeCodec::default(), &root).unwrap();
        let removed = trie.remove(b"test44").unwrap();
        assert_eq!(true, removed);
        let removed = trie.remove(b"test33").unwrap();
        assert_eq!(true, removed);
        let removed = trie.remove(b"test23").unwrap();
        assert_eq!(true, removed);
    }

    #[test]
    fn test_multiple_trie_roots() {
        let k0: ethereum_types::H256 = 0.into();
        let k1: ethereum_types::H256 = 1.into();
        let v: ethereum_types::H256 = 0x1234.into();

        let root1 = {
            let db = Arc::new(MemoryDB::new(true));
            let mut trie = PatriciaTrie::new(db, RLPNodeCodec::default());
            trie.insert(k0.as_ref(), v.as_bytes()).unwrap();
            trie.root().unwrap()
        };

        let root2 = {
            let db = Arc::new(MemoryDB::new(true));
            let mut trie = PatriciaTrie::new(db, RLPNodeCodec::default());
            trie.insert(k0.as_ref(), v.as_bytes()).unwrap();
            trie.insert(k1.as_ref(), v.as_bytes()).unwrap();
            trie.root().unwrap();
            trie.remove(k1.as_ref()).unwrap();
            trie.root().unwrap()
        };

        let root3 = {
            let db = Arc::new(MemoryDB::new(true));
            let mut t1 = PatriciaTrie::new(Arc::clone(&db), RLPNodeCodec::default());
            t1.insert(k0.as_ref(), v.as_bytes()).unwrap();
            t1.insert(k1.as_ref(), v.as_bytes()).unwrap();
            t1.root().unwrap();
            let root = t1.root().unwrap();
            let mut t2 =
                PatriciaTrie::from(Arc::clone(&db), RLPNodeCodec::default(), &root).unwrap();
            t2.remove(k1.as_ref()).unwrap();
            t2.root().unwrap()
        };

        assert_eq!(root1, root2);
        assert_eq!(root2, root3);
    }

    #[test]
    fn test_delete_stale_keys_with_random_insert_and_delete() {
        let memdb = Arc::new(MemoryDB::new(true));
        let mut trie = PatriciaTrie::new(memdb, RLPNodeCodec::default());

        let mut rng = rand::thread_rng();
        let mut keys = vec![];
        for _ in 0..100 {
            let random_bytes: Vec<u8> = (0..rng.gen_range(2, 30))
                .map(|_| rand::random::<u8>())
                .collect();
            trie.insert(&random_bytes, &random_bytes).unwrap();
            keys.push(random_bytes.clone());
        }
        trie.commit().unwrap();
        let slice = &mut keys;
        slice.shuffle(&mut rng);

        for key in slice.iter() {
            trie.remove(key).unwrap();
        }
        trie.commit().unwrap();

        let codec = RLPNodeCodec::default();
        let empty_node_key = codec.decode_hash(&codec.encode_empty(), false);
        let value = trie.db.get(empty_node_key.as_ref()).unwrap().unwrap();
        assert_eq!(value, codec.encode_empty())
    }

    #[test]
    fn insert_full_branch() {
        let memdb = Arc::new(MemoryDB::new(true));
        let mut trie = PatriciaTrie::new(memdb, RLPNodeCodec::default());

        trie.insert(b"test", b"test").unwrap();
        trie.insert(b"test1", b"test").unwrap();
        trie.insert(b"test2", b"test").unwrap();
        trie.insert(b"test23", b"test").unwrap();
        trie.insert(b"test33", b"test").unwrap();
        trie.insert(b"test44", b"test").unwrap();
        trie.root().unwrap();
        let v = trie.get(b"test").unwrap();
        assert_eq!(Some(b"test".to_vec()), v);
    }
}
