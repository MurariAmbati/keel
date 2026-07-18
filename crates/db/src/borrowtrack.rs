//! Re-entrancy detector for `Database`'s interior mutability (D-PAGER-7 follow-up).
//!
//! D-PAGER-7 converted `Database`'s `RefCell` fields to `Mutex`, reached its stated
//! goal (`assert_sync::<Database<PageCache>>()` compiled), and then hung. The recorded
//! cause is that `RefCell` permits unlimited nested **shared** borrows while
//! `Mutex::lock` permits none, so a green `RefCell` suite proves only "no nested
//! `borrow_mut`" — strictly weaker than what the conversion needed.
//!
//! What that entry does *not* record is **which** paths re-enter. The hang was observed
//! but never localised, which is why the fix was scoped as an open-ended restructuring
//! rather than a bounded worklist.
//!
//! [`TrackedRefCell`] closes that gap empirically. It is a drop-in for `RefCell` that
//! keeps a per-thread depth count per cell instance and reports any borrow taken while
//! the same cell is already borrowed on the same thread — that is, exactly the
//! condition that deadlocks under `Mutex`, and (for a read nested inside a read racing
//! a queued writer) under `RwLock` too.
//!
//! It is behind the `borrowtrack` feature and costs nothing when the feature is off,
//! where [`DbCell`] is a plain `RefCell`. It is a diagnostic, not a fix: it turns
//! "the suite hangs somewhere" into a list of call sites to restructure.
//!
//! Run with:
//!
//! ```text
//! cargo test -p keel-db --features borrowtrack -- --nocapture
//! ```

#[cfg(not(feature = "borrowtrack"))]
pub type DbCell<T> = std::cell::RefCell<T>;

#[cfg(feature = "borrowtrack")]
pub type DbCell<T> = TrackedRefCell<T>;

/// Construct a cell, naming the field so re-entrancy findings identify it.
/// The label is ignored when the `borrowtrack` feature is off.
#[cfg(not(feature = "borrowtrack"))]
pub fn new_cell<T>(v: T, _label: &'static str) -> DbCell<T> {
    std::cell::RefCell::new(v)
}

/// Construct a tracked cell, naming the field so findings identify it.
#[cfg(feature = "borrowtrack")]
pub fn new_cell<T>(v: T, label: &'static str) -> DbCell<T> {
    TrackedRefCell::labelled(v, label)
}

#[cfg(feature = "borrowtrack")]
mod tracking {
    use std::backtrace::Backtrace;
    use std::cell::{Ref, RefCell, RefMut};
    use std::collections::{HashMap, HashSet};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    static NEXT_ID: AtomicUsize = AtomicUsize::new(0);

    thread_local! {
        static HELD: RefCell<HashMap<usize, u32>> = RefCell::new(HashMap::new());
    }

    static REPORTED: Mutex<Option<HashSet<String>>> = Mutex::new(None);

    /// Every distinct re-entrant borrow site seen so far, in discovery order.
    pub fn findings() -> Vec<String> {
        let g = REPORTED.lock().unwrap();
        g.as_ref()
            .map(|s| {
                let mut v: Vec<String> = s.iter().cloned().collect();
                v.sort();
                v
            })
            .unwrap_or_default()
    }

    fn note(label: &'static str, kind: &'static str, depth: u32) {
        let bt = Backtrace::force_capture().to_string();
        let site: Vec<&str> = bt
            .lines()
            .filter(|l| l.contains("keel_db") && !l.contains("borrowtrack"))
            .take(4)
            .collect();
        let key = format!(
            "{label} [{kind}, depth {depth}]\n        {}",
            site.join("\n        ")
        );
        let mut g = REPORTED.lock().unwrap();
        let set = g.get_or_insert_with(HashSet::new);
        if set.insert(key.clone()) {
            eprintln!("re-entrant borrow: {key}");
        }
    }

    /// A `RefCell` that reports borrows taken while the same cell is already borrowed
    /// on the same thread — the exact condition that deadlocks under `Mutex`.
    pub struct TrackedRefCell<T> {
        inner: RefCell<T>,
        id: usize,
        label: &'static str,
    }

    impl<T> TrackedRefCell<T> {
        /// Wrap a value. Use [`TrackedRefCell::labelled`] to name the field in reports.
        pub fn new(v: T) -> Self {
            Self::labelled(v, "<unlabelled>")
        }

        /// Wrap a value, naming the field so findings identify it.
        pub fn labelled(v: T, label: &'static str) -> Self {
            TrackedRefCell {
                inner: RefCell::new(v),
                id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
                label,
            }
        }

        fn enter(&self, kind: &'static str) {
            let depth = HELD.with(|h| {
                let mut m = h.borrow_mut();
                let d = m.entry(self.id).or_insert(0);
                let prev = *d;
                *d += 1;
                prev
            });
            if depth > 0 {
                note(self.label, kind, depth);
            }
        }

        fn leave(&self) {
            HELD.with(|h| {
                let mut m = h.borrow_mut();
                if let Some(d) = m.get_mut(&self.id) {
                    *d = d.saturating_sub(1);
                }
            });
        }

        /// Shared borrow, tracked.
        pub fn borrow(&self) -> TrackedRef<'_, T> {
            self.enter("shared");
            TrackedRef {
                guard: Some(self.inner.borrow()),
                owner: self,
            }
        }

        /// Exclusive borrow, tracked.
        pub fn borrow_mut(&self) -> TrackedRefMut<'_, T> {
            self.enter("exclusive");
            TrackedRefMut {
                guard: Some(self.inner.borrow_mut()),
                owner: self,
            }
        }
    }

    /// Guard for [`TrackedRefCell::borrow`].
    pub struct TrackedRef<'a, T> {
        guard: Option<Ref<'a, T>>,
        owner: &'a TrackedRefCell<T>,
    }

    impl<T> std::ops::Deref for TrackedRef<'_, T> {
        type Target = T;
        fn deref(&self) -> &T {
            self.guard.as_ref().unwrap()
        }
    }

    impl<T> Drop for TrackedRef<'_, T> {
        fn drop(&mut self) {
            self.guard.take();
            self.owner.leave();
        }
    }

    /// Guard for [`TrackedRefCell::borrow_mut`].
    pub struct TrackedRefMut<'a, T> {
        guard: Option<RefMut<'a, T>>,
        owner: &'a TrackedRefCell<T>,
    }

    impl<T> std::ops::Deref for TrackedRefMut<'_, T> {
        type Target = T;
        fn deref(&self) -> &T {
            self.guard.as_ref().unwrap()
        }
    }

    impl<T> std::ops::DerefMut for TrackedRefMut<'_, T> {
        fn deref_mut(&mut self) -> &mut T {
            self.guard.as_mut().unwrap()
        }
    }

    impl<T> Drop for TrackedRefMut<'_, T> {
        fn drop(&mut self) {
            self.guard.take();
            self.owner.leave();
        }
    }
}

#[cfg(feature = "borrowtrack")]
pub use tracking::{findings, TrackedRefCell};
