//! `wl_output` — monitory. Model gotowy na wiele monitorów; wykrywanie w [`crate::drm`].
//!
//! Każdy monitor ma własną pozycję w globalnej przestrzeni współrzędnych, dzięki czemu
//! okna i powierzchnie warstwowe można rozkładać i kotwiczyć na konkretnym wyświetlaczu.

use std::sync::{Arc, Mutex};

use libluna_core::Rect;
use wayland_server::protocol::wl_output::{self, WlOutput};
use wayland_server::{Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource};

use crate::state::State;
use crate::sync::LockExt;

/// Informacje o monitorze (współdzielone z wątkiem wyjścia, który zna bieżący rozmiar).
#[derive(Debug, Clone)]
pub struct OutputInfo {
    pub name: String,
    pub width: i32,
    pub height: i32,
    /// Odświeżanie bieżącego trybu (Hz) — z EDID, nie stałe 60.
    pub refresh_hz: i32,
    pub scale: i32,
    /// Pozycja logiczna (lewy-górny narożnik) w globalnej przestrzeni współrzędnych.
    pub x: i32,
    pub y: i32,
}

impl Default for OutputInfo {
    fn default() -> Self {
        Self {
            name: "nested-0".to_owned(),
            width: 1280,
            height: 800,
            refresh_hz: 60,
            scale: 1,
            x: 0,
            y: 0,
        }
    }
}

impl OutputInfo {
    /// Prostokąt monitora w globalnej przestrzeni współrzędnych.
    pub fn rect(&self) -> Rect {
        Rect::from_xywh(
            self.x as f64,
            self.y as f64,
            self.width as f64,
            self.height as f64,
        )
    }
}

/// Współdzielona lista monitorów.
pub type Outputs = Arc<Mutex<Vec<OutputInfo>>>;

/// Układa monitory poziomo, od lewej do prawej (przydatne dla trybu nested i układu lustrzanego).
pub fn layout_horizontal(outs: &mut [OutputInfo]) {
    let mut x = 0;
    for o in outs.iter_mut() {
        o.x = x;
        o.y = 0;
        x += o.width;
    }
}

/// Indeks monitora zawierającego punkt (lub pierwszy, gdy żaden nie pasuje).
pub fn output_at(outs: &[OutputInfo], x: f64, y: f64) -> usize {
    outs.iter()
        .position(|o| o.rect().contains(libluna_core::Point::new(x, y)))
        .unwrap_or(0)
}

/// Wysyła klientowi opis monitora (geometria, tryb, skala, nazwa, gotowe).
pub fn send_info(output: &WlOutput, info: &OutputInfo) {
    let _ = output.send_event(wl_output::Event::Geometry {
        x: info.x,
        y: info.y,
        physical_width: info.width * 300 / 1000,
        physical_height: info.height * 300 / 1000,
        subpixel: wayland_server::WEnum::Value(wl_output::Subpixel::Unknown),
        make: "Luna".to_owned(),
        model: info.name.clone(),
        transform: wayland_server::WEnum::Value(wl_output::Transform::Normal),
    });
    let _ = output.send_event(wl_output::Event::Mode {
        flags: wayland_server::WEnum::Value(wl_output::Mode::Current),
        width: info.width,
        height: info.height,
        refresh: info.refresh_hz.max(1) * 1000,
    });
    let _ = output.send_event(wl_output::Event::Scale { factor: info.scale });
    let _ = output.send_event(wl_output::Event::Name {
        name: info.name.clone(),
    });
    let _ = output.send_event(wl_output::Event::Done);
}

impl GlobalDispatch<WlOutput, usize> for State {
    fn bind(
        state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<WlOutput>,
        data: &usize,
        data_init: &mut DataInit<'_, Self>,
    ) {
        // Zasób niesie swój indeks monitora (potrzebny m.in. dla `wlr-layer-shell`).
        let output = data_init.init(resource, *data);
        let info = state
            .outputs
            .lock_or_recover()
            .get(*data)
            .cloned()
            .unwrap_or_default();
        send_info(&output, &info);
        // Trzymamy zasób, żeby po faktycznym modesecie DRM wysłać poprawione
        // `geometry`/`mode` (wczesny probe i setup mogą chwilowo różnić się trybem).
        state.bound_outputs.push((*data, output));
    }
}

impl Dispatch<WlOutput, usize> for State {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &WlOutput,
        _request: wl_output::Request,
        _data: &usize,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
    }
}

/// Sygnatura listy monitorów — tania do porównania w pętli głównej.
pub fn signature(outs: &[OutputInfo]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for o in outs {
        for v in [o.x, o.y, o.width, o.height, o.refresh_hz, o.scale] {
            h ^= v as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        for b in o.name.bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    h
}

/// Wysyła klientom nowe `wl_output` po zmianie modelu (np. faktyczny tryb z DRM).
pub fn refresh_bound(state: &mut State) {
    let outs = state.outputs.lock_or_recover().clone();
    let sig = signature(&outs);
    if sig == state.outputs_sig {
        return;
    }
    state.outputs_sig = sig;
    state.bound_outputs.retain(|(_, r)| r.is_alive());
    for (idx, res) in &state.bound_outputs {
        if let Some(info) = outs.get(*idx) {
            send_info(res, info);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn out(name: &str, w: i32, h: i32) -> OutputInfo {
        OutputInfo {
            name: name.to_owned(),
            width: w,
            height: h,
            refresh_hz: 60,
            scale: 1,
            x: 0,
            y: 0,
        }
    }

    #[test]
    fn horizontal_layout_places_monitors_side_by_side() {
        let mut outs = vec![out("eDP-1", 1920, 1080), out("HDMI-1", 1280, 720)];
        layout_horizontal(&mut outs);
        assert_eq!((outs[0].x, outs[0].y), (0, 0));
        assert_eq!((outs[1].x, outs[1].y), (1920, 0));
    }

    #[test]
    fn signature_notices_size_and_refresh_changes() {
        let a = vec![out("eDP-1", 1920, 1080)];
        let mut b = a.clone();
        assert_eq!(signature(&a), signature(&b));
        b[0].refresh_hz = 75;
        assert_ne!(signature(&a), signature(&b));
        b[0].refresh_hz = 60;
        b[0].width = 1600;
        assert_ne!(signature(&a), signature(&b));
    }

    #[test]
    fn output_at_finds_the_monitor_under_the_point() {
        let mut outs = vec![out("eDP-1", 1920, 1080), out("HDMI-1", 1280, 720)];
        layout_horizontal(&mut outs);
        assert_eq!(output_at(&outs, 100.0, 100.0), 0);
        assert_eq!(output_at(&outs, 2000.0, 100.0), 1);
        // Poza wszystkimi monitorami → pierwszy.
        assert_eq!(output_at(&outs, -500.0, -500.0), 0);
    }
}
