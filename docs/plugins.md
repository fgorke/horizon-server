# Plugins

Horizon Server runs plugins: WebAssembly components a server loads from `plugins/` beside its
world folder. Anyone can write one, in Rust with `horizon-mod-sdk` or in any language that targets
the WIT world `horizon-server-mod` (`crates/horizon-mod-sdk/wit/horizon.wit`).

A plugin is a server mod (`docs/mods.md`, "Server mods"): the same manifest, the same SDK and the
same `cargo horizon-mod` tooling. Horizon Server runs every one it finds in `plugins/`; there is no
switch to turn one off but taking its file out.

## Official plugins

Each release carries them, signed with the server (`horizon-plugin-<id>.hmod` among the
release's files); their source is in [`plugins/`](../plugins).

| Plugin | What |
| --- | --- |
| [`spawn`](../plugins/spawn/src/lib.rs) | `/setspawn` (operators) sets the spawn spot where you stand, looking where you look; `/spawn` (everyone) takes you there after 5 seconds standing still, the seconds left in the action bar; moving cancels it |

## Install one

1. Make a folder for it in `plugins/` next to the world folder (the server makes `plugins/` on its
   first start), and put its `.hmod` file in there: `plugins/welcome/welcome.hmod`.
2. Restart the server. The log says `Plugin <name> <version> loaded`, or why it did not load.

One plugin per folder. A `.hmod` loose in `plugins/`, or a second one in a plugin's folder, is not
loaded and the log says so.

A plugin that traps, runs out of its time budget or its memory is stopped (`Plugin <name> stopped:
<reason>` in the log) and the server goes on without it.

## Where a plugin keeps its data

Each plugin keeps its data in `data/` of its own folder, `plugins/<name>/data/`, made the first
time it saves something. Nothing is ever loaded from `data/`.
It is the only folder a plugin writes files to: never the world folder, the server's settings or
another plugin's folder (blocks it sets with capability `world` are saved with the world, as any
block is). Deleting `plugins/<name>/data/` resets a plugin; deleting `plugins/<name>/` removes it.

```
server/
├── Winterprojekt/          the world (plugins never write here)
└── plugins/
    └── welcome/
        ├── welcome.hmod    the plugin
        └── data/           its storage
```

## The sandbox

Each plugin runs in its own wasmtime instance: no files, no network, no clock, no threads. What it
may do is what its manifest's `capabilities` list, and the server refuses to load a plugin that
imports an interface its capabilities don't cover.

| Capability | Interface | What |
| --- | --- | --- |
| always | `log` | Lines in the server's log |
| `storage` | `storage` | A key-value store of its own (4 MiB), kept across restarts in `plugins/<id>/data/` |
| `players` | `players` | Who is online (id, name, position, dimension, game mode, health); `rotation(player)`, `permission(player)`; `tell`, `broadcast`, `action-bar(player, text)` (with `§` colour codes); `teleport(player, dimension, x, y, z, yaw, pitch)` |
| `world` | `world` | Blocks and biomes of the loaded chunks; `set-blocks`, made as `/setblock` makes them |
| `command` | `command` | `run(line)`, `run-quiet(lines)`: vanilla commands, run as the server console (permission level 4) at the tick's end |

`command` is the strongest capability: a plugin with it can do whatever an operator can. Install
one with it only if you trust its author.

Network access (HTTP, UDP) is planned as capabilities of their own, each limited to the hosts and
ports the manifest names.

## What a plugin hears

| Hook | When |
| --- | --- |
| `init` | Once, after loading: the commands it adds and the hooks it wants |
| `tick` | Each server tick |
| `player-joined`, `player-left` | A player joins or leaves |
| `chat` | A player sends a chat message |
| `command` | A player types one of its commands (any player; the plugin checks what it needs) |
| `block-broken`, `block-placed` | A player breaks or places a block |

Its commands appear in every player's command tree, so the chat box completes them like vanilla's.
A plugin cannot take a vanilla command's name or one of Horizon Server's own (`spawn`, `setspawn`,
`spawnelytra`, `dimension`, `tps`, `list`).

## Write one

```sh
rustup target add wasm32-unknown-unknown
cargo install --git https://github.com/fgorke/horizon-server cargo-horizon-mod
cargo horizon-mod new my-plugin
```

Set `side = "server"` and `api = "0.3"` in `horizon-mod.toml`, implement
`horizon_mod_sdk::server::ServerMod`, and export it with `export_server_mod!`. Start from the
example [`welcome`](../crates/horizon-mod-sdk/examples/welcome/src/lib.rs) (greetings, `/online`,
`/mined`) or [`beacons`](../crates/horizon-mod-sdk/examples/beacons/src/lib.rs) (capability
`world`).

```sh
cargo horizon-mod build                              # target/horizon-mod/<id>.hmod
cargo horizon-mod test                               # tests/*.toml, no server needed
cargo horizon-mod check target/horizon-mod/my_plugin.hmod
mkdir -p /path/to/server/plugins/my-plugin && cp target/horizon-mod/my_plugin.hmod "$_"
```

`check` plays the plugin in the sandbox for a while and refuses one that traps, overruns or imports
more than its capabilities allow: run it before you share a plugin.

## Building the server with plugins

Release builds have plugins on. Building from source, add the feature:

```sh
cargo build --release --locked -p horizon-client --no-default-features --features plugins --bin horizon-client
```

Without it the server still starts, and logs a warning if `plugins/` holds any.

## Roadmap

- Spawn elytra, the stats page and voice chat move out of the server into plugins of their own,
  one by one, as `spawn` did.
- Capabilities for HTTP and UDP, limited to the hosts and ports a manifest names.
- Later, in a repository of its own: a bridge that runs Bukkit plugins.
