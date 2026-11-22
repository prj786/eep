//! An order-keeping parallel map on scoped threads (no runtime needed).

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Run `f` over `items` on `threads` threads; results keep the input order.
pub fn par_map<T: Sync, R: Send>(
    items: &[T],
    threads: usize,
    f: impl Fn(&T) -> R + Sync,
) -> Vec<R> {
    let next = AtomicUsize::new(0);
    let out: Mutex<Vec<(usize, R)>> = Mutex::new(Vec::with_capacity(items.len()));
    std::thread::scope(|s| {
        for _ in 0..threads.clamp(1, items.len().max(1)) {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(item) = items.get(i) else { break };
                    let r = f(item);
                    out.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push((i, r));
                }
            });
        }
    });
    let mut v = out
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    v.sort_by_key(|(i, _)| *i);
    v.into_iter().map(|(_, r)| r).collect()
}

#[cfg(test)]
mod tests {
    #[test]
    fn keeps_order() {
        let v: Vec<u32> = (0..100).collect();
        assert_eq!(
            super::par_map(&v, 8, |x| x * 2),
            (0..100).map(|x| x * 2).collect::<Vec<_>>()
        );
        assert!(super::par_map(&Vec::<u32>::new(), 8, |x| *x).is_empty());
    }
}
