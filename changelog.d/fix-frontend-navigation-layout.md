### Fixed

- Session page (F-05): the selected run is read from the `run` URL parameter and kept in sync with it, so switching between sibling runs or teammates and then Back, Forward or a reload shows the run the URL actually points at.
- Mobile layout (F-06): `.grow`, `.muted` and `.card.row` wrap long hostnames and unbroken commands (`overflow-wrap: anywhere; word-break: break-word`), so a terminal or session card no longer scrolls a phone sideways.
- Mobile topbar (F-07): padding drops to 10px at 700px and the nav wraps, so the top bar with the attention badge fits a 390px viewport.
- Chat deep links (F-08): opening a chat records `?chat=<id>` and "New chat" removes it, so the selected chat survives a reload and stays shareable.
