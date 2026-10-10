use std::{cell::Cell, num::NonZeroU64};

#[derive(Default)]
pub(crate) struct Memo(Cell<Option<(NonZeroU64, u64)>>);

impl Memo {
    pub(crate) fn get(&self, epoch: Option<NonZeroU64>) -> Option<u64> {
        match (epoch, self.0.get()) {
            (Some(current), Some((saved, source))) if current == saved => Some(source),
            _ => None,
        }
    }

    pub(crate) fn set(&self, epoch: Option<NonZeroU64>, source: Option<u64>) {
        self.0.set(epoch.zip(source));
    }
}

#[cfg(target_has_atomic = "64")]
fn allocate_epoch(counter: &std::sync::atomic::AtomicU64) -> Option<NonZeroU64> {
    use std::sync::atomic::Ordering;

    counter
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
            value.checked_add(1)
        })
        .ok()
        .and_then(NonZeroU64::new)
}

pub(crate) fn next_epoch() -> Option<NonZeroU64> {
    #[cfg(target_has_atomic = "64")]
    {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        allocate_epoch(&NEXT)
    }
    #[cfg(not(target_has_atomic = "64"))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positive_entries_require_the_same_epoch() {
        let memo = Memo::default();
        for epoch in 1..64 {
            let current = NonZeroU64::new(epoch);
            let foreign = NonZeroU64::new(epoch + 1);
            for source in [0, 1, 47, u64::MAX] {
                memo.set(current, Some(source));
                assert_eq!(memo.get(current), Some(source));
                assert_eq!(memo.get(foreign), None);
                assert_eq!(memo.get(None), None);
            }
        }
    }

    #[test]
    fn absence_and_disabled_epochs_do_not_cache_a_source() {
        let memo = Memo::default();
        let epoch = NonZeroU64::new(1);
        for (owner, source) in [(epoch, None), (None, Some(9)), (None, None)] {
            memo.set(epoch, Some(7));
            memo.set(owner, source);
            assert_eq!(memo.get(epoch), None);
        }
        memo.set(epoch, Some(11));
        assert_eq!(memo.get(epoch), Some(11));
    }

    #[test]
    fn memo_uses_two_scalar_words_without_drop() {
        assert_eq!(std::mem::size_of::<Memo>(), 16);
        assert!(!std::mem::needs_drop::<Memo>());
    }

    #[cfg(target_has_atomic = "64")]
    #[test]
    fn exhaustion_never_recycles_an_epoch() {
        use std::sync::atomic::{AtomicU64, Ordering};

        let counter = AtomicU64::new(u64::MAX - 1);
        assert_eq!(allocate_epoch(&counter), NonZeroU64::new(u64::MAX - 1));
        for _ in 0..16 {
            assert_eq!(allocate_epoch(&counter), None);
            assert_eq!(counter.load(Ordering::Relaxed), u64::MAX);
        }
        assert_ne!(next_epoch(), next_epoch());
    }

    #[cfg(target_has_atomic = "64")]
    #[test]
    fn concurrent_owners_get_distinct_epochs() {
        use std::{collections::BTreeSet, sync::atomic::AtomicU64, thread};

        let counter = AtomicU64::new(1);
        let epochs = thread::scope(|scope| {
            let workers: Vec<_> = (0..4)
                .map(|_| {
                    scope.spawn(|| {
                        (0..32)
                            .map(|_| allocate_epoch(&counter).unwrap())
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            workers
                .into_iter()
                .flat_map(|worker| worker.join().unwrap())
                .collect::<BTreeSet<_>>()
        });
        assert_eq!(epochs.len(), 128);
        assert_eq!(epochs.first().copied(), NonZeroU64::new(1));
        assert_eq!(epochs.last().copied(), NonZeroU64::new(128));
    }
}
