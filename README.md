# YTM Player

A lightweight Windows desktop app for [YouTube Music](https://music.youtube.com), built with
[Tauri 2](https://tauri.app). It shows the real YouTube Music site in a native window and adds a
few things the website doesn't have.

## Features

- **Stats sidebar**: what's playing, songs and listening time today (and all time), RAM usage,
  and your recent listening history. Toggle it with `Ctrl+D`.
- **10-band equalizer**: presets or your own curve, with automatic headroom so boosts don't clip.
- **Liked songs**: synced from YouTube Music, searchable, and playable from the sidebar.
- **Bans**: ban a song or an artist and it gets skipped whenever YouTube Music picks it.
- **Global shortcuts**: play/pause, next and previous work even when the app isn't focused.
  You can change them in Settings.
- **Tray icon and taskbar buttons**: ⏮ ⏯ ⏭ in the taskbar thumbnail preview.
- **Picks up where you left off**: the song and position are restored after a restart.
- **Memory watchdog**: reloads the page when memory gets high, waiting for the next track
  if music is playing.

Your stats, likes, bans and settings stay on your computer, in `%APPDATA%\com.merhmerh.ytmplayer`.

## Building it yourself

No prebuilt installers are published, so you build it from source. It's Windows only.

### Requirements

- [Node.js](https://nodejs.org) 18 or newer
- [Rust](https://rustup.rs) (stable)
- Microsoft C++ Build Tools and WebView2. See Tauri's
  [Windows prerequisites](https://tauri.app/start/prerequisites/#windows). WebView2 is already
  included in Windows 10 and 11.

### Steps

```bash
git clone https://github.com/merhmerh/ytm_player.git
cd ytm_player
npm install
npm run build
```

The installer ends up in `src-tauri/target/release/bundle/nsis/`. Run the `*-setup.exe` file there.
It installs for your user only, so it doesn't need admin rights.

The first build takes a while because it compiles every Rust dependency. Later builds are faster.

### Other commands

| Command | What it does |
| --- | --- |
| `npm run dev` | Runs a development build. Quit the installed app first, since only one copy can run at a time. |
| `npm run build` | Builds the release installer. |
| `npm run update` | Builds, then runs the new installer, which closes, updates and reopens the app. |

## Project layout

```
src-tauri/src/main.rs      App window, tray, shortcuts, watchdog, commands
src-tauri/src/stats.rs     SQLite database for plays, likes and bans
src-tauri/src/inject.js    Script injected into music.youtube.com (player control, equalizer, bans)
src-tauri/src/thumbbar.rs  Windows taskbar thumbnail buttons
ui/panel.html              Stats sidebar
ui/settings.html           Settings window
```

## Disclaimer

This is an unofficial personal project. It isn't affiliated with or endorsed by YouTube or Google.
