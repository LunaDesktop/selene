//! `wl_compositor`, `wl_surface`, `wl_region`, `wl_callback` — rdzeń powierzchni.

use std::sync::Mutex;

use libluna_core::Rect;
use libluna_render::ImageData;
use wayland_protocols::xdg::shell::server::{xdg_popup, xdg_surface, xdg_toplevel};
use wayland_protocols_wlr::layer_shell::v1::server::zwlr_layer_surface_v1;
use wayland_server::{
    protocol::{wl_buffer, wl_callback, wl_compositor, wl_region, wl_surface},
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, WEnum,
};

use crate::layer::LayerData;
use crate::shell::{XdgSurfaceData, XdgToplevelData};
use crate::shm::BufferData;
use crate::state::{State, Window};
use crate::sync::LockExt;

/// Dane powierzchni (`wl_surface`).
pub struct SurfaceData {
    /// Oczekujące `frame`-callbacki do wywołania przy następnym `commit`.
    pub frames: Mutex<Vec<wl_callback::WlCallback>>,
    /// Powiązana powierzchnia `xdg_surface` (jeśli klient ją utworzyła).
    pub xdg_surface: Mutex<Option<xdg_surface::XdgSurface>>,
    /// Ostatnio przypięty bufor (`wl_surface.attach`).
    pub buffer: Mutex<Option<wl_buffer::WlBuffer>>,
    /// Czy bieżący bufor czeka jeszcze na `wl_buffer.release`.
    ///
    /// Klient często robi `commit` bez nowego `attach` (np. po `ack_configure` albo
    /// zmianie regionu wejścia). Wysłanie `release` przy każdym `commit` kończy się
    /// **podwójnym** zwolnieniem bufora — GTK pada wtedy na asercji cairo
    /// (`cairo_surface_destroy`). Zwolnienie musi być dokładnie raz na `attach`.
    pub release_needed: Mutex<bool>,
    /// Poprzedni bufor `dmabuf` tej powierzchni — zwalniamy go przy **następnym**
    /// commicie, gdy GPU na pewno skończyło czytać poprzednią klatkę.
    pub pending_dmabuf_release: Mutex<Option<wl_buffer::WlBuffer>>,
    /// Slot okna w modelu (przydzielany przy pierwszym commicie).
    pub slot: Mutex<Option<usize>>,
    /// Powiązana powierzchnia warstwowa (`wlr-layer-shell`), jeśli klient ją utworzył.
    pub layer: Mutex<Option<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1>>,
    /// Uchwyty `zwlr_foreign_toplevel` (gdy to okno najwyższego poziomu).
    pub toplevel: Mutex<Option<crate::foreign_toplevel::ToplevelInfo>>,
    /// Region wejścia (`wl_surface.set_input_region`); `None` = cała powierzchnia.
    pub input: Mutex<Option<Vec<Rect>>>,
    /// Czy to powierzchnia okna X11 (XWayland) — nie ma `xdg_surface`, a mimo to jest oknem.
    pub xwayland: Mutex<bool>,
    /// Serial powierzchni nadany przez `xwayland_surface_v1.set_serial` — po nim XWM
    /// wiąże okno X11 z tą powierzchnią.
    pub x11_serial: Mutex<u64>,
    /// Rola powierzchni potomnej (`wl_subsurface`): pozycja, tryb `sync`, obrazy.
    pub subsurface: Mutex<Option<crate::subsurface::SubsurfaceState>>,
    /// Powierzchnie potomne w kolejności stosu (od dołu).
    pub children: Mutex<Vec<wayland_server::protocol::wl_surface::WlSurface>>,
    /// Ostatni obraz **własnego** bufora (bez dzieci) — potrzebny do przemalowania
    /// rodzica, gdy dziecko w trybie `desync` zrobi commit.
    pub base_image: Mutex<Option<std::sync::Arc<ImageData>>>,
    /// Czy klient faktycznie rysuje własne dekoracje (CSD). Ustawiane raz, gdy
    /// bufor okaże się większy od geometrii okna o cienie CSD — Chrome/Ecosia
    /// potwierdzają `server_side`, ale i tak rysują własny pasek. Flaga zostaje
    /// na całe życie powierzchni, żeby po maksymalizacji (cienie znikają) nie
    /// wrócił podwójny pasek tytułu.
    pub csd: Mutex<bool>,
}

impl SurfaceData {
    pub fn new() -> Self {
        Self {
            frames: Mutex::new(Vec::new()),
            xdg_surface: Mutex::new(None),
            buffer: Mutex::new(None),
            release_needed: Mutex::new(false),
            pending_dmabuf_release: Mutex::new(None),
            slot: Mutex::new(None),
            layer: Mutex::new(None),
            toplevel: Mutex::new(None),
            input: Mutex::new(None),
            xwayland: Mutex::new(false),
            x11_serial: Mutex::new(0u64),
            subsurface: Mutex::new(None),
            children: Mutex::new(Vec::new()),
            base_image: Mutex::new(None),
            csd: Mutex::new(false),
        }
    }

    /// Zwraca `true` (raz) i czyści flagę — wołający ma wysłać `wl_buffer.release`.
    pub(crate) fn take_release(&self) -> bool {
        let mut f = self.release_needed.lock_or_recover();
        if *f {
            *f = false;
            true
        } else {
            false
        }
    }
}

impl Default for SurfaceData {
    fn default() -> Self {
        Self::new()
    }
}

impl GlobalDispatch<wl_compositor::WlCompositor, ()> for State {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<wl_compositor::WlCompositor>,
        _data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<wl_compositor::WlCompositor, ()> for State {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &wl_compositor::WlCompositor,
        request: wl_compositor::Request,
        _data: &(),
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_compositor::Request::CreateSurface { id } => {
                data_init.init(id, SurfaceData::new());
                state.surfaces += 1;
            }
            wl_compositor::Request::CreateRegion { id } => {
                data_init.init(id, RegionRects::default());
            }
            _ => {}
        }
    }
}

/// Prostokąty regionu `wl_region` (współdzielone; klient może je zmieniać).
pub type RegionRects = std::sync::Arc<Mutex<Vec<Rect>>>;

impl Dispatch<wl_region::WlRegion, RegionRects> for State {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &wl_region::WlRegion,
        request: wl_region::Request,
        rects: &RegionRects,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        let mut r = rects.lock_or_recover();
        match request {
            wl_region::Request::Add {
                x,
                y,
                width,
                height,
            } => r.push(Rect::from_xywh(
                x as f64,
                y as f64,
                width as f64,
                height as f64,
            )),
            // `subtract` upraszczamy (rzadkie w regionach wejścia).
            wl_region::Request::Subtract { .. } => {}
            wl_region::Request::Destroy => r.clear(),
            _ => {}
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for State {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &wl_callback::WlCallback,
        _request: wl_callback::Request,
        _data: &(),
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
    }
}

/// Górny limit oczekujących frame-callbacków na powierzchnię (ochrona przed OOM).
const MAX_PENDING_FRAMES: usize = 256;

impl Dispatch<wl_surface::WlSurface, SurfaceData> for State {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &wl_surface::WlSurface,
        request: wl_surface::Request,
        data: &SurfaceData,
        handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_surface::Request::Frame { callback } => {
                let cb = data_init.init(callback, ());
                let mut frames = data.frames.lock_or_recover();
                // Limit oczekujących callbacków — klient bez `commit` nie wysypie pamięci.
                if frames.len() >= MAX_PENDING_FRAMES {
                    let _ = cb.send_event(wl_callback::Event::Done {
                        callback_data: state.time_ms(),
                    });
                } else {
                    frames.push(cb);
                }
            }
            wl_surface::Request::Attach { buffer, .. } => {
                let mut cur = data.buffer.lock_or_recover();
                let mut need = data.release_needed.lock_or_recover();
                // Poprzedni, jeszcze nieoddany bufor oddajemy klientowi teraz — inaczej
                // jego pula buforów się wyczerpuje i aplikacja staje.
                if *need {
                    if let Some(prev) = cur.take() {
                        let _ = prev.send_event(wl_buffer::Event::Release);
                    }
                }
                *need = buffer.is_some();
                *cur = buffer;
            }
            wl_surface::Request::SetInputRegion { region } => {
                // `None` = cała powierzchnia; inaczej migawka prostokątów regionu.
                *data.input.lock_or_recover() = region
                    .as_ref()
                    .and_then(|r| r.data::<RegionRects>().map(|m| m.lock_or_recover().clone()));
            }
            wl_surface::Request::Commit => {
                // Wywołaj oczekujące frame-callbacki.
                let now = state.time_ms();
                for cb in data.frames.lock_or_recover().drain(..) {
                    let _ = cb.send_event(wl_callback::Event::Done { callback_data: now });
                }
                // Powierzchnia kursora (`wl_pointer.set_cursor`) nie ma roli okna —
                // jej commit tylko podmienia obraz kursora.
                if state.cursor_surface.as_ref().map(Resource::id) == Some(resource.id()) {
                    crate::cursor::on_commit(state, resource, data);
                    return;
                }
                // Pierwszy commit powierzchni z `xdg_surface` → wyślij konfigurację.
                maybe_configure_xdg(state, data);
                // Powierzchnia potomna nie tworzy okna — jej bufor składamy na rodzicu.
                if data.subsurface.lock_or_recover().is_some() {
                    crate::subsurface::commit(state, data);
                } else {
                    // Skomponuj okno (jeśli to toplevel z buforem).
                    composite(state, resource, data, handle);
                }
            }
            wl_surface::Request::Destroy => {
                // Oddaj bufor, jeśli jeszcze go trzymamy.
                if data.take_release() {
                    if let Some(b) = data.buffer.lock_or_recover().take() {
                        let _ = b.send_event(wl_buffer::Event::Release);
                    }
                }
            }
            _ => {}
        }
    }

    /// Gdy powierzchnia znika — bo klient ją zniszczył **albo padł** (crash, `kill`) —
    /// chowamy okno i zgłaszamy zamknięcie paskowi. Bez tego na ekranie zostaje
    /// „duch”: nieruchomy pasek tytułu i treść po zamkniętej aplikacji.
    fn destroyed(
        state: &mut Self,
        _client: wayland_server::backend::ClientId,
        resource: &wl_surface::WlSurface,
        data: &SurfaceData,
    ) {
        // Kursor znika razem z powierzchnią klienta — wracamy do domyślnej strzałki.
        if state.cursor_surface.as_ref().map(Resource::id) == Some(resource.id()) {
            crate::cursor::surface_destroyed(state);
        }
        // Powierzchnia potomna znika — odczep ją od rodzica i przemaluj go.
        if let Some(sub) = data.subsurface.lock_or_recover().take() {
            if let Some(pd) = sub.parent.data::<SurfaceData>() {
                pd.children.lock_or_recover().retain(|s| s != resource);
            }
            crate::subsurface::repaint_parent(state, &sub.parent);
        }
        // Okno w modelu chowamy (nie usuwamy — indeks slotu musi zostać stabilny).
        // Powierzchnia warstwowa może mieć kilka slotów (np. ekran blokady rysowany
        // na wszystkich monitorach), więc zbieramy je wszystkie z `LayerData`.
        let layer_guard = data.layer.lock_or_recover();
        let layer_data = layer_guard
            .as_ref()
            .and_then(|ls| ls.data::<crate::layer::LayerData>());
        let was_lock = layer_data
            .map(|ld| {
                let st = *ld.state.lock_or_recover();
                st.layer == 3 && st.keyboard_interactivity == crate::layer::KEYBOARD_EXCLUSIVE
            })
            .unwrap_or(false);
        let mut slots: Vec<usize> = layer_data
            .map(|ld| {
                ld.slots
                    .lock_or_recover()
                    .iter()
                    .flatten()
                    .copied()
                    .collect()
            })
            .unwrap_or_default();
        drop(layer_guard);
        if was_lock {
            state.locked = false;
            state.reset_corner();
        }
        if let Some(slot) = *data.slot.lock_or_recover() {
            if !slots.contains(&slot) {
                slots.push(slot);
            }
        }
        for slot in slots {
            let was_popup = state
                .windows
                .lock_or_recover()
                .get(slot)
                .map(|w| w.popup)
                .unwrap_or(false);
            let was_focused = state.keyboard_focus.as_ref().map(|(s, _)| *s) == Some(slot);
            if let Some(win) = state.windows.lock_or_recover().get_mut(slot) {
                win.mapped = false;
                win.minimized = false;
                win.maximized = false;
                win.fullscreen = false;
                win.input_capture = false;
                win.input = None;
                // Zwolnij bufor ostatniej klatki (może mieć kilka MB).
                win.data = std::sync::Arc::new(ImageData::empty(1, 1));
            }
            // Fokus nie może wskazywać na powierzchnię, której już nie ma.
            crate::seat::clear_focus_if(state, slot);
            // Menu zniknęło — klawiatura wraca do okna rodzica, a nie zostaje „martwa”.
            if was_focused && was_popup {
                crate::seat::refocus_after_popup(state);
            }
            if std::env::var("SELENE_STATS").is_ok() {
                eprintln!("selene/win: powierzchnia zniszczona slot={slot}");
            }
        }
        // Pasek (foreign-toplevel) dowiaduje się, że okno zniknęło.
        crate::foreign_toplevel::close_for_data(data);
    }
}

/// Czy klient znany z rysowania własnych dekoracji na Waylandzie.
///
/// Chromium (Chrome, Ecosia, Edge, Brave…) potwierdza `server_side`, ale i tak
/// rysuje własny pasek z przyciskami. Dodatkowy pasek zasłaniał ich kontrolki
/// i przechwytywał kliknięcia w menu ⋮. Rozpoznajemy po `app_id`/tytule, żeby
/// działało też dla okien startujących od razu zmaksymalizowanych (bez cieni CSD).
fn known_csd_client(app_id: &str, title: &str) -> bool {
    let id = app_id.to_ascii_lowercase();
    const IDS: &[&str] = &[
        "google-chrome",
        "chromium",
        "ecosia",
        "microsoft-edge",
        "brave-browser",
        "vivaldi",
        "opera",
    ];
    if IDS.iter().any(|needle| id.contains(needle)) {
        return true;
    }
    let title = title.to_ascii_lowercase();
    [
        "google chrome",
        "ecosia browser",
        "microsoft edge",
        "brave browser",
        "vivaldi",
    ]
    .iter()
    .any(|needle| title.contains(needle))
}

/// Czy rysujemy nasze dekoracje (pasek tytułu) dla tego okna.
///
/// * popupy — nigdy (zamykane, bez paska),
/// * okna X11 — tak, chyba że klient wyraźnie poprosił o `client_side`,
/// * okna Waylanda — tylko gdy negocjowały `server_side`.
fn wants_ssd(is_x11: bool, is_popup: bool, deco: Option<crate::decoration::DecoMode>) -> bool {
    use crate::decoration::DecoMode;
    if is_popup {
        false
    } else if is_x11 {
        deco != Some(DecoMode::Client)
    } else {
        deco == Some(DecoMode::Server)
    }
}

/// Skomponuj powierzchnię do modelu okien (toplevel, popup lub powierzchnia warstwowa).
fn composite(
    state: &mut State,
    surface: &wl_surface::WlSurface,
    data: &SurfaceData,
    handle: &DisplayHandle,
) {
    // Powierzchnia warstwowa (panel/dock) — własna ścieżka (bez xdg_surface).
    if let Some(ls) = data.layer.lock_or_recover().clone() {
        composite_layer(state, surface, data, &ls);
        return;
    }
    // Okno X11 (XWayland) rozpoznajemy po znaczniku z `xwayland_shell_v1`, ale tylko gdy
    // nie jest to `xdg_popup` — popupy Xwaylanda (menu, podpowiedzi) mają własny
    // `xdg_surface` i idą zwykłą ścieżką Waylanda.
    let xdg_surface = data.xdg_surface.lock_or_recover().clone();
    let xdg_is_popup = xdg_surface
        .as_ref()
        .and_then(|xs| xs.data::<XdgSurfaceData>())
        .map(|d| *d.is_popup.lock_or_recover())
        .unwrap_or(false);
    let is_x11 = *data.xwayland.lock_or_recover() && !xdg_is_popup;
    // Okna X11: brak `xdg_surface`, ale mamy XWM — z niego bierzemy tytuł, typ okna
    // i informację, czy klient rysuje własne dekoracje (CSD przez `_MOTIF_WM_HINTS`).
    let mut x11_title = String::new();
    let mut x11_popup = false;
    let mut x11_ssd = true;
    let mut x11_pos: Option<(f64, f64)> = None;
    // Szerokość obramowania X11: bufor Waylanda je zawiera, a model ma opisywać treść.
    let mut x11_border = 0u32;
    // Szerokość treści z `ConfigureNotify` — tylko podpowiedź dla wyliczenia obramowania.
    let mut x11_content_w = 0i32;
    if is_x11 {
        let serial = *data.x11_serial.lock_or_recover();
        if let Some(info) = crate::xwm::global().and_then(|x| x.info_for_serial(serial)) {
            // Menu i podpowiedzi (`override_redirect`/`Transient`) to popupy — bez
            // dekoracji i bez wpisu w panelu. Okno z własnymi dekoracjami (CSD) jest
            // zwykłym oknem, tylko nie dokładamy mu drugiego paska tytułu.
            x11_popup = info.override_redirect || info.kind == crate::xwm::X11Kind::Transient;
            x11_ssd = info.decorated();
            x11_title = info.title;
            x11_border = info.border;
            x11_content_w = info.width;
            // Menu X11 ma współrzędne absolutne — stawiamy je tam, gdzie klient chce,
            // a nie w kaskadzie (inaczej klik w widoczne menu trafia obok).
            if x11_popup && info.width > 0 && info.height > 0 {
                x11_pos = Some((info.x as f64, info.y as f64));
            }
        }
    }
    let (is_toplevel, is_popup) = if is_x11 {
        (true, x11_popup)
    } else {
        let Some(xdg_surface) = xdg_surface.as_ref() else {
            return;
        };
        match xdg_surface.data::<XdgSurfaceData>() {
            Some(d) => (
                d.toplevel.lock_or_recover().is_some(),
                *d.is_popup.lock_or_recover(),
            ),
            None => (false, false),
        }
    };
    if !is_toplevel && !is_popup {
        return;
    }
    let Some(buffer) = data.buffer.lock_or_recover().clone() else {
        return;
    };
    let Some(buf_data) = buffer.data::<BufferData>() else {
        return;
    };
    let (bw, bh) = (buf_data.width, buf_data.height);
    // Bufory GPU (`dmabuf`) nie mają pikseli na CPU: trzymamy tylko atrapę 1×1,
    // a rozmiar obrazu bierze `Window::image_size()` z `DmabufImage`.
    let dmabuf = buf_data.dmabuf.clone();
    let image = if dmabuf.is_some() {
        std::sync::Arc::new(ImageData::empty(1, 1).with_sparse(true))
    } else {
        let Some((rgba, opaque)) = buf_data.to_rgba_with_opacity() else {
            return;
        };
        std::sync::Arc::new(ImageData::new(bw as u32, bh as u32, rgba).with_opaque(opaque))
    };
    // Powierzchnie potomne (`wl_subsurface`) składamy na własny bufor rodzica:
    // najpierw aplikujemy stan oczekujący (`sync`), potem nakładamy obrazy dzieci.
    // Dla `dmabuf` nie ma bufora CPU, więc dzieci pomijamy.
    crate::subsurface::apply_pending(surface);
    let image = if dmabuf.is_some() {
        image
    } else {
        *data.base_image.lock_or_recover() = Some(image.clone());
        crate::subsurface::overlay_children(surface, image)
    };
    let z = state.next_z();
    let title = if is_x11 {
        x11_title
    } else if is_toplevel {
        xdg_surface
            .as_ref()
            .and_then(|xs| xs.data::<XdgSurfaceData>())
            .and_then(|d| {
                d.toplevel.lock_or_recover().as_ref().and_then(|tl| {
                    tl.data::<XdgToplevelData>()
                        .map(|td| td.title.lock_or_recover().clone())
                })
            })
            .unwrap_or_default()
    } else {
        String::new()
    };

    let count = cascade_slots(&state.windows.lock_or_recover());
    // Tryb dekoracji zgłoszony przez klienta (jeśli ma obiekt `zxdg_toplevel_decoration_v1`).
    // Okna X11 też mają `xdg_toplevel` (tworzy go Xwayland), więc czytamy go tak samo.
    let deco_mode = xdg_surface
        .as_ref()
        .and_then(|xs| xs.data::<XdgSurfaceData>())
        .and_then(|d| {
            let tl = d.toplevel.lock_or_recover();
            tl.as_ref()
                .and_then(|tl| tl.data::<XdgToplevelData>())
                .map(|td| *td.deco.lock_or_recover())
        })
        .flatten();
    // `app_id` klienta — potrzebne do rozpoznania przeglądarek z własnymi dekoracjami.
    let app_id = xdg_surface
        .as_ref()
        .and_then(|xs| xs.data::<XdgSurfaceData>())
        .and_then(|d| {
            d.toplevel
                .lock_or_recover()
                .as_ref()
                .and_then(|tl| tl.data::<XdgToplevelData>())
                .map(|td| td.app_id.lock_or_recover().clone())
        })
        .unwrap_or_default();
    // Nasze dekoracje (SSD, tj. pasek tytułu) rysujemy:
    //  * oknom X11 — Xwayland sam nic nie rysuje (czeka na menedżera okien),
    //  * klientom Waylanda, które negocjowały tryb `server_side`.
    // Gdy klient X11 wyraźnie poprosił o `client_side` (`_MOTIF_WM_HINTS`), ustępujemy —
    // bez dublowania paska.
    let ssd = if is_x11 {
        x11_ssd
    } else {
        wants_ssd(false, is_popup, deco_mode)
    };
    // X11: Chrome i pochodne domyślnie rysują własny pasek i nie zawsze ustawiają
    // `_MOTIF_WM_HINTS` przed pierwszym mapowaniem — nie dokładamy im drugiego.
    let mut ssd = ssd;
    if is_x11 && ssd && !is_popup && known_csd_client("", &title) {
        ssd = false;
    }
    // Widoczna geometria (`xdg_surface.set_window_geometry`). Okna z własnymi
    // dekoracjami (GTK/libadwaita, Firefox, Electron) mają bufor większy od okna
    // o cienie CSD. Model trzyma **widoczny** prostokąt (to on idzie w `configure`
    // i wyznacza trafienia), a bufor rysujemy przesunięty o `off`.
    let ((off_x, off_y), (mut w, mut h)) = if !is_x11 {
        let geom = xdg_surface
            .as_ref()
            .and_then(|xs| xs.data::<XdgSurfaceData>())
            .and_then(|d| *d.window_geometry.lock_or_recover());
        match geom {
            // Geometria spoza bufora bywa skutkiem wyścigu (klient ustawia ją przed
            // `attach`) — bierzemy tylko sensowne wartości.
            Some(g) if g.width() > 0.0 && g.height() > 0.0 => (
                (g.x(), g.y()),
                (
                    (g.width() as i32).min(bw).max(1),
                    (g.height() as i32).min(bh).max(1),
                ),
            ),
            _ => ((0.0, 0.0), (bw, bh)),
        }
    } else if !x11_popup {
        // Okno X11: model = treść klienta, bufor może być większy o obramowanie X11.
        // Rysujemy go przesuniętego o `border`, dzięki czemu obramowanie zostaje
        // na ekranie, a okno nie przekracza obszaru roboczego po maksymalizacji.
        //
        // XWM działa w osobnym wątku, więc przy pierwszej klatce `info.border` może
        // być jeszcze 0. Wtedy wyprowadzamy je z różnicy: Xwayland wlicza obramowanie
        // do bufora, więc `(bw - treść) / 2` to `border_width`. Guard na różnicę chroni
        // przed nieświeżym `ConfigureNotify` po zmianie rozmiaru (skok o setki px).
        let derived = if x11_content_w > 0 && bw > x11_content_w && bw - x11_content_w <= 16 {
            ((bw - x11_content_w) / 2) as u32
        } else {
            0
        };
        let b = x11_border.max(derived).min(8) as i32;
        if b > 0 {
            (
                (f64::from(b), f64::from(b)),
                ((bw - 2 * b).max(1), (bh - 2 * b).max(1)),
            )
        } else {
            ((0.0, 0.0), (bw, bh))
        }
    } else {
        ((0.0, 0.0), (bw, bh))
    };
    // Klient, który mimo negocjacji `server_side` rysuje własne dekoracje, ma bufor
    // większy od geometrii okna o cienie CSD (`image_offset`). Dokładanie naszego
    // paska dawało wtedy **podwójny tytuł**, a jego strefa przechwytywała kliknięcia
    // w przyciski klienta (Chrome/Ecosia: menu ⋮, ustawienia, minimalizacja).
    // Wykrywamy to raz i pamiętamy na całe życie powierzchni — przy maksymalizacji
    // cienie znikają, ale pasek klienta zostaje.
    if ssd
        && !is_x11
        && !is_popup
        && (off_x > 0.5 || off_y > 0.5 || known_csd_client(&app_id, &title))
    {
        *data.csd.lock_or_recover() = true;
    }
    let ssd = ssd && !*data.csd.lock_or_recover();
    // Popup: pełne pozycjonowanie wg `xdg_positioner` (kotwica, grawitacja, flip/slide).
    // Menu i palety poleceń (Zed, VS Code, GTK) dostają wreszcie właściwe miejsce.
    let popup_pos = if is_popup {
        xdg_surface
            .as_ref()
            .map(|xs| popup_position(state, xs, (w as f64, h as f64)))
    } else {
        None
    };
    let (px, py) = match &popup_pos {
        Some((pos, _)) => *pos,
        None => x11_pos.unwrap_or_else(|| cascade_position(state, count)),
    };
    // Nowe okno nie może być większe niż obszar roboczy monitora — inaczej jego dolna
    // część (i przyciski klienta) ląduje poza ekranem. Klient dostanie mniejszy
    // `configure` i dorysuje treść w nowym rozmiarze.
    let mut shrunk = false;
    let new_window = data.slot.lock_or_recover().is_none();
    if new_window && !is_popup {
        let bar = if ssd { crate::state::TITLEBAR } else { 0.0 };
        let (ox, oy, ow, oh) = {
            let outs = state.outputs.lock_or_recover();
            let idx = crate::monitors::output_at(&outs, px, py);
            match outs.get(idx).or_else(|| outs.first()) {
                Some(o) => (o.x as f64, o.y as f64, o.width as f64, o.height as f64),
                None => (0.0, 0.0, 1280.0, 800.0),
            }
        };
        // Miejsce od pozycji okna do dolnej/prawej krawędzi obszaru roboczego —
        // samo ograniczenie do wysokości monitora nie wystarczy, bo okno startuje
        // kilkadziesiąt pikseli niżej (kaskada pod panelem).
        let max_w = (ox + ow - px).max(160.0);
        let max_h = (oy + oh - py - bar).max(120.0);
        let fit_w = (f64::from(w)).min(max_w) as i32;
        let fit_h = (f64::from(h)).min(max_h) as i32;
        if fit_w != w || fit_h != h {
            shrunk = true;
            w = fit_w;
            h = fit_h;
        }
    }
    // Stan okna **przed** aktualizacją — po niej `upsert_window` nadpisuje rozmiar,
    // więc porównanie „czy geometria się zmieniła” musi być zrobione wcześniej.
    let prev_geom = data.slot.lock_or_recover().and_then(|i| {
        state.windows.lock_or_recover().get(i).map(|win| {
            (
                win.width,
                win.height,
                win.data.width as i32,
                win.data.height as i32,
                win.image_offset,
            )
        })
    });
    let (slot, created) = upsert_window(
        state, surface, data, px, py, w, h, z, &title, is_popup, false, false, ssd, image,
    );
    // Bufor rysujemy z przesunięciem cieni CSD — bez tego okno „uciekało” o kilkanaście
    // pikseli, a po maksymalizacji prawa krawędź z przyciskami ✕/□ wychodziła za ekran.
    let mut pos = (px, py);
    if let Some(win) = state.windows.lock_or_recover().get_mut(slot) {
        win.image_offset = (off_x, off_y);
        // Bufor GPU bywa zmieniany na shm i odwrotnie — pole musi być aktualne.
        win.dmabuf = dmabuf.clone();
        pos = (win.x, win.y);
    }
    // Popup musi leżeć **nad** rodzicem także po ponownym pokazaniu. Klient często
    // używa tej samej powierzchni menu; kliknięcie w okno podnosi je wyżej, a menu
    // zostaje pod spodem — widać je, ale żaden klik w niego nie dociera.
    if is_popup {
        let parent_slot = xdg_surface
            .as_ref()
            .and_then(|xs| xs.data::<XdgSurfaceData>())
            .and_then(|d| d.parent.lock_or_recover().clone())
            .and_then(|parent| {
                parent
                    .data::<SurfaceData>()
                    .and_then(|sd| *sd.slot.lock_or_recover())
            });
        if let Some(parent_slot) = parent_slot {
            let candidate = state.next_z();
            let mut windows = state.windows.lock_or_recover();
            let parent_z = windows.get(parent_slot).map(|w| w.z).unwrap_or(i32::MIN);
            if let Some(win) = windows.get_mut(slot) {
                if win.z <= parent_z {
                    win.z = candidate;
                }
            }
        }
    }
    if created {
        if let Some(db) = &dmabuf {
            if std::env::var("SELENE_STATS").is_ok() {
                eprintln!(
                    "selene/dmabuf: okno \"{title}\" {}x{} format=0x{:08x} modifier=0x{:016x}",
                    db.width, db.height, db.format, db.modifier
                );
            }
        }
    }
    let geom = (w, h, bw, bh, (off_x, off_y));
    if prev_geom.is_some() && prev_geom != Some(geom) && std::env::var("SELENE_STATS").is_ok() {
        eprintln!(
            "selene/win: geometria \"{title}\" widoczne={}x{} bufor={}x{} @ ({:.0},{:.0}) cien=({:.0},{:.0})",
            w, h, bw, bh, pos.0, pos.1, off_x, off_y
        );
    }
    // Diagnostyka: co robimy z danym oknem (X11 czy Wayland, tryb dekoracji, czy nasz pasek).
    if created && std::env::var("SELENE_STATS").is_ok() {
        let geom = state
            .windows
            .lock_or_recover()
            .get(slot)
            .map(|win| (win.x, win.y, win.width, win.height));
        if let Some((x, y, cw, ch)) = geom {
            eprintln!(
                "selene/win: \"{}\" x11={} popup={} deco={:?} ssd={} widoczne={}x{} @ ({:.0},{:.0}) cien=({:.0},{:.0})",
                title, is_x11, is_popup, deco_mode, ssd, cw, ch, x, y, off_x, off_y
            );
        }
    }
    // Xwayland **nie tworzy** `xdg_toplevel`, więc okna X11 nie są rejestrowane
    // w `foreign_toplevel` przez `shell.rs` — a bez tego nie ma ich w pasku zadań.
    // Rejestrujemy je tutaj (tytuł i stan aktualizuje później `xwm::sync`).
    if is_x11 && created && !is_popup {
        let first = {
            let mut t = data.toplevel.lock_or_recover();
            let first = t.is_none();
            if first {
                *t = Some(crate::foreign_toplevel::ToplevelInfo {
                    title: title.clone(),
                    ..Default::default()
                });
            }
            first
        };
        if first {
            let managers = state.toplevel_managers.clone();
            for mgr in &managers {
                crate::foreign_toplevel::announce(state, handle, mgr, surface);
            }
        }
    }
    // Popup: wyślij konfigurację (rozmiar + pozycję **względem geometrii rodzica**).
    // Zwykle zrobiliśmy to już przy pierwszym commicie bez bufora; tutaj tylko
    // awaryjnie dla klientów, które od razu podpinają bufor.
    if is_popup && created {
        let configured = xdg_surface
            .as_ref()
            .and_then(|xs| xs.data::<XdgSurfaceData>())
            .map(|d| *d.configured.lock_or_recover())
            .unwrap_or(false);
        if !configured {
            if let (Some(xs), Some((abs, origin))) = (xdg_surface.as_ref(), popup_pos) {
                send_popup_configure(state, xs, (abs.0 - origin.0, abs.1 - origin.1), w, h);
            }
        }
    }
    // Okno X11: powiedz klientowi, ile miejsca zabiera nasz pasek tytułu
    // (`_NET_FRAME_EXTENTS`) — dzięki temu sam ustawia treść pod dekoracją.
    if is_x11 && created && !is_popup {
        let serial = *data.x11_serial.lock_or_recover();
        if let Some(xwm) = crate::xwm::global() {
            if let Some(win) = xwm.window_for_serial(serial) {
                // Tylko okna z naszym paskiem mają ramkę; CSD same pilnują swoich marginesów.
                let top = if ssd { crate::xwm::FRAME_TOP } else { 0 };
                xwm.set_frame_extents(win, top);
                // Zsynchronizuj geometrię okna X11 z modelem: X11 nie zna naszych
                // dekoracji, więc treść zaczyna się `top` pikseli niżej.
                xwm.set_geometry(
                    win,
                    px as i32,
                    (py + f64::from(top)) as i32,
                    w as u32,
                    h as u32,
                );
            }
        }
    } else if shrunk {
        // Wayland: nowy rozmiar treści (xdg_toplevel.configure).
        crate::seat::send_configure(state, slot, f64::from(w), f64::from(h));
    }
    // Pozwól klientowi ponownie użyć bufora — **raz na `attach`**, nie na każdy `commit`.
    // Dla `dmabuf` zwalniamy bufor **następny** raz: GPU czyta poprzednią klatkę
    // jeszcze po `commit`, więc natychmiastowy `release` dałby tearing.
    if dmabuf.is_some() {
        let old = data
            .pending_dmabuf_release
            .lock_or_recover()
            .replace(buffer.clone());
        if let Some(old) = old {
            let _ = old.send_event(wl_buffer::Event::Release);
        }
    } else if data.take_release() {
        let _ = buffer.send_event(wl_buffer::Event::Release);
    }
}

/// Pozycja nowego okna (kaskada w obszarze roboczym pierwszego monitora).
///
/// Klasyczna kaskada `60 + n*36` po kilkunastu oknach wyjeżdżała poza ekran —
/// pasek tytułu lądował pod dolną krawędzią i nie dało się go złapać. Zawijamy
/// kaskadę w siatkę, która zawsze mieści się w obszarze roboczym (i pod panelem).
/// Liczba okien branych pod uwagę przy kaskadowym układaniu nowych okien.
///
/// Tylko **widoczne** okna wyznaczają kaskadę. Sloty zamkniętych i zminimalizowanych
/// okien zostają w modelu (indeks musi być stabilny), więc liczenie ich wszystkich
/// przesuwało każde nowe okno coraz dalej w prawo/dół.
pub(crate) fn cascade_slots(windows: &[crate::state::Window]) -> usize {
    windows
        .iter()
        .filter(|w| w.mapped && !w.layer && !w.popup)
        .count()
}

fn cascade_position(state: &State, count: usize) -> (f64, f64) {
    let (ox, oy, ow, oh) = {
        let outs = state.outputs.lock_or_recover();
        match outs.first() {
            Some(o) => (o.x as f64, o.y as f64, o.width as f64, o.height as f64),
            None => (0.0, 0.0, 1280.0, 800.0),
        }
    };
    let step = 36.0;
    let cols = 6usize;
    let x = (60.0 + (count % cols) as f64 * step).min((ow - 280.0).max(0.0));
    let top = oy
        + state
            .top_insets
            .get(&0)
            .copied()
            .unwrap_or(0.0)
            .min(oh - 200.0);
    let y = (top + 20.0 + ((count / cols) % 5) as f64 * step).min((oy + oh - 200.0).max(top));
    (ox + x, y)
}

/// Skomponuj powierzchnię warstwową (`wlr-layer-shell`): konfiguracja + kotwiczenie.
fn composite_layer(
    state: &mut State,
    surface: &wl_surface::WlSurface,
    data: &SurfaceData,
    ls: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1,
) {
    let Some(ld) = ls.data::<LayerData>() else {
        return;
    };
    let out_idx = ld.output.unwrap_or(0);
    // `configure` liczymy dla monitora, do którego powierzchnia jest przypisana (albo
    // pierwszego, gdy `output=None`); samo rysowanie robimy niżej dla każdego wyjścia.
    let (_, _, out_w, out_h) = output_rect(state, out_idx);
    let st = *ld.state.lock_or_recover();
    // Ekran blokady (`luna-lock`) jest jedną powierzchnią na wszystkie monitory:
    // overlay + klawiatura wyłączna, `output=None`. Bufor ma rozmiar największego
    // wyjścia, żeby pokryć także ekrany o innej rozdzielczości.
    let outputs = state.outputs.lock_or_recover().clone();
    let lock_mirror = ld.output.is_none()
        && st.layer == 3
        && st.keyboard_interactivity == crate::layer::KEYBOARD_EXCLUSIVE;
    let (cfg_w, cfg_h) = if lock_mirror {
        outputs.iter().fold((0.0_f64, 0.0_f64), |(w, h), o| {
            (w.max(o.width as f64), h.max(o.height as f64))
        })
    } else {
        (out_w, out_h)
    };
    let buffer = data.buffer.lock_or_recover().clone();
    let debug = std::env::var("SELENE_STATS").is_ok();
    // Pierwszy `commit` (zwykle bez bufora) → wyślij `configure` z rozmiarem.
    if !*ld.configured.lock_or_recover() {
        let (cw, ch) = st.configure_size(cfg_w, cfg_h);
        let serial = state.next_serial();
        let _ = ls.send_event(zwlr_layer_surface_v1::Event::Configure {
            serial,
            width: cw.max(0) as u32,
            height: ch.max(0) as u32,
        });
        *ld.configured.lock_or_recover() = true;
        if debug {
            eprintln!("selene/layer: configure {}x{}", cw.max(0), ch.max(0));
        }
        if buffer.is_none() {
            return;
        }
    }
    let Some(buffer) = buffer else {
        return;
    };
    let Some(buf_data) = buffer.data::<BufferData>() else {
        if debug {
            eprintln!("selene/layer: commit bez BufferData");
        }
        return;
    };
    let (w, h) = (buf_data.width, buf_data.height);
    let Some(rgba) = buf_data.to_rgba() else {
        if debug {
            eprintln!(
                "selene/layer: bufor odrzucony (to_rgba None) {}x{} stride={} offset={}",
                buf_data.width, buf_data.height, buf_data.stride, buf_data.offset
            );
        }
        return;
    };
    let input_rects = opaque_input_rects(&rgba, w as usize, h as usize);
    let active_h = input_rects
        .iter()
        .map(|r| r.bottom() as usize)
        .max()
        .unwrap_or(0);
    // Jeśli dolna część bufora warstwy jest całkowicie przezroczysta (np. pasek 40 px na buforze 1080p),
    // przytnij bufor w miejscu do faktycznie używanej wysokości — zapobiega to blendowaniu 2 mln pustych pikseli.
    let (effective_h, final_rgba) = if active_h > 0 && active_h < h as usize {
        let mut r = rgba;
        r.truncate(w as usize * active_h * 4);
        (active_h as i32, r)
    } else {
        (h, rgba)
    };
    // Panele/popupy mają dużo w pełni przezroczystych obszarów; wtedy renderer
    // może pomijać `alpha == 0` zamiast blendować cały prostokąt.
    let covered: f64 = input_rects.iter().map(|r| r.width() * r.height()).sum();
    let total = (w as f64) * (effective_h as f64);
    let image = std::sync::Arc::new(
        ImageData::new(w as u32, effective_h as u32, final_rgba)
            .with_sparse(total > 0.0 && covered < total * 0.75),
    );
    let z = crate::layer::layer_z(st.layer);
    // Strefa wyłączna górnego panelu → obszar roboczy. Bez tego maksymalizowane okna
    // zaczynałyby się pod panelem, chowając własny pasek tytułu. Zapamiętujemy ją
    // **per monitor** — panel jest tylko na jednym wyjściu.
    let out_idx = ld.output.unwrap_or(0);
    if st.layer == 2 && st.anchor & crate::layer::ANCHOR_TOP != 0 && st.exclusive_zone > 0 {
        let inset = state.top_insets.entry(out_idx).or_insert(0.0);
        let next = inset.max(st.exclusive_zone as f64);
        if next != *inset && std::env::var("SELENE_STATS").is_ok() {
            eprintln!(
                "selene/layer: panel rezerwuje {:.0} px na wyjściu {out_idx}",
                st.exclusive_zone
            );
        }
        *inset = next;
    }
    // Jeden klient = jedna powierzchnia na **jednym** monitorze. Panele na kolejnych
    // monitorach to osobne instancje powłoki (każda wiąże swój `wl_output`). Wyjątkiem
    // jest ekran blokady: jedna powierzchnia overlay z fokusem wyłącznym (`luna-lock`)
    // jest rysowana na wszystkich wyjściach, żeby blokada obejmowała cały pulpit.
    // Region wejścia: klient (jeśli ustawił) albo nasz z kanału alfa — w układzie bufora.
    // Pusty jawny region = „wróć do alfy”; region pokrywający całą powierzchnię =
    // przechwytywanie kliknięć (otwarty popup powłoki).
    let explicit_input = data.input.lock_or_recover().clone();
    let (base_input, input_capture): (Vec<Rect>, bool) = match explicit_input {
        Some(rects) if !rects.is_empty() => {
            let capture = rects.iter().any(|r| covers_surface(r, w as f64, h as f64));
            (rects, capture)
        }
        _ => (input_rects.clone(), false),
    };
    let mut slots = ld.slots.lock_or_recover();
    let targets: Vec<usize> = if lock_mirror {
        (0..outputs.len()).collect()
    } else {
        vec![out_idx]
    };
    slots.resize(targets.len(), None);
    let mut first_slot = None;
    for (i, target) in targets.iter().enumerate() {
        let (ox, oy, out_w, out_h) = output_rect(state, *target);
        let (rx, ry) = st.position(out_w, out_h, w as f64, effective_h as f64);
        let (slot, created) = upsert_window_at(
            state,
            &mut slots[i],
            surface,
            ox + rx,
            oy + ry,
            w,
            effective_h,
            z,
            "",
            false,
            true,
            true,
            false,
            image.clone(),
            Some(base_input.clone()),
            input_capture,
        );
        first_slot.get_or_insert(slot);
        if debug && created {
            eprintln!(
                "selene/layer: okno utworzone {}x{} @ ({:.0},{:.0}) na wyjściu {}, z={}",
                w,
                effective_h,
                ox + rx,
                oy + ry,
                target,
                z
            );
        }
    }
    let slot = first_slot.unwrap_or(usize::MAX);
    drop(slots);
    // `SurfaceData.slot` — używane m.in. przy `destroy` powierzchni warstwowej.
    if slot != usize::MAX {
        if let Some(sd) = surface.data::<SurfaceData>() {
            *sd.slot.lock_or_recover() = Some(slot);
        }
    }
    // Ekran blokady ma fokus klawiatury od razu po zmapowaniu (bez kliknięcia).
    if st.layer == 3
        && st.keyboard_interactivity == crate::layer::KEYBOARD_EXCLUSIVE
        && slot != usize::MAX
    {
        state.locked = true;
        state.set_overview(false);
        state.reset_corner();
        crate::seat::set_keyboard_focus(state, slot);
    }
    if data.take_release() {
        let _ = buffer.send_event(wl_buffer::Event::Release);
    }
}

/// Czy prostokąt pokrywa całą powierzchnię (z tolerancją na zaokrąglenia).
fn covers_surface(r: &Rect, w: f64, h: f64) -> bool {
    r.x() <= 0.5 && r.y() <= 0.5 && r.right() >= w - 0.5 && r.bottom() >= h - 0.5
}

/// Region wejścia z kanału alfa: nieprzezroczyste bloki scalane poziomo w prostokąty.
fn opaque_input_rects(rgba: &[u8], iw: usize, ih: usize) -> Vec<Rect> {
    const BLOCK: usize = 16;
    const ALPHA_THRESHOLD: u8 = 8;
    if iw == 0 || ih == 0 || rgba.len() < iw * ih * 4 {
        return Vec::new();
    }
    let bw = iw.div_ceil(BLOCK);
    let bh = ih.div_ceil(BLOCK);
    let mut rects = Vec::new();
    for by in 0..bh {
        let y0 = by * BLOCK;
        let y1 = (y0 + BLOCK).min(ih);

        // Szybki filtr: jeśli żaden piksel w rzędzie y0..y1 nie przekracza progu alfy,
        // cały rząd 120 bloków pomijamy natychmiast (ogromne przyspieszenie dla 95% ekranu).
        let mut row_has_alpha = false;
        'check_row: for y in y0..y1 {
            let row = y * iw * 4;
            let slice = &rgba[row..row + iw * 4];
            for p in slice.chunks_exact(4) {
                if p[3] > ALPHA_THRESHOLD {
                    row_has_alpha = true;
                    break 'check_row;
                }
            }
        }
        if !row_has_alpha {
            continue;
        }

        let mut run: Option<usize> = None;
        for bx in 0..bw {
            let x0 = bx * BLOCK;
            let x1 = (x0 + BLOCK).min(iw);
            let mut opaque = false;
            'scan: for y in y0..y1 {
                let row = y * iw * 4;
                for x in x0..x1 {
                    if rgba[row + x * 4 + 3] > ALPHA_THRESHOLD {
                        opaque = true;
                        break 'scan;
                    }
                }
            }
            match (run, opaque) {
                (None, true) => run = Some(bx),
                (Some(s), false) => {
                    rects.push(block_run(s, bx, by, BLOCK, iw, ih));
                    run = None;
                }
                _ => {}
            }
        }
        if let Some(s) = run {
            rects.push(block_run(s, bw, by, BLOCK, iw, ih));
        }
    }
    rects
}

/// Prostokąt dla ciągu bloków `[bx0, bx1)` w rzędzie `by`.
fn block_run(bx0: usize, bx1: usize, by: usize, block: usize, iw: usize, ih: usize) -> Rect {
    let x = bx0 * block;
    let y = by * block;
    let w = (bx1 * block).min(iw).saturating_sub(x);
    let h = (y + block).min(ih) - y;
    Rect::from_xywh(x as f64, y as f64, w as f64, h as f64)
}

#[cfg(test)]
mod input_region_tests {
    use super::*;

    #[test]
    fn opaque_rects_track_alpha() {
        // 32×32, nieprzezroczysta tylko lewa połowa (blok 16 px).
        let mut rgba = vec![0u8; 32 * 32 * 4];
        for y in 0..32 {
            for x in 0..16 {
                rgba[(y * 32 + x) * 4 + 3] = 255;
            }
        }
        let rects = opaque_input_rects(&rgba, 32, 32);
        assert!(!rects.is_empty());
        assert!(rects.iter().all(|r| r.x() < 16.0));
        assert!(rects.iter().all(|r| r.x() + r.width() <= 16.001));
    }

    #[test]
    fn fully_transparent_yields_no_rects() {
        let rects = opaque_input_rects(&vec![0u8; 40 * 40 * 4], 40, 40);
        assert!(rects.is_empty());
    }

    #[test]
    fn full_surface_region_marks_input_capture() {
        let full = Rect::from_xywh(0.0, 0.0, 1920.0, 680.0);
        assert!(covers_surface(&full, 1920.0, 680.0));
        // Region tylko paska nie przechwytuje całej powierzchni.
        let bar = Rect::from_xywh(0.0, 0.0, 1920.0, 32.0);
        assert!(!covers_surface(&bar, 1920.0, 680.0));
    }
}

#[cfg(test)]
mod ssd_tests {
    use super::*;
    use crate::decoration::DecoMode;

    #[test]
    fn popup_never_gets_our_bar() {
        assert!(!wants_ssd(false, true, Some(DecoMode::Server)));
        assert!(!wants_ssd(true, true, None));
    }

    #[test]
    fn x11_windows_are_decorated_by_us() {
        // Xwayland nie tworzy obiektu dekoracji — pasek rysujemy sami.
        assert!(wants_ssd(true, false, None));
        // …ale jawne `client_side` od klienta X11 szanujemy.
        assert!(!wants_ssd(true, false, Some(DecoMode::Client)));
        assert!(wants_ssd(true, false, Some(DecoMode::Server)));
    }

    #[test]
    fn cascade_counts_only_visible_toplevels() {
        use std::sync::Arc;
        let make = |mapped: bool, layer: bool, popup: bool| Window {
            x: 0.0,
            y: 0.0,
            width: 10,
            height: 10,
            image_offset: (0.0, 0.0),
            z: 1,
            title: String::new(),
            popup,
            layer,
            mapped,
            focused: false,
            workspace: 0,
            input: None,
            input_capture: false,
            minimized: false,
            maximized: false,
            fullscreen: false,
            ssd: true,
            saved: None,
            dmabuf: None,
            data: Arc::new(ImageData::empty(10, 10)),
        };
        let windows = vec![
            make(true, false, false),
            make(false, false, false), // zamknięte/zminimalizowane — nie liczy się
            make(true, true, false),   // panel
            make(true, false, true),   // popup
        ];
        assert_eq!(cascade_slots(&windows), 1);
        assert_eq!(cascade_slots(&[]), 0);
    }

    #[test]
    fn wayland_only_when_server_side_negotiated() {
        assert!(wants_ssd(false, false, Some(DecoMode::Server)));
        // Klienty GTK/libadwaita: `client_side` → nie dublujemy paska.
        assert!(!wants_ssd(false, false, Some(DecoMode::Client)));
        // Brak obiektu dekoracji → brak naszego paska.
        assert!(!wants_ssd(false, false, None));
    }

    #[test]
    fn chromium_browsers_are_recognized_as_client_decorated() {
        // Chrome/Ecosia potwierdzają `server_side`, ale rysują własny pasek.
        assert!(known_csd_client(
            "google-chrome",
            "Bez tytułu - Google Chrome"
        ));
        assert!(known_csd_client("ecosia-browser", "New Tab"));
        assert!(known_csd_client("", "Welcome ! – Ecosia Browser"));
        assert!(known_csd_client("microsoft-edge", ""));
        // Zwykłe aplikacje zachowują nasz pasek.
        assert!(!known_csd_client("eu.luna.Store", "Sklep"));
        assert!(!known_csd_client("org.gnome.Nautilus", "Pliki"));
    }
}

/// Dodaje nowe okno lub aktualizuje istniejące (slot trzymany w [`SurfaceData`]).
#[allow(clippy::too_many_arguments)]
fn upsert_window(
    state: &mut State,
    surface: &wl_surface::WlSurface,
    data: &SurfaceData,
    x: f64,
    y: f64,
    w: i32,
    h: i32,
    z: i32,
    title: &str,
    popup: bool,
    layer: bool,
    reposition: bool,
    ssd: bool,
    image: std::sync::Arc<ImageData>,
) -> (usize, bool) {
    let input = data.input.lock_or_recover().clone();
    let mut slot = data.slot.lock_or_recover();
    upsert_window_at(
        state, &mut slot, surface, x, y, w, h, z, title, popup, layer, reposition, ssd, image,
        input, false,
    )
}

/// Jak [`upsert_window`], ale slot podajemy wprost.
///
/// Potrzebne dla powierzchni warstwowych na wielu monitorach: jedna `wl_surface`
/// odpowiada wtedy kilku oknom (po jednym na wyjście), więc jeden slot w `SurfaceData`
/// nie wystarcza.
#[allow(clippy::too_many_arguments)]
fn upsert_window_at(
    state: &mut State,
    slot: &mut Option<usize>,
    surface: &wl_surface::WlSurface,
    x: f64,
    y: f64,
    w: i32,
    h: i32,
    z: i32,
    title: &str,
    popup: bool,
    layer: bool,
    reposition: bool,
    ssd: bool,
    image: std::sync::Arc<ImageData>,
    input: Option<Vec<Rect>>,
    input_capture: bool,
) -> (usize, bool) {
    let ws = *state.ws_current.lock_or_recover();
    let mut windows = state.windows.lock_or_recover();
    let mut created = false;
    let idx = match *slot {
        Some(i) => {
            if let Some(win) = windows.get_mut(i) {
                // Pozycję stałą okna zmieniamy tylko dla powierzchni kotwiczonych.
                if reposition {
                    win.x = x;
                    win.y = y;
                    win.z = z;
                }
                win.width = w;
                win.height = h;
                win.mapped = true;
                win.popup = popup;
                win.layer = layer;
                win.ssd = ssd;
                if !title.is_empty() {
                    win.title = title.to_owned();
                }
                win.input = input.clone();
                win.input_capture = input_capture;
                win.data = image;
            }
            i
        }
        None => {
            let i = windows.len();
            windows.push(Window {
                x,
                y,
                width: w,
                height: h,
                image_offset: (0.0, 0.0),
                z,
                title: title.to_owned(),
                popup,
                layer,
                mapped: true,
                focused: false,
                workspace: ws,
                input,
                input_capture,
                minimized: false,
                maximized: false,
                fullscreen: false,
                ssd,
                saved: None,
                data: image,
                dmabuf: None,
            });
            *slot = Some(i);
            created = true;
            i
        }
    };
    drop(windows);
    // Zarejestruj powierzchnię okna (indeks = slot) do trasowania wejścia.
    register_slot(state, idx, surface);
    (idx, created)
}

/// Wpisuje powierzchnię w [`State::slots`] pod indeksem okna.
///
/// Wcześniej robiliśmy to tylko, gdy `slots.len() == idx` — przy powierzchni
/// tworzącej naraz kilka okien (panel na kilku monitorach) albo przy luce w
/// indeksach rejestracja po cichu się gubiła i wejście trafiało w złą powierzchnię.
fn register_slot(state: &mut State, idx: usize, surface: &wl_surface::WlSurface) {
    if state.slots.len() <= idx {
        state.slots.resize(idx + 1, surface.clone());
    }
    state.slots[idx] = surface.clone();
}

/// Geometria monitora `idx` (x, y, szerokość, wysokość); `idx` poza zakresem → pierwszy.
fn output_rect(state: &State, idx: usize) -> (f64, f64, f64, f64) {
    let outs = state.outputs.lock_or_recover();
    let o = outs.get(idx).or_else(|| outs.first());
    match o {
        Some(o) => (o.x as f64, o.y as f64, o.width as f64, o.height as f64),
        None => (0.0, 0.0, 1280.0, 800.0),
    }
}

/// Pozycja popupu względem treści rodzica (prostokąt kotwicy + offset).
fn popup_position(
    state: &State,
    xdg_surface: &xdg_surface::XdgSurface,
    size: (f64, f64),
) -> ((f64, f64), (f64, f64)) {
    let Some(d) = xdg_surface.data::<XdgSurfaceData>() else {
        return ((0.0, 0.0), (0.0, 0.0));
    };
    let geom = *d.geom.lock_or_recover();
    // Prostokąt treści rodzica: współrzędne positionera są względem jego geometrii okna.
    let parent_rect = d
        .parent
        .lock_or_recover()
        .clone()
        .and_then(|ps| {
            ps.data::<SurfaceData>()
                .map(|sd| *sd.slot.lock_or_recover())
        })
        .flatten()
        .and_then(|slot| {
            state
                .windows
                .lock_or_recover()
                .get(slot)
                .map(|w| w.content())
        })
        .unwrap_or_else(|| Rect::from_xywh(0.0, 0.0, 1280.0, 800.0));
    let work = work_area_at(state, parent_rect.center().x, parent_rect.center().y);
    let pos = crate::popup::place(&geom, parent_rect, work, size);
    (pos, (parent_rect.x(), parent_rect.y()))
}

/// Obszar roboczy monitora pod punktem (pełny ekran minus górny panel).
fn work_area_at(state: &State, x: f64, y: f64) -> Rect {
    let outs = state.outputs.lock_or_recover();
    let idx = crate::monitors::output_at(&outs, x, y);
    match outs.get(idx).or_else(|| outs.first()) {
        Some(o) => {
            let top = state.top_insets.get(&idx).copied().unwrap_or(0.0);
            Rect::from_xywh(
                o.x as f64,
                o.y as f64 + top,
                o.width as f64,
                (o.height as f64 - top).max(1.0),
            )
        }
        None => Rect::from_xywh(0.0, 0.0, 1280.0, 800.0),
    }
}

/// Wysyła `xdg_popup.configure` + `xdg_surface.configure` (raz).
fn send_popup_configure(
    state: &mut State,
    xdg_surface: &xdg_surface::XdgSurface,
    (x, y): (f64, f64),
    w: i32,
    h: i32,
) {
    let serial = state.next_serial();
    if let Some(d) = xdg_surface.data::<XdgSurfaceData>() {
        if let Some(popup) = d.popup.lock_or_recover().as_ref() {
            let _ = popup.send_event(xdg_popup::Event::Configure {
                x: x as i32,
                y: y as i32,
                width: w,
                height: h,
            });
        }
        if !*d.configured.lock_or_recover() {
            let _ = xdg_surface.send_event(xdg_surface::Event::Configure { serial });
            *d.configured.lock_or_recover() = true;
        }
    }
}

/// Jeśli toplevel nie ma jeszcze konfiguracji, wyślij `configure` (popupy mają własną).
fn maybe_configure_xdg(state: &mut State, data: &SurfaceData) {
    let Some(xdg_surface) = data.xdg_surface.lock_or_recover().clone() else {
        return;
    };
    let Some(xdg_data) = xdg_surface.data::<XdgSurfaceData>() else {
        return;
    };
    if *xdg_data.is_popup.lock_or_recover() {
        // Popup czeka na `configure` **przed** pierwszym buforem — bez tego klienty
        // takie jak Chrome/GTK nie pokażą menu w ogóle (albo pokażą je bez wejścia).
        // Rozmiar bierzemy z `xdg_positioner`; pozycję z geometrii rodzica.
        if *xdg_data.configured.lock_or_recover() {
            return;
        }
        let geom = *xdg_data.geom.lock_or_recover();
        let w = geom.width.max(1);
        let h = geom.height.max(1);
        let ((ax, ay), (ox, oy)) =
            popup_position(state, &xdg_surface, (f64::from(w), f64::from(h)));
        send_popup_configure(state, &xdg_surface, (ax - ox, ay - oy), w, h);
        return;
    }
    if *xdg_data.configured.lock_or_recover() {
        return;
    }
    let serial = state.next_serial();
    // Dekoracja **przed** konfiguracją powierzchni: klient stosuje tryb dekoracji przy
    // zatwierdzeniu tej właśnie konfiguracji. Bez tego część toolkitów (GTK) nie zrzuca
    // własnych dekoracji i na oknie widać podwójny pasek tytułu + czarną ramkę treści.
    if let Some(td) = xdg_data
        .toplevel
        .lock_or_recover()
        .as_ref()
        .and_then(|tl| tl.data::<XdgToplevelData>())
    {
        if let Some(deco) = td.decoration.lock_or_recover().as_ref() {
            use wayland_protocols::xdg::decoration::zv1::server::zxdg_toplevel_decoration_v1 as deco_proto;
            let _ = deco.send_event(deco_proto::Event::Configure {
                mode: WEnum::Value(deco_proto::Mode::ServerSide),
            });
        }
    }
    if let Some(toplevel) = xdg_data.toplevel.lock_or_recover().as_ref() {
        let _ = toplevel.send_event(xdg_toplevel::Event::Configure {
            // 0×0 = „klient wybiera rozmiar”. Wcześniej twarde 1024×720 kurczyło
            // **każde** okno (np. demo libLuny prosi o 1200×1028) i rozjeżdżało
            // późniejsze trafienia w kontrolki. Rozmiar i tak jest potem ograniczany
            // do obszaru roboczego przy pierwszym `commit` z buforem.
            width: 0,
            height: 0,
            // Tablica **u32** (4 bajty na stan), nie pojedyncze bajty — patrz
            // `seat::window_states`.
            states: 4u32.to_ne_bytes().to_vec(), // ACTIVATED
        });
        // UWAGA: **nie** wysyłamy tutaj `wm_capabilities` — to zdarzenie istnieje dopiero
        // od wersji 5 `xdg_toplevel`, a nasz `xdg_wm_base` ogłaszamy jako 1. Wysłanie go
        // klientowi v1 kończy się twardym przerwaniem klienta (Xwayland: „listener
        // function for opcode 3 of xdg_toplevel is NULL” → SIGABRT).
        // Gdy podniesiemy wersję globalu, trzeba je wysyłać warunkowo (`version() >= 5`).
    }
    let _ = xdg_surface.send_event(xdg_surface::Event::Configure { serial });
    *xdg_data.configured.lock_or_recover() = true;
}
