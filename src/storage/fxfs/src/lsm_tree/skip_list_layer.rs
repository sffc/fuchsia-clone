// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// There are a great many optimisations that could be considered to improve performance and maybe
// memory usage.

use {
    crate::lsm_tree::{
        merge::{self, MergeFn},
        types::{
            BoxedLayerIterator, Item, ItemRef, Key, Layer, LayerIterator, LayerIteratorMut,
            MutableLayer, OrdLowerBound, Value,
        },
    },
    anyhow::Error,
    async_trait::async_trait,
    futures::future::poll_fn,
    std::{
        cell::UnsafeCell,
        cmp::min,
        ops::Bound,
        sync::{
            atomic::{AtomicPtr, AtomicU32, Ordering},
            Arc,
        },
        task::{Poll, Waker},
    },
};

// Each skip list node contains a variable sized pointer list. The head pointers also exist in the
// form of a pointer list. Index 0 in the pointer list is the chain with the most elements i.e.
// contains every element in the list.
struct PointerList<K, V>(Box<[AtomicPtr<SkipListNode<K, V>>]>);

impl<K, V> PointerList<K, V> {
    fn new(count: usize) -> PointerList<K, V> {
        let mut pointers = Vec::new();
        for _ in 0..count {
            pointers.push(AtomicPtr::new(std::ptr::null_mut()));
        }
        PointerList(pointers.into_boxed_slice())
    }

    fn len(&self) -> usize {
        self.0.len()
    }

    // Extracts the pointer at the given index.
    fn get_mut<'a>(&self, index: usize) -> Option<&'a mut SkipListNode<K, V>> {
        unsafe { self.0[index].load(Ordering::SeqCst).as_mut() }
    }

    // Same as previous, but returns an immutable reference.
    fn get<'a>(&self, index: usize) -> Option<&'a SkipListNode<K, V>> {
        unsafe { self.0[index].load(Ordering::SeqCst).as_ref() }
    }

    // Sets the pointer at the given index.
    fn set(&self, index: usize, node: Option<&SkipListNode<K, V>>) {
        self.0[index].store(
            match node {
                None => std::ptr::null_mut(),
                Some(node) => {
                    // https://github.com/rust-lang/rust/issues/66136#issuecomment-550003651
                    // suggests that the following is the best way to cast from const* to mut*.
                    unsafe {
                        (&*(node as *const SkipListNode<K, V>
                            as *const UnsafeCell<SkipListNode<K, V>>))
                            .get()
                    }
                }
            },
            Ordering::SeqCst,
        );
    }

    fn get_ptr(&self, index: usize) -> *mut SkipListNode<K, V> {
        self.0[index].load(Ordering::SeqCst)
    }
}

struct SkipListNode<K, V> {
    item: Item<K, V>,
    pointers: PointerList<K, V>,
}

pub struct SkipListLayer<K, V> {
    // These are the head pointers for the list.
    pointers: PointerList<K, V>,

    // The writer needs to synchronize with the readers and this is done by keeping track of read
    // counts.  We could, in theory, remove the mutex and make this atomic (and thus make reads
    // truly lock free) but it's simpler and easier to reason about with a mutex and what matters
    // most is that we avoid using a futures::lock::Mutex for readers because that can be blocked
    // for relatively long periods of time.
    read_counts: std::sync::Mutex<ReadCounts>,

    // Writes are locked using this lock.
    writer_lock: futures::lock::Mutex<()>,

    // The number of nodes that have been allocated.  This is only used for debugging purposes.
    allocated: AtomicU32,
}

struct ReadCounts {
    // Readers can be in one of two epochs.  After a write, the epoch changes and new readers will
    // be in a new epoch.  When all the old readers finish in the previous epoch, the write can
    // continue and memory can be freed.
    epoch: u8,

    // These are the counts of active readers for each epoch.
    counts: [u16; 2],

    // This is the waker for a writer.
    waker: Option<Waker>,
}

impl ReadCounts {
    fn new() -> Self {
        ReadCounts { epoch: 0, counts: [0, 0], waker: None }
    }
}

impl<K, V> SkipListLayer<K, V> {
    pub fn new(max_item_count: usize) -> Arc<SkipListLayer<K, V>> {
        Arc::new(SkipListLayer {
            pointers: PointerList::new((max_item_count as f32).log2() as usize + 1),
            read_counts: std::sync::Mutex::new(ReadCounts::new()),
            writer_lock: futures::lock::Mutex::new(()),
            allocated: AtomicU32::new(0),
        })
    }

    fn alloc_node(&self, item: Item<K, V>, pointer_count: usize) -> Box<SkipListNode<K, V>> {
        self.allocated.fetch_add(1, Ordering::Relaxed);
        Box::new(SkipListNode { item, pointers: PointerList::new(pointer_count) })
    }

    // Frees and then returns the next node in the chain.
    fn free_node(&self, node: &mut SkipListNode<K, V>) -> Option<&mut SkipListNode<K, V>> {
        self.allocated.fetch_sub(1, Ordering::Relaxed);
        unsafe { Box::from_raw(node).pointers.get_mut(0) }
    }
}

impl<K: Key, V: Value> SkipListLayer<K, V> {
    // Erases the given item. Does nothing if the item doesn't exist.
    pub async fn erase(&self, item: ItemRef<'_, K, V>)
    where
        K: std::cmp::Eq,
    {
        let mut iter = SkipListLayerIterMut::new(self, Bound::Included(&item.key)).await;
        if let Some(ItemRef { key, .. }) = iter.get() {
            if key == item.key {
                iter.erase();
                iter.commit().await;
            }
        }
    }
}

// We have to manually manage memory.
impl<K, V> Drop for SkipListLayer<K, V> {
    fn drop(&mut self) {
        let mut next = self.pointers.get_mut(0);
        while let Some(node) = next {
            next = self.free_node(node);
        }
        assert_eq!(self.allocated.load(Ordering::Relaxed), 0);
    }
}

#[async_trait]
impl<K: Key, V: Value> Layer<K, V> for SkipListLayer<K, V> {
    async fn seek<'a>(
        &'a self,
        bound: std::ops::Bound<&K>,
    ) -> Result<BoxedLayerIterator<'a, K, V>, Error> {
        Ok(Box::new(SkipListLayerIter::new(self, bound)))
    }
}

#[async_trait]
impl<K: Key + OrdLowerBound, V: Value> MutableLayer<K, V> for SkipListLayer<K, V> {
    fn as_layer(self: Arc<Self>) -> Arc<dyn Layer<K, V>> {
        self
    }

    // Inserts the given item.
    async fn insert(&self, item: Item<K, V>) {
        let mut iter = SkipListLayerIterMut::new(self, Bound::Included(&item.key)).await;
        iter.insert(item);
        iter.commit().await;
    }

    // Replaces or inserts the given item.
    async fn replace_or_insert(&self, item: Item<K, V>) {
        let mut iter = SkipListLayerIterMut::new(self, Bound::Included(&item.key)).await;
        if let Some(found_item) = iter.get() {
            if found_item.key == &item.key {
                iter.erase();
            }
        }
        iter.insert(item);
        iter.commit().await;
    }

    async fn merge_into(&self, item: Item<K, V>, lower_bound: &K, merge_fn: MergeFn<K, V>) {
        merge::merge_into(
            Box::new(SkipListLayerIterMut::new(self, Bound::Included(lower_bound)).await),
            item,
            merge_fn,
        )
        .await
        .unwrap();
    }
}

// -- SkipListLayerIter --

struct SkipListLayerIter<'a, K, V> {
    skip_list: &'a SkipListLayer<K, V>,

    // The epoch for the reader count.
    epoch: usize,

    // The current node.
    node: Option<&'a SkipListNode<K, V>>,
}

impl<'a, K: Ord, V> SkipListLayerIter<'a, K, V> {
    fn new(skip_list: &'a SkipListLayer<K, V>, bound: Bound<&K>) -> Self {
        let epoch = {
            let mut read_counts = skip_list.read_counts.lock().unwrap();
            let epoch = read_counts.epoch as usize;
            read_counts.counts[epoch] += 1;
            epoch
        };
        match bound {
            Bound::Unbounded => {
                SkipListLayerIter { skip_list, epoch, node: skip_list.pointers.get(0) }
            }
            Bound::Included(key) => {
                let mut index = skip_list.pointers.len() - 1;
                let mut last_pointers = &skip_list.pointers;
                loop {
                    // We could optimise for == if we could be bothered (via a different method
                    // maybe).
                    if let Some(node) = last_pointers.get(index) {
                        // Keep iterating along this level until we encounter a key that's >= our
                        // search key.
                        if &node.item.key < key {
                            last_pointers = &node.pointers;
                            continue;
                        }
                    }
                    // There are no more levels so we are done.
                    if index == 0 {
                        break SkipListLayerIter { skip_list, epoch, node: last_pointers.get(0) };
                    }
                    // Move to the next level.
                    index -= 1;
                }
            }
            Bound::Excluded(_) => panic!("Excluded bounds not supported"),
        }
    }
}

impl<K, V> Drop for SkipListLayerIter<'_, K, V> {
    fn drop(&mut self) {
        if self.epoch != usize::MAX {
            let mut read_counts = self.skip_list.read_counts.lock().unwrap();
            read_counts.counts[self.epoch] -= 1;
            if read_counts.counts[self.epoch] == 0 && read_counts.epoch != self.epoch as u8 {
                if let Some(waker) = read_counts.waker.take() {
                    waker.wake();
                }
            }
        }
    }
}

#[async_trait]
impl<K: Key, V: Value> LayerIterator<K, V> for SkipListLayerIter<'_, K, V> {
    async fn advance(&mut self) -> Result<(), Error> {
        match self.node {
            None => {}
            Some(node) => self.node = node.pointers.get(0),
        }
        Ok(())
    }

    fn get(&self) -> Option<ItemRef<'_, K, V>> {
        self.node.map(|node| node.item.as_item_ref())
    }
}

type PointerListRefArray<'a, K, V> = Box<[&'a PointerList<K, V>]>;

// -- SkipListLayerIterMut --

// This works by building an insertion chain.  When that chain is committed, it is done atomically
// so that readers are not interrupted.  When the existing readers are finished, it is then safe to
// release memory for any nodes that might have been erased.  In the case that we are only erasing
// elements, there will be no insertion chain, in which case we just atomically remove the elements
// from the chain.
struct SkipListLayerIterMut<'a, K, V> {
    skip_list: &'a SkipListLayer<K, V>,

    // Since this is a mutable iterator, we need to keep pointers to all the nodes that precede the
    // current position at every level, so that we can update them when inserting or erasing
    // elements.
    prev_pointers: PointerListRefArray<'a, K, V>,

    // When we first insert or erase an element, we take a copy of prev_pointers so that
    // we know which pointers need to be updated when we commit.
    insertion_point: Option<PointerListRefArray<'a, K, V>>,

    // These are the nodes that we should point to when we commit.
    insertion_nodes: PointerList<K, V>,

    // Only one write can proceed at a time.  We only need a place to keep the mutex guard, which is
    // why Rust thinks this is unused.
    #[allow(dead_code)]
    write_guard: futures::lock::MutexGuard<'a, ()>,
}

impl<K: Ord, V> SkipListLayerIterMut<'_, K, V> {
    async fn new<'a>(
        skip_list: &'a SkipListLayer<K, V>,
        bound: std::ops::Bound<&K>,
    ) -> SkipListLayerIterMut<'a, K, V> {
        let len = skip_list.pointers.len();
        let write_guard = skip_list.writer_lock.lock().await;

        // Start by setting all the previous pointers to the head.
        //
        // To understand how the previous pointers work, imagine the list looks something like the
        // following:
        //
        // 2  |--->|
        // 1  |--->|--|------->|
        // 0  |--->|--|--|--|->|
        //  HEAD   A  B  C  D  E  F
        //
        // Now imagine that the iterator is pointing at element D. In that case, the previous
        // pointers will point at C for index 0, B for index 1 and A for index 2. With that
        // information, it will be possible to insert an element immediately prior to D and
        // correctly update as many pointers as required (remember a new element will be given a
        // random number of levels).
        let mut prev_pointers = vec![&skip_list.pointers; len].into_boxed_slice();
        match bound {
            Bound::Unbounded => {}
            Bound::Included(key) => {
                let mut index = len - 1;
                let mut p = skip_list.pointers.get(index);
                let pointers = &mut prev_pointers;
                loop {
                    // We could optimise for == if we could be bothered (via a different method
                    // maybe).
                    if let Some(node) = p {
                        if &(node.item.key) < key {
                            pointers[index] = &node.pointers;
                            p = node.pointers.get(index);
                            continue;
                        }
                    }
                    // There are no more levels so we are done.
                    if index == 0 {
                        break;
                    }
                    // Move to the next level.
                    index -= 1;
                    pointers[index] = pointers[index + 1];
                    p = pointers[index].get(index);
                }
            }
            Bound::Excluded(_) => panic!("Excluded bounds not supported"),
        }
        SkipListLayerIterMut {
            skip_list,
            prev_pointers,
            insertion_point: None,
            insertion_nodes: PointerList::new(len),
            write_guard,
        }
    }
}

impl<K, V> Drop for SkipListLayerIterMut<'_, K, V> {
    fn drop(&mut self) {
        assert!(self.insertion_point.is_none());
    }
}

#[async_trait]
impl<K: Key + Clone, V: Value + Clone> LayerIterator<K, V> for SkipListLayerIterMut<'_, K, V> {
    async fn advance(&mut self) -> Result<(), Error> {
        if self.insertion_point.is_some() {
            if let Some(item) = self.get() {
                // Copy the current item into the insertion chain.
                let copy = item.cloned();
                self.insert(copy);
                self.erase();
            }
        } else {
            let pointers = &mut self.prev_pointers;
            if let Some(next) = pointers[0].get_mut(0) {
                for i in 0..next.pointers.len() {
                    pointers[i] = &next.pointers;
                }
            }
        }
        Ok(())
    }

    fn get(&self) -> Option<ItemRef<'_, K, V>> {
        self.prev_pointers[0].get(0).map(|node| node.item.as_item_ref())
    }
}

#[async_trait]
impl<K: Key + Clone, V: Value + Clone> LayerIteratorMut<K, V> for SkipListLayerIterMut<'_, K, V> {
    fn as_iterator_mut(&mut self) -> &mut dyn LayerIterator<K, V> {
        self
    }
    fn as_iterator(&self) -> &dyn LayerIterator<K, V> {
        self
    }

    fn insert(&mut self, item: Item<K, V>) {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        let max_pointers = self.skip_list.pointers.len();
        // This chooses a random number of pointers such that each level has half the number of
        // pointers of the previous one.
        let pointer_count = max_pointers
            - min(
                (rng.gen_range(0, 2u32.pow(max_pointers as u32) - 1) as f32).log2() as usize,
                max_pointers - 1,
            );
        let node = Box::leak(self.skip_list.alloc_node(item, pointer_count));
        if self.insertion_point.is_none() {
            self.insertion_point = Some(self.prev_pointers.clone());
        }
        for i in 0..pointer_count {
            let pointers = self.prev_pointers[i];
            node.pointers.set(i, pointers.get(i));
            if self.insertion_nodes.get(i).is_none() {
                // If there's no insertion node at this level, record this node as the node to
                // switch in when we commit.
                self.insertion_nodes.set(i, Some(node));
            } else {
                // There's already an insertion node at this level which means that it's part of the
                // insertion chain, so we can just update the pointers now.
                pointers.set(i, Some(&node));
            }
            // The iterator should point at the node following the new node i.e. the existing node.
            self.prev_pointers[i] = &node.pointers;
        }
    }

    fn erase(&mut self) {
        let pointers = &mut self.prev_pointers;
        if let Some(next) = pointers[0].get_mut(0) {
            if self.insertion_point.is_none() {
                self.insertion_point = Some(pointers.clone());
            }
            if self.insertion_nodes.get(0).is_none() {
                // If there's no insertion node, then just update the iterator position to point to
                // the next node, and then when we commit, it'll get erased.
                pointers[0] = &next.pointers;
            } else {
                // There's an insertion node, so the current element must be part of the insertion
                // chain and so we can update the pointers immediately.  There will be another node
                // that isn't part of the insertion chain that will still point at this node, but it
                // will disappear when we commit.
                pointers[0].set(0, next.pointers.get(0));
            }
            // Fix up all the pointers except the bottom one. Readers will still find this node,
            // just not as efficiently.
            for i in 1..next.pointers.len() {
                pointers[i].set(i, next.pointers.get(i));
            }
        }
    }

    // Commit splices the new insertion chain into the list.  This *must* be called before the
    // iterator is dropped.  This will block waiting for existing readers to finish.
    async fn commit(&mut self) {
        let prev_pointers = match self.insertion_point.take() {
            Some(prev_pointers) => prev_pointers,
            None => return,
        };

        // Keep track of the first node that we might need to erase later.
        let mut maybe_erase = prev_pointers[0].get_mut(0);

        // If there are no insertion nodes, then it means that we're only erasing nodes.
        if self.insertion_nodes.get(0).is_none() {
            // Erase all elements between the insertion point and the current element. The
            // pointers for levels > 0 should already have been done, so it's only level 0 we
            // need to worry about.
            prev_pointers[0].set(0, self.prev_pointers[0].get(0));
        } else {
            // Switch the pointers over so that the insertion chain is spliced in.  This is safe
            // so long as the bottom pointer is done first because that guarantees the new nodes
            // will be found, just maybe not as efficiently.
            for i in 0..self.insertion_nodes.len() {
                if let Some(node) = self.insertion_nodes.get_mut(i) {
                    prev_pointers[i].set(i, Some(node));
                }
            }
        }

        // Switch the epoch so that we can track when existing readers have finished.
        let epoch = {
            let mut read_counts = self.skip_list.read_counts.lock().unwrap();
            let epoch = read_counts.epoch;
            read_counts.epoch = 1 - epoch;
            epoch
        } as usize;

        // Now we need to wait for the old readers to finish.
        poll_fn(|cx| {
            let mut read_counts = self.skip_list.read_counts.lock().unwrap();
            if read_counts.counts[epoch] == 0 {
                Poll::Ready(())
            } else {
                read_counts.waker = Some(cx.waker().clone());
                Poll::Pending
            }
        })
        .await;

        // Now we can free the memory for the erased items.
        let end = self.prev_pointers[0].get_ptr(0);
        loop {
            match maybe_erase {
                Some(node) if node as *const _ != end => {
                    maybe_erase = self.skip_list.free_node(node)
                }
                _ => break,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use {
        super::{SkipListLayer, SkipListLayerIterMut},
        crate::lsm_tree::{
            merge::{
                ItemOp::{Discard, Replace},
                MergeIterator, MergeResult,
            },
            types::{
                Item, ItemRef, Layer, LayerIterator, LayerIteratorMut, MutableLayer, OrdLowerBound,
            },
        },
        fuchsia_async as fasync,
        std::ops::Bound,
        std::time::{Duration, Instant},
    };

    #[derive(
        Clone, Eq, PartialEq, PartialOrd, Ord, Debug, serde::Serialize, serde::Deserialize,
    )]
    struct TestKey(i32);

    impl OrdLowerBound for TestKey {
        fn cmp_lower_bound(&self, other: &Self) -> std::cmp::Ordering {
            std::cmp::Ord::cmp(self, other)
        }
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_iteration() {
        // Insert two items and make sure we can iterate back in the correct order.
        let skip_list = SkipListLayer::new(100);
        let items = [Item::new(TestKey(1), 1), Item::new(TestKey(2), 2)];
        skip_list.insert(items[1].clone()).await;
        skip_list.insert(items[0].clone()).await;
        let mut iter = skip_list.seek(Bound::Unbounded).await.unwrap();
        let ItemRef { key, value } = iter.get().expect("missing item");
        assert_eq!((key, value), (&items[0].key, &items[0].value));
        iter.advance().await.unwrap();
        let ItemRef { key, value } = iter.get().expect("missing item");
        assert_eq!((key, value), (&items[1].key, &items[1].value));
        iter.advance().await.unwrap();
        assert!(iter.get().is_none());
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_seek_exact() {
        // Seek for an exact match.
        let skip_list = SkipListLayer::new(100);
        for i in (0..100).rev() {
            skip_list.insert(Item::new(TestKey(i), i)).await;
        }
        let mut iter = skip_list.seek(Bound::Included(&TestKey(57))).await.unwrap();
        let ItemRef { key, value } = iter.get().expect("missing item");
        assert_eq!((key, value), (&TestKey(57), &57));

        // And check the next item is correct.
        iter.advance().await.unwrap();
        let ItemRef { key, value } = iter.get().expect("missing item");
        assert_eq!((key, value), (&TestKey(58), &58));
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_seek_lower_bound() {
        // Seek for a non-exact match.
        let skip_list = SkipListLayer::new(100);
        for i in (0..100).rev() {
            skip_list.insert(Item::new(TestKey(i * 3), i * 3)).await;
        }
        let mut expected_index = 57 * 3;
        let mut iter = skip_list.seek(Bound::Included(&TestKey(expected_index - 1))).await.unwrap();
        let ItemRef { key, value } = iter.get().expect("missing item");
        assert_eq!((key, value), (&TestKey(expected_index), &expected_index));

        // And check the next item is correct.
        expected_index += 3;
        iter.advance().await.unwrap();
        let ItemRef { key, value } = iter.get().expect("missing item");
        assert_eq!((key, value), (&TestKey(expected_index), &expected_index));
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_replace_or_insert_replaces() {
        let skip_list = SkipListLayer::new(100);
        let items = [Item::new(TestKey(1), 1), Item::new(TestKey(2), 2)];
        skip_list.insert(items[1].clone()).await;
        skip_list.insert(items[0].clone()).await;
        let replacement_value = 3;
        skip_list.replace_or_insert(Item::new(items[1].key.clone(), replacement_value)).await;

        let mut iter = skip_list.seek(Bound::Unbounded).await.unwrap();
        let ItemRef { key, value } = iter.get().expect("missing item");
        assert_eq!((key, value), (&items[0].key, &items[0].value));
        iter.advance().await.unwrap();
        let ItemRef { key, value } = iter.get().expect("missing item");
        assert_eq!((key, value), (&items[1].key, &replacement_value));
        iter.advance().await.unwrap();
        assert!(iter.get().is_none());
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_replace_or_insert_inserts() {
        let skip_list = SkipListLayer::new(100);
        let items = [Item::new(TestKey(1), 1), Item::new(TestKey(2), 2), Item::new(TestKey(3), 3)];
        skip_list.insert(items[2].clone()).await;
        skip_list.insert(items[0].clone()).await;
        skip_list.replace_or_insert(items[1].clone()).await;

        let mut iter = skip_list.seek(Bound::Unbounded).await.unwrap();
        let ItemRef { key, value } = iter.get().expect("missing item");
        assert_eq!((key, value), (&items[0].key, &items[0].value));
        iter.advance().await.unwrap();
        let ItemRef { key, value } = iter.get().expect("missing item");
        assert_eq!((key, value), (&items[1].key, &items[1].value));
        iter.advance().await.unwrap();
        let ItemRef { key, value } = iter.get().expect("missing item");
        assert_eq!((key, value), (&items[2].key, &items[2].value));
        iter.advance().await.unwrap();
        assert!(iter.get().is_none());
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_erase() {
        let skip_list = SkipListLayer::new(100);
        let items = [Item::new(TestKey(1), 1), Item::new(TestKey(2), 2)];
        skip_list.insert(items[1].clone()).await;
        skip_list.insert(items[0].clone()).await;

        skip_list.erase(items[1].as_item_ref()).await;

        {
            let mut iter = skip_list.seek(Bound::Unbounded).await.unwrap();
            let ItemRef { key, value } = iter.get().expect("missing item");
            assert_eq!((key, value), (&items[0].key, &items[0].value));
            iter.advance().await.unwrap();
            assert!(iter.get().is_none());
        }

        skip_list.erase(items[0].as_item_ref()).await;

        {
            let iter = skip_list.seek(Bound::Unbounded).await.unwrap();
            assert!(iter.get().is_none());
        }
    }

    // This test ends up being flaky on CQ. It is left here as it might be useful in case
    // significant changes are made.
    #[fasync::run_singlethreaded(test)]
    #[ignore]
    async fn test_seek_is_log_n_complexity() {
        // Keep doubling up the number of items until it takes about 500ms to search and then go
        // back and measure something that should, in theory, take about half that time.
        let mut n = 100;
        let mut loops = 0;
        const TARGET_TIME: Duration = Duration::from_millis(500);
        let time = loop {
            let skip_list = SkipListLayer::new(n as usize);
            for i in 0..n {
                skip_list.insert(Item::new(TestKey(i), i)).await;
            }
            let start = Instant::now();
            for i in 0..n {
                skip_list.seek(Bound::Included(&TestKey(i))).await.unwrap();
            }
            let elapsed = Instant::now() - start;
            if elapsed > TARGET_TIME {
                break elapsed;
            }
            n *= 2;
            loops += 1;
        };

        let seek_count = n;
        n >>= loops / 2; // This should, in theory, result in 50% seek time.
        let skip_list = SkipListLayer::new(n as usize);
        for i in 0..n {
            skip_list.insert(Item::new(TestKey(i), i)).await;
        }
        let start = Instant::now();
        for i in 0..seek_count {
            skip_list.seek(Bound::Included(&TestKey(i))).await.unwrap();
        }
        let elapsed = Instant::now() - start;

        eprintln!(
            "{} items: {}ms, {} items: {}ms",
            seek_count,
            time.as_millis(),
            n,
            elapsed.as_millis()
        );

        // Experimental results show that typically we do a bit better than log(n), but here we just
        // check that the time we just measured is above 25% of the time we first measured, the
        // theory suggests it should be around 50%.
        assert!(elapsed * 4 > time);
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_large_number_of_items() {
        let item_count = 1000;
        let skip_list = SkipListLayer::new(1000);
        for i in 1..item_count {
            skip_list.insert(Item::new(TestKey(i), 1)).await;
        }
        let mut iter = skip_list.seek(Bound::Included(&TestKey(item_count - 10))).await.unwrap();
        for i in item_count - 10..item_count {
            assert_eq!(iter.get().expect("missing item").key, &TestKey(i));
            iter.advance().await.unwrap();
        }
        assert!(iter.get().is_none());
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_mutliple_readers_allowed() {
        let skip_list = SkipListLayer::new(100);
        let items = [Item::new(TestKey(1), 1), Item::new(TestKey(2), 2)];
        skip_list.insert(items[1].clone()).await;
        skip_list.insert(items[0].clone()).await;

        // Create the first iterator and check the first item.
        let mut iter = skip_list.seek(Bound::Unbounded).await.unwrap();
        let ItemRef { key, value } = iter.get().expect("missing item");
        assert_eq!((key, value), (&items[0].key, &items[0].value));

        // Create a second iterator and check the first item.
        let iter2 = skip_list.seek(Bound::Unbounded).await.unwrap();
        let ItemRef { key, value } = iter2.get().expect("missing item");
        assert_eq!((key, value), (&items[0].key, &items[0].value));

        // Now go back to the first iterator and check the second item.
        iter.advance().await.unwrap();
        let ItemRef { key, value } = iter.get().expect("missing item");
        assert_eq!((key, value), (&items[1].key, &items[1].value));
    }

    fn merge(
        left: &'_ MergeIterator<'_, TestKey, i32>,
        right: &'_ MergeIterator<'_, TestKey, i32>,
    ) -> MergeResult<TestKey, i32> {
        MergeResult::Other {
            emit: None,
            left: Replace(Item::new((*left.key()).clone(), *left.value() + *right.value())),
            right: Discard,
        }
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_merge_into() {
        let skip_list = SkipListLayer::new(100);
        skip_list.insert(Item::new(TestKey(1), 1)).await;

        skip_list.merge_into(Item::new(TestKey(2), 2), &TestKey(1), merge).await;

        let mut iter = skip_list.seek(Bound::Unbounded).await.unwrap();
        let ItemRef { key, value } = iter.get().expect("missing item");
        assert_eq!((key, value), (&TestKey(1), &3));
        iter.advance().await.unwrap();
        assert!(iter.get().is_none());
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_two_inserts() {
        let skip_list = SkipListLayer::new(100);
        let items = [Item::new(TestKey(1), 1), Item::new(TestKey(2), 2)];
        {
            let mut iter = SkipListLayerIterMut::new(&skip_list, std::ops::Bound::Unbounded).await;
            iter.insert(items[0].clone());
            iter.insert(items[1].clone());
            iter.commit().await;
        }

        let mut iter = skip_list.seek(Bound::Unbounded).await.unwrap();
        let ItemRef { key, value } = iter.get().expect("missing item");
        assert_eq!((key, value), (&items[0].key, &items[0].value));
        iter.advance().await.unwrap();
        let ItemRef { key, value } = iter.get().expect("missing item");
        assert_eq!((key, value), (&items[1].key, &items[1].value));
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_erase_after_insert() {
        let skip_list = SkipListLayer::new(100);
        let items = [Item::new(TestKey(1), 1), Item::new(TestKey(2), 2)];
        skip_list.insert(items[1].clone()).await;
        {
            let mut iter = SkipListLayerIterMut::new(&skip_list, std::ops::Bound::Unbounded).await;
            iter.insert(items[0].clone());
            iter.erase();
            iter.commit().await;
        }

        let mut iter = skip_list.seek(Bound::Unbounded).await.unwrap();
        let ItemRef { key, value } = iter.get().expect("missing item");
        assert_eq!((key, value), (&items[0].key, &items[0].value));
        iter.advance().await.unwrap();
        assert!(iter.get().is_none());
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_insert_after_erase() {
        let skip_list = SkipListLayer::new(100);
        let items = [Item::new(TestKey(1), 1), Item::new(TestKey(2), 2)];
        skip_list.insert(items[1].clone()).await;
        {
            let mut iter = SkipListLayerIterMut::new(&skip_list, std::ops::Bound::Unbounded).await;
            iter.erase();
            iter.insert(items[0].clone());
            iter.commit().await;
        }

        let mut iter = skip_list.seek(Bound::Unbounded).await.unwrap();
        let ItemRef { key, value } = iter.get().expect("missing item");
        assert_eq!((key, value), (&items[0].key, &items[0].value));
        iter.advance().await.unwrap();
        assert!(iter.get().is_none());
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_insert_erase_insert() {
        let skip_list = SkipListLayer::new(100);
        let items = [Item::new(TestKey(1), 1), Item::new(TestKey(2), 2), Item::new(TestKey(3), 3)];
        skip_list.insert(items[0].clone()).await;
        {
            let mut iter = SkipListLayerIterMut::new(&skip_list, std::ops::Bound::Unbounded).await;
            iter.insert(items[1].clone());
            iter.erase();
            iter.insert(items[2].clone());
            iter.commit().await;
        }

        let mut iter = skip_list.seek(Bound::Unbounded).await.unwrap();
        let ItemRef { key, value } = iter.get().expect("missing item");
        assert_eq!((key, value), (&items[1].key, &items[1].value));
        iter.advance().await.unwrap();
        let ItemRef { key, value } = iter.get().expect("missing item");
        assert_eq!((key, value), (&items[2].key, &items[2].value));
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_two_erase_erases() {
        let skip_list = SkipListLayer::new(100);
        let items = [Item::new(TestKey(1), 1), Item::new(TestKey(2), 2), Item::new(TestKey(3), 3)];
        skip_list.insert(items[0].clone()).await;
        skip_list.insert(items[1].clone()).await;
        skip_list.insert(items[2].clone()).await;
        {
            let mut iter = SkipListLayerIterMut::new(&skip_list, std::ops::Bound::Unbounded).await;
            iter.erase();
            iter.erase();
            iter.commit().await;
        }

        let mut iter = skip_list.seek(Bound::Unbounded).await.unwrap();
        let ItemRef { key, value } = iter.get().expect("missing item");
        assert_eq!((key, value), (&items[2].key, &items[2].value));
        iter.advance().await.unwrap();
        assert!(iter.get().is_none());
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_readers_not_blocked_by_writers() {
        let skip_list = SkipListLayer::new(100);
        let items = [Item::new(TestKey(1), 1), Item::new(TestKey(2), 2)];
        skip_list.insert(items[1].clone()).await;

        let mut iter = skip_list.seek(Bound::Unbounded).await.unwrap();
        let ItemRef { key, value } = iter.get().expect("missing item");
        assert_eq!((key, value), (&items[1].key, &items[1].value));

        let mut iter2 = skip_list.seek(Bound::Unbounded).await.unwrap();
        let ItemRef { key, value } = iter.get().expect("missing item");
        assert_eq!((key, value), (&items[1].key, &items[1].value));

        futures::join!(skip_list.insert(items[0].clone()), async {
            loop {
                let iter = skip_list.seek(Bound::Unbounded).await.unwrap();
                let ItemRef { key, .. } = iter.get().expect("missing item");
                if key == &items[0].key {
                    break;
                }
            }
            iter.advance().await.unwrap();
            assert!(iter.get().is_none());
            std::mem::drop(iter);
            iter2.advance().await.unwrap();
            assert!(iter2.get().is_none());
            std::mem::drop(iter2);
        });
    }

    #[fasync::run(20, test)]
    async fn test_many_readers_and_writers() {
        let skip_list = SkipListLayer::new(100);
        let mut tasks = Vec::new();
        for i in 0..10 {
            let skip_list_clone = skip_list.clone();
            tasks.push(fasync::Task::spawn(async move {
                for j in 0..10 {
                    skip_list_clone.insert(Item::new(TestKey(i * 100 + j), i)).await;
                }
            }));
        }
        for _ in 0..10 {
            let skip_list_clone = skip_list.clone();
            tasks.push(fasync::Task::spawn(async move {
                for _ in 0..300 {
                    let mut iter =
                        skip_list_clone.seek(Bound::Unbounded).await.expect("seek failed");
                    let mut last_item: Option<TestKey> = None;
                    while let Some(item) = iter.get() {
                        if let Some(last) = last_item {
                            assert!(item.key > &last);
                        }
                        last_item = Some(item.key.clone());
                        iter.advance().await.expect("advance failed");
                    }
                }
            }));
        }
        for task in &mut tasks {
            task.await;
        }
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_insert_advance_erase() {
        let skip_list = SkipListLayer::new(100);
        let items = [Item::new(TestKey(1), 1), Item::new(TestKey(2), 2), Item::new(TestKey(3), 3)];
        skip_list.insert(items[1].clone()).await;
        skip_list.insert(items[2].clone()).await;
        {
            let mut iter = SkipListLayerIterMut::new(&skip_list, std::ops::Bound::Unbounded).await;
            iter.insert(items[0].clone());
            iter.advance().await.expect("advance failed");
            iter.erase();
            iter.commit().await;
        }
        let mut iter = skip_list.seek(Bound::Unbounded).await.unwrap();
        let ItemRef { key, value } = iter.get().expect("missing item");
        assert_eq!((key, value), (&items[0].key, &items[0].value));
        iter.advance().await.unwrap();
        let ItemRef { key, value } = iter.get().expect("missing item");
        assert_eq!((key, value), (&items[1].key, &items[1].value));
        iter.advance().await.unwrap();
        assert!(iter.get().is_none());
    }
}
