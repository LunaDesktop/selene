//! Sonda X11: okno + menu override-redirect, które wypisują kliknięcia.
//!
//! ```sh
//! DISPLAY=:1 cargo run --release --example x11-probe
//! ```
//!
//! Pierwszy klik w okno otwiera „menu” (override-redirect) pod (300,300).
//! Klik w menu powinien trafić **menu**, a nie okno pod spodem — to sprawdza
//! pozycjonowanie i routing kliknięć dla okien X11.
//!
//! Nie jest instalowana — narzędzie diagnostyczne.

use std::error::Error;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt, CreateWindowAux, EventMask, Window, WindowClass};
use x11rb::protocol::Event;
use x11rb::COPY_FROM_PARENT;

fn open_window(
    conn: &impl Connection,
    screen: &x11rb::protocol::xproto::Screen,
    x: i16,
    y: i16,
    w: u16,
    h: u16,
    override_redirect: bool,
    color: u32,
) -> Result<Window, Box<dyn Error>> {
    let win = conn.generate_id()?;
    let mut aux = CreateWindowAux::new().background_pixel(color).event_mask(
        EventMask::EXPOSURE
            | EventMask::BUTTON_PRESS
            | EventMask::BUTTON_RELEASE
            | EventMask::STRUCTURE_NOTIFY,
    );
    if override_redirect {
        aux = aux.override_redirect(1);
    }
    conn.create_window(
        COPY_FROM_PARENT as u8,
        win,
        screen.root,
        x,
        y,
        w,
        h,
        0,
        WindowClass::INPUT_OUTPUT,
        COPY_FROM_PARENT as u32,
        &aux,
    )?;
    conn.map_window(win)?;
    conn.flush()?;
    Ok(win)
}

fn main() -> Result<(), Box<dyn Error>> {
    let (conn, screen_num) = x11rb::connect(None)?;
    let screen = conn.setup().roots[screen_num].clone();
    let main = open_window(&conn, &screen, 60, 60, 600, 400, false, screen.white_pixel)?;
    eprintln!("x11-probe: okno {main} zmapowane na ekranie {screen_num}");
    let mut popup: Option<Window> = None;
    loop {
        let event = conn.wait_for_event()?;
        match event {
            Event::ButtonPress(e) if Some(e.event) == popup => {
                eprintln!(
                    "x11-probe: POPUP press {} at ({},{}) w oknie {}",
                    e.detail, e.event_x, e.event_y, e.event
                );
            }
            Event::ButtonPress(e) => {
                eprintln!(
                    "x11-probe: press {} at ({},{}) w oknie {}",
                    e.detail, e.event_x, e.event_y, e.event
                );
                if popup.is_none() && e.event == main {
                    let p =
                        open_window(&conn, &screen, 300, 300, 200, 150, true, screen.black_pixel)?;
                    eprintln!("x11-probe: menu override-redirect {p} na (300,300)");
                    popup = Some(p);
                }
            }
            Event::ButtonRelease(e) => eprintln!(
                "x11-probe: release {} at ({},{}) w oknie {}",
                e.detail, e.event_x, e.event_y, e.event
            ),
            Event::Error(e) => eprintln!("x11-probe: error {e:?}"),
            _ => {}
        }
    }
}
