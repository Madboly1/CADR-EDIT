# CADR Studio app icon

The approved CADR Studio mark is based on the original artwork supplied by the owner: a geometric
open C-frame with an orange play triangle, on a charcoal rounded-square field. The vector master
is traced from that artwork and preserves its two-tone light frame.

## Brand palette

| Colour | Hex | Use |
|---|---|---|
| Charcoal | `#101116` | Icon background |
| Off-white | `#F4F4F5` | Upper part of the C mark |
| Cool gray | `#D9DCE3` | Lower part of the C mark |
| Orange | `#FF642F` | Play symbol |
| Border | `#242630` | Subtle rounded-square outline |

## Canonical artwork

- `filmcraft.svg` — canonical 512-unit scalable master (legacy filename retained for build compatibility).
- `filmcraft-small.svg` — compact copy of the same approved vector.
- `hicolor/scalable/apps/ai.storyteller.filmcraft.svg` — Linux scalable icon.

The old owl/violet artwork and the unrelated violet/pink concept are not part of the CADR Studio
identity. Do not restore or reuse them.

## Platform derivatives

`packaging/icons.sh` regenerates the PNG, ICO, ICNS, Linux hicolor, and web favicon derivatives
from the canonical SVG. Run it after changing the master; the script requires `resvg`, and
macOS `.icns` generation additionally requires `iconutil`. Legacy filenames and app IDs remain
temporarily because the packaging/build configuration still references them.
