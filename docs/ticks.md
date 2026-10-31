# Ticks

[Tickloop](../README.md) · English | [简体中文](zh-CN/ticks.md)

A Tick is a live program the agent can open, watch and act on, such as a market data feed or a document reader. Ticks are installed like plugins and share their marketplaces and storage, but you manage them on their own: open `/ticks` to find, install and log in to them, or use the command line:

```sh
tickloop tick list --available
tickloop tick install NAME                # or NAME@MARKETPLACE
tickloop tick list
tickloop tick login NAME                  # when the data source requires login
tickloop tick logout NAME
tickloop tick update NAME                 # omit NAME to update all Ticks
tickloop tick disable NAME
tickloop tick enable NAME
tickloop tick uninstall NAME              # deletes its data; keeps saved login
tickloop tick uninstall NAME --keep-data  # keeps its data too
tickloop tick install --path ./my-tick    # a local Tick directory or .tar.gz archive
```

Official Ticks come from the built-in `tickloop` marketplace; their sources live in [tickloopai/ticks](https://github.com/tickloopai/ticks). A plugin that carries a Tick program is a Tick, even when it also brings Skills: once installed it appears only in `/ticks` and `tickloop tick`, never in `/plugins`. When the kind is known before installing (a Tick marketplace package, or a local directory), `tickloop plugin` refuses a Tick and names the `tickloop tick` command to use instead, and the other way round.

To write a Tick, see [Writing Ticks](writing-ticks.md).

## The /ticks panel

In `/ticks`, `Tab` or left/right switches between **Installed** and **Available**. Use `Ctrl+N`/`Ctrl+P` or up/down to select. On Installed, `Enter` opens a Tick's actions: log in or out, update, enable or disable. Installed rows show the marketplace, enabled state, version and login state. Login happens in the panel: an OAuth Tick shows a link and code to approve in a browser on any device, and a key is typed masked. On Available, `Enter` installs or updates the selected Tick, and `/` starts a case-insensitive filter across names, descriptions and marketplaces: letters enter text, `Ctrl+N`/`Ctrl+P` select, and `Ctrl+G` opens an editor. `Esc` leaves filtering and keeps the query; another `Esc` closes the panel.

To uninstall in the panel, press `x` twice on the same Installed row within three seconds. This deletes the Tick's data, keeps saved login, and leaves running Ticks running. Moving selection, another key, leaving the panel or waiting cancels confirmation. Saved logins remain under `credentials/NAME/` in your configuration directory; log out before uninstalling to remove one.

Once a Tick is installed (and logged in, if it needs a login), ask the agent to open it by name, for example "open the NAME Tick and watch it". Installation runs nothing: a Tick's program runs as you only when opened under your permission settings. A disabled Tick cannot be opened by name. Installing, updating, removing and logging in are your actions; the agent only asks you to do them.
