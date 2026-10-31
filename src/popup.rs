//! Pozycjonowanie `xdg_popup` zgodnie z `xdg_positioner`.
//!
//! Wcześniej liczyliśmy tylko „anchor + offset”, ignorując krawędź zakotwiczenia,
//! kierunek rozwoju (`gravity`) i `constraint_adjustment`. Efekt: menu i palety poleceń
//! (Zed, VS Code, GTK) potrafiły wylądować obok miejsca, w które użytkownik klikał —
//! wyglądało to jak „nieklikalna treść”. Tutaj jest pełny algorytm z xdg-shell:
//! punkt zakotwiczenia → grawitacja → przesunięcie → flip/slide w obszarze roboczym.

use libluna_core::Rect;

use crate::shell::PopupGeom;

/// `xdg_positioner.constraint_adjustment`: przesuń w bok, żeby zmieścić popup.
pub const ADJUST_SLIDE_X: u32 = 1;
/// Przesuń w pionie.
pub const ADJUST_SLIDE_Y: u32 = 2;
/// Odbij w poziomie na drugą stronę kotwicy.
pub const ADJUST_FLIP_X: u32 = 4;
/// Odbij w pionie.
pub const ADJUST_FLIP_Y: u32 = 8;

// Wartości enuma `xdg_positioner.anchor` / `.gravity` (0–8, nie bitmaski).
#[cfg(test)]
const NONE: u8 = 0;
const TOP: u8 = 1;
const BOTTOM: u8 = 2;
const LEFT: u8 = 3;
const RIGHT: u8 = 4;
const TOP_LEFT: u8 = 5;
const BOTTOM_LEFT: u8 = 6;
const TOP_RIGHT: u8 = 7;
const BOTTOM_RIGHT: u8 = 8;

/// Oblicza lewy-górny róg **widocznego** popupu w układzie bezwzględnym.
///
/// * `geom` — stan `xdg_positioner` (kotwica względem geometrii okna rodzica),
/// * `parent` — prostokąt treści rodzica (bezwzględny),
/// * `work` — obszar roboczy monitora (bez panelu),
/// * `fallback` — rozmiar popupu, gdy positioner nie podał go w `set_size`.
pub fn place(geom: &PopupGeom, parent: Rect, work: Rect, fallback: (f64, f64)) -> (f64, f64) {
    let size = (
        if geom.width > 0 {
            geom.width as f64
        } else {
            fallback.0
        },
        if geom.height > 0 {
            geom.height as f64
        } else {
            fallback.1
        },
    );
    let anchor_rect = Rect::from_xywh(
        parent.x() + geom.anchor_x,
        parent.y() + geom.anchor_y,
        geom.anchor_w,
        geom.anchor_h,
    );
    let offset = (geom.offset_x, geom.offset_y);
    let (mut x, mut y) = raw_position(anchor_rect, geom.anchor, geom.gravity, offset, size);

    // Kolejność jak w specyfikacji: flip_x, flip_y, slide_x, slide_y.
    let outside_x = |x: f64| x < work.x() - 0.5 || x + size.0 > work.right() + 0.5;
    let outside_y = |y: f64| y < work.y() - 0.5 || y + size.1 > work.bottom() + 0.5;

    if outside_x(x) && geom.constraint & ADJUST_FLIP_X != 0 {
        let (fx, _) = raw_position(
            anchor_rect,
            flip_x(geom.anchor),
            flip_x(geom.gravity),
            offset,
            size,
        );
        x = fx;
    }
    if outside_y(y) && geom.constraint & ADJUST_FLIP_Y != 0 {
        let (_, fy) = raw_position(
            anchor_rect,
            flip_y(geom.anchor),
            flip_y(geom.gravity),
            offset,
            size,
        );
        y = fy;
    }
    if geom.constraint & ADJUST_SLIDE_X != 0 {
        x = clamp_axis(x, size.0, work.x(), work.right());
    }
    if geom.constraint & ADJUST_SLIDE_Y != 0 {
        y = clamp_axis(y, size.1, work.y(), work.bottom());
    }
    (x, y)
}

/// Pozycja bez uwzględnienia ograniczeń monitora.
fn raw_position(
    anchor_rect: Rect,
    anchor: u8,
    gravity: u8,
    offset: (f64, f64),
    size: (f64, f64),
) -> (f64, f64) {
    let (ax, ay) = anchor_point(anchor_rect, anchor);
    let (gx, gy) = gravity_offset(gravity, size);
    (ax + offset.0 + gx, ay + offset.1 + gy)
}

/// Punkt na krawędzi/narożniku prostokąta kotwicy (`none` = środek).
fn anchor_point(r: Rect, anchor: u8) -> (f64, f64) {
    let x = match anchor {
        LEFT | TOP_LEFT | BOTTOM_LEFT => r.x(),
        RIGHT | TOP_RIGHT | BOTTOM_RIGHT => r.right(),
        _ => r.center().x,
    };
    let y = match anchor {
        TOP | TOP_LEFT | TOP_RIGHT => r.y(),
        BOTTOM | BOTTOM_LEFT | BOTTOM_RIGHT => r.bottom(),
        _ => r.center().y,
    };
    (x, y)
}

/// Przesunięcie popupu względem punktu kotwicy (kierunek „rozwoju”).
fn gravity_offset(gravity: u8, size: (f64, f64)) -> (f64, f64) {
    let x = match gravity {
        LEFT | TOP_LEFT | BOTTOM_LEFT => -size.0,
        RIGHT | TOP_RIGHT | BOTTOM_RIGHT => 0.0,
        _ => -size.0 / 2.0,
    };
    let y = match gravity {
        TOP | TOP_LEFT | TOP_RIGHT => -size.1,
        BOTTOM | BOTTOM_LEFT | BOTTOM_RIGHT => 0.0,
        _ => -size.1 / 2.0,
    };
    (x, y)
}

fn flip_x(bits: u8) -> u8 {
    match bits {
        LEFT => RIGHT,
        RIGHT => LEFT,
        TOP_LEFT => TOP_RIGHT,
        TOP_RIGHT => TOP_LEFT,
        BOTTOM_LEFT => BOTTOM_RIGHT,
        BOTTOM_RIGHT => BOTTOM_LEFT,
        other => other,
    }
}

fn flip_y(bits: u8) -> u8 {
    match bits {
        TOP => BOTTOM,
        BOTTOM => TOP,
        TOP_LEFT => BOTTOM_LEFT,
        BOTTOM_LEFT => TOP_LEFT,
        TOP_RIGHT => BOTTOM_RIGHT,
        BOTTOM_RIGHT => TOP_RIGHT,
        other => other,
    }
}

/// Wsuwa popup w obszar; gdy jest większy niż obszar, wyrównuje do początku.
fn clamp_axis(pos: f64, size: f64, start: f64, end: f64) -> f64 {
    let max = (end - size).max(start);
    pos.clamp(start, max)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geom(anchor: u8, gravity: u8) -> PopupGeom {
        PopupGeom {
            width: 200,
            height: 100,
            anchor_x: 100.0,
            anchor_y: 200.0,
            anchor_w: 50.0,
            anchor_h: 30.0,
            offset_x: 0.0,
            offset_y: 0.0,
            anchor,
            gravity,
            constraint: 0,
        }
    }

    fn parent() -> Rect {
        Rect::from_xywh(0.0, 0.0, 800.0, 600.0)
    }

    fn work() -> Rect {
        Rect::from_xywh(0.0, 0.0, 1920.0, 1080.0)
    }

    #[test]
    fn center_anchor_and_gravity_center_on_anchor() {
        let g = geom(NONE, NONE);
        // Kotwica: (100,200)+(50x30) → środek (125,215); popup 200x100 → lewy-górny (25,165).
        assert_eq!(place(&g, parent(), work(), (200.0, 100.0)), (25.0, 165.0));
    }

    #[test]
    fn bottom_anchor_with_bottom_gravity_opens_below() {
        // GTK/Qt domyślnie: kotwica na dolnej krawędzi, popup rośnie w dół.
        let g = geom(BOTTOM, BOTTOM);
        // Punkt kotwicy: (środek x = 125, dolna krawędź = 230); popup 200 px → x = -75? nie:
        // grawitacja „dół” nie przesuwa w poziomie, ale brak kotwicy poziomej = środek.
        assert_eq!(
            place(&g, parent(), work(), (200.0, 100.0)),
            (125.0 - 100.0, 230.0)
        );
    }

    #[test]
    fn offset_is_applied_after_gravity() {
        let mut g = geom(BOTTOM, BOTTOM);
        g.offset_x = 7.0;
        g.offset_y = -3.0;
        assert_eq!(place(&g, parent(), work(), (200.0, 100.0)), (32.0, 227.0));
    }

    #[test]
    fn flip_y_puts_popup_above_when_below_does_not_fit() {
        // Rodzic nisko: dolna krawędź kotwicy na 1030, popup 100 px nie mieści się.
        let mut g = geom(BOTTOM, BOTTOM);
        g.anchor_y = 1000.0;
        g.constraint = ADJUST_FLIP_Y;
        let parent = Rect::from_xywh(0.0, 0.0, 800.0, 1080.0);
        let (_, y) = place(&g, parent, work(), (200.0, 100.0));
        // Po odbiciu kotwica jest na górnej krawędzi (1000), popup rośnie w górę.
        assert_eq!(y, 900.0);
    }

    #[test]
    fn slide_x_keeps_popup_inside_output() {
        let mut g = geom(NONE, NONE);
        g.anchor_x = 1800.0;
        g.constraint = ADJUST_SLIDE_X;
        let (x, _) = place(&g, parent(), work(), (200.0, 100.0));
        assert_eq!(x, 1920.0 - 200.0);
    }

    #[test]
    fn oversized_popup_is_aligned_to_work_area_start() {
        let mut g = geom(NONE, NONE);
        g.width = 4000;
        g.height = 4000;
        g.constraint = ADJUST_SLIDE_X | ADJUST_SLIDE_Y;
        let (x, y) = place(&g, parent(), work(), (4000.0, 2000.0));
        assert_eq!((x, y), (0.0, 0.0));
    }

    #[test]
    fn corner_anchor_matches_bottom_right() {
        let mut g = geom(BOTTOM_RIGHT, TOP_LEFT);
        g.anchor_x = 300.0;
        // Kotwica (350,230); grawitacja w lewo/górę — popup kończy się w tym punkcie.
        assert_eq!(place(&g, parent(), work(), (200.0, 100.0)), (150.0, 130.0));
    }
}
