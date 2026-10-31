//! XWayland — obsługa aplikacji X11.
//!
//! Selene jest kompozytorem bez `smithay`, więc X11 obsługujemy sami: uruchamiamy
//! `Xwayland` i implementujemy `xwayland_shell_v1`. Xwayland wiąże swoje okno X11
//! z powierzchnią `wl_surface`, a my traktujemy tę powierzchnię jak zwykłe okno
//! (dostaje nasze dekoracje i trafia do taskbara).
//!
//! Numer ekranu (`DISPLAY=:N`) zapisujemy w `$XDG_RUNTIME_DIR/luna-display`, żeby
//! menedżer sesji mógł go wyeksportować, zanim wystartuje powłoka (a przez nią
//! aplikacje).
//!
//! Wyłączenie: `SELENE_NO_XWAYLAND=1`.

use std::path::PathBuf;

use wayland_protocols::xwayland::shell::v1::server::{
    xwayland_shell_v1::{self, XwaylandShellV1},
    xwayland_surface_v1::{self, XwaylandSurfaceV1},
};
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource};

use crate::compositor::SurfaceData;
use crate::state::State;
use crate::sync::LockExt;

/// Nazwa pliku z numerem ekranu X11 w katalogu runtime. Pisze go XWM, gdy połączenie
/// z X11 naprawdę działa (samo gniazdo to za mało — bywa martwe, gdy Xwayland padnie).
pub const DISPLAY_MARKER: &str = "luna-display";

/// Plik z wyjściem Xwaylanda (diagnostyka: dlaczego padł).
pub const XWAYLAND_LOG: &str = "luna-xwayland.log";

/// Znacznik powierzchni X11 (dane zasobu `xwayland_surface_v1`).
struct XwaylandSurfaceData {
    /// Powierzchnia Wayland, z którą powiązane jest okno X11.
    surface: wayland_server::protocol::wl_surface::WlSurface,
}

impl GlobalDispatch<XwaylandShellV1, ()> for State {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<XwaylandShellV1>,
        _data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<XwaylandShellV1, ()> for State {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &XwaylandShellV1,
        request: xwayland_shell_v1::Request,
        _data: &(),
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let xwayland_shell_v1::Request::GetXwaylandSurface { id, surface } = request {
            if std::env::var("SELENE_STATS").is_ok() {
                eprintln!("selene/xwayland: GetXwaylandSurface");
            }
            // Oznaczamy powierzchnię jako okno X11 — od tego momentu `composite` traktuje
            // ją jak zwykłe okno (bez `xdg_surface`).
            if let Some(sd) = surface.data::<SurfaceData>() {
                *sd.xwayland.lock_or_recover() = true;
            }
            data_init.init(
                id,
                XwaylandSurfaceData {
                    surface: surface.clone(),
                },
            );
        }
    }
}

impl Dispatch<XwaylandSurfaceV1, XwaylandSurfaceData> for State {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &XwaylandSurfaceV1,
        request: xwayland_surface_v1::Request,
        data: &XwaylandSurfaceData,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            // `set_serial` domyka parowanie X11-okno ↔ powierzchnia. To jest ten serial,
            // który Xwayland wyśle do okna X11 komunikatem `WL_SURFACE_SERIAL` — po nim
            // XWM rozpozna, które okno X11 odpowiada tej powierzchni. Serial jest 64-bitowy,
            // protokół przesyła go w dwóch częściach.
            xwayland_surface_v1::Request::SetSerial {
                serial_lo,
                serial_hi,
            } => {
                if let Some(sd) = data.surface.data::<SurfaceData>() {
                    let serial = (u64::from(serial_hi) << 32) | u64::from(serial_lo);
                    if std::env::var("SELENE_STATS").is_ok() {
                        eprintln!("selene/xwayland: SetSerial {serial}");
                    }
                    *sd.x11_serial.lock_or_recover() = serial;
                }
            }
            xwayland_surface_v1::Request::Destroy => {
                if let Some(sd) = data.surface.data::<SurfaceData>() {
                    *sd.xwayland.lock_or_recover() = false;
                    *sd.x11_serial.lock_or_recover() = 0u64;
                }
            }
            _ => {}
        }
    }
}

/// Uruchamia `Xwayland` i zwraca numer ekranu (`DISPLAY=:N`).
///
/// `None`, gdy Xwayland nie jest zainstalowany albo start się nie powiódł — brak X11
/// nie może przewrócić sesji Wayland.
pub fn spawn(wayland_socket: &str) -> Option<u32> {
    let display = free_display()?;
    let mut cmd = std::process::Command::new("Xwayland");
    cmd.arg(format!(":{display}"))
        // **`-rootless` jest kluczowe.** Xwayland domyślnie startuje w trybie
        // `rootful`: tworzy wtedy **jedną** powierzchnię na cały ekran X (tytuł
        // „Xwayland on :N”), a wszystkie okna X11 rysują się w jej wnętrzu. Widać to
        // jak „sklonowany ekran” i dostaje jeden wspólny pasek zamiast dekoracji na
        // okno. W trybie rootless każde okno X ma własną powierzchnię Wayland.
        // Rootless wymaga menedżera okien — nasz XWM nim jest (`SubstructureRedirect`).
        .arg("-rootless")
        .arg("-noreset")
        // `-ac`: bez pliku ciasteczek X11. Nasze połączenie XWM (`x11rb`) nie wysyła
        // ciasteczka, a gniazdo X jest lokalne i per-użytkownik — to standardowy wybór
        // dla Xwaylanda uruchamianego przez kompozytor.
        .arg("-ac")
        .arg("-nolisten")
        .arg("tcp")
        // Xwayland łączy się z nami jak zwykły klient Wayland.
        .env("WAYLAND_DISPLAY", wayland_socket);
    if std::env::var("SELENE_XWAYLAND_VERBOSE").is_ok() {
        cmd.arg("-verbose");
    }
    // Wyjście Xwaylanda trafia do pliku w katalogu runtime. Wcześniej szło do /dev/null
    // i nie było widać, **dlaczego** Xwayland pada — a padał, bo gniazdo powstawało,
    // ale połączenia były odrzucane (`Connection refused`).
    if let Ok(rt) = std::env::var("XDG_RUNTIME_DIR") {
        let path = PathBuf::from(rt).join(XWAYLAND_LOG);
        if let Ok(f) = std::fs::File::create(&path) {
            if let Ok(f2) = f.try_clone() {
                cmd.stdout(std::process::Stdio::from(f2));
            }
            cmd.stderr(std::process::Stdio::from(f));
        }
    }
    let child = cmd.spawn().ok()?;
    // Nie przejmujemy się cyklem życia dziecka: Xwayland kończy się razem z sesją
    // (traci połączenie z kompozytorem). Trzymamy PID tylko diagnostycznie.
    if std::env::var("SELENE_STATS").is_ok() {
        eprintln!(
            "selene/xwayland: uruchomiony Xwayland :{display} (pid {})",
            child.id()
        );
    }
    // `DISPLAY` ustawiamy **dopiero**, gdy XWM faktycznie połączy się z X11 (`xwm.rs`) —
    // samo powstanie gniazda nie znaczy, że serwer już przyjmuje klientów.
    Some(display)
}

/// Pierwszy wolny numer ekranu X11 (brak `/tmp/.X11-unix/Xn`).
fn free_display() -> Option<u32> {
    (0..64u32).find(|n| {
        let sock = PathBuf::from(format!("/tmp/.X11-unix/X{n}"));
        let lock = PathBuf::from(format!("/tmp/.X{n}-lock"));
        !sock.exists() && !lock.exists()
    })
}

/// Zapisuje numer ekranu dla menedżera sesji (`$XDG_RUNTIME_DIR/luna-display`).
///
/// Woła to XWM po **udanym** połączeniu — dzięki temu sesja nigdy nie ustawi `DISPLAY`
/// na ekran, który nie odpowiada.
pub fn write_display_marker(display: u32) {
    let Ok(rt) = std::env::var("XDG_RUNTIME_DIR") else {
        return;
    };
    let path = PathBuf::from(rt).join(DISPLAY_MARKER);
    let _ = std::fs::write(path, display.to_string());
}

/// Kasuje marker z poprzedniej sesji — inaczej menedżer sesji ustawiłby `DISPLAY`
/// na ekran, którego nowy Xwayland jeszcze (albo wcale) nie utworzył.
pub fn clear_display_marker() {
    let Ok(rt) = std::env::var("XDG_RUNTIME_DIR") else {
        return;
    };
    let _ = std::fs::remove_file(PathBuf::from(rt).join(DISPLAY_MARKER));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_is_a_runtime_file_name() {
        // Nazwa bez separatorów — trafia wprost do `$XDG_RUNTIME_DIR`.
        assert!(!DISPLAY_MARKER.contains('/'));
        assert_eq!(DISPLAY_MARKER, "luna-display");
        assert_eq!(XWAYLAND_LOG, "luna-xwayland.log");
    }

    #[test]
    fn free_display_is_in_valid_range() {
        // Na maszynie bez X11 pierwszy wolny numer to 0; funkcja nie może panikować.
        if let Some(n) = free_display() {
            assert!(n < 64);
        }
    }
}
