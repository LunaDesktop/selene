//! Ochrona sesji przed sygnałami z wirtualnego terminala.
//!
//! W trybie DRM sesja działa na VT ustawionym przez menedżera logowania (u nas
//! `greetd`, `vt = 7`). Konsola jądra zostaje w trybie tekstowym, więc każde
//! naciśnięcie klawisza nadal przechodzi przez sterownik terminala. Ctrl+C wysyła
//! wtedy `SIGINT` do **grupy procesów pierwszego planu tego VT** — a w tej grupie
//! siedzi cała sesja: `greetd` (proces nadzorujący sesję), Selene, powłoka i
//! aplikacje. Gdy ginie proces nadzorujący, logind kończy sesję i wygląda to
//! dokładnie jak wylogowanie.
//!
//! Dlatego Selene:
//! - zakłada własną grupę procesów i przejmuje pierwszy plan VT (sygnały z
//!   klawiatury trafiają wtedy wyłącznie do niego, a on je ignoruje),
//! - instaluje handlery `SIGTERM`/`SIGHUP`, które logują **kto** zakończył sesję.
//!
//! Przy wyjściu pierwszy plan wraca do pierwotnej grupy (greetd), żeby menedżer
//! logowania dalej działał normalnie.

use std::os::fd::RawFd;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

/// Deskryptor VT (dup, żeby nie zależeć od `stdin`) albo -1.
static VT_FD: AtomicI32 = AtomicI32::new(-1);
/// Pierwotna grupa procesów pierwszego planu VT (przywracana przy wyjściu).
static ORIGINAL_PGRP: AtomicI32 = AtomicI32::new(0);
/// Czy udało się przejąć pierwszy plan (żeby nie przywracać czegoś, czego nie zmieniliśmy).
static CLAIMED: AtomicBool = AtomicBool::new(false);

const KD_GET_MODE: libc::c_ulong = 0x4B3B; // KDGETMODE

/// Przejmuje pierwszy plan VT dla procesu Selene.
///
/// Zwraca `true`, gdy udało się przełączyć grupę procesów. Wywołuj tylko w trybie
/// DRM — w trybie nested (Selene w oknie innego kompozytora) nie wolno ruszać
/// terminala, z którego uruchomił nas użytkownik.
pub fn claim_foreground() -> bool {
    let Some(fd) = vt_fd() else {
        if std::env::var_os("SELENE_STATS").is_some() {
            eprintln!(
                "selene/vt: brak dostępu do VT (XDG_VTNR={:?}, /dev/tty, stdin) — \
                 Ctrl+C na konsoli może zakończyć sesję",
                std::env::var("XDG_VTNR").ok()
            );
        }
        return false;
    };

    // Sprawdzamy, czy deskryptor naprawdę wskazuje na VT. `KDGETMODE` na zwykłym
    // pliku/gnieździe zwraca błąd — wtedy nie ruszamy terminala.
    let mut mode: libc::c_int = 0;
    if unsafe { libc::ioctl(fd, KD_GET_MODE, &mut mode) } < 0 {
        if std::env::var_os("SELENE_STATS").is_some() {
            eprintln!("selene/vt: deskryptor nie jest VT (KDGETMODE) — nie przejmuję planu");
        }
        unsafe { libc::close(fd) };
        return false;
    }
    // Tryb graficzny zostawiamy systemowi: przełącza go sam sterownik DRM.
    // Interesuje nas wyłącznie routing sygnałów, a nie wygaszanie konsoli —
    // dzięki temu nadal działa awaryjne Ctrl+Alt+F1…F12.
    let _ = mode;

    let original = unsafe { libc::tcgetpgrp(fd) };

    // Własna grupa procesów: Selene nie może dzielić grupy z greetd, bo wtedy
    // SIGINT z klawiatury zabija proces nadzorujący sesję.
    // `setpgid` może się nie udać, gdy jesteśmy liderem sesji (wtedy nie ma czego
    // izolować) — to nie jest błąd.
    unsafe {
        libc::setpgid(0, 0);
        // `tcsetpgrp` z procesu w tle wysyła SIGTTOU; ignorujemy go, jak robią to
        // powłoki z obsługą zadań.
        libc::signal(libc::SIGTTOU, libc::SIG_IGN);
    }
    let pid = unsafe { libc::getpid() };
    if unsafe { libc::tcsetpgrp(fd, pid) } < 0 {
        if std::env::var_os("SELENE_STATS").is_some() {
            let err = std::io::Error::last_os_error();
            eprintln!("selene/vt: tcsetpgrp nie powiodło się ({err}) — sesja bez ochrony");
        }
        unsafe { libc::close(fd) };
        return false;
    }

    VT_FD.store(fd, Ordering::Relaxed);
    ORIGINAL_PGRP.store(original, Ordering::Relaxed);
    CLAIMED.store(true, Ordering::Relaxed);

    if std::env::var_os("SELENE_STATS").is_some() {
        eprintln!(
            "selene/vt: przejąłem pierwszy plan VT (grupa {original} → {pid}); \
             sygnały z klawiatury konsoli nie zabiją już sesji"
        );
    }
    true
}

/// Przywraca pierwotną grupę procesów pierwszego planu (idempotentne).
pub fn restore() {
    if !CLAIMED.swap(false, Ordering::Relaxed) {
        return;
    }
    let fd = VT_FD.swap(-1, Ordering::Relaxed);
    if fd < 0 {
        return;
    }
    let original = ORIGINAL_PGRP.swap(0, Ordering::Relaxed);
    if original > 0 {
        unsafe {
            libc::tcsetpgrp(fd, original);
            libc::close(fd);
        }
    } else {
        unsafe { libc::close(fd) };
    }
}

/// Deskryptor terminala sesji: `/dev/tty<XDG_VTNR>`, potem `/dev/tty` (terminal
/// kontrolujący procesu), a na końcu `stdin`, o ile jest terminalem.
fn vt_fd() -> Option<RawFd> {
    if let Ok(vt) = std::env::var("XDG_VTNR") {
        if !vt.is_empty() {
            if let Ok(path) = std::ffi::CString::new(format!("/dev/tty{vt}")) {
                let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_NOCTTY) };
                if fd >= 0 {
                    return Some(fd);
                }
            }
        }
    }
    // `/dev/tty` to alias terminala kontrolującego sesji — otwiera się bez ACL
    // na węzeł VT (uprawnienia sprawdza tylko sam `/dev/tty`, 0666).
    if let Ok(path) = std::ffi::CString::new("/dev/tty") {
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_NOCTTY) };
        if fd >= 0 && unsafe { libc::isatty(fd) } == 1 {
            return Some(fd);
        }
        if fd >= 0 {
            unsafe { libc::close(fd) };
        }
    }
    if unsafe { libc::isatty(0) } == 1 {
        let fd = unsafe { libc::dup(0) };
        if fd >= 0 {
            return Some(fd);
        }
    }
    None
}

// ── Diagnostyka sygnałów ──

/// Handlery `SIGTERM`/`SIGHUP` logują nadawcę i kończą sesję; `SIGINT`, `SIGQUIT`
/// i `SIGTSTP` z klawiatury konsoli są ignorowane (nie mogą zatrzymać ani zabić
/// kompozytora). Bez tego przy nagłym końcu sesji w logu zostaje tylko cisza.
pub fn install_signal_handlers() {
    unsafe {
        for sig in [libc::SIGINT, libc::SIGQUIT, libc::SIGTSTP, libc::SIGTTIN] {
            libc::signal(sig, libc::SIG_IGN);
        }
        for sig in [libc::SIGTERM, libc::SIGHUP] {
            let mut action: libc::sigaction = std::mem::zeroed();
            action.sa_sigaction = on_terminate as *const () as usize;
            action.sa_flags = libc::SA_SIGINFO;
            libc::sigemptyset(&mut action.sa_mask);
            libc::sigaction(sig, &action, std::ptr::null_mut());
        }
        // Sprzątanie przy zwykłym wyjściu (`return` z `main`).
        libc::atexit(at_exit);
    }
}

extern "C" fn at_exit() {
    restore();
}

/// Handler `SIGTERM`/`SIGHUP`: zapisuje, kto zakończył sesję, przywraca VT i kończy.
extern "C" fn on_terminate(sig: libc::c_int, info: *mut libc::siginfo_t, _: *mut libc::c_void) {
    // Wyłącznie wywołania bezpieczne w kontekście sygnału (bez alokacji i std).
    let mut buf = [0u8; 96];
    let mut len = 0;
    push(&mut buf, &mut len, b"selene: sygnal ");
    let name: &[u8] = match sig {
        libc::SIGTERM => b"SIGTERM",
        libc::SIGHUP => b"SIGHUP",
        _ => b"?",
    };
    push(&mut buf, &mut len, name);
    let sender = if info.is_null() {
        0
    } else {
        unsafe { (*info).si_pid() }
    };
    if sender > 0 {
        push(&mut buf, &mut len, b" od pid ");
        push_u32(&mut buf, &mut len, sender as u32);
    }
    push(&mut buf, &mut len, b" - koncze sesje\n");
    unsafe {
        libc::write(2, buf.as_ptr().cast(), len);
    }
    restore();
    unsafe { libc::_exit(128 + sig) };
}

fn push(buf: &mut [u8; 96], len: &mut usize, bytes: &[u8]) {
    let n = bytes.len().min(buf.len().saturating_sub(*len));
    buf[*len..*len + n].copy_from_slice(&bytes[..n]);
    *len += n;
}

fn push_u32(buf: &mut [u8; 96], len: &mut usize, mut value: u32) {
    let mut digits = [0u8; 10];
    let mut n = 0;
    loop {
        digits[n] = b'0' + (value % 10) as u8;
        n += 1;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    for i in (0..n).rev() {
        push(buf, len, &[digits[i]]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_signal_sender() {
        let mut buf = [0u8; 96];
        let mut len = 0;
        push(&mut buf, &mut len, b"pid ");
        push_u32(&mut buf, &mut len, 4242);
        assert_eq!(&buf[..len], b"pid 4242");
    }

    #[test]
    fn restore_without_claim_is_a_noop() {
        CLAIMED.store(false, Ordering::Relaxed);
        restore();
    }
}
