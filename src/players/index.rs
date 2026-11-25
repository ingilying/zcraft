//! Sparse chunk-to-player spatial index for entity visibility and block watchers.
use super::Chunk;
use std::collections::{HashMap, HashSet, hash_map::Entry};
use uuid::Uuid;

#[derive(Default, Debug)]
pub(super) struct ChunkIndex(pub(super) HashMap<Chunk, HashSet<Uuid>>);

impl ChunkIndex {
    pub(super) fn insert(&mut self, chunk: Chunk, uuid: Uuid) {
        self.0.entry(chunk).or_default().insert(uuid);
    }

    pub(super) fn remove(&mut self, chunk: Chunk, uuid: Uuid) {
        if let Entry::Occupied(mut bucket) = self.0.entry(chunk) {
            bucket.get_mut().remove(&uuid);
            if bucket.get().is_empty() {
                bucket.remove();
            }
        }
    }

    pub(super) fn at(&self, chunk: Chunk) -> impl Iterator<Item = Uuid> + '_ {
        self.0.get(&chunk).into_iter().flatten().copied()
    }

    pub(super) fn take(&mut self, chunk: Chunk) -> impl Iterator<Item = Uuid> + use<> {
        self.0.remove(&chunk).into_iter().flatten()
    }
}
