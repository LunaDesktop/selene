//! `zwlr_foreign_toplevel_management` (unstable v1) — lista okien dla powłoki.
//!
//! Kompozytor wystawia globalny menedżer; każdy klient (np. pasek `luna-shell`) dostaje
//! **uchwyty** do wszystkich okien najwyższego poziomu i powiadomienia o zmianach
//! (tytuł, `app_id`, stan: aktywny/zminimalizowany/zmaksymalizowany/fullscreen, zamknięcie).
//! Klient może poprosić o aktywację albo zamknięcie okna — to czyni pasek użytecznym.

use wayland_protocols_wlr::foreign_toplevel::v1::server::{
    zwlr_foreign_toplevel_handle_v1 as handle, zwlr_foreign_toplevel_manager_v1 as manager,
};
use wayland_server::{
    protocol::wl_surface::WlSurface, Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch,
    New, Resource,
};

use crate::compositor::SurfaceData;
use crate::seat;
use crate::state::State;
use crate::sync::LockExt;

/// Stany okna (`zwlr_foreign_toplevel_handle_v1.state`).
pub const STATE_MAXIMIZED: u32 = 0;
pub const STATE_MINIMIZED: u32 = 1;
pub const STATE_ACTIVATED: u32 = 2;
pub const STATE_FULLSCREEN: u32 = 3;

/// Uchwyt okna po stronie klienta (`zwlr_foreign_toplevel_handle_v1`).
pub struct ToplevelHandleData {
    /// Powierzchnia, którą ten uchwyt reprezentuje.
    pub surface: WlSurface,
}

/// Informacje o toplevelu trzymane przy powierzchni (do rozsyłania do menedżerów).
#[derive(Default)]
pub struct ToplevelInfo {
    pub title: String,
    pub app_id: String,
    /// Uchwyty utworzone dla podłączonych menedżerów (po jednym na klienta).
    pub handles: Vec<handle::ZwlrForeignToplevelHandleV1>,
}

impl GlobalDispatch<manager::ZwlrForeignToplevelManagerV1, ()> for State {
    fn bind(
        state: &mut Self,
        handle: &DisplayHandle,
        _client: &Client,
        resource: New<manager::ZwlrForeignToplevelManagerV1>,
        _data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        let mgr = data_init.init(resource, ());
        // Nowo podłączony menedżer: zgłoś istniejące okna.
        for surface in state.slots.clone() {
            if is_toplevel(&surface) {
                announce(state, handle, &mgr, &surface);
            }
        }
        state.toplevel_managers.push(mgr);
    }
}

impl Dispatch<manager::ZwlrForeignToplevelManagerV1, ()> for State {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &manager::ZwlrForeignToplevelManagerV1,
        request: manager::Request,
        _data: &(),
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        if let manager::Request::Stop = request {
            // Klient kończy: zdejmij menedżera i wszystkie jego uchwyty.
            let mgr_id = resource.id();
            let client_id = resource.client().map(|c| c.id());
            state.toplevel_managers.retain(|m| m.id() != mgr_id);
            if let Some(client_id) = client_id {
                for surface in state.slots.clone() {
                    if let Some(sd) = surface.data::<SurfaceData>() {
                        if let Some(info) = sd.toplevel.lock_or_recover().as_mut() {
                            info.handles.retain(|h| {
                                h.client().map(|c| c.id() != client_id).unwrap_or(false)
                            });
                        }
                    }
                }
            }
        }
    }
}

impl Dispatch<handle::ZwlrForeignToplevelHandleV1, ToplevelHandleData> for State {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &handle::ZwlrForeignToplevelHandleV1,
        request: handle::Request,
        data: &ToplevelHandleData,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        match request {
            handle::Request::Activate { .. } => {
                if let Some(slot) = slot_of(&data.surface) {
                    seat::activate_slot(state, slot);
                    broadcast_states(state);
                }
            }
            handle::Request::Close => {
                seat::close_surface(state, &data.surface);
            }
            handle::Request::SetMinimized => {
                if let Some(slot) = slot_of(&data.surface) {
                    seat::set_minimized(state, slot, true);
                }
            }
            handle::Request::UnsetMinimized => {
                if let Some(slot) = slot_of(&data.surface) {
                    seat::set_minimized(state, slot, false);
                }
            }
            handle::Request::SetMaximized => {
                if let Some(slot) = slot_of(&data.surface) {
                    seat::set_maximized(state, slot, true);
                }
            }
            handle::Request::UnsetMaximized => {
                if let Some(slot) = slot_of(&data.surface) {
                    seat::set_maximized(state, slot, false);
                }
            }
            handle::Request::SetFullscreen { .. } => {
                if let Some(slot) = slot_of(&data.surface) {
                    seat::set_fullscreen(state, slot, true);
                }
            }
            handle::Request::UnsetFullscreen => {
                if let Some(slot) = slot_of(&data.surface) {
                    seat::set_fullscreen(state, slot, false);
                }
            }
            handle::Request::Destroy => {
                forget_handle(&data.surface, resource.id());
            }
            _ => {}
        }
    }
}

/// Czy powierzchnia jest oknem najwyższego poziomu (`xdg_toplevel`, nie popup/layer)?
fn is_toplevel(surface: &WlSurface) -> bool {
    surface
        .data::<SurfaceData>()
        .map(|sd| sd.toplevel.lock_or_recover().is_some())
        .unwrap_or(false)
}

/// Slot okna w modelu dla powierzchni.
fn slot_of(surface: &WlSurface) -> Option<usize> {
    surface
        .data::<SurfaceData>()
        .and_then(|sd| *sd.slot.lock_or_recover())
}

/// Rejestruje okno jako toplevel (wołane przy `get_toplevel`).
pub fn register_toplevel(surface: &WlSurface) {
    if let Some(sd) = surface.data::<SurfaceData>() {
        let mut t = sd.toplevel.lock_or_recover();
        if t.is_none() {
            *t = Some(ToplevelInfo::default());
        }
    }
}

/// Zgłasza nowo podłączonemu menedżerowi istniejące okno (uchwyt + pierwszy stan).
pub fn announce(
    state: &State,
    display: &DisplayHandle,
    mgr: &manager::ZwlrForeignToplevelManagerV1,
    surface: &WlSurface,
) {
    let Some(client) = mgr.client() else {
        return;
    };
    let Ok(handle) = client
        .create_resource::<handle::ZwlrForeignToplevelHandleV1, ToplevelHandleData, State>(
            display,
            3,
            ToplevelHandleData {
                surface: surface.clone(),
            },
        )
    else {
        return;
    };
    let _ = mgr.send_event(manager::Event::Toplevel {
        toplevel: handle.clone(),
    });
    if let Some(sd) = surface.data::<SurfaceData>() {
        if let Some(info) = sd.toplevel.lock_or_recover().as_mut() {
            info.handles.push(handle.clone());
        }
    }
    send_full(state, surface, &handle);
}

/// Rozsyła aktualizację tytułu do wszystkich menedżerów.
pub fn broadcast_title(state: &State, surface: &WlSurface, title: &str) {
    for_each_handle(surface, |h| {
        let _ = h.send_event(handle::Event::Title {
            title: title.to_owned(),
        });
        let _ = h.send_event(handle::Event::Done);
    });
    let _ = state;
}

/// Rozsyła aktualizację `app_id`.
pub fn broadcast_app_id(state: &State, surface: &WlSurface, app_id: &str) {
    for_each_handle(surface, |h| {
        let _ = h.send_event(handle::Event::AppId {
            app_id: app_id.to_owned(),
        });
        let _ = h.send_event(handle::Event::Done);
    });
    let _ = state;
}

/// Rozsyła stan (aktywny/minimalizowany/…) wszystkich okien — po zmianie fokusu.
pub fn broadcast_states(state: &State) {
    for surface in state.slots.clone() {
        if !is_toplevel(&surface) {
            continue;
        }
        let Some(slot) = slot_of(&surface) else {
            continue;
        };
        let bytes = state_bytes(state, slot);
        for_each_handle(&surface, |h| {
            let _ = h.send_event(handle::Event::State {
                state: bytes.clone(),
            });
            let _ = h.send_event(handle::Event::Done);
        });
    }
}

/// Powiadamia o zamknięciu okna (i zwalnia uchwyty).
pub fn announce_closed(surface: &WlSurface) {
    if let Some(sd) = surface.data::<SurfaceData>() {
        if let Some(mut info) = sd.toplevel.lock_or_recover().take() {
            for h in info.handles.drain(..) {
                let _ = h.send_event(handle::Event::Closed);
            }
        }
    }
}

/// Jak [`announce_closed`], ale działa wprost na danych powierzchni — używane przy
/// zniknięciu powierzchni (`Dispatch::destroyed`), gdy zasób jest już niszczony i nie
/// wolno się do niego odwoływać.
pub fn close_for_data(data: &SurfaceData) {
    if let Some(mut info) = data.toplevel.lock_or_recover().take() {
        for h in info.handles.drain(..) {
            let _ = h.send_event(handle::Event::Closed);
        }
    }
}

/// Pełny stan okna wysyłany nowemu uchwytowi (title + app_id + stan + `done`).
fn send_full(state: &State, surface: &WlSurface, handle: &handle::ZwlrForeignToplevelHandleV1) {
    let (title, app_id) = toplevel_text(surface);
    let _ = handle.send_event(handle::Event::Title { title });
    let _ = handle.send_event(handle::Event::AppId { app_id });
    if let Some(slot) = slot_of(surface) {
        let _ = handle.send_event(handle::Event::State {
            state: state_bytes(state, slot),
        });
    }
    let _ = handle.send_event(handle::Event::Done);
}

/// `(title, app_id)` okna z jego danych.
fn toplevel_text(surface: &WlSurface) -> (String, String) {
    surface
        .data::<SurfaceData>()
        .and_then(|sd| {
            sd.toplevel
                .lock_or_recover()
                .as_ref()
                .map(|t| (t.title.clone(), t.app_id.clone()))
        })
        .unwrap_or_default()
}

/// Bity stanu okna (`slot`) zakodowane jako tablica `u32` (native endian).
fn state_bytes(state: &State, slot: usize) -> Vec<u8> {
    let focused = state.keyboard_focus.as_ref().map(|(s, _)| *s);
    let mut flags: Vec<u32> = Vec::new();
    {
        let windows = state.windows.lock_or_recover();
        if let Some(w) = windows.get(slot) {
            if w.maximized {
                flags.push(STATE_MAXIMIZED);
            }
            if w.minimized {
                flags.push(STATE_MINIMIZED);
            }
            if w.fullscreen {
                flags.push(STATE_FULLSCREEN);
            }
        }
    }
    if focused == Some(slot) {
        flags.push(STATE_ACTIVATED);
    }
    let mut out = Vec::with_capacity(flags.len() * 4);
    for f in flags {
        out.extend_from_slice(&f.to_ne_bytes());
    }
    out
}

fn for_each_handle(surface: &WlSurface, mut f: impl FnMut(&handle::ZwlrForeignToplevelHandleV1)) {
    let handles = surface
        .data::<SurfaceData>()
        .and_then(|sd| {
            sd.toplevel
                .lock_or_recover()
                .as_ref()
                .map(|t| t.handles.clone())
        })
        .unwrap_or_default();
    for h in &handles {
        f(h);
    }
}

/// Dezaktywuje pojedynczy uchwyt (po jego `destroy`).
fn forget_handle(surface: &WlSurface, id: wayland_server::backend::ObjectId) {
    if let Some(sd) = surface.data::<SurfaceData>() {
        if let Some(info) = sd.toplevel.lock_or_recover().as_mut() {
            info.handles.retain(|h| h.id() != id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_encoding_is_one_u32_per_flag() {
        let s = State::default();
        s.windows.lock_or_recover().push(crate::state::Window {
            x: 0.0,
            y: 0.0,
            width: 10,
            height: 10,
            image_offset: (0.0, 0.0),
            z: 1,
            title: "t".into(),
            popup: false,
            layer: false,
            mapped: true,
            focused: false,
            workspace: 0,
            input: None,
            input_capture: false,
            minimized: true,
            maximized: true,
            fullscreen: false,
            ssd: false,
            saved: None,
            dmabuf: None,
            data: std::sync::Arc::new(libluna_render::ImageData::new(1, 1, vec![0, 0, 0, 0])),
        });
        let bytes = state_bytes(&s, 0);
        assert_eq!(bytes.len(), 4 * 2); // maximized + minimized (brak fokusu)
        assert_eq!(
            u32::from_ne_bytes(bytes[0..4].try_into().unwrap()),
            STATE_MAXIMIZED
        );
        assert_eq!(
            u32::from_ne_bytes(bytes[4..8].try_into().unwrap()),
            STATE_MINIMIZED
        );
    }
}
