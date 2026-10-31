//! XWM — menedżer okien X11 dla XWayland.
//!
//! Selene jest kompozytorem, więc rolę menedżera okien X11 musi pełnić sam: przejmuje
//! `SubstructureRedirect` na oknie głównym X, zarządza oknami X11 i tłumaczy ich stan na
//! model Luny (tytuł, typ okna, zamykanie, maksymalizacja, geometria).
//!
//! Dlaczego osobny wątek: X11 ma własny protokół i własne gniazdo, a pętla protokołu
//! Waylanda nie może się na nim blokować. Kompozytor rozmawia z XWM przez kanał
//! ([`XwmCommand`]), a odczytuje stan przez [`Xwm::info_for_serial`].
//!
//! Powiązanie okno X11 ↔ powierzchnia Wayland: Xwayland wysyła do okna X11 komunikat
//! `WL_SURFACE_SERIAL` z serialem, który wcześniej ustawił na powierzchni
//! (`xwayland_surface_v1.set_serial`). Dlatego trzymamy mapę `serial → okno X11`.

use std::collections::HashMap;
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use wayland_server::Resource;

use x11rb::connection::Connection;
use x11rb::protocol::composite::{ConnectionExt as CompositeExt, Redirect};
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ChangeWindowAttributesAux, ClientMessageEvent, ConfigWindow,
    ConfigureWindowAux, ConnectionExt, EventMask, InputFocus, PropMode, Window,
};
use x11rb::protocol::Event;
use x11rb::rust_connection::RustConnection;
use x11rb::x11_utils::Serialize;

use crate::compositor::SurfaceData;
use crate::state::State;
use crate::sync::LockExt;

/// Rodzaj okna X11 (z `_NET_WM_WINDOW_TYPE`), o tyle, o ile wpływa na wygląd.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum X11Kind {
    #[default]
    Normal,
    Dialog,
    /// Menu, podpowiedź, listy rozwijane — bez dekoracji, nad innymi oknami.
    Transient,
}

/// Stan okna X11, którego nie da się odczytać z Waylanda.
#[derive(Debug, Clone, Default)]
pub struct X11Info {
    pub title: String,
    pub override_redirect: bool,
    pub kind: X11Kind,
    /// Klient wyraźnie poprosił o brak ramki (`_MOTIF_WM_HINTS` bez dekoracji).
    /// Tak robią aplikacje z własnymi dekoracjami (CSD) — nie dokładamy wtedy naszego paska.
    pub client_decorations: bool,
    /// Pozycja i rozmiar okna w przestrzeni roota (dla menu/podpowiedzi).
    /// Menu X11 (override-redirect) ma współrzędne absolutne — musimy je znać,
    /// żeby narysować je tam, gdzie klient je umieścił, a nie w kaskadzie.
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    /// Szerokość obramowania X11 (`border_width` z `GetGeometry`).
    /// Bufor Waylanda z Xwaylanda zawiera obramowanie, a my pokazujemy treść.
    pub border: u32,
}

impl X11Info {
    /// Czy okno ma dostać nasze dekoracje (menu i podpowiedzi — nie).
    pub fn decorated(&self) -> bool {
        !self.override_redirect && self.kind != X11Kind::Transient && !self.client_decorations
    }
}

/// Zmiana stanu okna X11, o której kompozytor musi się dowiedzieć (XWM to osobny wątek).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum X11Change {
    Fullscreen(bool),
    Maximized(bool),
    /// Klient prosi o zminimalizowanie/przywrócenie (`WM_CHANGE_STATE` / IconicState).
    Minimize(bool),
    /// Interaktywne przesunięcie od pozycji wskaźnika (`_NET_WM_MOVERESIZE`, kierunek 8).
    Move {
        x: i32,
        y: i32,
    },
    /// Interaktywna zmiana rozmiaru (bitmaska jak w `seat`: 1=góra, 2=dół, 4=lewo, 8=prawo).
    Resize {
        edges: u8,
        x: i32,
        y: i32,
    },
    /// Klient sam przesunął okno (`ConfigureRequest` z X/Y).
    ///
    /// Tak robią okna CSD, które nie używają `_NET_WM_MOVERESIZE` (np. część
    /// wersji Firefoksa): bez tego model okna zostawał w starym miejscu.
    Place {
        x: i32,
        y: i32,
    },
}

/// Polecenia z kompozytora do wątku XWM.
#[derive(Debug, Clone)]
pub enum XwmCommand {
    Close(Window),
    SetGeometry {
        window: Window,
        x: i32,
        y: i32,
        w: u32,
        h: u32,
    },
    SetActive(Option<Window>),
    SetMaximized {
        window: Window,
        on: bool,
    },
    /// Informuje klienta X11, ile miejsca zajmują nasze dekoracje.
    SetFrameExtents {
        window: Window,
        top: u32,
    },
    /// Prosi klienta o wejście/wyjście z pełnego ekranu (EWMH `_NET_WM_STATE_FULLSCREEN`).
    SetFullscreen {
        window: Window,
        on: bool,
    },
}

/// Współdzielony stan XWM (czytany przez kompozytor).
#[derive(Default)]
pub struct XwmState {
    /// `seriał powierzchni Wayland → okno X11` (z komunikatu `WL_SURFACE_SERIAL`).
    by_serial: HashMap<u64, Window>,
    /// `okno X11 → informacje` (tytuł, typ, override-redirect).
    windows: HashMap<Window, X11Info>,
    /// Okna zarządzane (na liście klientów), w kolejności pojawiania się.
    managed: Vec<Window>,
    /// Zmiany stanu zgłoszone przez klientów X11 (np. `_NET_WM_STATE_FULLSCREEN`).
    changes: Vec<(Window, X11Change)>,
    /// Ostatnio znany stan EWMH okna: `(fullscreen, maximized)`.
    ewmh: HashMap<Window, (bool, bool)>,
    /// Czy udało się zostać menedżerem okien X11.
    pub is_wm: bool,
    /// Numer ekranu X11 (`DISPLAY=:N`); 0 przed połączeniem.
    pub display: u32,
}

/// Stan X11 do diagnostyki (`luna-control x11`, testy sesji).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct X11Status {
    pub display: u32,
    pub is_wm: bool,
    /// Okna zarządzane (na `_NET_CLIENT_LIST`).
    pub managed: usize,
    /// Wszystkie znane okna X11 (z nadrzędnymi i override-redirect).
    pub windows: usize,
}

impl XwmState {
    /// Stan X11 do diagnostyki.
    pub fn status(&self) -> X11Status {
        X11Status {
            display: self.display,
            is_wm: self.is_wm,
            managed: self.managed.len(),
            windows: self.windows.len(),
        }
    }

    /// Okna zarządzane z informacjami z X11 (tytuł, treść, obramowanie).
    pub fn managed_info(&self) -> Vec<(Window, X11Info)> {
        self.managed
            .iter()
            .filter_map(|w| self.windows.get(w).map(|i| (*w, i.clone())))
            .collect()
    }
}

impl XwmState {
    /// Informacje o oknie powiązanym z serialem powierzchni Wayland.
    pub fn info_for_serial(&self, serial: u64) -> Option<X11Info> {
        let win = *self.by_serial.get(&serial)?;
        self.windows.get(&win).cloned()
    }

    /// Okno X11 powiązane z serialem.
    pub fn window_for_serial(&self, serial: u64) -> Option<Window> {
        self.by_serial.get(&serial).copied()
    }

    /// Serial powierzchni Wayland odpowiadający oknu X11.
    pub fn serial_for_window(&self, window: Window) -> Option<u64> {
        self.by_serial
            .iter()
            .find(|(_, w)| **w == window)
            .map(|(serial, _)| *serial)
    }

    /// Zmiany stanu oczekujące na odebranie przez kompozytor.
    pub fn take_changes(&mut self) -> Vec<(Window, X11Change)> {
        std::mem::take(&mut self.changes)
    }
}

/// Uchwyt XWM: kanał poleceń + współdzielony stan.
#[derive(Clone)]
pub struct Xwm {
    tx: Sender<XwmCommand>,
    state: Arc<Mutex<XwmState>>,
}

/// Globalny uchwyt XWM.
///
/// Jest dokładnie jeden kompozytor w procesie, a XWM powstaje raz przy starcie — dzięki
/// temu kompozytor nie musi przekazywać go przez cały model okien.
static GLOBAL: OnceLock<Arc<Xwm>> = OnceLock::new();

/// Rejestruje działający XWM jako globalny.
pub fn set_global(xwm: Xwm) {
    let _ = GLOBAL.set(Arc::new(xwm));
}

/// Globalny XWM (jeśli X11 działa).
pub fn global() -> Option<&'static Arc<Xwm>> {
    GLOBAL.get()
}

impl Xwm {
    pub fn info_for_serial(&self, serial: u64) -> Option<X11Info> {
        self.state.lock_or_recover().info_for_serial(serial)
    }

    pub fn window_for_serial(&self, serial: u64) -> Option<Window> {
        self.state.lock_or_recover().window_for_serial(serial)
    }

    pub fn serial_for_window(&self, window: Window) -> Option<u64> {
        self.state.lock_or_recover().serial_for_window(window)
    }

    pub fn is_wm(&self) -> bool {
        self.state.lock_or_recover().is_wm
    }

    /// Stan X11 do diagnostyki (numer ekranu, WM, liczba okien).
    pub fn status(&self) -> X11Status {
        self.state.lock_or_recover().status()
    }

    /// Okna zarządzane z informacjami z X11.
    pub fn managed_info(&self) -> Vec<(Window, X11Info)> {
        self.state.lock_or_recover().managed_info()
    }

    fn send(&self, cmd: XwmCommand) {
        // Kanał jest jedynym sposobem rozmowy z wątkiem X11; jego zamknięcie (X11 padł)
        // nie może przewrócić kompozytora.
        let _ = self.tx.send(cmd);
    }

    pub fn close(&self, window: Window) {
        self.send(XwmCommand::Close(window));
    }

    pub fn set_geometry(&self, window: Window, x: i32, y: i32, w: u32, h: u32) {
        self.send(XwmCommand::SetGeometry { window, x, y, w, h });
    }

    pub fn set_active(&self, window: Option<Window>) {
        self.send(XwmCommand::SetActive(window));
    }

    pub fn set_maximized(&self, window: Window, on: bool) {
        self.send(XwmCommand::SetMaximized { window, on });
    }

    pub fn set_frame_extents(&self, window: Window, top: u32) {
        self.send(XwmCommand::SetFrameExtents { window, top });
    }

    pub fn set_fullscreen(&self, window: Window, on: bool) {
        self.send(XwmCommand::SetFullscreen { window, on });
    }

    /// Odbiera zmiany stanu zgłoszone przez klientów X11 (woła kompozytor).
    pub fn take_changes(&self) -> Vec<(Window, X11Change)> {
        self.state.lock_or_recover().take_changes()
    }
}

/// Wysokość naszego paska tytułu — ile miejsca oddajemy klientom X11 w `_NET_FRAME_EXTENTS`.
/// Musi być równa [`crate::state::TITLEBAR`], bo dokładnie tyle miejsca zabiera nasz pasek.
pub const FRAME_TOP: u32 = crate::state::TITLEBAR as u32;

/// Uruchamia XWM. **Nie blokuje kompozytora.**
///
/// Xwayland potrzebuje chwili, zanim zacznie przyjmować połączenia, dlatego łączenie
/// ponawiamy w wątku; gdy się uda, XWM rejestruje się jako globalny ([`global`]).
/// Kompozytor nie musi więc czekać i od razu obsługuje klientów Waylanda.
pub fn spawn(display: u32) {
    let (tx, rx) = std::sync::mpsc::channel();
    let state: Arc<Mutex<XwmState>> = Arc::new(Mutex::new(XwmState::default()));
    let st = Arc::clone(&state);
    {
        // Numer ekranu znamy od razu — `x11` w `luna-control` ma go pokazać
        // nawet przed udanym połączeniem.
        st.lock_or_recover().display = display;
    }

    std::thread::spawn(move || {
        // 1) Połączenie z X11 — z ponowieniami (Xwayland wstaje asynchronicznie).
        let Some((conn, screen)) = connect_retry(display, Duration::from_secs(8)) else {
            return;
        };
        // Dopiero teraz X11 naprawdę działa — sesja może dostać `DISPLAY`
        // (wcześniej gniazdo bywało martwe i aplikacje nie mogły się połączyć).
        crate::xwayland::write_display_marker(display);
        let root = conn.setup().roots[screen].root;
        let Ok(mut atoms) = Atoms::new(&conn) else {
            return;
        };
        let is_wm = become_wm(&conn, root, &mut atoms).unwrap_or(false);
        st.lock_or_recover().is_wm = is_wm;
        if std::env::var("SELENE_STATS").is_ok() {
            eprintln!(
                "selene/xwm: X11 gotowy (ekran :{display}), menedżer okien: {}",
                if is_wm { "tak" } else { "nie" }
            );
        }
        // 2) Rejestracja globalna — kompozytor czyta `global()` przy każdej operacji,
        //    więc podłączenie „w locie” jest bezpieczne.
        set_global(Xwm {
            tx: tx.clone(),
            state: Arc::clone(&st),
        });
        run(conn, root, atoms, rx, st);
    });
}

/// Łączy się z X11, ponawiając próbę aż do `timeout`.
fn connect_retry(display: u32, timeout: Duration) -> Option<(RustConnection, usize)> {
    let name = format!(":{display}");
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match x11rb::connect(Some(&name)) {
            Ok(v) => return Some(v),
            Err(e) => {
                if std::time::Instant::now() >= deadline {
                    if std::env::var("SELENE_STATS").is_ok() {
                        eprintln!("selene/xwm: brak połączenia z X11: {e}");
                    }
                    return None;
                }
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Pętla wątku X11: polecenia z kompozytora + zdarzenia z X11.
fn run(
    conn: RustConnection,
    root: Window,
    atoms: Atoms,
    rx: Receiver<XwmCommand>,
    state: Arc<Mutex<XwmState>>,
) {
    loop {
        // 1) Polecenia od kompozytora.
        while let Ok(cmd) = rx.try_recv() {
            apply_command(&conn, &root, &atoms, &state, cmd);
        }
        // 2) Zdarzenia X11.
        let mut handled = false;
        while let Ok(Some(event)) = conn.poll_for_event() {
            handled = true;
            handle_event(&conn, root, &atoms, &state, event);
        }
        let _ = conn.flush();
        if !handled {
            std::thread::sleep(Duration::from_millis(4));
        }
    }
}

/// Przejmuje rolę menedżera okien X11 (`SubstructureRedirect`).
fn become_wm(conn: &RustConnection, root: Window, atoms: &mut Atoms) -> Option<bool> {
    let aux = ChangeWindowAttributesAux::new()
        .event_mask(EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY);
    // Gdy inny menedżer okien już działa, X zwraca `Access` — nie walczymy z nim.
    let ok = conn
        .change_window_attributes(root, &aux)
        .ok()
        .and_then(|c| c.check().ok())
        .is_some();
    if !ok {
        return Some(false);
    }
    // `_NET_SUPPORTING_WM_CHECK`: dowód dla klientów, że mamy EWMH.
    let wm_window = conn.generate_id().ok()?;
    conn.create_window(
        x11rb::COPY_FROM_PARENT as u8,
        wm_window,
        root,
        0,
        0,
        1,
        1,
        0,
        x11rb::protocol::xproto::WindowClass::INPUT_OUTPUT,
        x11rb::COPY_FROM_PARENT as u32,
        &Default::default(),
    )
    .ok()?;
    set_prop32(
        conn,
        root,
        atoms.net_supporting_wm_check,
        AtomEnum::WINDOW.into(),
        &[wm_window],
    );
    set_prop32(
        conn,
        wm_window,
        atoms.net_supporting_wm_check,
        AtomEnum::WINDOW.into(),
        &[wm_window],
    );
    set_prop_str(conn, wm_window, AtomEnum::WM_NAME.into(), "Luna");
    // Lista obsługiwanych rozszerzeń EWMH, których naprawdę używamy.
    let supported: Vec<Atom> = vec![
        atoms.net_wm_state,
        atoms.net_wm_state_maximized_vert,
        atoms.net_wm_state_maximized_horz,
        atoms.net_wm_state_fullscreen,
        atoms.net_close_window,
        atoms.net_active_window,
        atoms.net_client_list,
        atoms.net_wm_name,
        atoms.net_wm_window_type,
        atoms.net_frame_extents,
        atoms.net_wm_moveresize,
    ];
    set_prop32(
        conn,
        root,
        atoms.net_supported,
        AtomEnum::ATOM.into(),
        &supported,
    );
    // Composite: w trybie rootless Xwayland tworzy powierzchnię Wayland dla okna dopiero
    // wtedy, gdy okno jest przekierowane (`redirectDraw == RedirectDrawManual`). Bez tego
    // klient X11 działa w tle, ale kompozytor nie dostaje żadnej powierzchni do rysowania.
    // `RedirectSubwindows` obejmuje zarówno okna już istniejące, jak i wszystkie przyszłe
    // (X serwera przekierowuje je w hooku `CreateWindow`), więc wystarczy jedno wywołanie.
    if !enable_composite(conn, root) {
        if std::env::var("SELENE_STATS").is_ok() {
            eprintln!("selene/xwm: Composite niedostępny — okna X11 nie będą widoczne");
        }
    }
    let _ = conn.flush();
    Some(true)
}

/// Włącza przekierowanie Composite dla okien potomnych roota.
fn enable_composite(conn: &RustConnection, root: Window) -> bool {
    let version = conn
        .composite_query_version(0, 4)
        .ok()
        .and_then(|c| c.reply().ok());
    let Some(version) = version else {
        return false;
    };
    let ok = conn
        .composite_redirect_subwindows(root, Redirect::MANUAL)
        .ok()
        .and_then(|c| c.check().ok())
        .is_some();
    if std::env::var("SELENE_STATS").is_ok() {
        eprintln!(
            "selene/xwm: Composite {}.{} → przekierowanie okien: {}",
            version.major_version,
            version.minor_version,
            if ok { "tak" } else { "nie" }
        );
    }
    ok
}

fn handle_event(
    conn: &RustConnection,
    root: Window,
    atoms: &Atoms,
    state: &Arc<Mutex<XwmState>>,
    event: Event,
) {
    match event {
        Event::CreateNotify(e) => {
            // Zmiany własności (`_NET_WM_NAME`, `_NET_WM_WINDOW_TYPE`, `_MOTIF_WM_HINTS`)
            // trafiają **do okna**, nie do roota — musimy wybrać maskę na samym oknie.
            // Każdy klient ma własną maskę, więc nie nadpisujemy wyboru aplikacji.
            if e.parent == root {
                let aux = ChangeWindowAttributesAux::new()
                    .event_mask(EventMask::PROPERTY_CHANGE | EventMask::STRUCTURE_NOTIFY);
                let _ = conn.change_window_attributes(e.window, &aux);
                // Zapamiętaj pozycję od razu — menu może się pojawić zanim przyjdzie
                // `WL_SURFACE_SERIAL`, a wtedy chcemy je postawić we właściwym miejscu.
                {
                    let mut st = state.lock_or_recover();
                    let info = st.windows.entry(e.window).or_default();
                    info.x = i32::from(e.x);
                    info.y = i32::from(e.y);
                    info.width = i32::from(e.width);
                    info.height = i32::from(e.height);
                    // Obramowanie trzeba znać **przed** pierwszym commitem, bo Xwayland
                    // tworzy bufor razem z nim (inaczej okno startuje o 2 px za szerokie).
                    info.border = window_border(conn, e.window);
                }
            }
        }
        Event::ConfigureNotify(e) => {
            let mut st = state.lock_or_recover();
            let info = st.windows.entry(e.window).or_default();
            info.x = i32::from(e.x);
            info.y = i32::from(e.y);
            info.width = i32::from(e.width);
            info.height = i32::from(e.height);
        }
        Event::MapRequest(e) => {
            let over = is_override_redirect(conn, e.window);
            let info = read_info(conn, atoms, e.window);
            if std::env::var("SELENE_STATS").is_ok() {
                eprintln!(
                    "selene/xwm: MapRequest 0x{:x} '{}' over={} csd={}",
                    e.window, info.title, over, info.client_decorations
                );
            }
            {
                let mut info = info;
                info.override_redirect = over;
                insert_info(state, e.window, info);
            }
            if !over {
                let mut st = state.lock_or_recover();
                if !st.managed.contains(&e.window) {
                    st.managed.push(e.window);
                }
            }
            // Menu i podpowiedzi mapujemy od razu; reszta i tak trafia do naszego modelu
            // przez `commit` powierzchni Wayland — mapujemy zawsze, żeby klient żył.
            let mapped = conn.map_window(e.window);
            let _ = conn.flush();
            if std::env::var("SELENE_STATS").is_ok() {
                let state = conn
                    .get_window_attributes(e.window)
                    .ok()
                    .and_then(|c| c.reply().ok())
                    .map(|a| a.map_state);
                eprintln!(
                    "selene/xwm: map_window 0x{:x} -> ok={} state={:?}",
                    e.window,
                    mapped.is_ok(),
                    state
                );
            }
            publish_client_list(conn, root, atoms, state);
        }
        Event::ConfigureRequest(e) => {
            // Klient prosi o geometrię. Stosujemy **tylko pola z maski** — przy
            // samym przesunięciu `width`/`height` w evencie są zerowe i wcześniej
            // mogły skurczyć okno do 0×0. Zmianę pozycji odbijamy też w modelu
            // kompozytora: okna CSD (Firefox) przesuwają się same, bez moveresize.
            let mask = u16::from(e.value_mask);
            if std::env::var("SELENE_STATS").is_ok() {
                eprintln!(
                    "selene/xwm: ConfigureRequest 0x{:x} mask={mask:#x} pos=({},{}) size={}x{}",
                    e.window, e.x, e.y, e.width, e.height
                );
            }
            let mut aux = ConfigureWindowAux::new();
            if mask & u16::from(ConfigWindow::X) != 0 {
                aux = aux.x(i32::from(e.x));
            }
            if mask & u16::from(ConfigWindow::Y) != 0 {
                aux = aux.y(i32::from(e.y));
            }
            if mask & u16::from(ConfigWindow::WIDTH) != 0 {
                aux = aux.width(u32::from(e.width));
            }
            if mask & u16::from(ConfigWindow::HEIGHT) != 0 {
                aux = aux.height(u32::from(e.height));
            }
            let _ = conn.configure_window(e.window, &aux);
            let _ = conn.flush();
            if mask & (u16::from(ConfigWindow::X) | u16::from(ConfigWindow::Y)) != 0 {
                // Tylko okna z własnymi dekoracjami (CSD) przesuwają się same.
                // Oknom SSD pozycję ustala kompozytor — ich `ConfigureRequest`
                // nie może nadpisywać modelu (byłby skok przy mapowaniu).
                let csd = state
                    .lock_or_recover()
                    .windows
                    .get(&e.window)
                    .map(|i| i.client_decorations)
                    .unwrap_or(false);
                if csd {
                    if std::env::var("SELENE_STATS").is_ok() {
                        eprintln!(
                            "selene/xwm: ConfigureRequest 0x{:x} mask={mask:#x} pos=({},{})",
                            e.window, e.x, e.y
                        );
                    }
                    state.lock_or_recover().changes.push((
                        e.window,
                        X11Change::Place {
                            x: i32::from(e.x),
                            y: i32::from(e.y),
                        },
                    ));
                }
            }
        }
        Event::PropertyNotify(e) => {
            if e.atom == atoms.net_wm_name
                || e.atom == AtomEnum::WM_NAME.into()
                || e.atom == atoms.net_wm_window_type
                || e.atom == atoms.motif_wm_hints
            {
                let info = read_info(conn, atoms, e.window);
                if std::env::var("SELENE_STATS").is_ok() && e.atom == atoms.motif_wm_hints {
                    eprintln!(
                        "selene/xwm: 0x{:x} dekoracje klienta: {}",
                        e.window, info.client_decorations
                    );
                }
                insert_info(state, e.window, info);
            }
        }
        Event::ClientMessage(e) => {
            if e.type_ == atoms.wl_surface_serial {
                // Powiązanie okno X11 ↔ powierzchnia Wayland (patrz opis modułu).
                let serial = serial_from_client_message(&e.data);
                if serial != 0 {
                    let info = read_info(conn, atoms, e.window);
                    if std::env::var("SELENE_STATS").is_ok() {
                        eprintln!(
                            "selene/xwm: WL_SURFACE_SERIAL 0x{:x} '{}' serial={}",
                            e.window, info.title, serial
                        );
                    }
                    let mut st = state.lock_or_recover();
                    st.by_serial.insert(serial, e.window);
                    drop(st);
                    insert_info(state, e.window, info);
                }
            } else if e.type_ == atoms.net_close_window {
                request_close(conn, e.window);
            } else if e.type_ == atoms.net_wm_state {
                // Klient prosi o zmianę stanu (pełny ekran, maksymalizacja).
                // `data = [action, atom1, atom2, source, 0]`; akcja 0 = usuń, 1 = dodaj,
                // 2 = przełącz. Samo ustawienie robi kompozytor — my tylko przekazujemy
                // życzenie (XWM nie zna modelu okien Waylanda).
                let data = e.data.as_data32();
                let action = data[0];
                let mut st = state.lock_or_recover();
                let mut flags = *st.ewmh.get(&e.window).unwrap_or(&(false, false));
                if std::env::var("SELENE_STATS").is_ok() {
                    let name = |a: u32| {
                        if a == 0 {
                            return String::new();
                        }
                        conn.get_atom_name(a)
                            .ok()
                            .and_then(|c| c.reply().ok())
                            .map(|r| String::from_utf8_lossy(&r.name).into_owned())
                            .unwrap_or_default()
                    };
                    eprintln!(
                        "selene/xwm: _NET_WM_STATE 0x{:x} action={action} a1={} a2={} source={} flags={flags:?}",
                        e.window,
                        name(data[1]),
                        name(data[2]),
                        data[3]
                    );
                }
                let mut pending: Vec<X11Change> = Vec::new();
                // Akcja 2 = toggle. Cel liczymy **raz**, przed pętlą: komunikat
                // zawiera dwa atomy (MAX_VERT + MAX_HORZ), a wcześniej drugi atom
                // widział już zmieniony `flags` i natychmiast cofał pierwszy —
                // maksymalizacja z przycisku Firefoksa nie działała wcale.
                let toggle_fullscreen = !flags.0;
                let toggle_maximized = !flags.1;
                for state_atom in [data[1], data[2]] {
                    if state_atom == 0 {
                        continue;
                    }
                    let fullscreen = if state_atom == atoms.net_wm_state_fullscreen {
                        true
                    } else if state_atom == atoms.net_wm_state_maximized_vert
                        || state_atom == atoms.net_wm_state_maximized_horz
                    {
                        false
                    } else {
                        continue;
                    };
                    let on = match net_wm_state_target(
                        action,
                        fullscreen,
                        toggle_fullscreen,
                        toggle_maximized,
                    ) {
                        Some(on) => on,
                        None => continue,
                    };
                    let change = if fullscreen {
                        flags.0 = on;
                        X11Change::Fullscreen(on)
                    } else {
                        flags.1 = on;
                        X11Change::Maximized(on)
                    };
                    pending.push(change);
                }
                st.ewmh.insert(e.window, flags);
                st.changes
                    .extend(pending.into_iter().map(|c| (e.window, c)));
            } else if e.type_ == atoms.wm_change_state {
                // `WM_CHANGE_STATE`: 1 = NormalState, 3 = IconicState (minimalizacja).
                let data = e.data.as_data32();
                let iconic = data[0] == 3;
                state
                    .lock_or_recover()
                    .changes
                    .push((e.window, X11Change::Minimize(iconic)));
            } else if e.type_ == atoms.net_wm_moveresize {
                // `data = [x_root, y_root, direction, button, source]`.
                let data = e.data.as_data32();
                let (x, y, direction) = (data[0] as i32, data[1] as i32, data[2]);
                if std::env::var("SELENE_STATS").is_ok() {
                    let serial = state
                        .lock_or_recover()
                        .by_serial
                        .iter()
                        .find(|(_, w)| **w == e.window)
                        .map(|(s, _)| *s);
                    eprintln!(
                        "selene/xwm: _NET_WM_MOVERESIZE 0x{:x} dir={direction} pos=({x},{y}) serial={serial:?}",
                        e.window
                    );
                }
                let change = match direction {
                    // 8 = Move, 0..=7 = krawędzie/narożniki, 11 = Cancel.
                    8 => Some(X11Change::Move { x, y }),
                    0..=7 => Some(X11Change::Resize {
                        edges: move_resize_edges(direction),
                        x,
                        y,
                    }),
                    _ => None,
                };
                if let Some(change) = change {
                    state.lock_or_recover().changes.push((e.window, change));
                }
            }
        }
        Event::UnmapNotify(e) => {
            let mut st = state.lock_or_recover();
            st.managed.retain(|w| *w != e.window);
            drop(st);
            publish_client_list(conn, root, atoms, state);
        }
        Event::DestroyNotify(e) => {
            let mut st = state.lock_or_recover();
            st.windows.remove(&e.window);
            st.managed.retain(|w| *w != e.window);
            st.by_serial.retain(|_, w| *w != e.window);
            st.ewmh.remove(&e.window);
        }
        _ => {}
    }
}

fn apply_command(
    conn: &RustConnection,
    root: &Window,
    atoms: &Atoms,
    state: &Arc<Mutex<XwmState>>,
    cmd: XwmCommand,
) {
    match cmd {
        XwmCommand::Close(win) => request_close(conn, win),
        XwmCommand::SetGeometry { window, x, y, w, h } => {
            let aux = ConfigureWindowAux::new().x(x).y(y).width(w).height(h);
            let _ = conn.configure_window(window, &aux);
        }
        XwmCommand::SetActive(window) => {
            let list: Vec<u32> = window.into_iter().collect();
            set_prop32(
                conn,
                *root,
                atoms.net_active_window,
                AtomEnum::WINDOW.into(),
                &list,
            );
            if let Some(w) = window {
                let _ = conn.set_input_focus(InputFocus::POINTER_ROOT, w, x11rb::CURRENT_TIME);
            }
        }
        XwmCommand::SetMaximized { window, on } => {
            // Prosimy klienta o zmianę stanu (EWMH) — tak działają wszystkie pulpity.
            let action = if on { 1u32 } else { 0u32 }; // _NET_WM_STATE_ADD / _REMOVE
            for state_atom in [
                atoms.net_wm_state_maximized_vert,
                atoms.net_wm_state_maximized_horz,
            ] {
                send_wm_state(conn, atoms, window, action, state_atom);
            }
        }
        XwmCommand::SetFrameExtents { window, top } => {
            set_prop32(
                conn,
                window,
                atoms.net_frame_extents,
                AtomEnum::CARDINAL.into(),
                &[0, 0, top, 0],
            );
        }
        XwmCommand::SetFullscreen { window, on } => {
            let action = if on { 1u32 } else { 0u32 }; // _NET_WM_STATE_ADD / _REMOVE
            send_wm_state(conn, atoms, window, action, atoms.net_wm_state_fullscreen);
            state
                .lock_or_recover()
                .ewmh
                .entry(window)
                .or_insert((false, false))
                .0 = on;
        }
    }
    let _ = conn.flush();
    let _ = state;
}

/// Docelowy stan jednego atomu `_NET_WM_STATE`.
///
/// Akcja 2 (toggle) wymaga wartości wyliczonej **raz** przed pętlą po atomach:
/// inaczej drugi atom (`MAXIMIZED_HORZ`) widzi już zmieniony stan po pierwszym
/// (`MAXIMIZED_VERT`) i natychmiast go cofa.
fn net_wm_state_target(
    action: u32,
    fullscreen: bool,
    toggle_fullscreen: bool,
    toggle_maximized: bool,
) -> Option<bool> {
    match action {
        0 => Some(false),
        1 => Some(true),
        2 => Some(if fullscreen {
            toggle_fullscreen
        } else {
            toggle_maximized
        }),
        _ => None,
    }
}

/// Odczytuje serial z komunikatu `WL_SURFACE_SERIAL` (dwa razy 32 bity: `lo`, `hi`).
/// Bitmaska krawędzi dla kierunku `_NET_WM_MOVERESIZE` (1=góra, 2=dół, 4=lewo, 8=prawo).
fn move_resize_edges(direction: u32) -> u8 {
    match direction {
        0 => 1 | 4, // lewy-górny
        1 => 1,     // góra
        2 => 1 | 8, // prawy-górny
        3 => 8,     // prawo
        4 => 2 | 8, // prawy-dolny
        5 => 2,     // dół
        6 => 2 | 4, // lewy-dolny
        7 => 4,     // lewo
        _ => 0,
    }
}

fn serial_from_client_message(data: &x11rb::protocol::xproto::ClientMessageData) -> u64 {
    let raw = data.serialize();
    if raw.len() < 8 {
        return 0;
    }
    let lo = u32::from_ne_bytes([raw[0], raw[1], raw[2], raw[3]]);
    let hi = u32::from_ne_bytes([raw[4], raw[5], raw[6], raw[7]]);
    (u64::from(hi) << 32) | u64::from(lo)
}

/// Prosi klienta o zamknięcie okna (`WM_DELETE_WINDOW`), a gdy nie umie — ubija połączenie.
fn request_close(conn: &RustConnection, window: Window) {
    // `WM_PROTOCOLS` to lista protokołów obsługiwanych przez klienta; zamykanie
    // odbywa się przez wysłanie `ClientMessage` typu `WM_PROTOCOLS` z `WM_DELETE_WINDOW`
    // w pierwszym słowie. Wcześniej wysyłaliśmy typ = pierwszy atom z listy i dane
    // `[16, 0, …]`, co żaden klient nie rozpoznawał — „X” na pasku nic nie robiło.
    let Ok(wm_protocols) = atom(conn, "WM_PROTOCOLS") else {
        return;
    };
    let Ok(wm_delete) = atom(conn, "WM_DELETE_WINDOW") else {
        return;
    };
    let protocols = get_atoms_prop(conn, window, wm_protocols);
    if protocols.contains(&wm_delete) {
        send_client_message(conn, window, wm_protocols, &[wm_delete, 0, 0, 0, 0]);
    }
}

fn send_wm_state(conn: &RustConnection, atoms: &Atoms, window: Window, action: u32, state: Atom) {
    send_client_message(conn, window, atoms.net_wm_state, &[action, state, 0, 1, 0]);
}

/// Wysyła `ClientMessage` (`WM_PROTOCOLS`-style) do okna.
fn send_client_message(conn: &RustConnection, window: Window, type_: Atom, data: &[u32; 5]) {
    let event = ClientMessageEvent::new(32, window, type_, *data);
    let _ = conn.send_event(false, window, EventMask::NO_EVENT, event);
    let _ = conn.flush();
}

/// Ustawia listę okien klientów (`_NET_CLIENT_LIST`) — używają jej panele i aplikacje.
fn publish_client_list(
    conn: &RustConnection,
    root: Window,
    atoms: &Atoms,
    state: &Arc<Mutex<XwmState>>,
) {
    let list = state.lock_or_recover().managed.clone();
    set_prop32(
        conn,
        root,
        atoms.net_client_list,
        AtomEnum::WINDOW.into(),
        &list,
    );
    let _ = conn.flush();
}

/// Czy okno ma `override_redirect` (menu, podpowiedź, okna „niezarządzane”).
fn is_override_redirect(conn: &RustConnection, window: Window) -> bool {
    conn.get_window_attributes(window)
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|a| a.override_redirect)
        .unwrap_or(false)
}

/// Zapisuje informacje o oknie, **zachowując geometrię** z `CreateNotify`/
/// `ConfigureNotify` (odczyt własności jej nie zawiera).
fn insert_info(state: &Arc<Mutex<XwmState>>, window: Window, mut info: X11Info) {
    let mut st = state.lock_or_recover();
    if let Some(prev) = st.windows.get(&window) {
        info.x = prev.x;
        info.y = prev.y;
        info.width = prev.width;
        info.height = prev.height;
    }
    st.windows.insert(window, info);
}

/// Czyta tytuł i typ okna X11.
fn read_info(conn: &RustConnection, atoms: &Atoms, window: Window) -> X11Info {
    let title = read_title(conn, atoms, window);
    let kind = read_kind(conn, atoms, window);
    X11Info {
        title,
        override_redirect: is_override_redirect(conn, window),
        kind,
        client_decorations: has_client_decorations(conn, atoms, window),
        border: window_border(conn, window),
        ..X11Info::default()
    }
}

/// Szerokość obramowania X11 (`border_width` z `GetGeometry`).
///
/// `ConfigureNotify` go nie zawiera, a Xwayland wlicza obramowanie do bufora
/// Waylanda — bez tej wartości model okna byłby o `2 × border` za duży.
fn window_border(conn: &RustConnection, window: Window) -> u32 {
    conn.get_geometry(window)
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|g| u32::from(g.border_width))
        .unwrap_or(0)
}

/// Czy klient chce sam rysować dekoracje (`_MOTIF_WM_HINTS` z wyłączonymi dekoracjami).
///
/// To standardowy sposób, w jaki GTK/Qt/Chrome mówią menedżerowi okien „nie dodawaj ramki”.
/// Flagi: bit 2 (`1 << 1`) mówi, że pole `decorations` jest istotne; `decorations == 0`
/// oznacza „bez ramki” (dekoracje po stronie klienta).
fn has_client_decorations(conn: &RustConnection, atoms: &Atoms, window: Window) -> bool {
    let hints = get_atoms_prop(conn, window, atoms.motif_wm_hints);
    hints.len() >= 3 && hints[0] & 0b10 != 0 && hints[2] == 0
}

/// Tytuł: najpierw `_NET_WM_NAME` (UTF-8), potem klasyczny `WM_NAME`.
fn read_title(conn: &RustConnection, atoms: &Atoms, window: Window) -> String {
    if let Some(utf8) = get_prop8(conn, window, atoms.net_wm_name) {
        if let Ok(s) = String::from_utf8(utf8) {
            let s = s.trim_end_matches('\0').trim();
            if !s.is_empty() {
                return s.to_owned();
            }
        }
    }
    get_prop8(conn, window, AtomEnum::WM_NAME.into())
        .and_then(|b| String::from_utf8(b).ok())
        .map(|s| s.trim_end_matches('\0').trim().to_owned())
        .unwrap_or_default()
}

/// Typ okna: menu/podpowiedzi nie dostają dekoracji.
fn read_kind(conn: &RustConnection, atoms: &Atoms, window: Window) -> X11Kind {
    let types = get_atoms_prop(conn, window, atoms.net_wm_window_type);
    for t in types {
        if t == atoms.net_wm_window_type_menu
            || t == atoms.net_wm_window_type_dropdown
            || t == atoms.net_wm_window_type_tooltip
            || t == atoms.net_wm_window_type_popup
            || t == atoms.net_wm_window_type_notification
        {
            return X11Kind::Transient;
        }
        if t == atoms.net_wm_window_type_dialog
            || t == atoms.net_wm_window_type_utility
            || t == atoms.net_wm_window_type_splash
        {
            return X11Kind::Dialog;
        }
    }
    X11Kind::Normal
}

/// Wartość własności jako lista 32-bitowa.
fn get_atoms_prop(conn: &RustConnection, window: Window, property: Atom) -> Vec<u32> {
    conn.get_property(false, window, property, AtomEnum::ANY, 0, 1024)
        .ok()
        .and_then(|c| c.reply().ok())
        .and_then(|r| r.value32().map(|it| it.collect()))
        .unwrap_or_default()
}

/// Wartość własności jako bajty (tekst UTF-8 / Latin-1).
fn get_prop8(conn: &RustConnection, window: Window, property: Atom) -> Option<Vec<u8>> {
    let reply = conn
        .get_property(false, window, property, AtomEnum::ANY, 0, 1024)
        .ok()?
        .reply()
        .ok()?;
    if reply.value.is_empty() {
        return None;
    }
    Some(reply.value)
}

fn set_prop32(conn: &RustConnection, window: Window, property: Atom, type_: Atom, data: &[u32]) {
    // x11rb 0.14 nie ma `change_property32` — serializujemy liczby sami.
    let bytes: Vec<u8> = data.iter().flat_map(|v| v.to_ne_bytes()).collect();
    let _ = conn.change_property(
        PropMode::REPLACE,
        window,
        property,
        type_,
        32,
        data.len() as u32,
        &bytes,
    );
}

fn set_prop_str(conn: &RustConnection, window: Window, property: Atom, text: &str) {
    let _ = conn.change_property(
        PropMode::REPLACE,
        window,
        property,
        AtomEnum::STRING,
        8,
        text.len() as u32,
        text.as_bytes(),
    );
}

/// Internowane atomy, których używamy (internowanie to jedno zapytanie do X).
struct Atoms {
    net_supported: Atom,
    net_supporting_wm_check: Atom,
    net_client_list: Atom,
    net_active_window: Atom,
    net_close_window: Atom,
    net_wm_state: Atom,
    net_wm_state_maximized_vert: Atom,
    net_wm_state_maximized_horz: Atom,
    net_wm_state_fullscreen: Atom,
    motif_wm_hints: Atom,
    net_wm_name: Atom,
    net_wm_window_type: Atom,
    net_wm_window_type_dialog: Atom,
    net_wm_window_type_utility: Atom,
    net_wm_window_type_splash: Atom,
    net_wm_window_type_menu: Atom,
    net_wm_window_type_dropdown: Atom,
    net_wm_window_type_tooltip: Atom,
    net_wm_window_type_popup: Atom,
    net_wm_window_type_notification: Atom,
    net_frame_extents: Atom,
    wl_surface_serial: Atom,
    wm_change_state: Atom,
    net_wm_moveresize: Atom,
}

impl Atoms {
    fn new(conn: &RustConnection) -> Result<Self, x11rb::errors::ReplyError> {
        Ok(Self {
            net_supported: atom(conn, "_NET_SUPPORTED")?,
            net_supporting_wm_check: atom(conn, "_NET_SUPPORTING_WM_CHECK")?,
            net_client_list: atom(conn, "_NET_CLIENT_LIST")?,
            net_active_window: atom(conn, "_NET_ACTIVE_WINDOW")?,
            net_close_window: atom(conn, "_NET_CLOSE_WINDOW")?,
            net_wm_state: atom(conn, "_NET_WM_STATE")?,
            net_wm_state_maximized_vert: atom(conn, "_NET_WM_STATE_MAXIMIZED_VERT")?,
            net_wm_state_maximized_horz: atom(conn, "_NET_WM_STATE_MAXIMIZED_HORZ")?,
            net_wm_state_fullscreen: atom(conn, "_NET_WM_STATE_FULLSCREEN")?,
            motif_wm_hints: atom(conn, "_MOTIF_WM_HINTS")?,
            net_wm_name: atom(conn, "_NET_WM_NAME")?,
            net_wm_window_type: atom(conn, "_NET_WM_WINDOW_TYPE")?,
            net_wm_window_type_dialog: atom(conn, "_NET_WM_WINDOW_TYPE_DIALOG")?,
            net_wm_window_type_utility: atom(conn, "_NET_WM_WINDOW_TYPE_UTILITY")?,
            net_wm_window_type_splash: atom(conn, "_NET_WM_WINDOW_TYPE_SPLASH")?,
            net_wm_window_type_menu: atom(conn, "_NET_WM_WINDOW_TYPE_MENU")?,
            net_wm_window_type_dropdown: atom(conn, "_NET_WM_WINDOW_TYPE_DROPDOWN_MENU")?,
            net_wm_window_type_tooltip: atom(conn, "_NET_WM_WINDOW_TYPE_TOOLTIP")?,
            net_wm_window_type_popup: atom(conn, "_NET_WM_WINDOW_TYPE_POPUP_MENU")?,
            net_wm_window_type_notification: atom(conn, "_NET_WM_WINDOW_TYPE_NOTIFICATION")?,
            net_frame_extents: atom(conn, "_NET_FRAME_EXTENTS")?,
            wl_surface_serial: atom(conn, "WL_SURFACE_SERIAL")?,
            wm_change_state: atom(conn, "WM_CHANGE_STATE")?,
            net_wm_moveresize: atom(conn, "_NET_WM_MOVERESIZE")?,
        })
    }
}

fn atom(conn: &RustConnection, name: &str) -> Result<Atom, x11rb::errors::ReplyError> {
    conn.intern_atom(false, name.as_bytes())?
        .reply()
        .map(|r| r.atom)
}

/// Synchronizuje model kompozytora z XWM (wołane raz na iterację pętli głównej).
///
/// XWM działa w osobnym wątku i nie ma dostępu do obiektów Waylanda, więc tytuły oraz
/// życzenia stanu (pełny ekran, maksymalizacja) od klientów X11 odbieramy tutaj.
pub fn sync(state: &mut State) {
    sync_window_info(state);
    let Some(xwm) = global() else {
        return;
    };
    for (window, change) in xwm.take_changes() {
        let Some(serial) = xwm.serial_for_window(window) else {
            continue;
        };
        let Some(slot) = slot_for_serial(state, serial) else {
            continue;
        };
        match change {
            X11Change::Fullscreen(on) => crate::seat::set_fullscreen(state, slot, on),
            X11Change::Maximized(on) => crate::seat::set_maximized(state, slot, on),
            X11Change::Minimize(on) => crate::seat::set_minimized(state, slot, on),
            X11Change::Move { x, y } => {
                if std::env::var("SELENE_STATS").is_ok() {
                    eprintln!("selene/xwm: move serial={serial} slot={slot} pos=({x},{y})");
                }
                crate::seat::interactive_move_at(
                    state,
                    slot,
                    libluna_core::Point::new(x as f64, y as f64),
                )
            }
            X11Change::Resize { edges, x, y } => crate::seat::interactive_resize_at(
                state,
                slot,
                edges,
                libluna_core::Point::new(x as f64, y as f64),
            ),
            X11Change::Place { x, y } => {
                if std::env::var("SELENE_STATS").is_ok() {
                    eprintln!("selene/xwm: self-move slot={slot} pos=({x},{y})");
                }
                if let Some(w) = state.windows.lock_or_recover().get_mut(slot) {
                    w.x = x as f64;
                    w.y = y as f64;
                }
            }
        }
    }
}

/// Aktualizuje tytuł i tryb dekoracji okien X11.
///
/// Xwayland wysyła `WL_SURFACE_SERIAL` **po** pierwszym `commit`, więc informacje z XWM
/// docierają chwilę po utworzeniu okna. Bez tego kroku tytuł zostałby pusty, a klient
/// z własnymi dekoracjami (CSD) dostałby od nas drugi pasek tytułu.
fn sync_window_info(state: &mut State) {
    let Some(xwm) = global() else {
        return;
    };
    let updates: Vec<(usize, X11Info)> = state
        .slots
        .iter()
        .enumerate()
        .filter_map(|(slot, surface)| {
            let data = surface.data::<SurfaceData>()?;
            if !*data.xwayland.lock_or_recover() {
                return None;
            }
            let serial = *data.x11_serial.lock_or_recover();
            if serial == 0 {
                return None;
            }
            let info = xwm.info_for_serial(serial)?;
            let is_popup = info.override_redirect || info.kind == X11Kind::Transient;
            let changed = {
                let windows = state.windows.lock_or_recover();
                let w = windows.get(slot)?;
                let moved =
                    is_popup && info.width > 0 && (w.x != info.x as f64 || w.y != info.y as f64);
                // X11: model trzyma **treść** (bez obramowania), a bufor jest o nie większy.
                // XWM działa w osobnym wątku, więc pierwszy `commit` mógł wyprzedzić
                // `WL_SURFACE_SERIAL` — tutaj domykamy różnicę i wymuszamy przeliczenie.
                let b = info.border.min(8) as i32;
                let resized = !is_popup
                    && b > 0
                    && (w.width != (w.data.width as i32 - 2 * b).max(1)
                        || w.height != (w.data.height as i32 - 2 * b).max(1));
                w.title != info.title
                    || w.ssd != info.decorated()
                    || w.popup != is_popup
                    || moved
                    || resized
            };
            changed.then_some((slot, info))
        })
        .collect();
    for (slot, info) in updates {
        let is_popup = info.override_redirect || info.kind == X11Kind::Transient;
        if std::env::var("SELENE_STATS").is_ok() {
            eprintln!(
                "selene/win: X11 zaktualizowane '{}' ssd={} popup={}",
                info.title,
                info.decorated(),
                is_popup
            );
        }
        {
            let mut windows = state.windows.lock_or_recover();
            if let Some(w) = windows.get_mut(slot) {
                w.title = info.title.clone();
                w.ssd = info.decorated();
                w.popup = is_popup;
                // X11: model = treść klienta; bufor rysujemy przesunięty o obramowanie.
                if !is_popup {
                    let b = info.border.min(8) as i32;
                    let (cw, ch) = if b > 0 {
                        (
                            (w.data.width as i32 - 2 * b).max(1),
                            (w.data.height as i32 - 2 * b).max(1),
                        )
                    } else {
                        (w.data.width as i32, w.data.height as i32)
                    };
                    w.width = cw;
                    w.height = ch;
                    w.image_offset = (f64::from(b), f64::from(b));
                }
                // Menu X11 ma współrzędne absolutne — ustawiamy je dokładnie tam,
                // gdzie klient je otworzył (klik w menu wtedy trafia w menu).
                if is_popup && info.width > 0 && info.height > 0 {
                    w.x = info.x as f64;
                    w.y = info.y as f64;
                }
            }
        }
        if let Some(surface) = state.slots.get(slot).cloned() {
            crate::foreign_toplevel::broadcast_title(state, &surface, &info.title);
        }
    }
}

/// Slot okna odpowiadający serialowi powierzchni X11.
fn slot_for_serial(state: &State, serial: u64) -> Option<usize> {
    state.slots.iter().position(|surface| {
        surface
            .data::<SurfaceData>()
            .map(|d| *d.xwayland.lock_or_recover() && *d.x11_serial.lock_or_recover() == serial)
            .unwrap_or(false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn moveresize_directions_map_to_edges() {
        assert_eq!(move_resize_edges(0), 1 | 4); // lewy-górny
        assert_eq!(move_resize_edges(1), 1); // góra
        assert_eq!(move_resize_edges(3), 8); // prawo
        assert_eq!(move_resize_edges(4), 2 | 8); // prawy-dolny
        assert_eq!(move_resize_edges(5), 2); // dół
        assert_eq!(move_resize_edges(7), 4); // lewo
        assert_eq!(move_resize_edges(8), 0); // „move” nie jest zmianą rozmiaru
        assert_eq!(move_resize_edges(99), 0);
    }

    #[test]
    fn toggle_maximize_targets_both_atoms_consistently() {
        // Komunikat Firefoksa: action=2 (toggle), MAX_VERT + MAX_HORZ.
        // Oba atomy muszą dostać ten sam cel — inaczej drugi cofa pierwszy.
        let toggle_maximized = true; // okno było niezmaxymalizowane
        let vertical = net_wm_state_target(2, false, false, toggle_maximized);
        let horizontal = net_wm_state_target(2, false, false, toggle_maximized);
        assert_eq!(vertical, Some(true));
        assert_eq!(horizontal, Some(true));
    }

    #[test]
    fn state_actions_map_to_targets() {
        assert_eq!(net_wm_state_target(0, false, true, true), Some(false));
        assert_eq!(net_wm_state_target(1, false, false, false), Some(true));
        assert_eq!(net_wm_state_target(2, true, true, false), Some(true));
        assert_eq!(net_wm_state_target(2, true, false, true), Some(false));
        assert_eq!(net_wm_state_target(9, false, true, true), None);
    }

    #[test]
    fn normal_windows_are_decorated_but_menus_are_not() {
        let normal = X11Info {
            kind: X11Kind::Normal,
            ..Default::default()
        };
        assert!(normal.decorated());
        let menu = X11Info {
            kind: X11Kind::Transient,
            ..Default::default()
        };
        assert!(!menu.decorated());
        // Override-redirect (np. podpowiedź bez typu) też bez dekoracji.
        let over = X11Info {
            override_redirect: true,
            ..Default::default()
        };
        assert!(!over.decorated());
        // Klient z własnymi dekoracjami (CSD) nie dostaje drugiego paska.
        let csd = X11Info {
            client_decorations: true,
            ..Default::default()
        };
        assert!(!csd.decorated());
    }
}
