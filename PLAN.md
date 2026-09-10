# Later

Scanbro should work with Brother scanners in general, not only the DS-940DW. Keep the app opinionated: scan, PDF, email, optional text. Do not turn it into a generic scanning suite.

## Other Brother scanners

- Drop the DS-940 name filter. Accept any Brother eSCL device (network `ty=Brother`, USB vendor `04f9`).
- Drive the UI from `GET /eSCL/ScannerCapabilities`, not from a list of model names: feeder vs glass, duplex, max length, DPI, colour.
- Hide Long receipt, duplex, and continuous feed when the device cannot do them.
- Add a scanner picker only when two devices show up. Until then, keep auto-connect.
- Add a glass-platen path for MFC / DCP all-in-ones. The “load next sheet” loop is wrong for that.
- Keep a tiny quirks table only where Brother’s capabilities XML is wrong. Do not start with per-model profiles.
- Stay on eSCL (Wi-Fi) and SANE for USB-only leftovers. Do not chase old `brscan` / TWAIN stacks.

## Good next machines to try

- An ADS desktop unit (bigger feeder, real multi-sheet).
- A recent MFC (forces platen).
- A simplex DS-640 (forces duplex to be optional).

Leave the product named Scanbro. The DS-940DW stays the well-tested default until a second model actually works.

## Keyboard (Omarchy)

Made for Omarchy, so the app should be usable from the keyboard without the mouse. Scan, save, email, quality, connection, and checkboxes should all have keys. Tab order should follow the screen. Do not add actions that exist only as a click.
