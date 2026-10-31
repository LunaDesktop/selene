//! Blokady odporne na „zatrucie” po panice.
//!
//! Po panice w obsłudze klienta (patrz `catch_unwind` w `main`) mutex, którego
//! `MutexGuard` był wtedy żywy, zostaje „zatruty”. Standardowe `.lock().unwrap()`
//! panikowałoby przy każdym kolejnym użyciu i — mimo izolacji panik — skutecznie
//! psuto by daną ścieżkę. [`LockExt::lock_or_recover`] odzyskuje wartość mimo
//! zatrucia, więc jeden błędny klient nie unieruchamia kompozytora.

use std::sync::{Mutex, MutexGuard};

/// Rozszerzenie [`Mutex`] o blokadę odzyskującą wartość z zatrutego mutexa.
pub trait LockExt<T> {
    /// Blokuje mutex; przy zatruciu (po panice) zwraca wewnętrzną wartość bez paniki.
    fn lock_or_recover(&self) -> MutexGuard<'_, T>;
}

impl<T> LockExt<T> for Mutex<T> {
    fn lock_or_recover(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|poison| poison.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::panic::{catch_unwind, AssertUnwindSafe};

    #[test]
    fn recovers_value_from_poisoned_mutex() {
        let m = Mutex::new(0i32);
        // Zatruwamy mutex: panika przy żywym guardzie.
        let _ = catch_unwind(AssertUnwindSafe(|| {
            let _guard = m.lock().unwrap();
            panic!("boom");
        }));
        assert!(m.lock().is_err(), "mutex powinien być zatruty po panice");

        // Mimo zatrucia odzyskujemy wartość i możemy ją zmienić.
        *m.lock_or_recover() = 42;
        assert_eq!(*m.lock_or_recover(), 42);
    }
}
