//! Preallocated SPSC transport with exclusive endpoints and wrapping cursors.
//! Only Copy values are accepted: no callback-side heap ownership or destructor work.
use crate::{AudioError, AudioResult};
use std::{
    cell::{Cell, UnsafeCell},
    marker::PhantomData,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};

struct Storage<T: Copy> {
    slots: Box<[UnsafeCell<T>]>,
    read: AtomicUsize,
    write: AtomicUsize,
}
// SAFETY: construction returns exactly one non-Clone producer/consumer. Endpoints
// are Send but !Sync, and mutation requires &mut. Release publishes initialized
// slots; Acquire ensures a producer never reuses a slot still being read.
unsafe impl<T: Copy + Send> Send for Storage<T> {}
unsafe impl<T: Copy + Send> Sync for Storage<T> {}

/// Sole producer. Moving ownership is allowed; concurrent writes are not.
pub struct Producer<T: Copy> {
    storage: Arc<Storage<T>>,
    _not_sync: PhantomData<Cell<()>>,
}
/// Sole consumer. Moving ownership is allowed; concurrent reads are not.
pub struct Consumer<T: Copy> {
    storage: Arc<Storage<T>>,
    _not_sync: PhantomData<Cell<()>>,
}

/// Allocates a power-of-two ring with caller-provided initial slot values.
/// Capacity is 1..=1048576 values; construction is worker-only.
pub fn bounded<T: Copy + Send>(
    capacity: usize,
    initial: T,
) -> AudioResult<(Producer<T>, Consumer<T>)> {
    if capacity == 0 || capacity > 1_048_576 || !capacity.is_power_of_two() {
        return Err(AudioError::InvalidConfig(
            "SPSC capacity must be a power of two in 1..=1048576".into(),
        ));
    }
    let storage = Arc::new(Storage {
        slots: std::iter::repeat_with(|| UnsafeCell::new(initial))
            .take(capacity)
            .collect(),
        read: AtomicUsize::new(0),
        write: AtomicUsize::new(0),
    });
    Ok((
        Producer {
            storage: Arc::clone(&storage),
            _not_sync: PhantomData,
        },
        Consumer {
            storage,
            _not_sync: PhantomData,
        },
    ))
}
impl<T: Copy> Producer<T> {
    /// Attempts one bounded write. Full rings retain queued data and reject the new value.
    pub fn push(&mut self, value: T) -> bool {
        let write = self.storage.write.load(Ordering::Relaxed);
        let read = self.storage.read.load(Ordering::Acquire);
        if write.wrapping_sub(read) >= self.storage.slots.len() {
            return false;
        }
        // SAFETY: this endpoint is the only producer and the slot is outside the
        // consumer's published range until the following Release store.
        unsafe {
            *self.storage.slots[write & (self.storage.slots.len() - 1)].get() = value;
        }
        self.storage
            .write
            .store(write.wrapping_add(1), Ordering::Release);
        true
    }
    /// Snapshot of queued values; suitable for diagnostics, not multi-producer admission.
    pub fn len(&self) -> usize {
        self.storage
            .write
            .load(Ordering::Relaxed)
            .wrapping_sub(self.storage.read.load(Ordering::Acquire))
            .min(self.storage.slots.len())
    }
    /// Reports whether this producer's queue snapshot is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// Returns the fixed ring capacity in values.
    pub fn capacity(&self) -> usize {
        self.storage.slots.len()
    }
}
impl<T: Copy> Consumer<T> {
    /// Copies one available value without allocation/waiting; empty returns None.
    pub fn pop(&mut self) -> Option<T> {
        let read = self.storage.read.load(Ordering::Relaxed);
        let write = self.storage.write.load(Ordering::Acquire);
        if read == write {
            return None;
        }
        // SAFETY: Acquire observed the producer's initialized value; producer cannot
        // reuse this slot until our subsequent Release advances the read cursor.
        let value = unsafe { *self.storage.slots[read & (self.storage.slots.len() - 1)].get() };
        self.storage
            .read
            .store(read.wrapping_add(1), Ordering::Release);
        Some(value)
    }
    /// Snapshot of queued values after publication.
    pub fn len(&self) -> usize {
        self.storage
            .write
            .load(Ordering::Acquire)
            .wrapping_sub(self.storage.read.load(Ordering::Relaxed))
            .min(self.storage.slots.len())
    }
    /// Reports whether no value is currently available.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn capacity_and_cursor_wrap_are_exact() {
        let (mut p, mut c) = bounded(4, 0_u32).unwrap();
        p.storage.read.store(usize::MAX - 1, Ordering::Relaxed);
        p.storage.write.store(usize::MAX - 1, Ordering::Relaxed);
        for i in 0..4 {
            assert!(p.push(i));
        }
        assert!(!p.push(99));
        for i in 0..4 {
            assert_eq!(c.pop(), Some(i));
        }
        assert_eq!(c.pop(), None);
    }
    #[test]
    fn exclusive_endpoints_transfer_between_workers() {
        let (mut p, mut c) = bounded(128, 0_u64).unwrap();
        let producer = std::thread::spawn(move || {
            for i in 0..100_000 {
                while !p.push(i) {
                    std::hint::spin_loop();
                }
            }
        });
        for i in 0..100_000 {
            let received = loop {
                if let Some(value) = c.pop() {
                    break value;
                }
                std::hint::spin_loop();
            };
            assert_eq!(received, i);
        }
        producer.join().unwrap();
    }
}
