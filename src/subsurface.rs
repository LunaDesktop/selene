//! `wl_subcompositor` — powierzchnie potomne (`wl_subsurface`).
//!
//! Bez tego globalu klienty, które rysują część UI w powierzchniach potomnych
//! (GTK4/GtkGLArea, Qt, Electron, część gier) nie mają gdzie ich umieścić: treść
//! po prostu nie pojawia się na ekranie, a kliknięcia trafiają w pustkę.
//!
//! Model: dziecko ma własny bufor, pozycję względem rodzica i tryb `sync`.
//! W trybie `sync` (domyślnym) zmiany wchodzą dopiero przy commicie rodzica;
//! w `desync` — natychmiast. Rodzic trzyma listę dzieci w kolejności stosu,
//! a wynik składamy w jeden obraz (`ImageData`) przy każdym commicie.

use std::sync::Arc;

use libluna_render::ImageData;
use wayland_server::{
    protocol::{wl_subcompositor, wl_subsurface, wl_surface::WlSurface},
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource,
};

use crate::compositor::SurfaceData;
use crate::shm::BufferData;
use crate::state::State;
use crate::sync::LockExt;

/// Stan powierzchni potomnej (trzymany w [`SurfaceData::subsurface`]).
#[derive(Debug, Clone)]
pub struct SubsurfaceState {
    pub parent: WlSurface,
    /// Pozycja względem lewego-górnego rogu bufora rodzica.
    pub position: (i32, i32),
    /// Pozycja z commita czekająca na commit rodzica (tryb `sync`).
    pub pending_position: Option<(i32, i32)>,
    /// Czy zmiany czekają na commit rodzica (domyślnie tak).
    pub sync: bool,
    /// Obraz aktualnie widoczny.
    pub image: Option<Arc<ImageData>>,
    /// Obraz z commita czekający na commit rodzica (tryb `sync`).
    pub pending: Option<Arc<ImageData>>,
}

impl SubsurfaceState {
    pub fn new(parent: WlSurface) -> Self {
        Self {
            parent,
            position: (0, 0),
            pending_position: None,
            sync: true,
            image: None,
            pending: None,
        }
    }
}

/// Dane obiektu `wl_subsurface` (do sprzątania przy zniszczeniu).
pub struct SubsurfaceData {
    pub surface: WlSurface,
    pub parent: WlSurface,
}

impl GlobalDispatch<wl_subcompositor::WlSubcompositor, ()> for State {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<wl_subcompositor::WlSubcompositor>,
        _data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<wl_subcompositor::WlSubcompositor, ()> for State {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &wl_subcompositor::WlSubcompositor,
        request: wl_subcompositor::Request,
        _data: &(),
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let wl_subcompositor::Request::GetSubsurface {
            id,
            surface,
            parent,
        } = request
        {
            // Powierzchnia może mieć tylko jedną rolę; nie może być własnym rodzicem.
            if surface == parent {
                return;
            }
            let Some(sd) = surface.data::<SurfaceData>() else {
                return;
            };
            if sd.subsurface.lock_or_recover().is_some() {
                return;
            }
            data_init.init(
                id,
                SubsurfaceData {
                    surface: surface.clone(),
                    parent: parent.clone(),
                },
            );
            *sd.subsurface.lock_or_recover() = Some(SubsurfaceState::new(parent.clone()));
            if let Some(pd) = parent.data::<SurfaceData>() {
                let mut list = pd.children.lock_or_recover();
                list.push(surface.clone());
                if std::env::var("SELENE_STATS").is_ok() {
                    eprintln!(
                        "selene/subsurface: nowa powierzchnia potomna (dzieci={})",
                        list.len()
                    );
                }
            }
        }
    }
}

impl Dispatch<wl_subsurface::WlSubsurface, SubsurfaceData> for State {
    fn request(
        _state: &mut Self,
        _client: &wayland_server::Client,
        _resource: &wl_subsurface::WlSubsurface,
        request: wl_subsurface::Request,
        data: &SubsurfaceData,
        _handle: &DisplayHandle,
        _data_init: &mut wayland_server::DataInit<'_, Self>,
    ) {
        match request {
            wl_subsurface::Request::SetPosition { x, y } => {
                let Some(sd) = data.surface.data::<SurfaceData>() else {
                    return;
                };
                let mut guard = sd.subsurface.lock_or_recover();
                if let Some(sub) = guard.as_mut() {
                    if sub.sync {
                        sub.pending_position = Some((x, y));
                    } else {
                        sub.position = (x, y);
                    }
                }
            }
            wl_subsurface::Request::PlaceAbove { sibling } => {
                restack(&data.parent, &data.surface, Some(&sibling), true);
            }
            wl_subsurface::Request::PlaceBelow { sibling } => {
                restack(&data.parent, &data.surface, Some(&sibling), false);
            }
            wl_subsurface::Request::SetSync => set_sync(&data.surface, true),
            wl_subsurface::Request::SetDesync => set_sync(&data.surface, false),
            wl_subsurface::Request::Destroy => cleanup(&data.surface, &data.parent),
            _ => {}
        }
    }

    fn destroyed(
        _state: &mut Self,
        _client: wayland_server::backend::ClientId,
        _resource: &wl_subsurface::WlSubsurface,
        data: &SubsurfaceData,
    ) {
        cleanup(&data.surface, &data.parent);
    }
}

fn set_sync(surface: &WlSurface, sync: bool) {
    let Some(sd) = surface.data::<SurfaceData>() else {
        return;
    };
    let mut guard = sd.subsurface.lock_or_recover();
    if let Some(sub) = guard.as_mut() {
        // Przejście `sync` → `desync` natychmiast aplikuje oczekujący stan.
        if !sync {
            if let Some(p) = sub.pending_position.take() {
                sub.position = p;
            }
            if let Some(img) = sub.pending.take() {
                sub.image = Some(img);
            }
        }
        sub.sync = sync;
    }
}

/// Przenosi dziecko na wierzch/spód stosu albo tuż nad/pod `sibling`.
fn restack(parent: &WlSurface, child: &WlSurface, sibling: Option<&WlSurface>, above: bool) {
    let Some(pd) = parent.data::<SurfaceData>() else {
        return;
    };
    let mut list = pd.children.lock_or_recover();
    let Some(from) = list.iter().position(|s| s == child) else {
        return;
    };
    list.remove(from);
    let idx = match sibling {
        // `place_above(parent)` = nad wszystkimi braćmi, `place_below(parent)` = pod nimi.
        Some(sib) if sib == parent => {
            if above {
                list.len()
            } else {
                0
            }
        }
        Some(sib) => list
            .iter()
            .position(|s| s == sib)
            .map(|i| if above { i + 1 } else { i })
            .unwrap_or(if above { list.len() } else { 0 }),
        None => {
            if above {
                list.len()
            } else {
                0
            }
        }
    };
    let len = list.len();
    list.insert(idx.min(len), child.clone());
}

fn cleanup(surface: &WlSurface, parent: &WlSurface) {
    if let Some(sd) = surface.data::<SurfaceData>() {
        *sd.subsurface.lock_or_recover() = None;
    }
    if let Some(pd) = parent.data::<SurfaceData>() {
        pd.children.lock_or_recover().retain(|s| s != surface);
    }
}

/// Odczytuje bufor powierzchni do obrazu RGBA (premultiplied).
fn decode(data: &SurfaceData) -> Option<Arc<ImageData>> {
    let buffer = data.buffer.lock_or_recover().clone()?;
    let bd = buffer.data::<BufferData>()?;
    let rgba = bd.to_rgba()?;
    Some(Arc::new(ImageData::new(
        bd.width as u32,
        bd.height as u32,
        rgba,
    )))
}

/// Commit powierzchni potomnej: bufor trafia do obrazu od razu (`desync`)
/// albo czeka na commit rodzica (`sync`).
pub fn commit(state: &mut State, data: &SurfaceData) {
    let image = decode(data);
    // Bufor oddajemy klientowi raz na `attach` — tak samo jak dla okien.
    if data.take_release() {
        if let Some(buffer) = data.buffer.lock_or_recover().clone() {
            let _ = buffer.send_event(wayland_server::protocol::wl_buffer::Event::Release);
        }
    }
    let (parent, repaint) = {
        let mut guard = data.subsurface.lock_or_recover();
        let Some(sub) = guard.as_mut() else {
            return;
        };
        let was_empty = sub.image.is_none();
        match image {
            Some(img) if sub.sync => sub.pending = Some(img),
            Some(img) => {
                if was_empty && std::env::var("SELENE_STATS").is_ok() {
                    eprintln!(
                        "selene/subsurface: pierwszy obraz desync {}x{}",
                        img.width, img.height
                    );
                }
                sub.image = Some(img);
            }
            None => {}
        }
        // Zmiana pozycji/obrazu w trybie `desync` działa natychmiast.
        let repaint = !sub.sync;
        (sub.parent.clone(), repaint)
    };
    if repaint {
        repaint_parent(state, &parent);
    }
}

/// Aplikuje oczekujące zmiany dzieci — wołane przy commicie rodzica.
pub fn apply_pending(parent: &WlSurface) {
    let Some(pd) = parent.data::<SurfaceData>() else {
        return;
    };
    // Listę kopiujemy, żeby nie trzymać blokady rodzica przy blokadach dzieci.
    let children = pd.children.lock_or_recover().clone();
    for child in children {
        let Some(sd) = child.data::<SurfaceData>() else {
            continue;
        };
        let mut guard = sd.subsurface.lock_or_recover();
        if let Some(sub) = guard.as_mut() {
            if let Some(p) = sub.pending_position.take() {
                sub.position = p;
            }
            if let Some(img) = sub.pending.take() {
                sub.image = Some(img);
            }
        }
    }
}

/// Składa dzieci na obraz rodzica. Gdy dzieci nie mają jeszcze obrazu,
/// zwraca `base` bez kopiowania.
pub fn overlay_children(parent: &WlSurface, base: Arc<ImageData>) -> Arc<ImageData> {
    let Some(pd) = parent.data::<SurfaceData>() else {
        return base;
    };
    let children = pd.children.lock_or_recover().clone();
    if children.is_empty() {
        return base;
    }
    let mut out: Option<Vec<u8>> = None;
    for child in children {
        let Some(sd) = child.data::<SurfaceData>() else {
            continue;
        };
        let (image, pos) = {
            let guard = sd.subsurface.lock_or_recover();
            match guard.as_ref() {
                Some(sub) => (sub.image.clone(), sub.position),
                None => continue,
            }
        };
        let Some(img) = image else {
            continue;
        };
        let dst = out.get_or_insert_with(|| base.rgba.clone());
        blend(dst, base.width, base.height, &img, pos);
    }
    match out {
        Some(rgba) => {
            Arc::new(ImageData::new(base.width, base.height, rgba).with_opaque(base.opaque))
        }
        None => base,
    }
}

/// Przemalowuje okno rodzica po natychmiastowej zmianie dziecka (`desync`).
pub fn repaint_parent(state: &mut State, parent: &WlSurface) {
    let Some(pd) = parent.data::<SurfaceData>() else {
        return;
    };
    let Some(base) = pd.base_image.lock_or_recover().clone() else {
        return;
    };
    let image = overlay_children(parent, base);
    if let Some(slot) = *pd.slot.lock_or_recover() {
        if let Some(w) = state.windows.lock_or_recover().get_mut(slot) {
            w.data = image;
        }
    }
}

/// Nakłada obraz dziecka na bufor rodzica (premultiplied source-over) z przycięciem.
fn blend(dst: &mut [u8], dw: u32, dh: u32, src: &ImageData, pos: (i32, i32)) {
    let sw = src.width as i32;
    let sh = src.height as i32;
    for sy in 0..sh {
        let dy = pos.1 + sy;
        if dy < 0 || dy >= dh as i32 {
            continue;
        }
        for sx in 0..sw {
            let dx = pos.0 + sx;
            if dx < 0 || dx >= dw as i32 {
                continue;
            }
            let si = ((sy as usize * sw as usize) + sx as usize) * 4;
            let a = u32::from(src.rgba[si + 3]);
            if a == 0 {
                continue;
            }
            let di = (dy as usize * dw as usize + dx as usize) * 4;
            if a == 255 {
                dst[di..di + 4].copy_from_slice(&src.rgba[si..si + 4]);
            } else {
                let inv = 255 - a;
                for c in 0..3 {
                    dst[di + c] =
                        (u32::from(src.rgba[si + c]) + u32::from(dst[di + c]) * inv / 255) as u8;
                }
                dst[di + 3] = (a + u32::from(dst[di + 3]) * inv / 255).min(255) as u8;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base(rgba: Vec<u8>, w: u32, h: u32) -> Arc<ImageData> {
        Arc::new(ImageData::new(w, h, rgba))
    }

    #[test]
    fn opaque_child_replaces_pixels() {
        let mut dst = vec![9, 9, 9, 255, 9, 9, 9, 255, 9, 9, 9, 255, 9, 9, 9, 255];
        let src = ImageData::new(2, 1, vec![255, 0, 0, 255, 0, 255, 0, 255]);
        blend(&mut dst, 2, 2, &src, (0, 0));
        assert_eq!(&dst[0..4], &[255, 0, 0, 255]);
        assert_eq!(&dst[4..8], &[0, 255, 0, 255]);
        // Drugi wiersz nietknięty.
        assert_eq!(&dst[8..12], &[9, 9, 9, 255]);
    }

    #[test]
    fn half_transparent_child_is_blended_premultiplied() {
        let mut dst = vec![0, 0, 0, 255];
        // Premultiplied: pełne czerwone światło przy alpha 128 → R=128, A=128.
        let src = ImageData::new(1, 1, vec![128, 0, 0, 128]);
        blend(&mut dst, 1, 1, &src, (0, 0));
        assert_eq!(dst[0], 128);
        assert_eq!(dst[3], 255);
    }

    #[test]
    fn child_is_clipped_to_parent_bounds() {
        let mut dst = vec![0, 0, 0, 255];
        let src = ImageData::new(2, 1, vec![255, 0, 0, 255, 0, 0, 255, 255]);
        // Pozycja (-1,0): pierwszy piksel poza ekranem, drugi (zielony) trafia w x=0.
        blend(&mut dst, 1, 1, &src, (-1, 0));
        assert_eq!(&dst[0..4], &[0, 0, 255, 255]);
    }

    #[test]
    fn fully_transparent_pixels_are_skipped() {
        let mut dst = vec![7, 7, 7, 255];
        let src = ImageData::new(1, 1, vec![255, 0, 0, 0]);
        blend(&mut dst, 1, 1, &src, (0, 0));
        assert_eq!(&dst[0..4], &[7, 7, 7, 255]);
    }
}
