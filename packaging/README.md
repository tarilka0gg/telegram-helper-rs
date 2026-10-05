# Packaging

`packaging/package.sh` → `dist/tgh-server-<version>-linux-<arch>.tar.gz` (+ `.sha256`). Needs zig to build. Ships the binary, a dashboard launcher entry, an **OpenRC** service (`/etc/init.d/tgh-server`, `/etc/conf.d/tgh-server`) and a generic systemd user unit. No ebuild (by choice). Files are named one by one: this checkout's `.env` and `data/` hold secrets and are never packed.

Install from the tarball: `./install.sh` (under `/usr/local`), `DESTDIR=… ./install.sh` to stage, `./install.sh uninstall` to remove
what it installed (it records a manifest). Existing files in `/etc` are never overwritten; the new copy is written as `<name>.new`.
Checked: install/uninstall in a DESTDIR, `tgh-server --demo` from the installed binary, no `.env`/`data` in the tarball. The OpenRC script was only syntax-checked, not started.
