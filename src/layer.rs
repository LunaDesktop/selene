//! `zwlr_layer_shell_v1` / `zwlr_layer_surface_v1` — powierzchnie warstwowe (panele, docki).
//!
//! To fundament przyszłego pulpitu Luna: pasek górny, dock i tło to powierzchnie
//! kotwiczone do krawędzi monitora, niezależne od zwykłych okien `xdg_toplevel`.

use std::sync::Mutex;

use wayland_protocols_wlr::layer_shell::v1::server::{
    zwlr_layer_shell_v1 as layer_shell, zwlr_layer_surface_v1 as layer_surface,
};
use wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, WEnum,
};

use crate::compositor::SurfaceData;
use crate::state::State;
use crate::sync::LockExt;

/// Kotwice krawędzi (`zwlr_layer_surface_v1.anchor`).
pub const ANCHOR_TOP: u32 = 1;
pub const ANCHOR_BOTTOM: u32 = 2;
pub const ANCHOR_LEFT: u32 = 4;
pub const ANCHOR_RIGHT: u32 = 8;

/// Zebrane właściwości powierzchni warstwowej.
#[derive(Debug, Clone, Copy, Default)]
pub struct LayerState {
    /// Warstwa (`background`/`bottom`/`top`/`overlay`).
    pub layer: u32,
    /// Bitmaska kotwic.
    pub anchor: u32,
    /// Żądana szerokość (0 = wybiera kompozytor/klient).
    pub width: i32,
    /// Żądana wysokość (0 = wybiera kompozytor/klient).
    pub height: i32,
    /// Strefa wyłączna (zarezerwowane miejsce na krawędzi).
    pub exclusive_zone: i32,
    pub margin_top: i32,
    pub margin_right: i32,
    pub margin_bottom: i32,
    pub margin_left: i32,
    /// `none`/`exclusive`/`on_demand`.
    pub keyboard_interactivity: u32,
}

impl LayerState {
    fn anchors(&self) -> (bool, bool, bool, bool) {
        (
            self.anchor & ANCHOR_LEFT != 0,
            self.anchor & ANCHOR_RIGHT != 0,
            self.anchor & ANCHOR_TOP != 0,
            self.anchor & ANCHOR_BOTTOM != 0,
        )
    }

    /// Rozmiar, jaki kompozytor proponuje klientowi w `configure`.
    pub fn configure_size(&self, out_w: f64, out_h: f64) -> (i32, i32) {
        let (left, right, top, bottom) = self.anchors();
        let w = if left && right && self.width == 0 {
            (out_w - self.margin_left as f64 - self.margin_right as f64).max(0.0) as i32
        } else {
            self.width
        };
        let h = if top && bottom && self.height == 0 {
            (out_h - self.margin_top as f64 - self.margin_bottom as f64).max(0.0) as i32
        } else {
            self.height
        };
        (w, h)
    }

    /// Pozycja lewego-górnego narożnika na monitorze (z uwzględnieniem kotwic i marginesów).
    pub fn position(&self, out_w: f64, out_h: f64, w: f64, h: f64) -> (f64, f64) {
        let (left, right, top, bottom) = self.anchors();
        let x = if left {
            self.margin_left as f64
        } else if right {
            out_w - self.margin_right as f64 - w
        } else {
            ((out_w - w) / 2.0).max(0.0)
        };
        let y = if top {
            self.margin_top as f64
        } else if bottom {
            out_h - self.margin_bottom as f64 - h
        } else {
            ((out_h - h) / 2.0).max(0.0)
        };
        (x, y)
    }
}

/// `keyboard_interactivity` z protokołu: fokus wyłączny (ekran blokady).
/// `none` = 0, `on_demand` = 2 (panele).
pub const KEYBOARD_EXCLUSIVE: u32 = 1;

/// Dane powierzchni warstwowej.
pub struct LayerData {
    /// Monitor, na którym kotwiczymy (indeks w [`crate::monitors::Outputs`]); `None` = wszystkie.
    pub output: Option<usize>,
    /// Właściwości (aktualizowane przez `set_*`).
    pub state: Mutex<LayerState>,
    /// Czy wysłano już `configure`.
    pub configured: Mutex<bool>,
    /// Slot okna na każdym monitorze (`output=None` → jedno okno na wyjście).
    pub slots: Mutex<Vec<Option<usize>>>,
}

impl LayerData {
    pub fn new(layer: u32, output: Option<usize>) -> Self {
        Self {
            output,
            state: Mutex::new(LayerState {
                layer,
                ..LayerState::default()
            }),
            configured: Mutex::new(false),
            slots: Mutex::new(Vec::new()),
        }
    }
}

/// Kolejność rysowania wynikająca z warstwy (panele nad oknami, tło pod nimi).
pub fn layer_z(layer: u32) -> i32 {
    match layer {
        0 => -200,      // background
        1 => -100,      // bottom
        2 => 1_000_000, // top
        _ => 2_000_000, // overlay
    }
}

fn anchor_bits(a: WEnum<layer_surface::Anchor>) -> u32 {
    match a {
        WEnum::Value(v) => v.bits(),
        WEnum::Unknown(v) => v,
    }
}

impl GlobalDispatch<layer_shell::ZwlrLayerShellV1, ()> for State {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<layer_shell::ZwlrLayerShellV1>,
        _data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<layer_shell::ZwlrLayerShellV1, ()> for State {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &layer_shell::ZwlrLayerShellV1,
        request: layer_shell::Request,
        _data: &(),
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let layer_shell::Request::GetLayerSurface {
            id,
            surface,
            output,
            layer,
            ..
        } = request
        {
            let layer = match layer {
                WEnum::Value(v) => v as u32,
                WEnum::Unknown(v) => v,
            };
            // Indeks monitora niesiony przez zasób `wl_output`.
            let output = output.and_then(|o| o.data::<usize>().copied());
            if std::env::var("SELENE_STATS").is_ok() {
                eprintln!("selene/layer: get_layer_surface (layer={layer}, output={output:?})");
            }
            let ls = data_init.init(id, LayerData::new(layer, output));
            // Zapamiętaj powiązanie po stronie `wl_surface` (potrzebne przy `commit`).
            if let Some(sd) = surface.data::<SurfaceData>() {
                *sd.layer.lock_or_recover() = Some(ls);
            }
        }
    }
}

impl Dispatch<layer_surface::ZwlrLayerSurfaceV1, LayerData> for State {
    fn request(
        state: &mut Self,
        _client: &Client,
        resource: &layer_surface::ZwlrLayerSurfaceV1,
        request: layer_surface::Request,
        data: &LayerData,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        let mut st = data.state.lock_or_recover();
        match request {
            layer_surface::Request::SetSize { width, height } => {
                st.width = width as i32;
                st.height = height as i32;
            }
            layer_surface::Request::SetAnchor { anchor } => {
                st.anchor = anchor_bits(anchor);
            }
            layer_surface::Request::SetExclusiveZone { zone } => {
                st.exclusive_zone = zone;
            }
            layer_surface::Request::SetMargin {
                top,
                right,
                bottom,
                left,
            } => {
                st.margin_top = top;
                st.margin_right = right;
                st.margin_bottom = bottom;
                st.margin_left = left;
            }
            layer_surface::Request::SetKeyboardInteractivity {
                keyboard_interactivity,
            } => {
                st.keyboard_interactivity = match keyboard_interactivity {
                    WEnum::Value(v) => v as u32,
                    WEnum::Unknown(v) => v,
                };
            }
            layer_surface::Request::SetLayer { layer } => {
                st.layer = match layer {
                    WEnum::Value(v) => v as u32,
                    WEnum::Unknown(v) => v,
                };
            }
            layer_surface::Request::Destroy => {
                // Panel znika — jego rezerwacja miejsca też (per monitor).
                if st.layer == 2
                    && st.anchor & crate::layer::ANCHOR_TOP != 0
                    && st.exclusive_zone > 0
                {
                    state.top_insets.remove(&data.output.unwrap_or(0));
                }
                // Zdejmij powierzchnię z widoku (na wszystkich monitorach).
                drop(st);
                let slots = data.slots.lock_or_recover().clone();
                let mut windows = state.windows.lock_or_recover();
                for slot in slots.into_iter().flatten() {
                    if let Some(w) = windows.get_mut(slot) {
                        w.mapped = false;
                    }
                }
                let _ = resource;
            }
            // `get_popup` i pozostałe: poza zakresem MVP.
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn st(anchor: u32, width: i32, height: i32) -> LayerState {
        LayerState {
            anchor,
            width,
            height,
            ..LayerState::default()
        }
    }

    #[test]
    fn top_panel_stretches_full_width() {
        // Górny pasek: TOP|LEFT|RIGHT, wysokość 32, szerokość 0 → pełna szerokość.
        let s = st(ANCHOR_TOP | ANCHOR_LEFT | ANCHOR_RIGHT, 0, 32);
        assert_eq!(s.configure_size(1280.0, 800.0), (1280, 32));
        assert_eq!(s.position(1280.0, 800.0, 1280.0, 32.0), (0.0, 0.0));
    }

    #[test]
    fn bottom_right_dock_is_placed_at_corner() {
        // Dock przy prawym-dolnym narożniku, marginesy 8.
        let mut s = st(ANCHOR_BOTTOM | ANCHOR_RIGHT, 200, 400);
        s.margin_right = 8;
        s.margin_bottom = 8;
        assert_eq!(s.configure_size(1280.0, 800.0), (200, 400));
        assert_eq!(s.position(1280.0, 800.0, 200.0, 400.0), (1072.0, 392.0));
    }

    #[test]
    fn unanchored_surface_is_centered() {
        let s = st(0, 400, 200);
        assert_eq!(s.position(1280.0, 800.0, 400.0, 200.0), (440.0, 300.0));
    }

    #[test]
    fn exclusive_zone_does_not_resize_below_zero() {
        let mut s = st(ANCHOR_TOP | ANCHOR_LEFT | ANCHOR_RIGHT, 0, 0);
        // Marginesy większe niż ekran nie mogą dać ujemnego rozmiaru.
        s.margin_left = 5000;
        assert_eq!(s.configure_size(1280.0, 800.0).0, 0);
    }

    #[test]
    fn layer_order_puts_top_above_windows_and_background_below() {
        assert!(layer_z(2) > 0);
        assert!(layer_z(3) > layer_z(2));
        assert!(layer_z(0) < 0);
        // `bottom` leży nad `background`.
        assert!(layer_z(1) > layer_z(0));
    }
}
