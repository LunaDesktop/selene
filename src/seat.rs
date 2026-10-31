//! `wl_seat` — wejście: wskaźnik (`wl_pointer`), klawiatura (`wl_keyboard`) i trasowanie.
//!
//! Zdarzenia wejścia pochodzą z magistrali (`crate::input`) — przekazuje je wątek wyjścia,
//! który odbiera je od nadrzędnego kompozytora. Tutaj tłumaczymy je na zdarzenia protokołu
//! wysyłane do sfokusowanych powierzchni klientów oraz na operacje okien (przeciąganie,
//! przeskalowanie, zamknięcie) wykonywane przez wbudowaną dekorację (SSD).

use std::io::{Seek, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use libluna_core::{ButtonState, Event, Modifiers, Point, PointerButton, Rect};
use wayland_protocols::wp::cursor_shape::v1::server::wp_cursor_shape_device_v1::{
    self, WpCursorShapeDeviceV1,
};
use wayland_protocols::wp::cursor_shape::v1::server::wp_cursor_shape_manager_v1::{
    self, WpCursorShapeManagerV1,
};
use wayland_protocols::xdg::shell::server::{xdg_popup, xdg_surface, xdg_toplevel};
use wayland_server::protocol::{wl_keyboard, wl_pointer, wl_seat, wl_surface, wl_touch};
use wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, WEnum,
};

use crate::compositor::SurfaceData;
use crate::data;
use crate::shell::{XdgSurfaceData, XdgToplevelData};
use crate::state::{
    CornerHint, CornerKind, CursorImage, DragState, ResizeState, SavedGeom, State, TITLEBAR,
};
use crate::sync::LockExt;

/// Ile pikseli powierzchni odpowiada jednemu „ząbkowi” kółka myszy (`REL_WHEEL`).
///
/// `wl_pointer.axis` przenosi wartości w jednostkach powierzchni, a evdev daje ±1 na
/// obrót — dlatego mnożymy. Wartość dobrana tak, by jeden obrót przewijał ~1 wiersz
/// listy (~60 px) po uwzględnieniu mnożnika w powłoce.
const WHEEL_STEP_PX: f64 = 20.0;

/// Buduje tekst mapy klawiszy XKB dla danego układu (i opcjonalnego wariantu).
///
/// Nazwy są sanityzowane, aby nie można było wstrzyknąć treści spoza zestawu
/// znaków używanego przez XKB.
fn keymap_text_for(layout: &str, variant: Option<&str>) -> String {
    let layout = sanitize_xkb(layout);
    let layout = if layout.is_empty() {
        "us".to_owned()
    } else {
        layout
    };
    let variant = variant.map(sanitize_xkb).filter(|v| !v.is_empty());
    // `level3(ralt_switch)` sprawia, że prawy Alt jest AltGr (poziom 3) — bez tego
    // na układzie polskim nie da się wpisać ą, ć, ę, ł, ń, ó, ś, ź, ż.
    let symbols = match variant {
        Some(v) => format!("pc+{layout}({v})+inet(evdev)+level3(ralt_switch)"),
        None => format!("pc+{layout}+inet(evdev)+level3(ralt_switch)"),
    };
    format!(
        "xkb_keymap {{\n    xkb_keycodes {{ include \"evdev+aliases(qwerty)\" }};\n    xkb_types {{ include \"complete\" }};\n    xkb_compat {{ include \"complete\" }};\n    xkb_symbols {{ include \"{symbols}\" }};\n    xkb_geometry {{ include \"pc(pc105)\" }};\n}};\n"
    )
}

/// Bieżący układ z otoczenia (`LUNA_KEYBOARD_LAYOUT` / `LUNA_KEYBOARD_VARIANT`).
///
/// Najpierw próbujemy **skompilować** mapę do postaci bez `include`. Klienty z własnym
/// `libxkbcommon` bez ścieżek wyszukiwania (np. Zed) inaczej nie potrafią jej sparsować
/// („Couldn't find file keycodes/evdev”, „There are no include paths to search”).
pub(crate) fn keymap_text() -> String {
    let (layout, variant) = keymap_layout();
    if let Some(km) = compiled_keymap(&layout, variant.as_deref()) {
        return km;
    }
    keymap_text_for(&layout, variant.as_deref())
}

/// Kompiluje RMLVO do pełnej mapy XKB (bez dyrektyw `include`).
fn compiled_keymap(layout: &str, variant: Option<&str>) -> Option<String> {
    use xkbcommon::xkb;
    let layout = sanitize_xkb(layout);
    let layout = if layout.is_empty() {
        "us".to_owned()
    } else {
        layout
    };
    let variant = variant.map(sanitize_xkb).unwrap_or_default();
    let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let keymap = xkb::Keymap::new_from_names(
        &ctx,
        "",
        "",
        layout.as_str(),
        variant.as_str(),
        // W RMLVO opcje podaje się jako `nazwa:wartość`, a nie jako dyrektywę
        // `include` — `level3(ralt_switch)` było ignorowane z błędem XKB-595
        // (i AltGr nie wpisywał ą/ć/ę…).
        Some("lv3:ralt_switch".to_owned()),
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    )?;
    Some(keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1))
}

/// Ustala układ klawiatury: jawny override → standard Wayland → locale → `us`.
///
/// Wcześniej był tu na sztywno `us`, więc w launcherze (i w każdym innym polu
/// tekstowym) wpisywało się co innego, niż użytkownik ma na klawiaturze.
fn keymap_layout() -> (String, Option<String>) {
    let layout = std::env::var("LUNA_KEYBOARD_LAYOUT")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::env::var("XKB_DEFAULT_LAYOUT")
                .ok()
                .filter(|s| !s.is_empty())
        })
        .or_else(layout_from_locale)
        .unwrap_or_else(|| "us".to_owned());
    let variant = std::env::var("LUNA_KEYBOARD_VARIANT")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::env::var("XKB_DEFAULT_VARIANT")
                .ok()
                .filter(|s| !s.is_empty())
        });
    (layout, variant)
}

/// Zgaduje układ XKB z locale (`pl_PL.UTF-8` → `pl`).
fn layout_from_locale() -> Option<String> {
    let raw = ["LC_ALL", "LC_CTYPE", "LANG"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|s| !s.is_empty()))?;
    locale_to_layout(&raw)
}

/// Zamienia locale (`pl_PL.UTF-8`, `en_GB`, `C`) na nazwę układu XKB.
fn locale_to_layout(raw: &str) -> Option<String> {
    let lang = raw.split(['.', '@']).next().unwrap_or("");
    let mut parts = lang.split('_');
    let language = parts.next().unwrap_or("").to_ascii_lowercase();
    let country = parts.next().unwrap_or("").to_ascii_uppercase();
    if language.is_empty() || language == "c" || language == "posix" {
        return None;
    }
    // Brytyjski angielski to osobny układ w XKB (`gb`), nie `en`.
    if language == "en" && country == "GB" {
        return Some("gb".to_owned());
    }
    Some(language)
}

/// Dopuszcza tylko znaki legalne w nazwie układu XKB.
fn sanitize_xkb(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect()
}

/// Maski krawędzi zmiany rozmiaru.
const EDGE_TOP: u8 = 1;
const EDGE_BOTTOM: u8 = 2;
const EDGE_LEFT: u8 = 4;
const EDGE_RIGHT: u8 = 8;

/// Minimalny rozmiar treści okna przy przeskalowaniu.
const MIN_W: f64 = 120.0;
const MIN_H: f64 = 80.0;

/// Ile pikseli okna musi zostać widoczne, żeby dało się je złapać z powrotem.
/// Tyle samo trzymamy w pionie: pasek tytułu nigdy nie chowa się pod panelem.
const MIN_VISIBLE: f64 = 96.0;

/// Odległość od krawędzi monitora, przy której upuszczenie okna przyciąga je do strefy.
const SNAP_EDGE: f64 = 12.0;

impl GlobalDispatch<wl_seat::WlSeat, ()> for State {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<wl_seat::WlSeat>,
        _data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let seat = data_init.init(resource, ());
        let caps = wl_seat::Capability::Pointer | wl_seat::Capability::Keyboard;
        let _ = seat.send_event(wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        });
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &wl_seat::WlSeat,
        request: wl_seat::Request,
        _data: &(),
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            wl_seat::Request::GetPointer { id } => {
                state.pointers.retain(|p| p.is_alive());
                state.pointers.push(data_init.init(id, ()));
            }
            wl_seat::Request::GetKeyboard { id } => {
                let kb = data_init.init(id, ());
                if let Some((fd, size)) = make_keymap() {
                    let _ = kb.send_event(wl_keyboard::Event::Keymap {
                        format: WEnum::Value(wl_keyboard::KeymapFormat::XkbV1),
                        fd: fd.as_fd(),
                        size,
                    });
                }
                // Powtarzanie klawiszy z `~/.config/luna/input.conf`. `repeat_info`
                // istnieje dopiero od wersji 4 `wl_keyboard` — wysłanie go do klienta,
                // który związał starszą wersję, kończy klienta twardym przerwaniem.
                if kb.version() >= 4 {
                    let _ = kb.send_event(wl_keyboard::Event::RepeatInfo {
                        rate: state.repeat_rate as i32,
                        delay: state.repeat_delay as i32,
                    });
                }
                state.keyboards.retain(|k| k.is_alive());
                state.keyboards.push(kb);
            }
            wl_seat::Request::GetTouch { id } => {
                data_init.init(id, ());
            }
            _ => {}
        }
    }
}

macro_rules! empty_dispatch {
    ($iface:ty) => {
        impl Dispatch<$iface, ()> for State {
            fn request(
                _state: &mut Self,
                _client: &Client,
                _resource: &$iface,
                _request: <$iface as wayland_server::Resource>::Request,
                _data: &(),
                _handle: &DisplayHandle,
                _data_init: &mut DataInit<'_, Self>,
            ) {
            }
        }
    };
}

empty_dispatch!(wl_keyboard::WlKeyboard);
empty_dispatch!(wl_touch::WlTouch);

impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &wl_pointer::WlPointer,
        request: wl_pointer::Request,
        _data: &(),
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        // Klient przysyła własny obraz kursora (starsze toolkitty, aplikacje malujące
        // kursor same). `surface = None` = powrót do domyślnej strzałki.
        if let wl_pointer::Request::SetCursor {
            surface,
            hotspot_x,
            hotspot_y,
            ..
        } = request
        {
            crate::cursor::set_surface(state, surface, (hotspot_x, hotspot_y));
        }
    }
}

impl GlobalDispatch<WpCursorShapeManagerV1, ()> for State {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<WpCursorShapeManagerV1>,
        _data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<WpCursorShapeManagerV1, ()> for State {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &WpCursorShapeManagerV1,
        request: <WpCursorShapeManagerV1 as Resource>::Request,
        _data: &(),
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let wp_cursor_shape_manager_v1::Request::GetPointer {
            cursor_shape_device,
            ..
        } = request
        {
            data_init.init(cursor_shape_device, ());
        }
    }
}

impl Dispatch<WpCursorShapeDeviceV1, ()> for State {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &WpCursorShapeDeviceV1,
        request: <WpCursorShapeDeviceV1 as Resource>::Request,
        _data: &(),
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        if let wp_cursor_shape_device_v1::Request::SetShape { shape, .. } = request {
            let WEnum::Value(shape) = shape else { return };
            *state.cursor_image.lock_or_recover() = CursorImage::Shape(shape_name(shape));
            // Kształt zastępuje obraz klienta — powierzchnia nie jest już potrzebna.
            state.cursor_surface = None;
        }
    }
}

/// Nazwa kształtu `cursor-shape-v1` → plik w motywie XCursor (nazwy CSS).
fn shape_name(shape: wp_cursor_shape_device_v1::Shape) -> String {
    use wp_cursor_shape_device_v1::Shape;
    let name = match shape {
        Shape::Default => "left_ptr",
        Shape::ContextMenu => "context-menu",
        Shape::Help => "help",
        Shape::Pointer => "pointer",
        Shape::Progress => "progress",
        Shape::Wait => "wait",
        Shape::Cell => "cell",
        Shape::Crosshair => "crosshair",
        Shape::Text => "text",
        Shape::VerticalText => "vertical-text",
        Shape::Alias => "alias",
        Shape::Copy => "copy",
        Shape::Move => "move",
        Shape::NoDrop => "no-drop",
        Shape::NotAllowed => "not-allowed",
        Shape::Grab => "grab",
        Shape::Grabbing => "grabbing",
        Shape::EResize => "e-resize",
        Shape::NResize => "n-resize",
        Shape::NeResize => "ne-resize",
        Shape::NwResize => "nw-resize",
        Shape::SResize => "s-resize",
        Shape::SeResize => "se-resize",
        Shape::SwResize => "sw-resize",
        Shape::WResize => "w-resize",
        Shape::EwResize => "ew-resize",
        Shape::NsResize => "ns-resize",
        Shape::NeswResize => "nesw-resize",
        Shape::NwseResize => "nwse-resize",
        Shape::ColResize => "col-resize",
        Shape::RowResize => "row-resize",
        Shape::AllScroll => "all-scroll",
        Shape::ZoomIn => "zoom-in",
        Shape::ZoomOut => "zoom-out",
        Shape::DndAsk => "dnd-ask",
        Shape::AllResize => "all-resize",
        _ => "left_ptr",
    };
    name.to_owned()
}

/// Buduje deskryptor z mapą klawiszy (memfd + tekst XKB, zakończony NUL).
fn make_keymap() -> Option<(OwnedFd, u32)> {
    let fd = rustix::fs::memfd_create("selene-keymap", rustix::fs::MemfdFlags::CLOEXEC).ok()?;
    let mut file = std::fs::File::from(fd);
    let mut bytes = keymap_text().into_bytes();
    bytes.push(0);
    file.write_all(&bytes).ok()?;
    file.rewind().ok()?;
    Some((OwnedFd::from(file), bytes.len() as u32))
}

// --- Trasowanie wejścia ---

/// Część okna, w którą trafił wskaźnik.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Part {
    Close,
    Maximize,
    Minimize,
    Title,
    Content,
    /// Zmiana rozmiaru za krawędź/narożnik (bitmaska: 1=góra,2=dół,4=lewo,8=prawo).
    Resize(u8),
}

/// Trasuje jedno zdarzenie wejścia do klientów/okien.
pub fn handle_input(state: &mut State, handle: &DisplayHandle, event: &Event) {
    match event {
        Event::PointerMotion { position, .. } => pointer_motion(state, handle, *position),
        Event::PointerButton(pe) => pointer_button(state, handle, pe.position, pe.button, pe.state),
        Event::PointerLeave => pointer_leave(state),
        Event::Scroll(mut se) => {
            if state.natural_scroll {
                se.dx = -se.dx;
                se.dy = -se.dy;
            }
            pointer_axis(state, se.dx, se.dy)
        }
        Event::Key(ke) => keyboard_key(state, ke.keysym, ke.keycode, ke.state),
        _ => {}
    }
}

/// Trafienie w okno (najwyższe wg `z`) i w którą jego część.
fn hit(state: &State, pos: Point) -> Option<(usize, Part)> {
    let windows = state.windows.lock_or_recover();
    let cur_ws = *state.ws_current.lock_or_recover();
    let mut best: Option<(usize, i32)> = None;
    for (i, w) in windows.iter().enumerate() {
        let inside = match input_region_hit(w, pos) {
            Some(h) => h,
            None => w.outer().contains(pos),
        };
        if w.mapped && !w.minimized && w.in_workspace(cur_ws) && inside {
            match best {
                Some((_, z)) if z >= w.z => {}
                _ => best = Some((i, w.z)),
            }
        }
    }
    let (i, _) = best?;
    let w = &windows[i];
    let part = if w.close_rect().contains(pos) {
        Part::Close
    } else if w.maximize_rect().contains(pos) {
        Part::Maximize
    } else if w.minimize_rect().contains(pos) {
        Part::Minimize
    } else {
        let edges = w.resize_edges(pos);
        if edges != 0 {
            Part::Resize(edges)
        } else if w.titlebar().contains(pos) {
            Part::Title
        } else {
            Part::Content
        }
    };
    Some((i, part))
}

/// Najwyższe okno, którego **treść** zawiera punkt (do zdarzeń klienta).
fn content_window(state: &State, pos: Point) -> Option<usize> {
    let windows = state.windows.lock_or_recover();
    let cur_ws = *state.ws_current.lock_or_recover();
    let mut best: Option<(usize, i32)> = None;
    for (i, w) in windows.iter().enumerate() {
        let inside = match input_region_hit(w, pos) {
            Some(h) => h,
            None => w.content().contains(pos),
        };
        if w.mapped && !w.minimized && w.in_workspace(cur_ws) && inside {
            match best {
                Some((_, z)) if z >= w.z => {}
                _ => best = Some((i, w.z)),
            }
        }
    }
    best.map(|(i, _)| i)
}

/// Zamyka popupy, poza którymi kliknięto (wysyła `xdg_popup.popup_done`, zdejmuje z widoku).
fn dismiss_popups(state: &mut State, pos: Point) {
    loop {
        let top = {
            let windows = state.windows.lock_or_recover();
            let mut best: Option<(usize, i32)> = None;
            for (i, w) in windows.iter().enumerate() {
                if w.popup && w.mapped {
                    match best {
                        Some((_, z)) if z >= w.z => {}
                        _ => best = Some((i, w.z)),
                    }
                }
            }
            best.map(|(i, _)| i)
        };
        let Some(slot) = top else { break };
        let inside = state
            .windows
            .lock_or_recover()
            .get(slot)
            .map(|w| w.outer().contains(pos))
            .unwrap_or(false);
        if inside {
            break;
        }
        if let Some(surface) = state.slots.get(slot).cloned() {
            if let Some(sd) = surface.data::<SurfaceData>() {
                if let Some(xs) = sd.xdg_surface.lock_or_recover().clone() {
                    if let Some(d) = xs.data::<XdgSurfaceData>() {
                        if let Some(popup) = d.popup.lock_or_recover().as_ref() {
                            let _ = popup.send_event(xdg_popup::Event::PopupDone);
                        }
                    }
                }
            }
        }
        if let Some(w) = state.windows.lock_or_recover().get_mut(slot) {
            w.mapped = false;
        }
    }
    // Klawiatura nie może zostać na zamkniętym popupie — wraca do okna pod nim.
    // Sprawdzamy **po** pętli: przy submenu może zostać jeszcze otwarte menu rodzica,
    // do którego fokus ma wrócić dopiero wtedy, gdy zniknie cała hierarchia.
    let focus_dead = state
        .keyboard_focus
        .as_ref()
        .map(|(s, _)| *s)
        .map(|s| {
            state
                .windows
                .lock_or_recover()
                .get(s)
                .map(|w| w.popup && !w.mapped)
                .unwrap_or(false)
        })
        .unwrap_or(false);
    if focus_dead {
        refocus_after_popup(state);
    }
}

/// Po zamknięciu popupu przywraca klawiaturę: najpierw nadrzędnemu menu (submenu),
/// a gdy żadne menu nie zostało — najwyższemu zwykłemu oknu.
///
/// Bez tego fokus zostawał na ukrytej powierzchni menu i klawiatura „martwiła się”
/// po wybraniu pozycji lub kliknięciu poza menu.
pub(crate) fn refocus_after_popup(state: &mut State) {
    let ws = *state.ws_current.lock_or_recover();
    let slot = {
        let windows = state.windows.lock_or_recover();
        // Nadrzędne menu (submenu właśnie się zamknęło) ma pierwszeństwo przed oknem.
        let top_popup = windows
            .iter()
            .enumerate()
            .filter(|(_, w)| w.popup && w.mapped && !w.minimized && w.in_workspace(ws))
            .max_by_key(|(_, w)| w.z)
            .map(|(i, _)| i);
        top_popup.or_else(|| {
            windows
                .iter()
                .enumerate()
                .filter(|(_, w)| {
                    w.mapped && !w.minimized && !w.layer && !w.popup && w.in_workspace(ws)
                })
                .max_by_key(|(_, w)| w.z)
                .map(|(i, _)| i)
        })
    };
    match slot {
        Some(slot) => set_keyboard_focus(state, slot),
        None => set_keyboard_focus(state, usize::MAX),
    }
}

/// Okno docelowe dla punktu (publiczne dla drag & drop).
pub(crate) fn window_at(state: &State, pos: (f64, f64)) -> Option<usize> {
    content_window(state, Point::new(pos.0, pos.1))
}

/// Trafienie w region wejścia powierzchni warstwowej; `None` = brak regionu (cała).
fn input_region_hit(w: &crate::state::Window, pos: Point) -> Option<bool> {
    if !w.layer {
        return None;
    }
    w.input
        .as_ref()
        .map(|rects| rects.iter().any(|r| r.offset(w.x, w.y).contains(pos)))
}

/// Slot powierzchni warstwowej przechwytującej kliknięcia, gdy punkt leży **poza**
/// jej widoczną treścią. Używane, gdy powłoka ma otwarty popup/launcher: klik gdzieś
/// indziej (pulpit, drugi monitor, dolna część ekranu) ma zamknąć popup, a nie
/// aktywować okno pod spodem.
fn capturing_layer_outside(state: &State, pos: Point) -> Option<usize> {
    let windows = state.windows.lock_or_recover();
    windows
        .iter()
        .position(|w| w.layer && w.mapped && w.input_capture && !w.outer().contains(pos))
}

fn window_geometry(state: &State, slot: usize) -> Option<(f64, f64, f64, f64)> {
    let windows = state.windows.lock_or_recover();
    windows
        .get(slot)
        .map(|w| (w.x, w.y, w.width as f64, w.height as f64))
}

/// Bitmaska krawędzi z żądania `xdg_toplevel.resize` (bity Waylanda pokrywają się
/// z naszymi `EDGE_*`: 1=góra, 2=dół, 4=lewo, 8=prawo).
fn resize_edge_bits(edge: WEnum<xdg_toplevel::ResizeEdge>) -> u8 {
    match edge {
        WEnum::Unknown(v) => (v & 0xf) as u8,
        WEnum::Value(e) => match e {
            xdg_toplevel::ResizeEdge::Top => EDGE_TOP,
            xdg_toplevel::ResizeEdge::Bottom => EDGE_BOTTOM,
            xdg_toplevel::ResizeEdge::Left => EDGE_LEFT,
            xdg_toplevel::ResizeEdge::Right => EDGE_RIGHT,
            xdg_toplevel::ResizeEdge::TopLeft => EDGE_TOP | EDGE_LEFT,
            xdg_toplevel::ResizeEdge::TopRight => EDGE_TOP | EDGE_RIGHT,
            xdg_toplevel::ResizeEdge::BottomLeft => EDGE_BOTTOM | EDGE_LEFT,
            xdg_toplevel::ResizeEdge::BottomRight => EDGE_BOTTOM | EDGE_RIGHT,
            _ => 0,
        },
    }
}

/// Zaczyna przesuwanie okna na żądanie `xdg_toplevel.move` (okna CSD proszą kompozytor
/// o przesuwanie w trakcie przeciągania własnego paska tytułu).
pub fn interactive_move_from_toplevel(state: &mut State, data: &XdgToplevelData) {
    // Żądanie `move` może dotrzeć już po zwolnieniu przycisku (szybki klik).
    // Bez tej straży zostawał aktywny `drag`, który przy następnym ruchu myszy
    // przesuwał okno mimo puszczonego przycisku.
    if !state.left_down {
        return;
    }
    let Some(slot) = toplevel_slot(data) else {
        if std::env::var("SELENE_STATS").is_ok() {
            eprintln!("selene/win: move bez slotu");
        }
        return;
    };
    if std::env::var("SELENE_STATS").is_ok() {
        eprintln!("selene/win: move slot={slot} pos={:?}", state.pointer_pos);
    }
    // Pozycja wskaźnika aktualizowana przy każdym ruchu (`pointer_motion`) — działa też
    // w trybie nested, gdzie nie ma pętli evdev.
    let pos = Point::new(state.pointer_pos.0, state.pointer_pos.1);
    if window_geometry(state, slot).is_some() {
        raise_and_focus(state, slot);
        begin_drag(state, slot, pos);
    }
}

/// Zaczyna zmianę rozmiaru na żądanie `xdg_toplevel.resize`.
pub fn interactive_resize_from_toplevel(
    state: &mut State,
    data: &XdgToplevelData,
    edges: WEnum<xdg_toplevel::ResizeEdge>,
) {
    if !state.left_down {
        return;
    }
    let Some(slot) = toplevel_slot(data) else {
        return;
    };
    let pos = Point::new(state.pointer_pos.0, state.pointer_pos.1);
    let bits = resize_edge_bits(edges);
    if bits == 0 {
        return;
    }
    if let Some((x, y, w, h)) = window_geometry(state, slot) {
        state.resize = Some(ResizeState {
            slot,
            edges: bits,
            start_x: x,
            start_y: y,
            start_w: w,
            start_h: h,
            start_px: pos.x,
            start_py: pos.y,
        });
    }
}

/// Rozpoczyna przeciągnięcie okna z pozycji globalnej (X11 `_NET_WM_MOVERESIZE`).
pub fn interactive_move_at(state: &mut State, slot: usize, pos: Point) {
    if window_geometry(state, slot).is_none() {
        return;
    }
    if std::env::var("SELENE_STATS").is_ok() {
        eprintln!(
            "selene/win: drag start slot={slot} pos=({:.0},{:.0})",
            pos.x, pos.y
        );
    }
    raise_and_focus(state, slot);
    begin_drag(state, slot, pos);
}

/// Rozpoczyna zmianę rozmiaru z pozycji globalnej (X11 `_NET_WM_MOVERESIZE`).
pub fn interactive_resize_at(state: &mut State, slot: usize, edges: u8, pos: Point) {
    if edges == 0 {
        return;
    }
    let Some((x, y, w, h)) = window_geometry(state, slot) else {
        return;
    };
    raise_and_focus(state, slot);
    state.resize = Some(ResizeState {
        slot,
        edges,
        start_x: x,
        start_y: y,
        start_w: w,
        start_h: h,
        start_px: pos.x,
        start_py: pos.y,
    });
}

/// Slot okna dla danych `xdg_toplevel` (przez powiązaną `wl_surface`).
pub(crate) fn toplevel_slot(data: &XdgToplevelData) -> Option<usize> {
    let sd = data.surface.data::<SurfaceData>()?;
    *sd.slot.lock_or_recover()
}

fn content_local(state: &State, slot: usize, pos: Point) -> (f64, f64) {
    // Używamy `content()` — dla paneli/popupów pasek tytułu ma wysokość 0, więc
    // współrzędne nie są przesunięte (wcześniej odejmowano `TITLEBAR` zawsze).
    // Bufor okien CSD jest większy od widocznego okna o cienie — klient liczy
    // współrzędne od początku **bufora**, więc dodajemy `image_offset`.
    let windows = state.windows.lock_or_recover();
    match windows.get(slot) {
        Some(w) => {
            let c = w.content();
            (
                pos.x - c.x() + w.image_offset.0,
                pos.y - c.y() + w.image_offset.1,
            )
        }
        None => (pos.x, pos.y),
    }
}

/// Klawiatura klienta, do którego należy `surface`.
///
/// **Nie wolno** wysyłać zdarzeń klawiatury z powierzchnią innego klienta — protokół
/// (i `wayland-backend`) reaguje na to paniką. Dlatego każde zdarzenie szuka zasobu
/// należącego do tego samego klienta, co okno docelowe.
fn keyboard_for(state: &State, surface: &wl_surface::WlSurface) -> Option<wl_keyboard::WlKeyboard> {
    let client = surface.client()?;
    state
        .keyboards
        .iter()
        .find(|kb| kb.is_alive() && kb.client().as_ref() == Some(&client))
        .cloned()
}

/// Wskaźnik klienta, do którego należy `surface` (patrz [`keyboard_for`]).
fn pointer_for(state: &State, surface: &wl_surface::WlSurface) -> Option<wl_pointer::WlPointer> {
    let client = surface.client()?;
    state
        .pointers
        .iter()
        .find(|p| p.is_alive() && p.client().as_ref() == Some(&client))
        .cloned()
}

fn pointer_motion(state: &mut State, handle: &DisplayHandle, pos: Point) {
    state.note_activity();
    state.pointer_pos = (pos.x, pos.y);
    if state.overview_on() {
        // W podglądzie ruch myszy tylko aktualizuje „ducha” przeciąganego okna.
        state.move_overview_drag((pos.x, pos.y));
        return;
    }
    if state.resize.is_some() {
        do_resize(state, pos);
        return;
    }
    if let Some(drag) = state.drag {
        let width = state
            .windows
            .lock_or_recover()
            .get(drag.slot)
            .map(|w| w.width as f64)
            .unwrap_or(MIN_W);
        let area = output_rect_at(state, pos);
        let (x, y) = clamp_drag_pos(
            area,
            inset_at(state, pos),
            width,
            pos.x - drag.off_x,
            pos.y - drag.off_y,
        );
        {
            let mut windows = state.windows.lock_or_recover();
            if let Some(w) = windows.get_mut(drag.slot) {
                w.x = x;
                w.y = y;
            }
        }
        // Okno X11 musi znać nową pozycję — inaczej jego menu i podpowiedzi
        // pojawiają się względem starego miejsca.
        sync_x11_geometry(state, drag.slot);
    }
    // Rogi ekranu (opcja w Ustawieniach): zapamiętujemy wejście, a akcję wykona
    // `tick_corner` po przytrzymaniu — dzięki temu poświata zdąży się pokazać.
    // Działa też w trakcie przeciągania okna (okno przenosi się razem z kursorem).
    note_corner(state, pos);
    // Przeciąganie danych (drag & drop) aktualizuje cel.
    if state.dnd.is_some() {
        data::dnd_motion(state, handle, (pos.x, pos.y));
    }
    update_pointer_focus(state, pos);
    if let Some((slot, surface)) = state.pointer_focus.clone() {
        let (lx, ly) = content_local(state, slot, pos);
        let time = state.time_ms();
        if let Some(ptr) = pointer_for(state, &surface) {
            let _ = ptr.send_event(wl_pointer::Event::Motion {
                time,
                surface_x: lx,
                surface_y: ly,
            });
            pointer_frame(&ptr);
        }
    }
}

fn pointer_button(
    state: &mut State,
    handle: &DisplayHandle,
    pos: Point,
    button: PointerButton,
    st: ButtonState,
) {
    state.note_activity();
    if button == PointerButton::Left {
        state.left_down = st == ButtonState::Pressed;
    }
    // W trybie podglądu: przeciąganie kafelka, wybór przestrzeni albo zamknięcie.
    if state.overview_on() {
        if button == PointerButton::Left {
            match st {
                ButtonState::Pressed => {
                    if let Some(slot) = overview_tile_at(state, pos) {
                        state.set_overview_drag(Some((slot, (pos.x, pos.y))));
                    } else if let Some(i) = overview_chip_at(state, pos) {
                        state.set_overview(false);
                        switch_workspace(state, i);
                    } else {
                        state.set_overview(false);
                    }
                }
                ButtonState::Released => {
                    let drag = state.overview.lock_or_recover().drag;
                    if let Some((slot, _)) = drag {
                        if let Some(i) = overview_chip_at(state, pos) {
                            // Upuszczenie na przestrzeni — przenieś okno.
                            move_window_to_workspace(state, slot, i);
                        } else {
                            // Zwykły klik — aktywuj okno.
                            raise_and_focus(state, slot);
                        }
                        state.set_overview(false);
                    }
                }
            }
        }
        return;
    }
    // Otwarty popup powłoki przechwytuje kliknięcia: klik poza jej widoczną treścią
    // (pulpit, drugi monitor, dolna część ekranu) ma ją zamknąć, a nie aktywować
    // okno pod spodem. Wysyłamy powłoce `enter` + `button` we współrzędnych lokalnych.
    if st == ButtonState::Pressed {
        if let Some(slot) = capturing_layer_outside(state, pos) {
            if let Some(surface) = state.slots.get(slot).cloned() {
                pointer_leave(state);
                state.pointer_focus = Some((slot, surface.clone()));
                if let Some(ptr) = pointer_for(state, &surface) {
                    let (lx, ly) = content_local(state, slot, pos);
                    let enter_serial = state.next_serial();
                    let _ = ptr.send_event(wl_pointer::Event::Enter {
                        serial: enter_serial,
                        surface: surface.clone(),
                        surface_x: lx,
                        surface_y: ly,
                    });
                    let serial = state.next_serial();
                    let time = state.time_ms();
                    let _ = ptr.send_event(wl_pointer::Event::Button {
                        serial,
                        time,
                        button: button_code(button),
                        state: WEnum::Value(wl_pointer::ButtonState::Pressed),
                    });
                    pointer_frame(&ptr);
                }
            }
            return;
        }
    }

    if st == ButtonState::Released {
        if let Some(drag) = state.drag.take() {
            // Upuszczenie przy krawędzi/narożniku: połowa ekranu, ćwiartka albo
            // maksymalizacja („Aero Snap” / kafelkowanie). Zwykły klik bez ruchu
            // (np. na pasku tytułu przy krawędzi) nie może niczego przyciągać.
            let moved = (pos.x - drag.start_px).abs() + (pos.y - drag.start_py).abs() > 6.0;
            if moved {
                apply_drag_snap(state, drag.slot, pos, drag.origin);
            }
        }
        state.resize = None;
        // Zakończ przeciąganie danych, jeśli trwało.
        if state.dnd.is_some() {
            data::dnd_drop(state, handle);
        }
    } else {
        // Klik poza popupem zamyka popup (przed trafieniem w okno).
        dismiss_popups(state, pos);
    }

    // Obsłuż dekorację/okno pod kursorem.
    if let Some((slot, part)) = hit(state, pos) {
        if st == ButtonState::Pressed && std::env::var("SELENE_STATS").is_ok() {
            let (lx, ly) = content_local(state, slot, pos);
            eprintln!("selene/input: klik slot={slot} part={part:?} lokalne=({lx:.0},{ly:.0})");
        }
        if st == ButtonState::Pressed {
            raise_and_focus(state, slot);

            // Alt+Drag lub Super+Drag: przesuwanie (LPM) i skalowanie (PPM) okna z dowolnego punktu
            let is_normal_win = state
                .windows
                .lock_or_recover()
                .get(slot)
                .map(|w| !w.layer && !w.popup)
                .unwrap_or(false);
            if is_normal_win && (state.sent_mods.alt || state.sent_mods.logo) {
                pointer_leave(state);
                if button == PointerButton::Left {
                    begin_drag(state, slot, pos);
                    return;
                } else if button == PointerButton::Right {
                    if let Some((x, y, w, h)) = window_geometry(state, slot) {
                        state.resize = Some(ResizeState {
                            slot,
                            edges: EDGE_RIGHT | EDGE_BOTTOM,
                            start_x: x,
                            start_y: y,
                            start_w: w,
                            start_h: h,
                            start_px: pos.x,
                            start_py: pos.y,
                        });
                    }
                    return;
                }
            }
            match part {
                Part::Close => {
                    pointer_leave(state);
                    debug_part(state, slot, "close");
                    send_close(state, slot);
                    return;
                }
                Part::Minimize => {
                    pointer_leave(state);
                    debug_part(state, slot, "minimize");
                    toggle_minimize(state, slot);
                    return;
                }
                Part::Maximize => {
                    pointer_leave(state);
                    debug_part(state, slot, "maximize");
                    toggle_maximize(state, slot);
                    return;
                }
                Part::Title => {
                    pointer_leave(state);
                    // Podwójny klik na pasku tytułu: maksymalizacja / powrót do domyślnego
                    // rozmiaru (standardowe zachowanie pulpitów).
                    if button == PointerButton::Left {
                        let now = Instant::now();
                        let double = state
                            .last_title_click
                            .map(|(s, t)| {
                                s == slot && now.duration_since(t) < Duration::from_millis(400)
                            })
                            .unwrap_or(false);
                        if double {
                            state.last_title_click = None;
                            toggle_maximize(state, slot);
                            return;
                        }
                        state.last_title_click = Some((slot, now));
                    }
                    begin_drag(state, slot, pos);
                    return;
                }
                Part::Resize(edges) => {
                    pointer_leave(state);
                    // Zmaxymalizowane/pełnoekranowe okno najpierw wraca do rozmiaru
                    // domyślnego — inaczej „ciągnięcie za krawędź” zaczynałoby od
                    // geometrii całego ekranu.
                    let special = state
                        .windows
                        .lock_or_recover()
                        .get(slot)
                        .map(|w| w.maximized || w.fullscreen)
                        .unwrap_or(false);
                    if special {
                        restore_window(state, slot);
                    }
                    if let Some((x, y, w, h)) = window_geometry(state, slot) {
                        state.resize = Some(ResizeState {
                            slot,
                            edges,
                            start_x: x,
                            start_y: y,
                            start_w: w,
                            start_h: h,
                            start_px: pos.x,
                            start_py: pos.y,
                        });
                    }
                    return;
                }
                Part::Content => update_pointer_focus(state, pos),
            }
        } else if part == Part::Content {
            // Zwolnienie nad treścią → przekaż je klientowi poniżej.
            update_pointer_focus(state, pos);
        } else {
            return;
        }
    } else {
        update_pointer_focus(state, pos);
    }

    // Zdarzenie przycisku do klienta — tylko gdy kursor nad treścią okna.
    if let Some((_, surface)) = state.pointer_focus.clone() {
        if let Some(ptr) = pointer_for(state, &surface) {
            let serial = state.next_serial();
            let time = state.time_ms();
            let wl_state = match st {
                ButtonState::Pressed => WEnum::Value(wl_pointer::ButtonState::Pressed),
                ButtonState::Released => WEnum::Value(wl_pointer::ButtonState::Released),
            };
            let _ = ptr.send_event(wl_pointer::Event::Button {
                serial,
                time,
                button: button_code(button),
                state: wl_state,
            });
            pointer_frame(&ptr);
        }
    }
}

/// Diagnostyka (`SELENE_STATS=1`): który element dekoracji został kliknięty.
fn debug_part(state: &State, slot: usize, what: &str) {
    if std::env::var("SELENE_STATS").is_err() {
        return;
    }
    let geometry = state
        .windows
        .lock_or_recover()
        .get(slot)
        .map(|w| (w.x, w.y, w.width, w.height, w.ssd, w.maximized, w.minimized));
    eprintln!("selene/win: klik {what} slot={slot} {geometry:?}");
}

fn pointer_leave(state: &mut State) {
    if let Some((_, surface)) = state.pointer_focus.clone() {
        if let Some(ptr) = pointer_for(state, &surface) {
            let serial = state.next_serial();
            let _ = ptr.send_event(wl_pointer::Event::Leave { serial, surface });
            pointer_frame(&ptr);
        }
    }
    state.pointer_focus = None;
    // Kursor należał do klienta, który właśnie stracił wskaźnik. Nie pokazujemy
    // jego kształtu nad inną aplikacją — nowy klient ustawi własny.
    state.cursor_surface = None;
    *state.cursor_image.lock_or_recover() = CursorImage::Default;
}

/// Zamyka grupę zdarzeń wskaźnika (`wl_pointer.frame`, protokół ≥ 5).
///
/// Klienty takie jak GPUI (Zed) czekają z przetworzeniem ruchu/scrolla na `frame`;
/// bez tego zdarzenia są gromadzone i nigdy nie trafiają do aplikacji.
fn pointer_frame(ptr: &wl_pointer::WlPointer) {
    if ptr.version() >= 5 {
        let _ = ptr.send_event(wl_pointer::Event::Frame);
    }
}

fn pointer_axis(state: &mut State, dx: f64, dy: f64) {
    state.note_activity();
    let Some((_, surface)) = state.pointer_focus.clone() else {
        return;
    };
    let Some(ptr) = pointer_for(state, &surface) else {
        return;
    };
    let time = state.time_ms();
    // `wl_pointer.axis` oczekuje wartości w **jednostkach powierzchni** (pikselach),
    // a evdev daje „ząbki” (±1). Skalujemy więc ząbek do [`WHEEL_STEP_PX`] px —
    // inaczej klienci (a nasza powłoka) przewijaliby 1 px na obrót kółka.
    //
    // Od wersji 5 protokołu wysyłamy też `axis_source` i `axis_discrete`: GPUI
    // (Zed) ignoruje `axis` przy źródle „wheel” i przewija na podstawie `discrete`.
    if ptr.version() >= 5 {
        let _ = ptr.send_event(wl_pointer::Event::AxisSource {
            axis_source: WEnum::Value(wl_pointer::AxisSource::Wheel),
        });
    }
    if dy != 0.0 {
        let _ = ptr.send_event(wl_pointer::Event::Axis {
            time,
            axis: WEnum::Value(wl_pointer::Axis::VerticalScroll),
            value: dy * WHEEL_STEP_PX,
        });
        if ptr.version() >= 5 {
            let _ = ptr.send_event(wl_pointer::Event::AxisDiscrete {
                axis: WEnum::Value(wl_pointer::Axis::VerticalScroll),
                discrete: dy.round() as i32,
            });
        }
    }
    if dx != 0.0 {
        let _ = ptr.send_event(wl_pointer::Event::Axis {
            time,
            axis: WEnum::Value(wl_pointer::Axis::HorizontalScroll),
            value: dx * WHEEL_STEP_PX,
        });
        if ptr.version() >= 5 {
            let _ = ptr.send_event(wl_pointer::Event::AxisDiscrete {
                axis: WEnum::Value(wl_pointer::Axis::HorizontalScroll),
                discrete: dx.round() as i32,
            });
        }
    }
    pointer_frame(&ptr);
}

fn update_pointer_focus(state: &mut State, pos: Point) {
    let target = content_window(state, pos);
    if std::env::var_os("SELENE_TRACE_POINTER").is_some() {
        eprintln!(
            "selene/pointer: pos=({:.0},{:.0}) target={target:?} focus={:?} pointers={}",
            pos.x,
            pos.y,
            state.pointer_focus.as_ref().map(|(slot, _)| *slot),
            state.pointers.len()
        );
    }
    let same = matches!((state.pointer_focus.as_ref(), target), (Some((s, _)), Some(t)) if *s == t);
    if same {
        return;
    }
    let old_client = state
        .pointer_focus
        .as_ref()
        .and_then(|(_, surface)| surface.client())
        .map(|client| client.id());
    if let Some((_, surface)) = state.pointer_focus.clone() {
        if let Some(ptr) = pointer_for(state, &surface) {
            let serial = state.next_serial();
            let _ = ptr.send_event(wl_pointer::Event::Leave { serial, surface });
            pointer_frame(&ptr);
        }
    }
    state.pointer_focus = None;
    // Zmiana klienta: stary kształt (np. kursor gry albo Flatpaka) nie może
    // "przykleić się" do nowego okna. Reset przed `enter` — nowy klient
    // ustawi własny kursor w `wl_pointer.set_cursor`.
    let new_client = target
        .and_then(|slot| state.slots.get(slot))
        .and_then(|surface| surface.client())
        .map(|client| client.id());
    if old_client != new_client {
        state.cursor_surface = None;
        *state.cursor_image.lock_or_recover() = CursorImage::Default;
    }
    if let Some(slot) = target {
        if let Some(surface) = state.slots.get(slot).cloned() {
            let serial = state.next_serial();
            let (lx, ly) = content_local(state, slot, pos);
            if let Some(ptr) = pointer_for(state, &surface) {
                if std::env::var_os("SELENE_TRACE_POINTER").is_some() {
                    eprintln!("selene/pointer: enter slot={slot} ptr={ptr:?}");
                }
                let _ = ptr.send_event(wl_pointer::Event::Enter {
                    serial,
                    surface: surface.clone(),
                    surface_x: lx,
                    surface_y: ly,
                });
                pointer_frame(&ptr);
            }
            state.pointer_focus = Some((slot, surface));
        }
    }
}

/// Przeskalowanie okna w trakcie przeciągania krawędzi/narożnika; wysyła `xdg_toplevel.configure`.
///
/// Okno nie może wyjść za monitor ani schować paska tytułu pod górnym panelem —
/// przyciągamy krawędź do obszaru roboczego i przeliczamy rozmiar tak, by przeciwna
/// krawędź została w miejscu (zachowanie „gumki”).
fn do_resize(state: &mut State, pos: Point) {
    let Some(rs) = state.resize else {
        return;
    };
    let (ox, oy, ow, oh) = window_output_rect(state, rs.slot);
    let top = window_inset(state, rs.slot);
    let bar = state
        .windows
        .lock_or_recover()
        .get(rs.slot)
        .map(|w| w.bar())
        .unwrap_or(0.0);
    let right = ox + ow;
    let work_top = oy + top;
    let bottom = oy + oh;

    let dx = pos.x - rs.start_px;
    let dy = pos.y - rs.start_py;
    let mut x = rs.start_x;
    let mut y = rs.start_y;
    let mut w = rs.start_w;
    let mut h = rs.start_h;

    if rs.edges & EDGE_LEFT != 0 {
        w = (rs.start_w - dx).max(MIN_W);
        x = rs.start_x + (rs.start_w - w);
        if x < ox {
            x = ox;
            w = (rs.start_x + rs.start_w - x).max(MIN_W);
        }
    } else if rs.edges & EDGE_RIGHT != 0 {
        w = (rs.start_w + dx).max(MIN_W).min((right - x).max(MIN_W));
    }

    if rs.edges & EDGE_TOP != 0 {
        h = (rs.start_h - dy).max(MIN_H);
        y = rs.start_y + (rs.start_h - h);
        // Pasek tytułu nigdy pod panelem — dolna krawędź zostaje w miejscu.
        if y < work_top {
            y = work_top;
            h = (rs.start_y + rs.start_h - y).max(MIN_H);
        }
    } else if rs.edges & EDGE_BOTTOM != 0 {
        h = (rs.start_h + dy).max(MIN_H);
        h = h.min((bottom - y - bar).max(MIN_H));
    }

    {
        let mut windows = state.windows.lock_or_recover();
        if let Some(w2) = windows.get_mut(rs.slot) {
            w2.x = x;
            w2.y = y;
            w2.width = w as i32;
            w2.height = h as i32;
        }
    }
    send_configure(state, rs.slot, w, h);
    // Okno X11: bez tego klient nie wie o nowym rozmiarze (i nie przerysuje treści).
    sync_x11_geometry(state, rs.slot);
}

/// Wysyła konfigurację rozmiaru i stanów (`xdg_surface.configure` + `xdg_toplevel.configure`).
pub(crate) fn send_configure(state: &mut State, slot: usize, w: f64, h: f64) {
    let Some(surface) = state.slots.get(slot).cloned() else {
        return;
    };
    let Some(sd) = surface.data::<SurfaceData>() else {
        return;
    };
    let Some(xs) = sd.xdg_surface.lock_or_recover().clone() else {
        return;
    };
    let states = window_states(state, slot);
    if std::env::var("SELENE_STATS").is_ok() {
        eprintln!("selene/win: send_configure slot={slot} {w}x{h}");
    }
    let serial = state.next_serial();
    // Kolejność ma znaczenie: `xdg_toplevel.configure` **przed** `xdg_surface.configure`.
    // `xdg_surface.configure` kończy sekwencję konfiguracji — klient zatwierdza wtedy
    // ostatni stan toplevela. Odwrotna kolejność (surface → toplevel) sprawiała, że
    // klient aplikował **poprzedni** rozmiar, a nowy zostawał w kolejce na zawsze
    // (maksymalizacja „nie działała”, okno zostawało 1200×1028).
    if let Some(xd) = xs.data::<XdgSurfaceData>() {
        if let Some(tl) = xd.toplevel.lock_or_recover().as_ref() {
            let _ = tl.send_event(xdg_toplevel::Event::Configure {
                width: w as i32,
                height: h as i32,
                states,
            });
        }
    }
    let _ = xs.send_event(xdg_surface::Event::Configure { serial });
}

/// Stany `xdg_toplevel` dla okna (ACTIVATED + ewentualnie MAXIMIZED/FULLSCREEN).
///
/// Protokół przesyła je jako tablicę **u32** (`array` uint), a binding Waylanda
/// wystawia ją jako `Vec<u8>` — trzeba więc zapisać każdy stan jako 4 bajty.
/// Wcześniej wysyłaliśmy pojedyncze bajty i klient (SCTK/GTK) widział pustą listę:
/// okno nigdy nie było „zmaksymalizowane”, więc jego przycisk max/min/close
/// zachowywał się losowo.
fn window_states(state: &State, slot: usize) -> Vec<u8> {
    let mut list = vec![4u32]; // ACTIVATED
    if let Some(w) = state.windows.lock_or_recover().get(slot) {
        if w.maximized {
            list.push(1); // MAXIMIZED
        }
        if w.fullscreen {
            list.push(2); // FULLSCREEN
        }
    }
    encode_states(&list)
}

/// Koduje stany `xdg_toplevel` w tablicę bajtów oczekiwaną przez protokół.
fn encode_states(list: &[u32]) -> Vec<u8> {
    list.iter().flat_map(|s| s.to_ne_bytes()).collect()
}

/// Prostokąt monitora pod punktem (x, y, szerokość, wysokość).
fn output_rect_at(state: &State, pos: Point) -> (f64, f64, f64, f64) {
    let outs = state.outputs.lock_or_recover();
    let idx = crate::monitors::output_at(&outs, pos.x, pos.y);
    match outs.get(idx).or_else(|| outs.first()) {
        Some(o) => (o.x as f64, o.y as f64, o.width as f64, o.height as f64),
        None => (0.0, 0.0, 1280.0, 800.0),
    }
}

/// Bok strefy „gorącego rogu” (dolne rogi i lewy górny róg obszaru roboczego).
///
/// Strefa jest mała celowo: to nie jest cała krawędź, tylko sam róg. Wjazd w nią
/// po drodze do launchera nic nie robi — akcja czeka na przytrzymanie.
const CORNER: f64 = 32.0;

/// Jak długo kursor musi stać w rogu, zanim zadziała akcja.
///
/// Bez tego pośpiech przy panelu/launcherze otwierał podgląd albo przełączał
/// przestrzeń przypadkiem. Poświata w rogu pokazuje, że odliczanie trwa.
const CORNER_DWELL: Duration = Duration::from_millis(500);

/// Który róg obejmuje punkt (o ile dana strefa jest włączona w Ustawieniach).
fn corner_at(state: &State, pos: Point) -> Option<CornerKind> {
    if state.locked {
        return None;
    }
    let (ox, oy, ow, oh) = output_rect_at(state, pos);
    let top = inset_at(state, pos).min(oh);
    if state.ws_overview_corner
        && pos.x <= ox + CORNER
        && pos.y >= oy + top
        && pos.y <= oy + top + CORNER
    {
        return Some(CornerKind::TopLeft);
    }
    if state.ws_corner_switch {
        if pos.x <= ox + CORNER && pos.y >= oy + oh - CORNER {
            return Some(CornerKind::BottomLeft);
        }
        if pos.x >= ox + ow - CORNER && pos.y >= oy + oh - CORNER {
            return Some(CornerKind::BottomRight);
        }
    }
    None
}

/// Publikuje poświatę do wątku wyjścia (`None` = brak poświaty).
fn publish_corner(state: &State, progress: f64) {
    let hint = state.ws_corner_hover.and_then(|kind| {
        (!state.ws_corner_latch).then_some(CornerHint {
            kind,
            progress,
            top_inset: state.ws_corner_top,
        })
    });
    state.overview.lock_or_recover().corner = hint;
}

/// Zapamiętuje wejście/wyjście z rogu. **Nie** wykonuje akcji — od tego jest
/// [`tick_corner`], żeby przytrzymanie działało także bez ruchu myszy.
fn note_corner(state: &mut State, pos: Point) {
    let corner = corner_at(state, pos);
    if corner == state.ws_corner_hover {
        return;
    }
    state.ws_corner_hover = corner;
    state.ws_corner_since = corner.map(|_| Instant::now());
    state.ws_corner_top = if corner.is_some() {
        inset_at(state, pos)
    } else {
        0.0
    };
    // Nowy róg = nowe odliczanie; zapadka z poprzedniego rogu nie może blokować.
    state.ws_corner_latch = false;
    publish_corner(state, 0.0);
}

/// Odlicza przytrzymanie w rogu i po [`CORNER_DWELL`] wykonuje akcję.
///
/// Wywoływane w każdej iteracji pętli głównej (także po wybudzeniu timerem),
/// więc postęp poświaty nie zależy od ruchu wskaźnika.
pub(crate) fn tick_corner(state: &mut State) {
    let Some(kind) = state.ws_corner_hover else {
        return;
    };
    if state.locked {
        state.reset_corner();
        return;
    }
    if state.ws_corner_latch {
        return;
    }
    let Some(since) = state.ws_corner_since else {
        return;
    };
    let progress = (since.elapsed().as_secs_f64() / CORNER_DWELL.as_secs_f64()).clamp(0.0, 1.0);
    if progress < 1.0 {
        publish_corner(state, progress);
        return;
    }
    // Akcja zadziała raz na wejście w róg — dopiero wyjście i powrót odblokuje ją znowu.
    state.ws_corner_latch = true;
    publish_corner(state, 1.0);
    activate_corner(state, kind);
}

/// Wykonuje akcję rogu: podgląd (lewy górny) albo zmiana przestrzeni (dolne).
fn activate_corner(state: &mut State, kind: CornerKind) {
    if kind == CornerKind::TopLeft {
        state.set_overview(true);
        return;
    }
    let current = *state.ws_current.lock_or_recover();
    let target = match kind {
        CornerKind::BottomLeft => current.checked_sub(1),
        CornerKind::BottomRight => (current + 1 < state.ws_count).then_some(current + 1),
        CornerKind::TopLeft => return,
    };
    let Some(target) = target else {
        return;
    };
    if let Some(drag) = state.drag {
        move_window_to_workspace(state, drag.slot, target);
    }
    switch_workspace(state, target);
}

/// Akcja wynikająca z bezczynności (czysta decyzja — łatwa do testów).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum IdleAction {
    /// Zgaś ekran (czarna klatka, kursor schowany).
    Blank,
    /// Zablokuj sesję (`luna-lock`).
    Lock,
    /// Uśpij komputer (`systemctl suspend`).
    Suspend,
}

/// Co powinno się stać po bezczynności — bez wykonywania akcji.
///
/// Progi są niezależne; `lock_on_blank` dokłada blokadę do wygaszenia, więc
/// ekran nigdy nie gaśnie „bez zabezpieczenia”, jeśli użytkownik tego chce.
pub(crate) fn idle_actions(state: &State) -> Vec<IdleAction> {
    let mut actions = Vec::new();
    if state.locked {
        return actions;
    }
    let idle_secs = state.idle_last_input.elapsed().as_secs();
    let cfg = state.idle;
    if cfg.screen_off_secs > 0 && idle_secs >= u64::from(cfg.screen_off_secs) && !state.is_blanked()
    {
        actions.push(IdleAction::Blank);
    }
    let lock_due = cfg.lock_secs > 0 && idle_secs >= u64::from(cfg.lock_secs);
    let blank_lock = cfg.lock_on_blank && actions.contains(&IdleAction::Blank);
    if (lock_due || blank_lock) && !state.idle_lock_fired {
        actions.push(IdleAction::Lock);
    }
    if cfg.suspend_secs > 0 && idle_secs >= u64::from(cfg.suspend_secs) && !state.idle_suspend_fired
    {
        actions.push(IdleAction::Suspend);
    }
    actions
}

/// Wykonuje akcje bezczynności (wywoływane w pętli głównej co ~1 s).
pub(crate) fn tick_idle(state: &mut State) {
    for action in idle_actions(state) {
        match action {
            IdleAction::Blank => state.set_blanked(true),
            IdleAction::Lock => {
                state.idle_lock_fired = true;
                spawn_lock();
            }
            IdleAction::Suspend => {
                state.idle_suspend_fired = true;
                if !state.locked {
                    state.idle_lock_fired = true;
                    spawn_lock();
                }
                let _ = Command::new("systemctl")
                    .arg("suspend")
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn();
            }
        }
    }
}

/// Ile milisekund do najbliższej akcji bezczynności (`None` = nic nie czeka).
pub(crate) fn idle_wait_ms(state: &State) -> Option<i64> {
    if state.locked {
        return None;
    }
    let idle_secs = state.idle_last_input.elapsed().as_secs();
    state
        .idle
        .next_deadline_secs(
            idle_secs,
            state.is_blanked(),
            state.idle_lock_fired,
            state.idle_suspend_fired,
        )
        .map(|secs| (secs.max(0.05) * 1000.0) as i64)
}

/// Uruchamia ekran blokady (`luna-lock` tworzy overlay i przejmuje klawiaturę).
fn spawn_lock() {
    if let Err(err) = Command::new("luna-lock")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        eprintln!("selene/idle: nie można uruchomić luna-lock: {err}");
    }
}

/// Prostokąt monitora, na którym leży okno (x, y, szerokość, wysokość).
fn window_output_rect(state: &State, slot: usize) -> (f64, f64, f64, f64) {
    let center = state
        .windows
        .lock_or_recover()
        .get(slot)
        .map(|w| w.content().center())
        .unwrap_or(Point::new(0.0, 0.0));
    output_rect_at(state, center)
}

/// Indeks monitora, na którym leży okno.
fn window_output_index(state: &State, slot: usize) -> usize {
    let center = state
        .windows
        .lock_or_recover()
        .get(slot)
        .map(|w| w.content().center())
        .unwrap_or(Point::new(0.0, 0.0));
    let outs = state.outputs.lock_or_recover();
    crate::monitors::output_at(&outs, center.x, center.y)
}

/// Górna strefa wyłączna panelu na danym monitorze (0 = brak panelu).
fn inset_for_output(state: &State, idx: usize) -> f64 {
    state.top_insets.get(&idx).copied().unwrap_or(0.0)
}

/// Górna strefa wyłączna panelu na monitorze, na którym leży okno.
fn window_inset(state: &State, slot: usize) -> f64 {
    inset_for_output(state, window_output_index(state, slot))
}

/// Górna strefa wyłączna panelu na monitorze pod punktem.
fn inset_at(state: &State, pos: Point) -> f64 {
    let idx = {
        let outs = state.outputs.lock_or_recover();
        crate::monitors::output_at(&outs, pos.x, pos.y)
    };
    inset_for_output(state, idx)
}

/// Ogranicza pozycję przeciąganego okna do obszaru roboczego.
///
/// Pionowo: pasek tytułu nigdy nie chowa się pod górnym panelem (`top_inset`) ani nie
/// wyjeżdża tak daleko, by nie dało się go złapać. Poziomo: co najmniej
/// [`MIN_VISIBLE`] pikseli okna zostaje na monitorze.
fn clamp_drag_pos(
    area: (f64, f64, f64, f64),
    top_inset: f64,
    width: f64,
    x: f64,
    y: f64,
) -> (f64, f64) {
    let (ox, oy, ow, oh) = area;
    let min_y = oy + top_inset;
    let max_y = (oy + oh - MIN_VISIBLE).max(min_y);
    let y = y.clamp(min_y, max_y);
    let min_x = ox - (width - MIN_VISIBLE).max(0.0);
    let max_x = (ox + ow - MIN_VISIBLE).max(min_x);
    let x = x.clamp(min_x, max_x);
    (x, y)
}

/// Zaczyna przeciąganie okna.
///
/// Zmaxymalizowane/pełnoekranowe okno najpierw wraca do rozmiaru domyślnego — kursor
/// zostaje w tym samym miejscu względem szerokości (typowe zachowanie pulpitów).
fn begin_drag(state: &mut State, slot: usize, pos: Point) {
    let info = {
        let windows = state.windows.lock_or_recover();
        let Some(w) = windows.get(slot) else {
            return;
        };
        (
            w.maximized,
            w.fullscreen,
            w.x,
            w.y,
            w.width as f64,
            w.height as f64,
        )
    };
    if info.0 || info.1 {
        let fx = if info.4 > 0.0 {
            ((pos.x - info.2) / info.4).clamp(0.0, 1.0)
        } else {
            0.5
        };
        restore_window(state, slot);
        if let Some((nx, ny, nw, nh)) = window_geometry(state, slot) {
            state.drag = Some(DragState {
                slot,
                off_x: fx * nw,
                // Chwytamy pasek tytułu, nawet jeśli kursor był wyżej (np. na kliencie CSD).
                off_y: (pos.y - ny).clamp(0.0, (TITLEBAR - 1.0).max(0.0)),
                start_px: pos.x,
                start_py: pos.y,
                origin: SavedGeom {
                    x: nx,
                    y: ny,
                    width: nw as i32,
                    height: nh as i32,
                },
            });
        }
        return;
    }
    state.drag = Some(DragState {
        slot,
        off_x: pos.x - info.2,
        off_y: pos.y - info.3,
        start_px: pos.x,
        start_py: pos.y,
        origin: SavedGeom {
            x: info.2,
            y: info.3,
            width: info.4 as i32,
            height: info.5 as i32,
        },
    });
}

/// Ustawia minimalizację okna (ukrycie / przywrócenie).
pub fn set_minimized(state: &mut State, slot: usize, minimized: bool) {
    let changed = {
        let mut windows = state.windows.lock_or_recover();
        match windows.get_mut(slot) {
            Some(w) if !w.popup && !w.layer && w.minimized != minimized => {
                w.minimized = minimized;
                true
            }
            _ => false,
        }
    };
    if changed {
        if minimized {
            clear_focus_if(state, slot);
        } else {
            raise_and_focus(state, slot);
        }
        crate::foreign_toplevel::broadcast_states(state);
    }
}

/// Przełącza minimalizację (ukrycie przez kompozytor).
fn toggle_minimize(state: &mut State, slot: usize) {
    let cur = state
        .windows
        .lock_or_recover()
        .get(slot)
        .map(|w| w.minimized)
        .unwrap_or(false);
    set_minimized(state, slot, !cur);
}

/// Ustawia stan maksymalizacji okna.
pub fn set_maximized(state: &mut State, slot: usize, maximized: bool) {
    let cur = state
        .windows
        .lock_or_recover()
        .get(slot)
        .map(|w| w.maximized);
    if std::env::var("SELENE_STATS").is_ok() {
        eprintln!("selene/win: set_maximized slot={slot} cur={cur:?} want={maximized}");
    }
    if cur != Some(maximized) {
        toggle_maximize(state, slot);
    }
}

/// Ustawia stan pełnego ekranu okna.
pub fn set_fullscreen(state: &mut State, slot: usize, fullscreen: bool) {
    let cur = state
        .windows
        .lock_or_recover()
        .get(slot)
        .map(|w| w.fullscreen);
    if cur != Some(fullscreen) {
        toggle_fullscreen(state, slot);
    }
}

/// Przełącza maksymalizację (zapamiętuje/odtwarza geometrię).
fn toggle_maximize(state: &mut State, slot: usize) {
    let (ox, oy, ow, oh) = window_output_rect(state, slot);
    // Górny panel rezerwuje miejsce **tylko na swoim monitorze**.
    let top = window_inset(state, slot);
    {
        let mut windows = state.windows.lock_or_recover();
        let Some(w) = windows.get_mut(slot) else {
            return;
        };
        if w.popup || w.layer {
            return;
        }
        if w.maximized {
            if let Some(sv) = w.saved.take() {
                w.x = sv.x;
                w.y = sv.y;
                w.width = sv.width;
                w.height = sv.height;
            }
            w.maximized = false;
        } else {
            // Zapamiętujemy **tylko** pierwszy rozmiar „normalny”. Po przyciągnięciu do
            // połowy/ćwiartki `saved` już jest ustawione i maksymalizacja nie może go
            // nadpisać — inaczej powrót z pełnego ekranu gubił rozmiar domyślny.
            if !w.fullscreen && w.saved.is_none() {
                w.saved = Some(SavedGeom {
                    x: w.x,
                    y: w.y,
                    width: w.width,
                    height: w.height,
                });
            }
            w.maximized = true;
            w.fullscreen = false;
            w.minimized = false;
            // Okna CSD (GTK/libadwaita, X11) nie mają naszego paska tytułu — nie
            // rezerwujemy dla niego wysokości paska, bo zostawałaby pusta szrama u dołu.
            let bar = if w.ssd { TITLEBAR } else { 0.0 };
            w.x = ox;
            w.y = oy + top;
            w.width = ow as i32;
            w.height = (oh - top - bar).max(MIN_H) as i32;
        }
    }
    let (nw, nh) = window_geometry(state, slot)
        .map(|(_, _, w, h)| (w, h))
        .unwrap_or((ow, oh));
    send_configure(state, slot, nw, nh);
    // Okno X11: powiadom klienta o stanie maksymalizacji (EWMH `_NET_WM_STATE`)
    // i przekaż mu nową geometrię — X11 nie zna `xdg_toplevel.configure`.
    let maximized = state
        .windows
        .lock_or_recover()
        .get(slot)
        .map(|w| w.maximized)
        .unwrap_or(false);
    if let Some(xwm) = crate::xwm::global() {
        if let Some(win) = x11_window(state, slot) {
            xwm.set_maximized(win, maximized);
        }
    }
    sync_x11_geometry(state, slot);
    crate::foreign_toplevel::broadcast_states(state);
}

/// Przełącza pełny ekran (ukrywa dekorację).
fn toggle_fullscreen(state: &mut State, slot: usize) {
    let (ox, oy, ow, oh) = window_output_rect(state, slot);
    // Panel górny jest zawsze widoczny na swoim monitorze — pełny ekran zajmuje
    // **obszar roboczy tylko tam**. Na monitorze bez panelu okno dostaje całą wysokość.
    let top = window_inset(state, slot);
    // Nowa kolejność dla wejścia i wyjścia — okno wraca nad pozostałe.
    let raise_z = state.next_z();
    {
        let mut windows = state.windows.lock_or_recover();
        let Some(w) = windows.get_mut(slot) else {
            return;
        };
        if w.popup || w.layer {
            return;
        }
        if w.fullscreen {
            if let Some(sv) = w.saved.take() {
                w.x = sv.x;
                w.y = sv.y;
                w.width = sv.width;
                w.height = sv.height;
            }
            w.fullscreen = false;
            w.z = raise_z;
        } else {
            if !w.maximized && w.saved.is_none() {
                w.saved = Some(SavedGeom {
                    x: w.x,
                    y: w.y,
                    width: w.width,
                    height: w.height,
                });
            }
            w.fullscreen = true;
            w.maximized = false;
            w.minimized = false;
            w.x = ox;
            w.y = oy + top;
            w.width = ow as i32;
            w.height = (oh - top).max(MIN_H) as i32;
            w.z = raise_z;
        }
    }
    let (nw, nh) = window_geometry(state, slot)
        .map(|(_, _, w, h)| (w, h))
        .unwrap_or((ow, oh));
    send_configure(state, slot, nw, nh);
    // Okno X11: powiadom klienta o stanie (EWMH `_NET_WM_STATE`) i geometrii pełnego ekranu.
    let (fullscreen, maximized) = state
        .windows
        .lock_or_recover()
        .get(slot)
        .map(|w| (w.fullscreen, w.maximized))
        .unwrap_or((false, false));
    if let Some(xwm) = crate::xwm::global() {
        if let Some(win) = x11_window(state, slot) {
            xwm.set_maximized(win, maximized);
            xwm.set_fullscreen(win, fullscreen);
        }
    }
    sync_x11_geometry(state, slot);
    crate::foreign_toplevel::broadcast_states(state);
}

/// Przekazuje oknu X11 bieżącą geometrię treści (pozycję i rozmiar).
///
/// X11 nie dostaje `xdg_toplevel.configure`, więc każda zmiana położenia
/// (przeciąganie, maksymalizacja, pełny ekran) musi iść osobnym `ConfigureWindow`.
fn sync_x11_geometry(state: &State, slot: usize) {
    let Some((x, y, w, h)) = window_geometry(state, slot) else {
        return;
    };
    // `Window.x/y` to lewy-górny róg **całego** okna (z naszym paskiem). Treść X11
    // zaczyna się `bar` pikseli niżej — bez tego klient rysowałby pod dekoracją.
    let bar = state
        .windows
        .lock_or_recover()
        .get(slot)
        .map(|w| w.bar())
        .unwrap_or(0.0);
    let Some(xwm) = crate::xwm::global() else {
        return;
    };
    if let Some(win) = x11_window(state, slot) {
        xwm.set_geometry(win, x as i32, (y + bar) as i32, w as u32, h as u32);
    }
}

/// Okno X11 odpowiadające slotowi (jeśli to nie okno X11 — `None`).
fn x11_window(state: &State, slot: usize) -> Option<u32> {
    let surface = state.slots.get(slot)?;
    let sd = surface.data::<SurfaceData>()?;
    if !*sd.xwayland.lock_or_recover() {
        return None;
    }
    let serial = *sd.x11_serial.lock_or_recover();
    crate::xwm::global()?.window_for_serial(serial)
}

/// Zdejmuje fokus klawiatury/wskaźnika z okna (np. po minimalizacji).
pub(crate) fn clear_focus_if(state: &mut State, slot: usize) {
    if state.keyboard_focus.as_ref().map(|(s, _)| *s) == Some(slot) {
        if let Some((_, surface)) = state.keyboard_focus.clone() {
            if let Some(kb) = keyboard_for(state, &surface) {
                let serial = state.next_serial();
                let _ = kb.send_event(wl_keyboard::Event::Leave { serial, surface });
            }
        }
        state.keyboard_focus = None;
        for window in state.windows.lock_or_recover().iter_mut() {
            window.focused = false;
        }
    }
    if state.pointer_focus.as_ref().map(|(s, _)| *s) == Some(slot) {
        pointer_leave(state);
    }
    crate::foreign_toplevel::broadcast_states(state);
}

/// Prosi klienta o zamknięcie okna (`xdg_toplevel.close`).
fn send_close(state: &mut State, slot: usize) {
    let Some(surface) = state.slots.get(slot).cloned() else {
        return;
    };
    let Some(sd) = surface.data::<SurfaceData>() else {
        return;
    };
    // Okno X11 nie ma `xdg_toplevel` — zamykamy je przez XWM (`WM_DELETE_WINDOW`).
    if *sd.xwayland.lock_or_recover() {
        let serial = *sd.x11_serial.lock_or_recover();
        if let Some(xwm) = crate::xwm::global() {
            if let Some(win) = xwm.window_for_serial(serial) {
                xwm.close(win);
            }
        }
        return;
    }
    let Some(xs) = sd.xdg_surface.lock_or_recover().clone() else {
        return;
    };
    if let Some(xd) = xs.data::<XdgSurfaceData>() {
        if let Some(tl) = xd.toplevel.lock_or_recover().as_ref() {
            let _ = tl.send_event(xdg_toplevel::Event::Close);
        }
    }
}

fn raise_and_focus(state: &mut State, slot: usize) {
    let z = state.next_z();
    {
        let mut windows = state.windows.lock_or_recover();
        if let Some(w) = windows.get_mut(slot) {
            // Powierzchnie warstwowe (panele/docki) trzymają stałą kolejność.
            if !w.layer {
                w.z = z;
            }
        }
    }
    set_keyboard_focus(state, slot);
    crate::foreign_toplevel::broadcast_states(state);
}

/// Aktywuje okno o podanym slocie (z `zwlr_foreign_toplevel_handle_v1.activate`).
/// Zminimalizowane okno zostaje przywrócone — inaczej klik w pasek nie pokazywał okna.
pub fn activate_slot(state: &mut State, slot: usize) {
    let minimized = state
        .windows
        .lock_or_recover()
        .get(slot)
        .map(|w| w.minimized)
        .unwrap_or(false);
    if minimized {
        set_minimized(state, slot, false);
    }
    raise_and_focus(state, slot);
}

/// Zamyka okno reprezentowane przez `surface` (z `...handle_v1.close`).
pub fn close_surface(state: &mut State, surface: &wayland_server::protocol::wl_surface::WlSurface) {
    if let Some(sd) = surface.data::<SurfaceData>() {
        if let Some(slot) = *sd.slot.lock_or_recover() {
            send_close(state, slot);
        }
    }
}

pub(crate) fn set_keyboard_focus(state: &mut State, slot: usize) {
    if std::env::var("SELENE_STATS").is_ok() {
        eprintln!("selene/focus: slot={slot}");
    }
    // Odzwierciedl fokus w modelu okien — renderer przyciemnia nieaktywne okna.
    {
        let mut windows = state.windows.lock_or_recover();
        for (index, window) in windows.iter_mut().enumerate() {
            window.focused = index == slot;
        }
    }
    if state.keyboard_focus.as_ref().map(|(s, _)| *s) == Some(slot) {
        return;
    }
    if let Some((_, surface)) = state.keyboard_focus.clone() {
        if let Some(kb) = keyboard_for(state, &surface) {
            let serial = state.next_serial();
            let _ = kb.send_event(wl_keyboard::Event::Leave { serial, surface });
        }
    }
    state.keyboard_focus = None;
    if let Some(surface) = state.slots.get(slot).cloned() {
        // Protokół: `wl_data_device.selection` musi dotrzeć bezpośrednio przed
        // `wl_keyboard.enter` (klienci unieważniają ofertę po utracie fokusu).
        crate::data::send_selection_for_slot(state, slot);
        if let Some(kb) = keyboard_for(state, &surface) {
            let serial = state.next_serial();
            let _ = kb.send_event(wl_keyboard::Event::Enter {
                serial,
                surface: surface.clone(),
                keys: vec![],
            });
        }
        state.keyboard_focus = Some((slot, surface));
        // X11: powiadom XWM, które okno jest aktywne (`_NET_ACTIVE_WINDOW`).
        notify_xwm_focus(state, slot);
    }
}

/// Informuje XWM o aktywnym oknie (jeśli to okno X11).
fn notify_xwm_focus(state: &State, slot: usize) {
    let Some(xwm) = crate::xwm::global() else {
        return;
    };
    let Some(surface) = state.slots.get(slot) else {
        return;
    };
    let Some(sd) = surface.data::<SurfaceData>() else {
        return;
    };
    if *sd.xwayland.lock_or_recover() {
        let serial = *sd.x11_serial.lock_or_recover();
        xwm.set_active(xwm.window_for_serial(serial));
    }
}

/// Zgłasza żądanie zrzutu ekranu (realizuje je wątek wyjścia).
fn request_screenshot(state: &mut State) {
    state
        .screenshot
        .store(true, std::sync::atomic::Ordering::Relaxed);
    if std::env::var("SELENE_STATS").is_ok() {
        eprintln!("selene: żądanie zrzutu ekranu");
    }
}

fn keyboard_key(state: &mut State, keysym: u32, keycode: u32, st: ButtonState) {
    state.note_activity();
    sync_modifiers(state, keysym, st);
    if is_modifier_keysym(keysym) || is_lock_keysym(keysym) {
        return;
    }
    // Gdy sesja jest zablokowana, **żaden** skrót kompozytora nie działa —
    // wszystkie klawisze (także Super, Print i multimedialne) idą do lockera.
    if !state.locked {
        // Escape zamyka podgląd (Super+Tab) i nie trafia do klienta.
        if keysym == 0xff1b && state.overview_on() {
            state.set_overview(false);
            return;
        }
        // `Print` — zrzut ekranu bez modyfikatora (alternatywa dla Super+F12).
        if keysym == 0xff61 {
            if st == ButtonState::Pressed {
                request_screenshot(state);
            }
            return;
        }
        // Alt+Tab — przełącz okna (klasyczny skrót pulpitu)
        if state.sent_mods.alt && keysym == 0xff09 && st == ButtonState::Pressed {
            cycle_windows(state);
            return;
        }
        // Alt+F4 — zamknij aktywne okno
        if state.sent_mods.alt && keysym == 0xffbe && st == ButtonState::Pressed {
            if let Some((s, _)) = state.keyboard_focus {
                send_close(state, s);
            }
            return;
        }
        // Klawisze multimedialne (dźwięk/głośność)
        match keysym {
            0x1008ff13 => {
                if st == ButtonState::Pressed {
                    let _ = std::process::Command::new("wpctl")
                        .args(["set-volume", "@DEFAULT_AUDIO_SINK@", "5%+"])
                        .spawn();
                }
                return;
            }
            0x1008ff11 => {
                if st == ButtonState::Pressed {
                    let _ = std::process::Command::new("wpctl")
                        .args(["set-volume", "@DEFAULT_AUDIO_SINK@", "5%-"])
                        .spawn();
                }
                return;
            }
            0x1008ff12 => {
                if st == ButtonState::Pressed {
                    let _ = std::process::Command::new("wpctl")
                        .args(["set-mute", "@DEFAULT_AUDIO_SINK@", "toggle"])
                        .spawn();
                }
                return;
            }
            _ => {}
        }
        // Skróty pulpitu z konfiguracji (`~/.config/luna/shortcuts.conf`)
        // przechwytujemy przed klientem; domyślnie wymagają Super.
        if window_shortcut(state, keysym, st) {
            return;
        }
    }
    // Klawiatura klienta okna z fokusem — nie wolno pisać do zasobu innego klienta.
    let Some((_, surface)) = state.keyboard_focus.clone() else {
        return;
    };
    let Some(kb) = keyboard_for(state, &surface) else {
        return;
    };
    // Do klienta trafia **fizyczny kod klawisza** (evdev), a nie kod X11 ani keysym —
    // klient liczy z niego znak według mapy klawiszy, którą mu wysłaliśmy.
    if keycode != 0 {
        let serial = state.next_serial();
        let time = state.time_ms();
        let wl_state = match st {
            ButtonState::Pressed => WEnum::Value(wl_keyboard::KeyState::Pressed),
            ButtonState::Released => WEnum::Value(wl_keyboard::KeyState::Released),
        };
        let _ = kb.send_event(wl_keyboard::Event::Key {
            serial,
            time,
            key: keycode,
            state: wl_state,
        });
    }
}

/// Skróty pulpitu z `shortcuts.conf`. Zwraca `true`, gdy obsłużono.
fn window_shortcut(state: &mut State, keysym: u32, st: ButtonState) -> bool {
    use libluna_core::shortcuts::{active, ShortcutAction};

    let settings = active();
    let mods = state.sent_mods;
    // Przestrzenie robocze: Shift nie jest częścią kombinacji — decyduje o
    // przeniesieniu okna zamiast przełączenia (Super+Shift+1 …).
    let mut matched = None;
    for action in ShortcutAction::ALL {
        let Some(binding) = settings.binding(action) else {
            continue;
        };
        let shift = match action {
            ShortcutAction::Workspace(_) => false,
            _ => mods.shift,
        };
        if binding.matches(mods.logo, mods.ctrl, mods.alt, shift, keysym) {
            matched = Some(action);
            break;
        }
    }
    let Some(action) = matched else {
        return false;
    };
    // Zjedz także zwolnienie klawisza, żeby nie trafiło do klienta.
    if st != ButtonState::Pressed {
        return true;
    }
    let focused = state.keyboard_focus.as_ref().map(|(s, _)| *s);
    match action {
        ShortcutAction::CloseWindow => {
            if let Some(s) = focused {
                send_close(state, s);
            }
        }
        ShortcutAction::Minimize => {
            if let Some(s) = focused {
                toggle_minimize(state, s);
            }
        }
        ShortcutAction::Maximize => {
            if let Some(s) = focused {
                toggle_maximize(state, s);
            }
        }
        ShortcutAction::Fullscreen => {
            if let Some(s) = focused {
                toggle_fullscreen(state, s);
            }
        }
        ShortcutAction::RestoreWindow => {
            if let Some(s) = focused {
                restore_window(state, s);
            }
        }
        ShortcutAction::SnapLeft => {
            if let Some(s) = focused {
                snap_window(state, s, true);
            }
        }
        ShortcutAction::SnapRight => {
            if let Some(s) = focused {
                snap_window(state, s, false);
            }
        }
        ShortcutAction::Overview => {
            let on = state.overview_on();
            state.set_overview(!on);
        }
        ShortcutAction::CycleWindows => cycle_windows(state),
        ShortcutAction::Workspace(index) => {
            if state.sent_mods.shift {
                if let Some(s) = focused {
                    move_window_to_workspace(state, s, index);
                }
            } else {
                switch_workspace(state, index);
            }
        }
        ShortcutAction::Screenshot => request_screenshot(state),
        ShortcutAction::Logout => {
            // Kończymy kompozytor (sesję), więc menedżer logowania wraca do greetera.
            println!("{}", libluna_i18n::tr("logging out"));
            crate::vt::restore();
            std::process::exit(0);
        }
    }
    true
}

/// Strefa przyciągania okna po upuszczeniu przy krawędzi/narożniku monitora.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SnapZone {
    Left,
    Right,
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
    Maximize,
}

/// Wykrywa strefę na podstawie pozycji kursora w obszarze roboczym.
fn snap_zone(pos: Point, area: Rect) -> Option<SnapZone> {
    let left = pos.x <= area.x() + SNAP_EDGE;
    let right = pos.x >= area.right() - SNAP_EDGE;
    let top = pos.y <= area.y() + SNAP_EDGE;
    let bottom = pos.y >= area.bottom() - SNAP_EDGE;
    match (left, top, bottom, right) {
        // Narożniki mają pierwszeństwo — dają ćwiartki.
        (true, true, _, _) => Some(SnapZone::TopLeft),
        (true, _, true, _) => Some(SnapZone::BottomLeft),
        (_, true, _, true) => Some(SnapZone::TopRight),
        (_, _, true, true) => Some(SnapZone::BottomRight),
        // Krawędzie — połowy; góra (środek) — maksymalizacja.
        (true, _, _, _) => Some(SnapZone::Left),
        (_, _, _, true) => Some(SnapZone::Right),
        (_, true, _, _) => Some(SnapZone::Maximize),
        _ => None,
    }
}

/// Geometria **treści** okna dla strefy: `(x, y, szerokość, wysokość)`.
///
/// `bar` to wysokość naszego paska tytułu (0 dla CSD) — dzięki temu zewnętrzna
/// krawędź okna dokładnie pokrywa się z krawędzią połowy/ćwiartki monitora.
fn snap_rect(zone: SnapZone, area: Rect, bar: f64) -> (f64, f64, f64, f64) {
    let half_w = (area.width() / 2.0).round().max(MIN_W);
    let right_w = (area.width() - half_w).max(MIN_W);
    let half_outer_h = (area.height() / 2.0).round();
    let quarter_h = (half_outer_h - bar).max(MIN_H);
    let full_h = (area.height() - bar).max(MIN_H);
    let bottom_y = area.y() + half_outer_h;
    match zone {
        SnapZone::Maximize => (area.x(), area.y(), area.width(), full_h),
        SnapZone::Left => (area.x(), area.y(), half_w, full_h),
        SnapZone::Right => (area.x() + half_w, area.y(), right_w, full_h),
        SnapZone::TopLeft => (area.x(), area.y(), half_w, quarter_h),
        SnapZone::TopRight => (area.x() + half_w, area.y(), right_w, quarter_h),
        SnapZone::BottomLeft => (area.x(), bottom_y, half_w, quarter_h),
        SnapZone::BottomRight => (area.x() + half_w, bottom_y, right_w, quarter_h),
    }
}

/// Obszar roboczy monitora pod kursorem (bez górnego panelu — jeśli tam jest).
fn workarea_at(state: &State, pos: Point) -> Rect {
    let (ox, oy, ow, oh) = output_rect_at(state, pos);
    let top = inset_at(state, pos).min(oh - MIN_H);
    Rect::from_xywh(ox, oy + top, ow, (oh - top).max(MIN_H))
}

/// Po upuszczeniu przeciąganego okna: przyciągnij je, jeśli kursor jest przy krawędzi.
fn apply_drag_snap(state: &mut State, slot: usize, pos: Point, origin: SavedGeom) {
    if let Some(zone) = snap_zone(pos, workarea_at(state, pos)) {
        snap_to_zone(state, slot, zone, Some(origin));
    }
}

/// Ustawia geometrię treści okna (bez ruszania zapamiętanego rozmiaru).
fn place_window(state: &mut State, slot: usize, x: f64, y: f64, w: f64, h: f64) {
    {
        let mut windows = state.windows.lock_or_recover();
        let Some(win) = windows.get_mut(slot) else {
            return;
        };
        if win.popup || win.layer {
            return;
        }
        win.x = x;
        win.y = y;
        win.width = w as i32;
        win.height = h as i32;
    }
    send_configure(state, slot, w, h);
    sync_x11_geometry(state, slot);
    crate::foreign_toplevel::broadcast_states(state);
}

/// Przyciąga okno do strefy: połowa, ćwiartka albo maksymalizacja.
///
/// `remember` to geometria sprzed przeciągania — wraca jako rozmiar domyślny
/// (Super+Down / podwójny klik po wyjściu ze stanu).
fn snap_to_zone(state: &mut State, slot: usize, zone: SnapZone, remember: Option<SavedGeom>) {
    if zone == SnapZone::Maximize {
        set_maximized(state, slot, true);
        return;
    }
    let (ox, oy, ow, oh) = window_output_rect(state, slot);
    let top = window_inset(state, slot).min(oh - MIN_H);
    let work = Rect::from_xywh(ox, oy + top, ow, (oh - top).max(MIN_H));
    let bar = {
        let windows = state.windows.lock_or_recover();
        match windows.get(slot) {
            Some(w) if !w.popup && !w.layer => {
                if w.ssd {
                    TITLEBAR
                } else {
                    0.0
                }
            }
            _ => return,
        }
    };
    let (x, y, w, h) = snap_rect(zone, work, bar);
    {
        let mut windows = state.windows.lock_or_recover();
        let Some(win) = windows.get_mut(slot) else {
            return;
        };
        // Pierwszy „normalny” rozmiar jest punktem powrotu dla maksymalizacji i
        // pełnego ekranu; kolejne przyciągnięcia go nie nadpisują.
        if win.saved.is_none() {
            win.saved = remember.or(Some(SavedGeom {
                x: win.x,
                y: win.y,
                width: win.width,
                height: win.height,
            }));
        }
        win.maximized = false;
        win.fullscreen = false;
        win.minimized = false;
    }
    place_window(state, slot, x, y, w, h);
}

/// Kafelkuje okno do lewej lub prawej połowy ekranu (skrót klawiszowy).
fn snap_window(state: &mut State, slot: usize, left: bool) {
    snap_to_zone(
        state,
        slot,
        if left {
            SnapZone::Left
        } else {
            SnapZone::Right
        },
        None,
    );
}

/// Przywraca okno z minimalizacji/maksymalizacji/fullscreenu/przyciągnięcia.
fn restore_window(state: &mut State, slot: usize) {
    let (min, max, full, saved) = {
        let windows = state.windows.lock_or_recover();
        match windows.get(slot) {
            Some(w) => (w.minimized, w.maximized, w.fullscreen, w.saved),
            None => return,
        }
    };
    if min {
        toggle_minimize(state, slot);
    } else if full {
        toggle_fullscreen(state, slot);
    } else if max {
        toggle_maximize(state, slot);
    } else if let Some(sv) = saved {
        // Okno przyciągnięte do połowy/ćwiartki — wróć do rozmiaru domyślnego.
        if let Some(w) = state.windows.lock_or_recover().get_mut(slot) {
            w.saved = None;
        }
        place_window(state, slot, sv.x, sv.y, sv.width as f64, sv.height as f64);
    }
}

/// Przełącza fokus na kolejne okno (przywracając je, jeśli było zminimalizowane).
fn cycle_windows(state: &mut State) {
    let slots: Vec<usize> = {
        let windows = state.windows.lock_or_recover();
        windows
            .iter()
            .enumerate()
            .filter(|(_, w)| w.mapped && !w.popup && !w.layer)
            .map(|(i, _)| i)
            .collect()
    };
    if slots.is_empty() {
        return;
    }
    let cur = state.keyboard_focus.as_ref().map(|(s, _)| *s);
    let next = match cur.and_then(|c| slots.iter().position(|&s| s == c)) {
        Some(p) => slots[(p + 1) % slots.len()],
        None => slots[0],
    };
    if let Some(w) = state.windows.lock_or_recover().get_mut(next) {
        w.minimized = false;
    }
    raise_and_focus(state, next);
}

/// Najwyższe zwykłe okno w danej przestrzeni roboczej.
fn top_window_in_ws(state: &State, ws: usize) -> Option<usize> {
    let windows = state.windows.lock_or_recover();
    windows
        .iter()
        .enumerate()
        .filter(|(_, w)| w.mapped && !w.minimized && !w.popup && !w.layer && w.workspace == ws)
        .max_by_key(|(_, w)| w.z)
        .map(|(i, _)| i)
}

/// Przełącza bieżącą przestrzeń roboczą, przenosząc fokus na jej wierzchołek.
pub fn switch_workspace(state: &mut State, ws: usize) {
    let ws = ws.min(state.ws_count.saturating_sub(1));
    {
        let mut cur = state.ws_current.lock_or_recover();
        if *cur == ws {
            return;
        }
        *cur = ws;
    }
    // Zdejmij fokus z okna, którego nie ma już na ekranie.
    let focused = state.keyboard_focus.as_ref().map(|(s, _)| *s);
    let keep = focused
        .and_then(|s| {
            state
                .windows
                .lock_or_recover()
                .get(s)
                .map(|w| w.in_workspace(ws))
        })
        .unwrap_or(false);
    if !keep {
        set_keyboard_focus(state, usize::MAX);
        if let Some(top) = top_window_in_ws(state, ws) {
            raise_and_focus(state, top);
        }
    }
    crate::workspace::broadcast_workspace_states(state);
}

/// Przenosi okno o podanym tytule na przestrzeń `ws` (polecenie z powłoki, PPM w taskbarze).
///
/// Preferujemy okno z fokusem (jeśli tytuł pasuje), potem pierwsze o dokładnie takim
/// tytule. Tytuły są tym, co widzi użytkownik w taskbarze — nie potrzebujemy wewnętrznych
/// identyfikatorów kompozytora.
pub fn move_window_by_title(state: &mut State, title: &str, ws: usize) {
    if title.is_empty() || ws >= state.ws_count {
        return;
    }
    let slot = {
        let windows = state.windows.lock_or_recover();
        let matches = |s: usize| {
            windows
                .get(s)
                .map(|w| !w.layer && !w.popup && w.title == title)
                .unwrap_or(false)
        };
        state
            .keyboard_focus
            .as_ref()
            .map(|(s, _)| *s)
            .filter(|s| matches(*s))
            .or_else(|| (0..windows.len()).find(|s| matches(*s)))
    };
    match slot {
        Some(slot) => {
            move_window_to_workspace(state, slot, ws);
            if std::env::var("SELENE_STATS").is_ok() {
                eprintln!("selene/control: przeniesiono {title:?} → przestrzeń {ws}");
            }
        }
        None => {
            if std::env::var("SELENE_STATS").is_ok() {
                eprintln!("selene/control: nie znaleziono okna {title:?}");
            }
        }
    }
}

/// Przenosi okno do zadanej przestrzeni roboczej (skrót Super+Shift+cyfra).
fn move_window_to_workspace(state: &mut State, slot: usize, ws: usize) {
    let ws = ws.min(state.ws_count.saturating_sub(1));
    {
        let mut windows = state.windows.lock_or_recover();
        match windows.get_mut(slot) {
            Some(w) if !w.layer && !w.popup => w.workspace = ws,
            _ => return,
        }
    }
    let cur = *state.ws_current.lock_or_recover();
    if ws != cur && state.keyboard_focus.as_ref().map(|(s, _)| *s) == Some(slot) {
        set_keyboard_focus(state, usize::MAX);
        if let Some(top) = top_window_in_ws(state, cur) {
            raise_and_focus(state, top);
        }
    }
    crate::workspace::broadcast_workspace_states(state);
}

/// Monitor pod punktem: `(origin_x, origin_y, (szerokość, wysokość))`.
fn overview_output(state: &State, pos: Point) -> (f64, f64, (f64, f64)) {
    let outs = state.outputs.lock_or_recover();
    let idx = crate::monitors::output_at(&outs, pos.x, pos.y);
    match outs.get(idx) {
        Some(o) => (o.x as f64, o.y as f64, (o.width as f64, o.height as f64)),
        None => (0.0, 0.0, (1280.0, 800.0)),
    }
}

/// Slot okna pod punktem w podglądzie (na monitorze pod kursorem).
fn overview_tile_at(state: &State, pos: Point) -> Option<usize> {
    let (ox, oy, size) = overview_output(state, pos);
    let local = Point::new(pos.x - ox, pos.y - oy);
    let cur_ws = *state.ws_current.lock_or_recover();
    let slots = {
        let w = state.windows.lock_or_recover();
        crate::state::overview_slots_on(&w, cur_ws, ox, size.0)
    };
    let tiles = crate::state::overview_tiles(slots.len(), size);
    tiles
        .iter()
        .position(|r| r.contains(local))
        .and_then(|k| slots.get(k).copied())
}

/// Indeks „chipa” przestrzeni pod punktem.
fn overview_chip_at(state: &State, pos: Point) -> Option<usize> {
    let (ox, oy, size) = overview_output(state, pos);
    let local = Point::new(pos.x - ox, pos.y - oy);
    crate::state::overview_ws_chips(state.ws_count, size)
        .iter()
        .position(|r| r.contains(local))
}

/// Wysyła wciśnięcia/zwolnienia klawiszy modyfikujących wynikające z bieżącego stanu.
fn sync_modifiers(state: &mut State, keysym: u32, st: ButtonState) {
    let mut mods = state.sent_mods;
    if let Some(field) = modifier_for_keysym(keysym) {
        match st {
            ButtonState::Pressed => *field(&mut mods) = true,
            ButtonState::Released => *field(&mut mods) = false,
        }
    }
    // CapsLock/NumLock to **blokady**, nie modyfikatory wciśnięte. Klient nie dostaje
    // ich jako zwykłego klawisza (inaczej przełączyłby stan drugi raz); dostaje
    // wyłącznie `wl_keyboard.modifiers` z `mods_locked`.
    let mut locked_changed = false;
    if st == ButtonState::Pressed {
        match keysym {
            0xffe5 => {
                state.caps_lock = !state.caps_lock;
                locked_changed = true;
            }
            0xff7f => {
                state.num_lock = !state.num_lock;
                locked_changed = true;
            }
            _ => {}
        }
    }
    // Stan modyfikatorów jest globalny, więc rozgłaszamy go do klawiatur wszystkich
    // klientów (każdy ma własny zasób `wl_keyboard`).
    let keyboards: Vec<wl_keyboard::WlKeyboard> = state
        .keyboards
        .iter()
        .filter(|k| k.is_alive())
        .cloned()
        .collect();
    for kb in &keyboards {
        for (before, now, code) in [
            // Kody **evdev** klawiszy modyfikujących (protokół Wayland używa evdev, nie X11).
            (state.sent_mods.shift, mods.shift, 42u32), // KEY_LEFTSHIFT
            (state.sent_mods.ctrl, mods.ctrl, 29),      // KEY_LEFTCTRL
            (state.sent_mods.alt, mods.alt, 56),        // KEY_LEFTALT
            (state.sent_mods.logo, mods.logo, 125),     // KEY_LEFTMETA
            (state.sent_mods.level3, mods.level3, 100), // KEY_RIGHTALT (AltGr)
        ] {
            if before != now {
                let serial = state.next_serial();
                let time = state.time_ms();
                let ks = if now {
                    WEnum::Value(wl_keyboard::KeyState::Pressed)
                } else {
                    WEnum::Value(wl_keyboard::KeyState::Released)
                };
                let _ = kb.send_event(wl_keyboard::Event::Key {
                    serial,
                    time,
                    key: code,
                    state: ks,
                });
            }
        }
    }
    if mods != state.sent_mods || locked_changed {
        state.sent_mods = mods;
        // Lock = bit 1, Mod2 (NumLock) = bit 4 — tak samo jak w masce XKB.
        let locked = (u32::from(state.caps_lock) << 1) | (u32::from(state.num_lock) << 4);
        for kb in &keyboards {
            let serial = state.next_serial();
            let _ = kb.send_event(wl_keyboard::Event::Modifiers {
                serial,
                mods_depressed: modifier_mask(mods),
                mods_latched: 0,
                mods_locked: locked,
                group: 0,
            });
        }
    }
}

fn modifier_for_keysym(keysym: u32) -> Option<fn(&mut Modifiers) -> &mut bool> {
    match keysym {
        0xffe1 | 0xffe2 => Some(|m| &mut m.shift), // Shift_L / Shift_R
        0xffe3 | 0xffe4 => Some(|m| &mut m.ctrl),  // Control_L / Control_R
        0xffe9 | 0xffea => Some(|m| &mut m.alt),   // Alt_L / Alt_R
        0xffeb | 0xffec => Some(|m| &mut m.logo),  // Super_L / Super_R
        0xfe03 => Some(|m| &mut m.level3),         // ISO_Level3_Shift (AltGr)
        _ => None,
    }
}

fn is_modifier_keysym(keysym: u32) -> bool {
    modifier_for_keysym(keysym).is_some()
}

/// CapsLock/NumLock — blokady obsługiwane przez `mods_locked` (nie przez zwykły klawisz).
fn is_lock_keysym(keysym: u32) -> bool {
    matches!(keysym, 0xffe5 | 0xff7f)
}

fn modifier_mask(m: Modifiers) -> u32 {
    (m.shift as u32)
        | ((m.ctrl as u32) << 2)
        | ((m.alt as u32) << 3)
        | ((m.logo as u32) << 6)
        // Poziom 3 (AltGr) w XKB to zwykle Mod5 → bit 7.
        | ((m.level3 as u32) << 7)
}

fn button_code(button: PointerButton) -> u32 {
    match button {
        PointerButton::Left => 0x110,
        PointerButton::Right => 0x111,
        PointerButton::Middle => 0x112,
        PointerButton::Back => 0x116,
        PointerButton::Forward => 0x115,
        PointerButton::Other(c) => c,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Window, BUTTON_W};
    use libluna_render::ImageData;
    use std::sync::Arc;

    /// Symuluje przytrzymanie kursora w rogu przez pełny czas (`CORNER_DWELL`).
    fn hold_in_corner(state: &mut State, pos: Point) {
        note_corner(state, pos);
        state.ws_corner_since = Some(Instant::now() - CORNER_DWELL - Duration::from_millis(1));
        tick_corner(state);
    }

    #[test]
    fn corners_open_overview_or_switch_workspace() {
        let mut state = State::default();
        state.ws_overview_corner = true;
        hold_in_corner(&mut state, Point::new(4.0, 4.0));
        assert!(state.overview_on(), "lewy górny róg ma otworzyć podgląd");

        let mut state = State::default();
        state.ws_overview_corner = false;
        state.ws_corner_switch = true;
        let before = *state.ws_current.lock_or_recover();
        hold_in_corner(&mut state, Point::new(1279.0, 799.0));
        assert_eq!(*state.ws_current.lock_or_recover(), before + 1);
    }

    #[test]
    fn corner_needs_a_hold_and_shows_a_glow() {
        let mut state = State::default();
        state.ws_overview_corner = true;
        // Sam wjazd w róg nie może nic otwierać — to była przyczyna „wariowania”
        // przy zjeżdżaniu z panelu do launchera.
        note_corner(&mut state, Point::new(4.0, 10.0));
        assert!(!state.overview_on());
        let hint = state
            .overview
            .lock_or_recover()
            .corner
            .expect("wejście w róg ma pokazać poświatę");
        assert_eq!(hint.kind, CornerKind::TopLeft);
        assert!(hint.progress < 0.1);

        // Połowa czasu: poświata rośnie, ale akcja jeszcze nie zadziałała.
        state.ws_corner_since = Some(Instant::now() - Duration::from_millis(250));
        tick_corner(&mut state);
        let hint = state.overview.lock_or_recover().corner.expect("poświata");
        assert!(hint.progress > 0.2 && hint.progress < 0.8, "{hint:?}");
        assert!(!state.overview_on());

        // Wyjście z rogu gasi poświatę i zeruje zapadkę.
        note_corner(&mut state, Point::new(400.0, 400.0));
        assert!(state.overview.lock_or_recover().corner.is_none());
        assert!(state.ws_corner_hover.is_none());
    }

    #[test]
    fn corner_does_not_open_when_leaving_panel() {
        // Droga „przycisk launchera → launcher”: zjazd z panelu prosto w dół
        // przechodzi przez strefę rogu, ale przytrzymanie liczy się dopiero
        // od wjazdu — jedno zdarzenie ruchu nie otwiera podglądu.
        let mut state = State::default();
        state.ws_overview_corner = true;
        state.top_insets.insert(0, 36.0);
        note_corner(&mut state, Point::new(16.0, 30.0));
        assert!(state.ws_corner_hover.is_none(), "panel to nie róg");
        note_corner(&mut state, Point::new(16.0, 40.0));
        tick_corner(&mut state);
        assert!(!state.overview_on());

        // Przytrzymanie w rogu działa (także gdy panel ma wysokość 36 px).
        state.ws_corner_since = Some(Instant::now() - CORNER_DWELL - Duration::from_millis(1));
        tick_corner(&mut state);
        assert!(state.overview_on());
    }

    #[test]
    fn corner_acts_once_per_entry() {
        let mut state = State::default();
        state.ws_corner_switch = true;
        let start = *state.ws_current.lock_or_recover();
        hold_in_corner(&mut state, Point::new(1279.0, 799.0));
        assert_eq!(*state.ws_current.lock_or_recover(), start + 1);
        // Kolejne ticki bez wyjścia z rogu nie przełączają dalej.
        for _ in 0..5 {
            tick_corner(&mut state);
        }
        assert_eq!(*state.ws_current.lock_or_recover(), start + 1);
        // Wyjście i ponowne wejście odblokowuje akcję.
        note_corner(&mut state, Point::new(600.0, 400.0));
        hold_in_corner(&mut state, Point::new(1279.0, 799.0));
        assert_eq!(*state.ws_current.lock_or_recover(), start + 2);
    }

    #[test]
    fn idle_blanks_then_wakes_on_activity() {
        let mut state = State::default();
        state.idle = libluna_core::idle::IdleSettings {
            screen_off_secs: 1,
            lock_secs: 0,
            suspend_secs: 0,
            lock_on_blank: false,
        };
        state.idle_last_input = Instant::now() - Duration::from_secs(2);
        assert_eq!(idle_actions(&state), vec![IdleAction::Blank]);
        tick_idle(&mut state);
        assert!(state.is_blanked());
        // Kolejny tick nie powtarza akcji.
        assert!(idle_actions(&state).is_empty());
        state.note_activity();
        assert!(!state.is_blanked());
    }

    #[test]
    fn idle_lock_is_independent_from_blanking() {
        let mut state = State::default();
        state.idle = libluna_core::idle::IdleSettings {
            screen_off_secs: 60,
            lock_secs: 30,
            suspend_secs: 0,
            lock_on_blank: false,
        };
        state.idle_last_input = Instant::now() - Duration::from_secs(40);
        assert_eq!(idle_actions(&state), vec![IdleAction::Lock]);
        // Zapadka: po „wykonaniu” blokady nie ma kolejnej akcji.
        state.idle_lock_fired = true;
        assert!(idle_actions(&state).is_empty());
    }

    #[test]
    fn lock_on_blank_adds_a_lock_action() {
        let mut state = State::default();
        state.idle = libluna_core::idle::IdleSettings {
            screen_off_secs: 10,
            lock_secs: 0,
            suspend_secs: 0,
            lock_on_blank: true,
        };
        state.idle_last_input = Instant::now() - Duration::from_secs(11);
        assert_eq!(
            idle_actions(&state),
            vec![IdleAction::Blank, IdleAction::Lock]
        );
    }

    #[test]
    fn idle_wait_points_at_the_next_deadline() {
        let mut state = State::default();
        state.idle = libluna_core::idle::IdleSettings {
            screen_off_secs: 60,
            lock_secs: 0,
            suspend_secs: 0,
            lock_on_blank: false,
        };
        state.idle_last_input = Instant::now();
        let wait = idle_wait_ms(&state).expect("jest deadline");
        assert!((50_000..=60_000).contains(&wait), "{wait}");
        state.set_blanked(true);
        assert!(idle_wait_ms(&state).is_none());
    }

    #[test]
    fn modifier_mask_matches_xkb() {
        let m = Modifiers {
            shift: true,
            ctrl: true,
            alt: false,
            logo: true,
            level3: false,
        };
        assert_eq!(modifier_mask(m), 1 | 4 | 64);
    }

    #[test]
    fn recognizes_modifier_keysyms() {
        assert!(is_modifier_keysym(0xffe1)); // Shift_L
        assert!(!is_modifier_keysym(0x0061));
    }

    #[test]
    fn lock_keys_use_locked_state() {
        // CapsLock/NumLock nie są „wciśniętymi” modyfikatorami — idą przez `mods_locked`.
        assert!(is_lock_keysym(0xffe5)); // Caps_Lock
        assert!(is_lock_keysym(0xff7f)); // Num_Lock
        assert!(!is_modifier_keysym(0xffe5));
    }

    #[test]
    fn locale_maps_to_xkb_layout() {
        assert_eq!(locale_to_layout("pl_PL.UTF-8"), Some("pl".to_owned()));
        assert_eq!(locale_to_layout("pl"), Some("pl".to_owned()));
        assert_eq!(locale_to_layout("de_DE@euro"), Some("de".to_owned()));
        assert_eq!(locale_to_layout("en_GB.UTF-8"), Some("gb".to_owned()));
        assert_eq!(locale_to_layout("en_US.UTF-8"), Some("en".to_owned()));
        // Bez języka nie ma czego zgadywać — zostaje domyślny `us`.
        assert_eq!(locale_to_layout("C"), None);
        assert_eq!(locale_to_layout("POSIX"), None);
        assert_eq!(locale_to_layout(""), None);
    }

    #[test]
    fn keymap_includes_locale_layout() {
        let text = keymap_text_for("pl", None);
        assert!(text.contains("pc+pl+inet(evdev)+level3(ralt_switch)"));
        let with_variant = keymap_text_for("pl", Some("legacy"));
        assert!(with_variant.contains("pc+pl(legacy)+inet(evdev)+level3(ralt_switch)"));
    }

    #[test]
    fn compiled_keymap_keeps_altgr() {
        // Kompilujemy RMLVO — opcja musi być w formacie `lv3:ralt_switch`.
        // Wcześniej `level3(ralt_switch)` było odrzucane przez xkbcommon (XKB-595)
        // i AltGr nie dawał poziomu 3 (ą, ć, ę, ł, ń, ó, ś, ź, ż).
        if let Some(km) = compiled_keymap("pl", None) {
            assert!(
                km.contains("level3(ralt_switch)"),
                "brak opcji level3 w skompilowanej mapie"
            );
            assert!(
                km.contains("0xfe03"),
                "AltGr (ISO_Level3_Shift) nieobecny w skompilowanej mapie"
            );
        }
    }

    #[test]
    fn altgr_is_level3_and_super_is_logo() {
        assert!(matches!(modifier_for_keysym(0xfe03), Some(_)));
        let mut m = Modifiers::NONE;
        if let Some(f) = modifier_for_keysym(0xfe03) {
            *f(&mut m) = true;
        }
        assert!(m.level3);
        assert_eq!(modifier_mask(m), 1 << 7);
    }

    fn sample_window() -> Window {
        Window {
            x: 100.0,
            y: 50.0,
            width: 200,
            height: 120,
            image_offset: (0.0, 0.0),
            z: 1,
            title: "Test".to_owned(),
            popup: false,
            layer: false,
            mapped: true,
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
            data: Arc::new(ImageData::empty(200, 120)),
        }
    }

    #[test]
    fn content_local_does_not_offset_layer_surfaces() {
        // Panel (powierzchnia warstwowa) nie ma paska tytułu — współrzędne bez przesunięcia.
        let s = State::default();
        let mut w = sample_window();
        w.layer = true;
        w.x = 0.0;
        w.y = 0.0;
        s.windows.lock_or_recover().push(w);
        assert_eq!(content_local(&s, 0, Point::new(50.0, 20.0)), (50.0, 20.0));
    }

    #[test]
    fn content_local_offsets_titlebar_for_windows() {
        let s = State::default();
        s.windows.lock_or_recover().push(sample_window()); // x=100, y=50
        assert_eq!(
            content_local(&s, 0, Point::new(120.0, 80.0)),
            (20.0, 80.0 - (50.0 + TITLEBAR))
        );
    }

    #[test]
    fn content_local_adds_csd_shadow_offset() {
        // Okno CSD: punkt ekranu (240,150) leży 40x50 od widocznego rogu, ale bufor
        // zaczyna się 14x12 wcześniej — klient musi dostać (54,62), inaczej jego
        // własne przyciski ✕/□ są „obok” kursora i klik nic nie robi.
        let s = State::default();
        let mut w = sample_window();
        w.ssd = false;
        w.image_offset = (14.0, 12.0);
        s.windows.lock_or_recover().push(w); // x=100, y=50
        assert_eq!(content_local(&s, 0, Point::new(140.0, 100.0)), (54.0, 62.0));
    }

    #[test]
    fn capturing_layer_only_outside_its_content() {
        // Powłoka z otwartym popupem ma `input_capture`; klik poza jej widoczną
        // treścią (np. dolna część ekranu) ma do niej trafić i zamknąć popup.
        let s = State::default();
        let mut w = sample_window();
        w.layer = true;
        w.input_capture = true;
        w.x = 0.0;
        w.y = 0.0;
        w.width = 1920;
        w.height = 680;
        s.windows.lock_or_recover().push(w);
        assert_eq!(capturing_layer_outside(&s, Point::new(900.0, 300.0)), None);
        assert_eq!(
            capturing_layer_outside(&s, Point::new(900.0, 900.0)),
            Some(0)
        );
        s.windows.lock_or_recover()[0].input_capture = false;
        assert_eq!(capturing_layer_outside(&s, Point::new(900.0, 900.0)), None);
    }

    #[test]
    fn workspace_predicate_switch_and_move() {
        let mut s = State::default();
        let mut a = sample_window();
        a.workspace = 0;
        let mut b = sample_window();
        b.workspace = 1;
        {
            let mut w = s.windows.lock_or_recover();
            w.push(a);
            w.push(b);
        }
        assert!(s.windows.lock_or_recover()[0].in_workspace(0));
        assert!(!s.windows.lock_or_recover()[0].in_workspace(1));
        // Powierzchnia warstwowa (panel) jest widoczna w każdej przestrzeni.
        let mut panel = sample_window();
        panel.layer = true;
        panel.workspace = 3;
        assert!(panel.in_workspace(0));

        switch_workspace(&mut s, 1);
        assert_eq!(*s.ws_current.lock_or_recover(), 1);
        move_window_to_workspace(&mut s, 0, 2);
        assert_eq!(s.windows.lock_or_recover()[0].workspace, 2);
        // Numer przestrzeni jest klamrowany do zakresu.
        switch_workspace(&mut s, 99);
        assert_eq!(*s.ws_current.lock_or_recover(), s.ws_count - 1);
    }

    #[test]
    fn window_controls_are_laid_out() {
        let w = sample_window();
        // Pasek tytułu na górze, treść pod nim.
        assert_eq!(w.titlebar().bottom(), w.y + TITLEBAR);
        assert_eq!(w.content().y(), w.y + TITLEBAR);
        // Zamknij po prawej, wyśrodkowane w pionie, 6 px od krawędzi (układ HeaderBar).
        let c = w.close_rect();
        assert_eq!((c.width(), c.height()), (BUTTON_W, BUTTON_W));
        assert!((c.center().y - (w.y + TITLEBAR / 2.0)).abs() < 0.01);
        assert!((w.x + 200.0 - c.right() - 6.0).abs() < 0.01);
        assert!(c.contains(c.center()));
        // Maksymalizuj po lewej od zamknij, minimalizuj po lewej od maksymalizuj.
        let m = w.maximize_rect();
        let n = w.minimize_rect();
        assert!(m.right() <= c.x() + 0.01);
        assert!(n.right() <= m.x() + 0.01);
    }

    #[test]
    fn resize_edges_detect_corners() {
        let w = sample_window();
        let o = w.outer();
        // Lewy-górny narożnik → góra + lewo.
        assert_eq!(
            w.resize_edges(Point::new(o.x() + 1.0, o.y() + 1.0)),
            EDGE_TOP | EDGE_LEFT
        );
        // Prawy-dolny narożnik → dół + prawo.
        assert_eq!(
            w.resize_edges(Point::new(o.right() - 1.0, o.bottom() - 1.0)),
            EDGE_BOTTOM | EDGE_RIGHT
        );
        // Środek → brak krawędzi.
        assert_eq!(w.resize_edges(o.center()), 0);
    }

    #[test]
    fn fullscreen_hides_titlebar() {
        let mut w = sample_window();
        w.fullscreen = true;
        assert_eq!(w.titlebar().bottom(), w.y);
        assert_eq!(w.content().y(), w.y);
    }

    #[test]
    fn minimize_and_restore_via_activation() {
        // Żądanie `xdg_toplevel.set_minimized` musi naprawdę ukryć okno, a aktywacja
        // (pasek/przełącznik) — przywrócić je. Wcześniej oba żądania były ignorowane.
        let mut s = State::default();
        s.windows.lock_or_recover().push(sample_window());
        set_minimized(&mut s, 0, true);
        assert!(s.windows.lock_or_recover()[0].minimized);
        activate_slot(&mut s, 0);
        assert!(!s.windows.lock_or_recover()[0].minimized);
    }

    #[test]
    fn maximize_and_restore_geometry() {
        let mut s = State::default();
        s.windows.lock_or_recover().push(sample_window());
        let before = {
            let windows = s.windows.lock_or_recover();
            let w = &windows[0];
            (w.x, w.y, w.width, w.height)
        };
        set_maximized(&mut s, 0, true);
        {
            let windows = s.windows.lock_or_recover();
            let w = &windows[0];
            assert!(w.maximized);
            assert!(w.saved.is_some());
        }
        set_maximized(&mut s, 0, false);
        {
            let windows = s.windows.lock_or_recover();
            let w = &windows[0];
            assert!(!w.maximized);
            assert_eq!((w.x, w.y, w.width, w.height), before);
        }
    }

    /// Dwa monitory obok siebie; panel rezerwuje 40 px **tylko na lewym**.
    fn two_output_state() -> State {
        let mut s = State::default();
        {
            let mut outs = s.outputs.lock_or_recover();
            *outs = vec![
                crate::monitors::OutputInfo {
                    name: "left".to_owned(),
                    width: 1920,
                    height: 1080,
                    refresh_hz: 60,
                    scale: 1,
                    x: 0,
                    y: 0,
                },
                crate::monitors::OutputInfo {
                    name: "right".to_owned(),
                    width: 1600,
                    height: 900,
                    refresh_hz: 60,
                    scale: 1,
                    x: 1920,
                    y: 0,
                },
            ];
        }
        s.top_insets.insert(0, 40.0);
        s
    }

    #[test]
    fn maximize_below_panel_only_on_panel_output() {
        let mut s = two_output_state();
        s.windows.lock_or_recover().push(sample_window());
        set_maximized(&mut s, 0, true);
        let (y, h) = {
            let ws = s.windows.lock_or_recover();
            (ws[0].y, ws[0].height)
        };
        assert_eq!(y, 40.0);
        assert_eq!(h, 1080 - 40 - TITLEBAR as i32);
    }

    #[test]
    fn maximize_uses_full_height_on_output_without_panel() {
        let mut s = two_output_state();
        let mut w = sample_window();
        w.x = 2000.0;
        w.y = 100.0;
        s.windows.lock_or_recover().push(w);
        set_maximized(&mut s, 0, true);
        let (x, y, width, height) = {
            let ws = s.windows.lock_or_recover();
            (ws[0].x, ws[0].y, ws[0].width, ws[0].height)
        };
        assert_eq!((x, y), (1920.0, 0.0));
        assert_eq!(width, 1600);
        assert_eq!(height, 900 - TITLEBAR as i32);
    }

    #[test]
    fn fullscreen_drops_panel_gap_only_on_output_without_panel() {
        let mut s = two_output_state();
        let mut w = sample_window();
        w.x = 2000.0;
        w.y = 100.0;
        s.windows.lock_or_recover().push(w);
        set_fullscreen(&mut s, 0, true);
        let (x, y, width, height) = {
            let ws = s.windows.lock_or_recover();
            (ws[0].x, ws[0].y, ws[0].width, ws[0].height)
        };
        assert_eq!((x, y, width, height), (1920.0, 0.0, 1600, 900));
    }

    #[test]
    fn fullscreen_keeps_panel_gap_on_panel_output() {
        let mut s = two_output_state();
        s.windows.lock_or_recover().push(sample_window());
        set_fullscreen(&mut s, 0, true);
        let (y, height) = {
            let ws = s.windows.lock_or_recover();
            (ws[0].y, ws[0].height)
        };
        assert_eq!(y, 40.0);
        assert_eq!(height, 1080 - 40);
    }

    #[test]
    fn xdg_toplevel_states_are_encoded_as_u32_array() {
        let bytes = encode_states(&[4, 1]);
        assert_eq!(bytes.len(), 8, "każdy stan to 4 bajty (uint), nie 1");
        let decoded: Vec<u32> = bytes
            .chunks_exact(4)
            .map(|c| u32::from_ne_bytes(c.try_into().unwrap()))
            .collect();
        assert_eq!(decoded, vec![4, 1]);
    }

    #[test]
    fn keymap_defaults_to_us_layout() {
        let km = keymap_text_for("us", None);
        assert!(km.contains("xkb_keymap"));
        assert!(km.contains("pc+us+inet(evdev)"));
    }

    #[test]
    fn keymap_supports_variants_and_sanitizes_input() {
        assert!(keymap_text_for("pl", Some("legacy")).contains("pc+pl(legacy)+inet(evdev)"));
        // Znaki spoza zestawu XKB są odfiltrowane (bezpieczeństwo).
        assert!(keymap_text_for("u; rm -rf", None).contains("pc+urm-rf+inet(evdev)"));
    }

    #[test]
    fn empty_layout_falls_back_to_us() {
        assert!(keymap_text_for("!!!", None).contains("pc+us+inet(evdev)"));
    }

    #[test]
    fn snap_zones_cover_edges_and_corners() {
        let area = Rect::from_xywh(0.0, 48.0, 1920.0, 1032.0);
        assert_eq!(
            snap_zone(Point::new(2.0, 500.0), area),
            Some(SnapZone::Left)
        );
        assert_eq!(
            snap_zone(Point::new(1918.0, 500.0), area),
            Some(SnapZone::Right)
        );
        assert_eq!(
            snap_zone(Point::new(960.0, 50.0), area),
            Some(SnapZone::Maximize)
        );
        assert_eq!(
            snap_zone(Point::new(2.0, 50.0), area),
            Some(SnapZone::TopLeft)
        );
        assert_eq!(
            snap_zone(Point::new(1918.0, 50.0), area),
            Some(SnapZone::TopRight)
        );
        assert_eq!(
            snap_zone(Point::new(2.0, 1078.0), area),
            Some(SnapZone::BottomLeft)
        );
        assert_eq!(
            snap_zone(Point::new(1918.0, 1078.0), area),
            Some(SnapZone::BottomRight)
        );
        // Środek ekranu nie przyciąga.
        assert_eq!(snap_zone(Point::new(960.0, 500.0), area), None);
    }

    #[test]
    fn snap_rect_fills_halves_and_quarters() {
        let area = Rect::from_xywh(0.0, 48.0, 1920.0, 1032.0);
        let bar = TITLEBAR;
        let (x, y, w, h) = snap_rect(SnapZone::Left, area, bar);
        assert_eq!((x, y, w), (0.0, 48.0, 960.0));
        assert_eq!(y + bar + h, area.bottom());
        let (x, y, w, h) = snap_rect(SnapZone::TopRight, area, bar);
        assert_eq!((x, y, w), (960.0, 48.0, 960.0));
        assert_eq!(h, 1032.0 / 2.0 - TITLEBAR);
        let (x, y, w, h) = snap_rect(SnapZone::BottomLeft, area, bar);
        assert_eq!((x, y, w), (0.0, 48.0 + 516.0, 960.0));
        assert_eq!(y + bar + h, area.bottom());
        // Prawa połowa dokładnie dopełnia lewą.
        let (rx, _, rw, _) = snap_rect(SnapZone::Right, area, bar);
        assert_eq!(rx, 960.0);
        assert_eq!(rw, 960.0);
    }

    #[test]
    fn drag_clamp_keeps_titlebar_below_panel() {
        let area = (0.0, 0.0, 1920.0, 1080.0);
        let (x, y) = clamp_drag_pos(area, 48.0, 800.0, 100.0, -200.0);
        assert_eq!((x, y), (100.0, 48.0));
        // Nie da się wywieźć okna całkowicie za prawą krawędź.
        let (x2, _) = clamp_drag_pos(area, 48.0, 800.0, 5000.0, 200.0);
        assert_eq!(x2, 1920.0 - MIN_VISIBLE);
    }
}
