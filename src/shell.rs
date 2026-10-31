//! `xdg_wm_base` / `xdg_surface` / `xdg_toplevel` — okna aplikacji (protokół xdg-shell).

use std::sync::Mutex;

use libluna_core::Rect;

use wayland_protocols::xdg::shell::server::{
    xdg_popup, xdg_positioner, xdg_surface, xdg_toplevel, xdg_wm_base,
};
use wayland_server::{
    protocol::wl_surface::WlSurface, Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch,
    New, Resource, WEnum,
};

use crate::compositor::SurfaceData;
use crate::seat::toplevel_slot;
use crate::state::State;
use crate::sync::LockExt;

/// Geometria popupu zebrana z `xdg_positioner`.
#[derive(Debug, Clone, Copy, Default)]
pub struct PopupGeom {
    pub width: i32,
    pub height: i32,
    pub anchor_x: f64,
    pub anchor_y: f64,
    pub anchor_w: f64,
    pub anchor_h: f64,
    pub offset_x: f64,
    pub offset_y: f64,
    /// Krawędź/narożnik kotwicy (enum `xdg_positioner.anchor`: 0=środek, 1=góra,
    /// 2=dół, 3=lewo, 4=prawo, 5–8=narożniki).
    pub anchor: u8,
    /// Kierunek rozwoju popupu (`xdg_positioner.gravity`, te same wartości).
    pub gravity: u8,
    /// `constraint_adjustment` (bity: 1=slide_x, 2=slide_y, 4=flip_x, 8=flip_y).
    pub constraint: u32,
}

/// Dane `xdg_positioner`.
#[derive(Debug, Default)]
pub struct XdgPositionerData {
    pub geom: Mutex<PopupGeom>,
}

/// Dane `xdg_surface`.
#[allow(dead_code)]
pub struct XdgSurfaceData {
    /// Powiązana powierzchnia `wl_surface`.
    pub wl_surface: WlSurface,
    /// Utworzona powierzchnia najwyższego poziomu (jeśli `get_toplevel`).
    pub toplevel: Mutex<Option<xdg_toplevel::XdgToplevel>>,
    /// Utworzony popup (jeśli `get_popup`).
    pub popup: Mutex<Option<xdg_popup::XdgPopup>>,
    /// Powierzchnia rodzica (dla popupu).
    pub parent: Mutex<Option<WlSurface>>,
    /// Geometria popupu z positionera.
    pub geom: Mutex<PopupGeom>,
    /// Widoczna geometria okna (`xdg_surface.set_window_geometry`) — bez cieni CSD.
    pub window_geometry: Mutex<Option<Rect>>,
    /// Czy to popup.
    pub is_popup: Mutex<bool>,
    /// Czy wysłano już pierwszą konfigurację.
    pub configured: Mutex<bool>,
}

impl XdgSurfaceData {
    pub fn new(wl_surface: WlSurface) -> Self {
        Self {
            wl_surface,
            toplevel: Mutex::new(None),
            popup: Mutex::new(None),
            parent: Mutex::new(None),
            geom: Mutex::new(PopupGeom::default()),
            window_geometry: Mutex::new(None),
            is_popup: Mutex::new(false),
            configured: Mutex::new(false),
        }
    }
}

/// Dane `xdg_toplevel`.
pub struct XdgToplevelData {
    pub title: Mutex<String>,
    pub app_id: Mutex<String>,
    /// Powiązana powierzchnia (do aktualizacji modelu okien).
    pub surface: WlSurface,
    /// Powiązany obiekt `zxdg_toplevel_decoration_v1` (jeśli klient go utworzył).
    pub decoration: Mutex<Option<crate::decoration::ToplevelDecoration>>,
    /// Negocjowany tryb dekoracji. `None` = klient nie ma obiektu dekoracji;
    /// `Some(Server)` = dekorujemy my (rysujemy pasek); `Some(Client)` = klient sam.
    pub deco: Mutex<Option<crate::decoration::DecoMode>>,
}

impl XdgToplevelData {
    pub fn new(surface: WlSurface) -> Self {
        Self {
            title: Mutex::new(String::new()),
            app_id: Mutex::new(String::new()),
            surface,
            decoration: Mutex::new(None),
            deco: Mutex::new(None),
        }
    }
}

impl GlobalDispatch<xdg_wm_base::XdgWmBase, ()> for State {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<xdg_wm_base::XdgWmBase>,
        _data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for State {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &xdg_wm_base::XdgWmBase,
        request: xdg_wm_base::Request,
        _data: &(),
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            xdg_wm_base::Request::CreatePositioner { id } => {
                data_init.init(id, XdgPositionerData::default());
            }
            xdg_wm_base::Request::GetXdgSurface { id, surface } => {
                let xdg = data_init.init(id, XdgSurfaceData::new(surface.clone()));
                // Zapamiętaj powiązanie po stronie `wl_surface` (do konfiguracji przy commit).
                if let Some(sd) = surface.data::<SurfaceData>() {
                    *sd.xdg_surface.lock_or_recover() = Some(xdg);
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, XdgSurfaceData> for State {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &xdg_surface::XdgSurface,
        request: xdg_surface::Request,
        data: &XdgSurfaceData,
        handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            xdg_surface::Request::GetToplevel { id } => {
                let toplevel = data_init.init(id, XdgToplevelData::new(data.wl_surface.clone()));
                *data.toplevel.lock_or_recover() = Some(toplevel);
                // Zarejestruj okno dla paska (foreign-toplevel) i zgłoś menedżerom.
                crate::foreign_toplevel::register_toplevel(&data.wl_surface);
                let managers = state.toplevel_managers.clone();
                for mgr in &managers {
                    crate::foreign_toplevel::announce(state, handle, mgr, &data.wl_surface);
                }
            }
            xdg_surface::Request::GetPopup {
                id,
                parent,
                positioner,
            } => {
                let popup = data_init.init(id, ());
                *data.popup.lock_or_recover() = Some(popup);
                *data.is_popup.lock_or_recover() = true;
                // `parent` to `xdg_surface` rodzica — zapamiętujemy jego `wl_surface`
                // (po nim `popup_position` znajduje okno i prostokąt kotwicy).
                *data.parent.lock_or_recover() = parent
                    .as_ref()
                    .and_then(|p| p.data::<XdgSurfaceData>())
                    .map(|p| p.wl_surface.clone());
                *data.geom.lock_or_recover() = positioner
                    .data::<XdgPositionerData>()
                    .map(|p| *p.geom.lock_or_recover())
                    .unwrap_or_default();
            }
            xdg_surface::Request::AckConfigure { .. } => {
                // Konfiguracja potwierdzona — nic więcej nie trzeba po stronie MVP.
            }
            xdg_surface::Request::SetWindowGeometry {
                x,
                y,
                width,
                height,
            } => {
                *data.window_geometry.lock_or_recover() = Some(Rect::from_xywh(
                    x as f64,
                    y as f64,
                    width as f64,
                    height as f64,
                ));
            }
            _ => {}
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, XdgToplevelData> for State {
    fn request(
        state: &mut Self,
        _client: &Client,
        _resource: &xdg_toplevel::XdgToplevel,
        request: xdg_toplevel::Request,
        data: &XdgToplevelData,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            xdg_toplevel::Request::SetTitle { title } => {
                *data.title.lock_or_recover() = title.clone();
                state.toplevels += 1;
                // Odśwież tytuł w modelu okien i rozgłoś do pasków (foreign-toplevel).
                if let Some(sd) = data.surface.data::<SurfaceData>() {
                    if let Some(info) = sd.toplevel.lock_or_recover().as_mut() {
                        info.title = title.clone();
                    }
                    if let Some(slot) = *sd.slot.lock_or_recover() {
                        if let Some(window) = state.windows.lock_or_recover().get_mut(slot) {
                            window.title = title.clone();
                        }
                    }
                }
                crate::foreign_toplevel::broadcast_title(state, &data.surface, &title);
            }
            xdg_toplevel::Request::SetAppId { app_id } => {
                *data.app_id.lock_or_recover() = app_id.clone();
                if let Some(sd) = data.surface.data::<SurfaceData>() {
                    if let Some(info) = sd.toplevel.lock_or_recover().as_mut() {
                        info.app_id = app_id.clone();
                    }
                }
                crate::foreign_toplevel::broadcast_app_id(state, &data.surface, &app_id);
            }
            xdg_toplevel::Request::Destroy => {
                crate::foreign_toplevel::announce_closed(&data.surface);
            }
            // Okna z własnymi dekoracjami (CSD, GTK/libadwaita) proszą kompozytor, żeby
            // to on przesuwał/skalował okno w trakcie przeciągania ich paska. Bez obsługi
            // tych żądań takich okien nie da się przesunąć w ogóle.
            xdg_toplevel::Request::Move { .. } => {
                crate::seat::interactive_move_from_toplevel(state, data);
            }
            xdg_toplevel::Request::Resize { edges, .. } => {
                crate::seat::interactive_resize_from_toplevel(state, data, edges);
            }
            // Stany okna. Klient (np. libLuna, GTK, Qt) może poprosić o minimalizację,
            // maksymalizację albo pełny ekran — wcześniej te żądania były ignorowane,
            // przez co przyciski w pasku tytułu aplikacji nic nie robiły.
            xdg_toplevel::Request::SetMinimized => {
                if std::env::var("SELENE_STATS").is_ok() {
                    eprintln!("selene/win: set_minimized slot={:?}", toplevel_slot(data));
                }
                if let Some(slot) = toplevel_slot(data) {
                    crate::seat::set_minimized(state, slot, true);
                }
            }
            // Uwaga: xdg-shell **nie ma** żądania `close` — przycisk ✕ w CSD
            // obsługuje sam klient (niszczy toplevel). Kompozytor wysyła tylko
            // zdarzenie `close` w drugą stronę (`send_close`).
            xdg_toplevel::Request::SetMaximized => {
                if std::env::var("SELENE_STATS").is_ok() {
                    eprintln!("selene/win: set_maximized slot={:?}", toplevel_slot(data));
                }
                if let Some(slot) = toplevel_slot(data) {
                    crate::seat::set_maximized(state, slot, true);
                }
            }
            xdg_toplevel::Request::UnsetMaximized => {
                if let Some(slot) = toplevel_slot(data) {
                    crate::seat::set_maximized(state, slot, false);
                }
            }
            xdg_toplevel::Request::SetFullscreen { .. } => {
                if let Some(slot) = toplevel_slot(data) {
                    crate::seat::set_fullscreen(state, slot, true);
                }
            }
            xdg_toplevel::Request::UnsetFullscreen => {
                if let Some(slot) = toplevel_slot(data) {
                    crate::seat::set_fullscreen(state, slot, false);
                }
            }
            // `Activate` nie istnieje w tej wersji xdg-shell (obsługujemy aktywację
            // przez `zwlr_foreign_toplevel_handle_v1.activate`).
            _ => {}
        }
    }
}

impl Dispatch<xdg_positioner::XdgPositioner, XdgPositionerData> for State {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &xdg_positioner::XdgPositioner,
        request: xdg_positioner::Request,
        data: &XdgPositionerData,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        let mut geom = data.geom.lock_or_recover();
        match request {
            xdg_positioner::Request::SetSize { width, height } => {
                geom.width = width;
                geom.height = height;
            }
            xdg_positioner::Request::SetAnchorRect {
                x,
                y,
                width,
                height,
            } => {
                geom.anchor_x = x as f64;
                geom.anchor_y = y as f64;
                geom.anchor_w = width as f64;
                geom.anchor_h = height as f64;
            }
            xdg_positioner::Request::SetOffset { x, y } => {
                geom.offset_x = x as f64;
                geom.offset_y = y as f64;
            }
            xdg_positioner::Request::SetAnchor { anchor } => {
                geom.anchor = match anchor {
                    WEnum::Value(v) => anchor_value(v),
                    WEnum::Unknown(v) => v.min(8) as u8,
                };
            }
            xdg_positioner::Request::SetGravity { gravity } => {
                geom.gravity = match gravity {
                    WEnum::Value(v) => gravity_value(v),
                    WEnum::Unknown(v) => v.min(8) as u8,
                };
            }
            xdg_positioner::Request::SetConstraintAdjustment {
                constraint_adjustment,
            } => {
                geom.constraint = match constraint_adjustment {
                    WEnum::Value(v) => v.bits(),
                    WEnum::Unknown(v) => v,
                };
            }
            _ => {}
        }
    }
}

/// Wartość `xdg_positioner.anchor` (enum 0–8, nie bitmaski).
fn anchor_value(v: xdg_positioner::Anchor) -> u8 {
    use xdg_positioner::Anchor;
    match v {
        Anchor::None => 0,
        Anchor::Top => 1,
        Anchor::Bottom => 2,
        Anchor::Left => 3,
        Anchor::Right => 4,
        Anchor::TopLeft => 5,
        Anchor::BottomLeft => 6,
        Anchor::TopRight => 7,
        Anchor::BottomRight => 8,
        _ => 0,
    }
}

/// Wartość `xdg_positioner.gravity` (enum 0–8, te same wartości co `anchor`).
fn gravity_value(v: xdg_positioner::Gravity) -> u8 {
    use xdg_positioner::Gravity;
    match v {
        Gravity::None => 0,
        Gravity::Top => 1,
        Gravity::Bottom => 2,
        Gravity::Left => 3,
        Gravity::Right => 4,
        Gravity::TopLeft => 5,
        Gravity::BottomLeft => 6,
        Gravity::TopRight => 7,
        Gravity::BottomRight => 8,
        _ => 0,
    }
}

impl Dispatch<xdg_popup::XdgPopup, ()> for State {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &xdg_popup::XdgPopup,
        _request: xdg_popup::Request,
        _data: &(),
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
    }
}
