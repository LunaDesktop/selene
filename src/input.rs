//! Magistrala wejścia: kolejka zdarzeń + para gniazd do wybudzania pętli głównej.
//!
//! Wątek wyjścia (osobne połączenie Wayland) odbiera zdarzenia wejścia od nadrzędnego
//! kompozytora i przekazuje je tutaj; wątek protokołu opróżnia kolejkę i trasuje je do
//! klientów (`wl_pointer`/`wl_keyboard`).

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};

use libluna_core::Event;

use crate::sync::LockExt;

/// Współdzielona kolejka zdarzeń wejścia.
pub type Events = Arc<Mutex<Vec<Event>>>;

/// Tworzy kolejkę i parę gniazd `(read, write)` do wybudzania.
pub fn channel() -> (Events, UnixStream, UnixStream) {
    let (read, write) = UnixStream::pair().expect("socketpair wejścia");
    (Arc::new(Mutex::new(Vec::new())), read, write)
}

/// Dokłada partię zdarzeń i budzi pętlę główną (jeden bajt).
pub fn push(events: &Events, wake: &mut UnixStream, batch: &[Event]) {
    if batch.is_empty() {
        return;
    }
    events.lock_or_recover().extend_from_slice(batch);
    let _ = wake.write_all(&[1u8]);
}

/// Opróżnia bajt wybudzający (nieblokująco).
pub fn drain_wake(wake: &mut UnixStream) {
    let _ = wake.set_nonblocking(true);
    let mut buf = [0u8; 64];
    let _ = wake.read(&mut buf);
    let _ = wake.set_nonblocking(false);
}
