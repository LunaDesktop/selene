//! Klient testowy `wl_subcompositor` — okno z powierzchnią potomną.
//!
//! Uruchomienie w sesji Selene:
//!
//! ```text
//! WAYLAND_DISPLAY=wayland-0 cargo run -p selene --example subsurface-probe
//! ```
//!
//! Rodzic to niebieskie okno 200×200, dziecko to czerwony kwadrat 60×60
//! w pozycji (40,40), w trybie `desync` (zmiany działają natychmiast).
//! Dodatkowo tworzymy `xdg_popup` zakotwiczony do dolnej krawędzi rodzica —
//! sprawdza pełne `xdg_positioner` (anchor + gravity).
//!
//! Selene powinna zalogować `selene/subsurface: nowa powierzchnia potomna`,
//! `pierwszy obraz desync 60x60` oraz popup `@ (110,260)`.

use std::error::Error;
use std::time::Duration;

use smithay_client_toolkit::shm::{slot::SlotPool, Shm, ShmHandler};
use wayland_client::protocol::{
    wl_buffer, wl_callback, wl_compositor, wl_registry, wl_shm, wl_shm_pool, wl_subcompositor,
    wl_subsurface, wl_surface,
};
use wayland_client::{
    globals::{registry_queue_init, GlobalListContents},
    Connection, Dispatch, QueueHandle,
};
use wayland_protocols::xdg::shell::client::{
    xdg_popup, xdg_positioner, xdg_surface, xdg_toplevel, xdg_wm_base,
};

#[derive(Default)]
struct App {
    configured: bool,
    shm: Option<Shm>,
}

smithay_client_toolkit::delegate_shm!(App);

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        self.shm.as_mut().expect("shm zainicjalizowany")
    }
}

macro_rules! ignore_events {
    ($($iface:ty),* $(,)?) => {
        $(
            impl Dispatch<$iface, ()> for App {
                fn event(
                    _state: &mut Self,
                    _proxy: &$iface,
                    _event: <$iface as wayland_client::Proxy>::Event,
                    _data: &(),
                    _conn: &Connection,
                    _qh: &QueueHandle<Self>,
                ) {
                }
            }
        )*
    };
}

ignore_events!(
    wl_compositor::WlCompositor,
    wl_shm_pool::WlShmPool,
    wl_buffer::WlBuffer,
    wl_surface::WlSurface,
    wl_callback::WlCallback,
    wl_subcompositor::WlSubcompositor,
    wl_subsurface::WlSubsurface,
    xdg_toplevel::XdgToplevel,
    xdg_popup::XdgPopup,
    xdg_positioner::XdgPositioner,
);

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for App {
    fn event(
        _state: &mut Self,
        _proxy: &wl_registry::WlRegistry,
        _event: wl_registry::Event,
        _data: &GlobalListContents,
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<xdg_wm_base::XdgWmBase, ()> for App {
    fn event(
        _state: &mut Self,
        proxy: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            proxy.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, ()> for App {
    fn event(
        state: &mut Self,
        proxy: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            proxy.ack_configure(serial);
            state.configured = true;
        }
    }
}

fn fill(canvas: &mut [u8], bytes: [u8; 4]) {
    for px in canvas.chunks_exact_mut(4) {
        px.copy_from_slice(&bytes);
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    let conn = Connection::connect_to_env()?;
    let (globals, mut queue) = registry_queue_init::<App>(&conn)?;
    let qh = queue.handle();

    let compositor = globals.bind::<wl_compositor::WlCompositor, _, _>(&qh, 1..=4, ())?;
    let subcompositor = globals.bind::<wl_subcompositor::WlSubcompositor, _, _>(&qh, 1..=1, ())?;
    let wm_base = globals.bind::<xdg_wm_base::XdgWmBase, _, _>(&qh, 1..=1, ())?;
    let shm = Shm::bind(&globals, &qh)?;
    let mut pool = SlotPool::new(256 * 256 * 4, &shm)?;
    let mut app = App {
        configured: false,
        shm: Some(shm),
    };

    let parent = compositor.create_surface(&qh, ());
    let child = compositor.create_surface(&qh, ());

    let xdg = wm_base.get_xdg_surface(&parent, &qh, ());
    let toplevel = xdg.get_toplevel(&qh, ());
    toplevel.set_title("subsurface-probe".to_owned());

    let subsurface = subcompositor.get_subsurface(&child, &parent, &qh, ());
    subsurface.set_position(40, 40);
    // `desync`: obraz dziecka wchodzi od razu, bez czekania na commit rodzica.
    subsurface.set_desync();

    // Pierwszy commit bez bufora → kompozytor wysyła `configure`.
    parent.commit();
    while !app.configured {
        queue.blocking_dispatch(&mut app)?;
    }

    // Rodzic: niebieskie tło (ARGB8888 w pamięci = B,G,R,A).
    let parent_buffer = {
        let (buffer, canvas) = pool.create_buffer(200, 200, 800, wl_shm::Format::Argb8888)?;
        fill(canvas, [0xff, 0x00, 0x00, 0xff]);
        buffer.attach_to(&parent)?;
        buffer
    };
    parent.commit();

    // Dziecko: czerwony kwadrat.
    let child_buffer = {
        let (buffer, canvas) = pool.create_buffer(60, 60, 240, wl_shm::Format::Argb8888)?;
        fill(canvas, [0x00, 0x00, 0xff, 0xff]);
        buffer.attach_to(&child)?;
        buffer
    };
    child.commit();
    // Czekamy, aż rodzic będzie w modelu okien — popup pozycjonujemy względem niego.
    queue.roundtrip(&mut app)?;

    // Popup: kotwica na dolnej krawędzi rodzica, rośnie w dół. Oczekiwana pozycja
    // to lewy-górny róg rodzica + (50, 220): x = środek kotwicy − pół popupu,
    // y = dolna krawędź kotwicy.
    let positioner = wm_base.create_positioner(&qh, ());
    positioner.set_size(100, 80);
    positioner.set_anchor_rect(0, 200, 200, 20);
    positioner.set_anchor(xdg_positioner::Anchor::Bottom);
    positioner.set_gravity(xdg_positioner::Gravity::Bottom);
    positioner.set_constraint_adjustment(
        xdg_positioner::ConstraintAdjustment::SlideX
            | xdg_positioner::ConstraintAdjustment::SlideY
            | xdg_positioner::ConstraintAdjustment::FlipX
            | xdg_positioner::ConstraintAdjustment::FlipY,
    );
    let popup_surface = compositor.create_surface(&qh, ());
    let popup_xdg = wm_base.get_xdg_surface(&popup_surface, &qh, ());
    let popup = popup_xdg.get_popup(Some(&xdg), &positioner, &qh, ());
    positioner.destroy();
    popup_surface.commit();
    queue.roundtrip(&mut app)?;

    // Popup: żółty prostokąt.
    let popup_buffer = {
        let (buffer, canvas) = pool.create_buffer(100, 80, 400, wl_shm::Format::Argb8888)?;
        fill(canvas, [0x00, 0xff, 0xff, 0xff]);
        buffer.attach_to(&popup_surface)?;
        buffer
    };
    popup_surface.commit();

    queue.roundtrip(&mut app)?;
    std::thread::sleep(Duration::from_secs(4));
    drop((
        child_buffer,
        parent_buffer,
        popup_buffer,
        popup,
        subsurface,
        toplevel,
        xdg,
    ));
    Ok(())
}
