//! Sonda klawiatury: co xkbcommon zwraca dla układu `pl` z AltGr i Shiftem.
//!
//! ```sh
//! cargo run --release --example keysym-probe -p selene
//! ```
//!
//! Pozwala sprawdzić, czy `"`, `„`, `”`, `€` itd. mają poprawne keysymy/tekst.

use xkbcommon::xkb;

fn dump(label: &str, keymap: &xkb::Keymap, held: &[u32]) {
    let mut state = xkb::State::new(keymap);
    for &code in held {
        state.update_key(xkb::Keycode::new(code + 8), xkb::KeyDirection::Down);
    }
    println!("== {label} ==");
    for code in 1..=60u32 {
        let kc = xkb::Keycode::new(code + 8);
        state.update_key(kc, xkb::KeyDirection::Down);
        let sym = state.key_get_one_sym(kc).raw();
        let text = state.key_get_utf8(kc);
        state.update_key(kc, xkb::KeyDirection::Up);
        if !text.is_empty() && (text.chars().any(|c| !c.is_ascii_alphanumeric()) || text.len() > 1)
        {
            println!("evdev {code:2}: keysym=0x{sym:08x} text={text:?}");
        }
    }
}

fn main() {
    let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let km = xkb::Keymap::new_from_names(
        &ctx,
        "",
        "",
        "pl",
        "",
        Some("lv3:ralt_switch".to_owned()),
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    )
    .expect("keymap pl");
    dump("bez modyfikatorów", &km, &[]);
    dump("Shift", &km, &[42]);
    dump("AltGr (RALT=100)", &km, &[100]);
    dump("Shift+AltGr", &km, &[42, 100]);
}
