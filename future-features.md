# Future features (deferred)

Ideas intentionally postponed. Not on the active roadmap.

## Save / Share actions on clips

Quick ways to get a clip out of open-recorder and into a chat:

- **Copy to clipboard as a file** — put the clip on the Wayland clipboard as
  `text/uri-list` (and `text/plain` path) via `wl-copy`, so it can be pasted /
  dragged straight into Discord, Slack, etc.
- **Share link (upload)** — upload the clip (or a Discord-sized export) to a
  host and copy a URL to the clipboard. Needs a destination decision
  (self-hosted vs. a service) and auth/secret handling, which is why it's parked.

Sketched module shape (when revisited): `share.rs` (upload/link) +
`clipboard.rs` (`wl-copy text/uri-list`), wired as extra actions on each clip
card next to Open / Export / Reveal / Delete.

Deferred on purpose — do not implement without an explicit ask.
