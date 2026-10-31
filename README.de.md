# Selene

Der Wayland-Compositor von Luna. xdg-shell-Fenster, wlr-layer-shell-Panels, evdev-Eingabe, DRM/KMS, XWayland und Multi-Monitor. In Rust ohne smithay.

> **Sprachen:** [Polski](README.pl.md) · [English](README.en.md) · [Deutsch](README.de.md)

## Teil des Luna-Projekts

Dieses Repository ist Teil der Organisation [LunaDesktop](https://github.com/LunaDesktop) und des **Luna**-Projekts — ein Desktop in Rust, ohne GTK, Qt oder GObject.

## Bauen

```bash
git clone https://github.com/LunaDesktop/selene.git
cd selene
cargo build --release
```

Anforderungen: Linux, Rust 1.80+, Wayland (für Fensteranwendungen).

## Starten

```bash
cargo run --release
```

## Dokumentation

- Projektseite: <https://luna.paganlinux.eu/>
- Entwicklerdokumentation: <https://luna.paganlinux.eu/en/docs/>
- Polnische Dokumentation: <https://luna.paganlinux.eu/docs/>
- API-Index: <https://luna.paganlinux.eu/en/docs/api.html>

## Lizenz

Der Code steht unter der Lizenz **GPL-3.0-or-later**. Der vollständige Text liegt in [LICENSE](LICENSE).
