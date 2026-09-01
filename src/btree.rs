/// A hand-written B+tree keyed on (Value, row_id), with linked leaves for
/// ordered range scans. No std BTreeMap — split/merge logic is ours.
///
/// Keys order by `Value::cmp_value`, tie-broken by row id, so duplicate values
/// on a non-unique index are all stored and all returned by range scans.
use crate::types::Value;
use std::cmp::Ordering;

pub const MAX_KEYS: usize = 64;

#[derive(Debug, Clone)]
struct Node {
    leaf: bool,
    keys: Vec<Value>,     // internal: separators; leaf: entry keys
    values: Vec<u64>,     // leaf: row ids aligned with keys
    children: Vec<usize>, // internal: child ids, len == keys.len()+1
    next: Option<usize>,  // leaf: next leaf for range scans
}

impl Node {
    fn new(leaf: bool) -> Node {
        Node {
            leaf,
            keys: Vec::new(),
            values: Vec::new(),
            children: Vec::new(),
            next: None,
        }
    }
}

fn cmp_entry(k: &Value, id: u64, key: &Value, key_id: u64) -> Ordering {
    match k.cmp_value(key) {
        Ordering::Equal => id.cmp(&key_id),
        other => other,
    }
}

#[derive(Debug, Clone)]
pub struct BTree {
    nodes: Vec<Node>,
    root: usize,
    pub size: usize,
}

impl Default for BTree {
    fn default() -> Self {
        let mut nodes = Vec::new();
        nodes.push(Node::new(true));
        BTree {
            nodes,
            root: 0,
            size: 0,
        }
    }
}

impl BTree {
    fn new_node(&mut self, leaf: bool) -> usize {
        let id = self.nodes.len();
        self.nodes.push(Node::new(leaf));
        id
    }

    pub fn is_empty(&self) -> bool {
        self.size == 0
    }

    /// Find the first leaf that can contain `value`.  Internal separators are
    /// the minimum key of their right child, so equality must choose the
    /// child on the left; callers then walk the linked leaves to cover all
    /// duplicate keys.
    fn find_first_leaf(&self, value: &Value) -> usize {
        let mut node = self.root;
        while !self.nodes[node].leaf {
            let n = &self.nodes[node];
            let idx = n
                .keys
                .iter()
                .position(|k| k.cmp_value(value) != Ordering::Less)
                .unwrap_or(n.keys.len());
            node = n.children[idx];
        }
        node
    }

    fn leftmost_leaf(&self) -> usize {
        let mut node = self.root;
        while !self.nodes[node].leaf {
            node = self.nodes[node].children[0];
        }
        node
    }

    fn find_entry_leaf(&self, value: &Value, row_id: u64) -> Option<usize> {
        let mut leaf = self.find_first_leaf(value);
        loop {
            let node = &self.nodes[leaf];
            for (key, id) in node.keys.iter().zip(&node.values) {
                match cmp_entry(key, *id, value, row_id) {
                    Ordering::Less => {}
                    Ordering::Equal => return Some(leaf),
                    Ordering::Greater => return None,
                }
            }
            leaf = node.next?;
        }
    }

    /// Locate the leaf where a new `(value, row_id)` entry belongs.  The
    /// linked-leaf walk is important because internal separators contain the
    /// value but not the row-id tie breaker.
    fn insertion_leaf(&self, value: &Value, row_id: u64) -> usize {
        let mut leaf = self.find_first_leaf(value);
        loop {
            let node = &self.nodes[leaf];
            if node
                .keys
                .iter()
                .zip(&node.values)
                .any(|(key, id)| cmp_entry(key, *id, value, row_id) != Ordering::Less)
            {
                return leaf;
            }
            match node.next {
                Some(next) => leaf = next,
                None => return leaf,
            }
        }
    }

    fn path_to_leaf(&self, node: usize, target: usize, path: &mut Vec<usize>) -> bool {
        path.push(node);
        if node == target {
            return self.nodes[node].leaf;
        }
        if self.nodes[node].leaf {
            path.pop();
            return false;
        }
        for child in self.nodes[node].children.iter().copied() {
            if self.path_to_leaf(child, target, path) {
                return true;
            }
        }
        path.pop();
        false
    }

    fn leaf_pos(&self, leaf: usize, value: &Value, row_id: u64) -> Option<usize> {
        let n = &self.nodes[leaf];
        let mut lo = 0usize;
        let mut hi = n.keys.len();
        while lo < hi {
            let mid = (lo + hi) / 2;
            match cmp_entry(&n.keys[mid], n.values[mid], value, row_id) {
                Ordering::Less => lo = mid + 1,
                Ordering::Greater => hi = mid,
                Ordering::Equal => return Some(mid),
            }
        }
        None
    }

    pub fn contains(&self, value: &Value, row_id: u64) -> bool {
        if self.is_empty() {
            return false;
        }
        self.find_entry_leaf(value, row_id).is_some()
    }

    /// All row ids whose key compares equal to `value`, in order.
    pub fn lookup_eq(&self, value: &Value) -> Vec<u64> {
        let mut out = Vec::new();
        if self.is_empty() {
            return out;
        }
        let mut node = self.find_first_leaf(value);
        loop {
            let n = &self.nodes[node];
            for (k, v) in n.keys.iter().zip(n.values.iter()) {
                match k.cmp_value(value) {
                    Ordering::Greater => return out,
                    Ordering::Equal => out.push(*v),
                    Ordering::Less => {}
                }
            }
            match n.next {
                Some(next) => node = next,
                None => return out,
            }
        }
    }

    /// All row ids with value in [low, high] inclusive, in key order.
    pub fn range_scan(&self, low: &Value, high: &Value) -> Vec<u64> {
        let mut out = Vec::new();
        if self.is_empty() {
            return out;
        }
        if low.cmp_value(high) == Ordering::Greater {
            return out;
        }
        let mut node = self.find_first_leaf(low);
        loop {
            let n = &self.nodes[node];
            for (k, v) in n.keys.iter().zip(n.values.iter()) {
                if k.cmp_value(high) == Ordering::Greater {
                    return out;
                }
                if k.cmp_value(low) != Ordering::Less {
                    out.push(*v);
                }
            }
            match n.next {
                Some(next) => node = next,
                None => return out,
            }
        }
    }

    /// All (key, row_id) in order — full index scan.
    pub fn scan_all(&self) -> Vec<(Value, u64)> {
        let mut out = Vec::new();
        if self.is_empty() {
            return out;
        }
        let mut node = self.leftmost_leaf();
        loop {
            let n = &self.nodes[node];
            for i in 0..n.keys.len() {
                out.push((n.keys[i].clone(), n.values[i]));
            }
            match n.next {
                Some(next) => node = next,
                None => return out,
            }
        }
    }

    pub fn insert(&mut self, value: Value, row_id: u64) {
        if self.contains(&value, row_id) {
            return;
        }
        let target = self.insertion_leaf(&value, row_id);
        let mut path = Vec::new();
        assert!(self.path_to_leaf(self.root, target, &mut path));
        let split = self.insert_rec_at(&path, 0, value, row_id);
        self.size += 1;
        if let Some((median, right)) = split {
            let left = self.root;
            let new_root = self.new_node(false);
            self.nodes[new_root].keys.push(median);
            self.nodes[new_root].children.push(left);
            self.nodes[new_root].children.push(right);
            self.root = new_root;
        }
    }

    fn insert_rec_at(
        &mut self,
        path: &[usize],
        depth: usize,
        value: Value,
        row_id: u64,
    ) -> Option<(Value, usize)> {
        let node = path[depth];
        if self.nodes[node].leaf {
            let pos = self.nodes[node]
                .keys
                .iter()
                .zip(&self.nodes[node].values)
                .position(|(key, id)| cmp_entry(key, *id, &value, row_id) == Ordering::Greater)
                .unwrap_or(self.nodes[node].keys.len());
            self.nodes[node].keys.insert(pos, value);
            self.nodes[node].values.insert(pos, row_id);
            if self.nodes[node].keys.len() > MAX_KEYS {
                Some(self.split_node(node))
            } else {
                None
            }
        } else {
            let child = path[depth + 1];
            let child_index = self.nodes[node]
                .children
                .iter()
                .position(|candidate| *candidate == child)
                .expect("path child belongs to parent");
            if let Some((median, right)) = self.insert_rec_at(path, depth + 1, value, row_id) {
                let n = &mut self.nodes[node];
                n.keys.insert(child_index, median);
                n.children.insert(child_index + 1, right);
                if n.keys.len() > MAX_KEYS {
                    Some(self.split_node(node))
                } else {
                    None
                }
            } else {
                None
            }
        }
    }

    fn split_node(&mut self, node: usize) -> (Value, usize) {
        let mid = MAX_KEYS / 2;
        let right = self.new_node(self.nodes[node].leaf);
        if self.nodes[node].leaf {
            let median = self.nodes[node].keys[mid].clone();
            self.nodes[right].keys = self.nodes[node].keys.split_off(mid);
            self.nodes[right].values = self.nodes[node].values.split_off(mid);
            self.nodes[right].next = self.nodes[node].next;
            self.nodes[node].next = Some(right);
            (median, right)
        } else {
            let mut keys_right = self.nodes[node].keys.split_off(mid);
            let sep = keys_right.remove(0);
            self.nodes[right].keys = keys_right;
            self.nodes[right].children = self.nodes[node].children.split_off(mid + 1);
            (sep, right)
        }
    }

    /// Remove one entry; returns true if found.  Underflow is lazy: empty
    /// leaves remain in the linked structure until the tree itself becomes
    /// empty.  This keeps separator maintenance simple while preserving
    /// ordered scans and allowing those leaves to be reused by later inserts.
    pub fn delete(&mut self, value: &Value, row_id: u64) -> bool {
        if self.is_empty() {
            return false;
        }
        let Some(leaf) = self.find_entry_leaf(value, row_id) else {
            return false;
        };
        let pos = self
            .leaf_pos(leaf, value, row_id)
            .expect("located entry exists");
        self.nodes[leaf].keys.remove(pos);
        self.nodes[leaf].values.remove(pos);
        self.size = self.size.saturating_sub(1);
        if self.size == 0 {
            *self = Self::default();
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(n: i64) -> Value {
        Value::Integer(n)
    }

    #[test]
    fn insert_and_lookup() {
        let mut t = BTree::default();
        for i in 0..1000 {
            t.insert(v(i), i as u64);
        }
        assert_eq!(t.size, 1000);
        for i in (0..1000).rev() {
            assert!(t.contains(&v(i), i as u64));
            assert_eq!(t.lookup_eq(&v(i)), vec![i as u64]);
        }
    }

    #[test]
    fn range_scan_order() {
        let mut t = BTree::default();
        for i in 0..5000 {
            t.insert(v((i * 7919) % 5000), i as u64); // pseudo-randomized
        }
        let res = t.range_scan(&v(100), &v(200));
        assert_eq!(res.len(), 101);
        // keys [100,200] each appear once
    }

    #[test]
    fn duplicates_by_row_id() {
        let mut t = BTree::default();
        t.insert(v(5), 1);
        t.insert(v(5), 2);
        t.insert(v(5), 3);
        assert_eq!(t.lookup_eq(&v(5)), vec![1, 2, 3]);
        assert_eq!(t.size, 3);
        t.delete(&v(5), 2);
        assert_eq!(t.lookup_eq(&v(5)), vec![1, 3]);
        assert_eq!(t.size, 2);
    }

    #[test]
    fn duplicate_keys_survive_leaf_splits_and_out_of_order_ids() {
        let mut t = BTree::default();
        for row_id in (0..256u64).rev() {
            t.insert(v(5), row_id);
        }
        assert_eq!(t.lookup_eq(&v(5)), (0..256u64).collect::<Vec<_>>());
        assert!(t.contains(&v(5), 127));
        assert!(t.delete(&v(5), 127));
        assert!(!t.contains(&v(5), 127));
        assert_eq!(t.lookup_eq(&v(5)).len(), 255);
    }

    #[test]
    fn delete_leaves_and_reuses_tombstone_order() {
        let mut t = BTree::default();
        for row_id in 0..512u64 {
            t.insert(v(row_id as i64), row_id);
        }
        for row_id in 0..512u64 {
            assert!(t.delete(&v(row_id as i64), row_id));
        }
        for row_id in (0..128u64).rev() {
            t.insert(v(9), row_id);
        }
        assert_eq!(t.lookup_eq(&v(9)), (0..128u64).collect::<Vec<_>>());
    }

    #[test]
    fn delete_and_shrink() {
        let mut t = BTree::default();
        for i in 0..200 {
            t.insert(v(i), i as u64);
        }
        for i in 0..200 {
            assert!(t.delete(&v(i), i as u64));
        }
        assert!(t.is_empty());
        t.insert(v(1), 1);
        assert_eq!(t.lookup_eq(&v(1)), vec![1]);
    }

    #[test]
    fn scan_all_order() {
        let mut t = BTree::default();
        let mut expect = Vec::new();
        for i in 0..300 {
            t.insert(v(i), i as u64);
            expect.push((v(i), i as u64));
        }
        assert_eq!(t.scan_all(), expect);
    }

    #[test]
    fn text_and_null_keys() {
        let mut t = BTree::default();
        t.insert(Value::Text("b".into()), 1);
        t.insert(Value::Text("a".into()), 2);
        t.insert(Value::Null, 3);
        let all = t.scan_all();
        assert_eq!(all[0].1, 3); // NULL sorts lowest
        assert_eq!(all[1].1, 2);
        assert_eq!(all[2].1, 1);
    }

    #[test]
    fn randomized_delete_preserves_order_and_lookup() {
        let mut tree = BTree::default();
        for id in 0..2000u64 {
            let key = ((id * 1103515245 + 12345) % 10000) as i64;
            tree.insert(v(key), id);
        }
        let mut expected = tree.scan_all();
        for id in (0..2000u64).step_by(3) {
            let key = ((id * 1103515245 + 12345) % 10000) as i64;
            assert!(tree.delete(&v(key), id));
            expected.retain(|(_, row_id)| *row_id != id);
        }
        expected.sort_by(|left, right| cmp_entry(&left.0, left.1, &right.0, right.1));
        assert_eq!(tree.scan_all(), expected);
        for (key, id) in expected {
            assert!(tree.contains(&key, id));
        }
    }

    #[test]
    fn mixed_insert_delete_matches_sorted_model() {
        let mut tree = BTree::default();
        let mut model: Vec<(Value, u64)> = Vec::new();
        for step in 0..3000u64 {
            let row_id = (step * 37) % 701;
            let key = v(((step * 97) % 23) as i64);
            if step % 3 == 0 {
                let removed = tree.delete(&key, row_id);
                let before = model.len();
                model.retain(|entry| entry != &(key.clone(), row_id));
                assert_eq!(removed, model.len() != before);
            } else {
                tree.insert(key.clone(), row_id);
                if !model.contains(&(key, row_id)) {
                    model.push((v(((step * 97) % 23) as i64), row_id));
                }
            }
            model.sort_by(|left, right| cmp_entry(&left.0, left.1, &right.0, right.1));
            assert_eq!(tree.scan_all(), model);
        }
    }
}
