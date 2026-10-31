//! `zxdg_decoration_manager_v1` — negocjacja dekoracji okien.
//!
//! Domyślnie **my** rysujemy pasek tytułu (spójny wygląd całej Luny): na prośbę
//! o `client_side` odpowiadamy `server_side`, więc GTK3 nie rysuje własnego CSD.
//! Wyjątki: `LUNA_CLIENT_DECORATIONS=1` (klient decyduje) — przydatne, gdy jakaś
//! aplikacja naprawdę wymaga własnych dekoracji.

use wayland_protocols::xdg::decoration::zv1::server::{
    zxdg_decoration_manager_v1 as decoration_manager,
    zxdg_toplevel_decoration_v1 as toplevel_decoration,
};
use wayland_protocols::xdg::shell::server::xdg_toplevel;
use wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, WEnum,
};

use crate::shell::XdgToplevelData;
use crate::state::State;
use crate::sync::LockExt;

use libluna_core::Color;
use libluna_render::ImageData;

/// Uchwyt obiektu dekoracji okna.
pub type ToplevelDecoration = toplevel_decoration::ZxdgToplevelDecorationV1;

/// Negocjowany tryb dekoracji okna.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DecoMode {
    /// Dekoracje rysuje serwer (nasz pasek tytułu).
    Server,
    /// Dekoracje rysuje klient (CSD).
    Client,
}

/// Dane obiektu dekoracji — trzymamy powiązany `xdg_toplevel`, by móc odczytać
/// i zapisać tryb w [`XdgToplevelData`].
pub struct DecoData {
    pub toplevel: xdg_toplevel::XdgToplevel,
}

/// Dobiera kolor paska SSD do górnej części treści okna.
///
/// Próbkujemy pierwsze ~28 px bufora klienta (tam zwykle jest pasek narzędzi/header
/// aplikacji), pomijając piksele przezroczyste (cienie CSD). Zwracamy parę
/// `(tło paska, kolor tekstu/ikon)` z automatycznym kontrastem.
pub fn tint_from_image(img: &ImageData) -> Option<(Color, Color)> {
    if img.width == 0 || img.height == 0 {
        return None;
    }
    let expected = img.width as usize * img.height as usize * 4;
    if img.rgba.len() < expected {
        return None;
    }
    let y1 = img.height.min(28);
    let step_x = (img.width / 48).max(1);
    let (mut r, mut g, mut b, mut n) = (0u64, 0u64, 0u64, 0u64);
    for y in 0..y1 {
        let mut x = 0;
        while x < img.width {
            let i = ((y * img.width + x) * 4) as usize;
            if img.rgba[i + 3] > 200 {
                r += img.rgba[i] as u64;
                g += img.rgba[i + 1] as u64;
                b += img.rgba[i + 2] as u64;
                n += 1;
            }
            x += step_x;
        }
    }
    if n < 8 {
        return None;
    }
    let bg = Color::from_rgba8((r / n) as u8, (g / n) as u8, (b / n) as u8, 0xff);
    // Percepcyjna luminancja — na jasnym tle dajemy ciemny tekst, na ciemnym biały.
    let lum = 0.2126 * bg.r + 0.7152 * bg.g + 0.0722 * bg.b;
    let fg = if lum > 0.60 {
        Color::from_rgba8(0x1c, 0x1c, 0x1c, 0xff)
    } else {
        Color::WHITE
    };
    Some((bg, fg))
}

impl GlobalDispatch<decoration_manager::ZxdgDecorationManagerV1, ()> for State {
    fn bind(
        _state: &mut Self,
        _handle: &DisplayHandle,
        _client: &Client,
        resource: New<decoration_manager::ZxdgDecorationManagerV1>,
        _data: &(),
        data_init: &mut DataInit<'_, Self>,
    ) {
        data_init.init(resource, ());
    }
}

impl Dispatch<decoration_manager::ZxdgDecorationManagerV1, ()> for State {
    fn request(
        _state: &mut Self,
        _client: &Client,
        _resource: &decoration_manager::ZxdgDecorationManagerV1,
        request: decoration_manager::Request,
        _data: &(),
        _handle: &DisplayHandle,
        data_init: &mut DataInit<'_, Self>,
    ) {
        if let decoration_manager::Request::GetToplevelDecoration { id, toplevel } = request {
            let deco = data_init.init(
                id,
                DecoData {
                    toplevel: toplevel.clone(),
                },
            );
            if let Some(td) = toplevel.data::<XdgToplevelData>() {
                *td.decoration.lock_or_recover() = Some(deco);
            }
        }
    }
}

impl Dispatch<toplevel_decoration::ZxdgToplevelDecorationV1, DecoData> for State {
    fn request(
        _state: &mut Self,
        _client: &Client,
        resource: &toplevel_decoration::ZxdgToplevelDecorationV1,
        request: toplevel_decoration::Request,
        data: &DecoData,
        _handle: &DisplayHandle,
        _data_init: &mut DataInit<'_, Self>,
    ) {
        let Some(td) = data.toplevel.data::<XdgToplevelData>() else {
            return;
        };
        match request {
            toplevel_decoration::Request::SetMode { mode } => {
                // **Zawsze dekorujemy sami.** GTK3 (thunar, xfce4-terminal, gedit…) domyślnie
                // prosi o `client_side`; gdybyśmy się zgodzili, każde okno miałoby inny
                // pasek tytułu (albo żaden). Odpowiadamy `server_side` — GTK wtedy wyłącza
                // własne CSD i korzysta z naszego paska, spójnego z resztą Luny.
                // `LUNA_CLIENT_DECORATIONS=1` przywraca dawną politykę (klient decyduje).
                let honor = std::env::var("LUNA_CLIENT_DECORATIONS").is_ok_and(|v| v != "0");
                let requested = if honor {
                    match mode {
                        WEnum::Value(toplevel_decoration::Mode::ServerSide) => DecoMode::Server,
                        _ => DecoMode::Client,
                    }
                } else {
                    DecoMode::Server
                };
                *td.deco.lock_or_recover() = Some(requested);
                let reply = match requested {
                    DecoMode::Server => toplevel_decoration::Mode::ServerSide,
                    DecoMode::Client => toplevel_decoration::Mode::ClientSide,
                };
                let _ = resource.send_event(toplevel_decoration::Event::Configure {
                    mode: WEnum::Value(reply),
                });
            }
            // Klient oddaje decyzję kompozytorowi — domyślnie dekorujemy my.
            toplevel_decoration::Request::UnsetMode => {
                *td.deco.lock_or_recover() = Some(DecoMode::Server);
                let _ = resource.send_event(toplevel_decoration::Event::Configure {
                    mode: WEnum::Value(toplevel_decoration::Mode::ServerSide),
                });
            }
            toplevel_decoration::Request::Destroy => {
                *td.deco.lock_or_recover() = None;
                *td.decoration.lock_or_recover() = None;
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(color: [u8; 3], alpha: u8) -> ImageData {
        let (w, h) = (64u32, 64u32);
        let mut rgba = vec![0u8; (w * h * 4) as usize];
        for y in 0..20 {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                rgba[i] = color[0];
                rgba[i + 1] = color[1];
                rgba[i + 2] = color[2];
                rgba[i + 3] = alpha;
            }
        }
        ImageData::new(w, h, rgba)
    }

    #[test]
    fn tint_takes_top_strip_and_picks_contrast() {
        let (bg, fg) = tint_from_image(&image([30, 40, 50], 255)).expect("ciemny pasek");
        assert!((bg.r - 30.0 / 255.0).abs() < 0.01);
        assert_eq!(fg, Color::WHITE);

        let (bg, fg) = tint_from_image(&image([240, 240, 240], 255)).expect("jasny pasek");
        assert!(bg.r > 0.9);
        assert!(fg.r < 0.2, "na jasnym tle tekst musi być ciemny");
    }

    #[test]
    fn transparent_pixels_do_not_tint() {
        assert!(tint_from_image(&image([200, 50, 50], 0)).is_none());
    }
}
