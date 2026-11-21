# protonfilter

A desktop app and command-line tool for finding games on
[ProtonDB](https://www.protondb.com) by genre, compatibility tier (native,
platinum, gold, …), number of reports, number of written comments and whether
they need DirectX 12.

ProtonDB doesn't offer a search API, so `protonfilter` builds a local SQLite
database from four public sources and runs every query offline.

| Data | Source |
| --- | --- |
| Every game with reports, report counts, **comment counts** | ProtonDB's monthly data export ([bdefore/protondb-data](https://github.com/bdefore/protondb-data)) |
| Tier (current / trending / best), score, live report total | ProtonDB's per-game summary endpoint (the one its own site uses) |
| Genres and tags, native Linux build, Steam Deck status | Steam store API (`IStoreBrowseService/GetItems`, no API key needed) |
| Graphics APIs: DirectX versions, Vulkan, OpenGL | [PCGamingWiki](https://www.pcgamingwiki.com) game pages, read through its MediaWiki API |

## Install

```bash
cargo install --path .
```

Or build it and run `./target/release/protonfilter`:

```bash
cargo build --release
```

## The window

```bash
protonfilter gui
```

- **Filters** (left): every change updates the list immediately.
  - *Compatibility*: toggle any of Native, Platinum, Gold, Silver, Bronze,
    Borked and Pending, or use the **Gold or better** shortcut. Choose whether
    to rate games by their current, trending or best-ever tier, and optionally
    hide games that have a native Linux build.
  - *Steam Deck*: Verified, Playable, Unsupported, Unknown.
  - *Graphics API*: under Proton, DX12 games run through VKD3D-Proton rather
    than DXVK. **Hide games that need DX12** removes games where DX12 is the
    only option. **Hide every game with DX12** also removes games that offer
    it alongside DX11 or Vulkan. **Only games with Vulkan** keeps games with a
    Vulkan renderer, which skip DirectX translation altogether. Games whose
    graphics APIs aren't known stay in the list unless you tick **Hide games
    with unknown graphics API** (or **Only games with Vulkan**).
  - *Genres & tags*: click a genre once to require it, again to exclude it,
    and a third time to clear it (right-click excludes straight away). Type in
    the box to add any Steam tag, such as Roguelike or Co-op. With two or more
    required tags you can match all or any of them. "Only look at each game's
    top tags" ignores tags that players voted low on a game's list.
  - *Reports* and *Comments*: at-least / at-most sliders.
  - **Reset all** clears the filters. Your filters are remembered next time.
- **Results** (middle): click a column header to sort, click again to reverse.
  Click a game to see its details, and double-click to open its ProtonDB page.
  The API column shows e.g. `DX11/12 + VK`, with DX12-only games in orange.
- **Details** (right): all three tiers, score, counts, graphics APIs, links to
  ProtonDB, Steam and PCGamingWiki, and **Refresh** to fetch that one game's
  latest data now. Click any of its tags to filter by it. Selecting a game the
  sync hasn't looked up on PCGamingWiki yet looks it up on the spot.
- **Sync** (top right) runs the same sync as `protonfilter sync`, with a
  progress bar. **Stop** keeps everything fetched so far, and the list updates
  as new ratings come in.

## First run

Either click **Sync** in the window, or run:

```bash
protonfilter sync
```

This downloads the latest ProtonDB export (about 70 MB, deleted once it's
imported), fetches Steam data in batches of 200, then fetches one ProtonDB
rating per game. **ProtonDB blocks clients for about a minute if they go over
roughly 1000 requests a minute**, so ratings are fetched at 12 per second by
default. That means a full first sync of about 35,000 games takes **about
50 minutes**. Games with the most reports are fetched first, and progress is
saved as it goes, so you can stop it with Ctrl-C and start searching right
away. The next `sync` picks up where it stopped.

The last step looks every game up on PCGamingWiki to find out which graphics
APIs it uses. The wiki is run by volunteers, so this goes at one request a
second. Games are looked up by Steam name, 50 per request, and well-reported
games the names miss (`--pcgw-min-reports`, default 10) are then looked up by
app id, one request each. The first time, this takes **about 50 minutes**.
After that, a sync only looks up new games and re-checks the rest every 90
days.

For a quicker start, only rate games that have some reports:

```bash
protonfilter sync --min-reports 10
```

After that, run `sync` once a month or so. It only downloads a new export when
one has been published, and it only re-checks ratings that are older than 30
days (`--stale-days`) or whose game got new reports in the latest export.

## Searching

```bash
# RPGs rated platinum or better (native counts as better), with 100+ reports
protonfilter search -g rpg --min-tier platinum -r 100

# Native Linux strategy games, sorted by comment count
protonfilter search -t native -g strategy -s comments

# Gold or platinum through Proton only, Deck verified, 50+ written comments
protonfilter search -t gold,platinum --exclude-native -d verified -c 50

# Several genres: must match all of them (add --any-genre to match any)
protonfilter search -g "Open World,Survival" -x "Early Access"

# Shooters whose recent reports are platinum, whatever their overall tier
protonfilter search -g FPS -t platinum --tier-source trending

# Gold or better, without games that need DirectX 12
protonfilter search -m gold --dx12 not-required

# Games with a Vulkan renderer
protonfilter search --vulkan
```

`filter` is an alias for `search`.

| Option | Meaning |
| --- | --- |
| `-g, --genre` | Genre or any Steam tag (`RPG`, `Roguelike`, `Metroidvania`, …). Case-insensitive. Repeat or comma-separate; games must match **all** of them. |
| `--any-genre` | Match **any** of the `--genre` values instead. |
| `--top-tags N` | Only match genres among each game's N most-voted tags. Steam tags are user-voted, so a shooter can carry "Strategy" as its 13th tag; `--top-tags 5` filters that out. |
| `-x, --exclude-genre` | Drop games with this genre or tag. |
| `-t, --tier` | One or more of `native platinum gold silver bronze borked pending`. A game matches `native` if it has a Linux build, and a Proton tier if ProtonDB rates it so. A native game can match both. |
| `-m, --min-tier` | This tier or better. Order: native > platinum > gold > silver > bronze > borked > pending. |
| `--tier-source` | Which ProtonDB tier to filter and sort on: `current` (default, what the game page shows), `trending` (recent reports), or `best` (best ever reported). |
| `--exclude-native` | Only games that need Proton. |
| `--dx12` | `any` (default); `not-required` hides games where DirectX 12 is the only option; `exclude` hides every game that supports DX12. Games with unknown graphics APIs are kept. |
| `--hide-unknown-api` | Also drop games whose graphics APIs aren't known. |
| `--vulkan` | Only games with a Vulkan renderer. Some start in DirectX and need a setting or launch option to switch. |
| `-r, --min-reports` / `--max-reports` | Report count (live from ProtonDB). |
| `-c, --min-comments` / `--max-comments` | Reports with written notes (from the monthly export, so up to a month old). |
| `-d, --deck` | Steam Deck status: `verified playable unsupported unknown`. |
| `-n, --name` | Name contains this text. |
| `-s, --sort` | `reports` (default), `comments`, `score`, `tier`, `name`, `recent` (latest report). |
| `-o, --order` | `asc` or `desc`. |
| `-l, --limit` | Rows to show, default 50. `0` shows all. |
| `-f, --format` | `table` (default), `json`, or `csv`. |

### Reports and comments

On ProtonDB every report is a compatibility verdict, and most also include
written notes. `reports` is the total ProtonDB shows on the game's page.
`comments` counts only the reports with non-empty notes, which tells you more
about how much real troubleshooting advice there is for a game. It comes from
the monthly export, so very recent reports aren't counted until the next one.

## Other commands

```bash
protonfilter gui               # the desktop window
protonfilter genres            # Steam's main genres with game counts
protonfilter genres --all      # every tag
protonfilter genres -s rogue   # tags containing "rogue"
protonfilter show 1245620      # one game, with fresh data fetched for it
protonfilter show "elden ring" # look it up by name
protonfilter status            # what's in the database
```

## Files

- Database: `~/.local/share/protonfilter/protonfilter.db` (override with `--db PATH`)
- Window state and saved filters: `~/.local/share/protonfilter/app.ron`
- Export download: `~/.cache/protonfilter/` (removed after import)

## Sync options

| Option | Default | Meaning |
| --- | --- | --- |
| `--min-reports N` | 0 | Only fetch ratings and Steam data for games with at least N reports. |
| `--rate N` | 12 | ProtonDB requests per second. If ProtonDB starts refusing requests, every worker pauses for 45 seconds and then carries on. |
| `-c, --concurrency N` | 4 | Parallel ProtonDB requests. |
| `--stale-days N` | 30 | Re-check ratings and Steam data older than this. |
| `--limit N` | – | Only process the N most-reported games per step. |
| `--dump PATH` | – | Import a local export (`.tar.gz` or the `.json` inside it). |
| `--pcgw-min-reports N` | 10 | Look up games that PCGamingWiki doesn't have under their Steam name by app id too, if they have at least N reports. |
| `--skip-dump`, `--skip-steam`, `--skip-protondb`, `--skip-pcgw` | – | Skip a step. |
| `--force-dump` | – | Re-import the latest export even if it was already imported. |

## Caveats

- These are unofficial endpoints, and ProtonDB or Steam could change them.
- Genres come from Steam's user-voted tags (the top 20 per game), not from the
  developer-chosen genre list, which is only available one game at a time.
- Games that were removed from Steam or are region locked show `?` for native
  and have no tags. ProtonDB tiers and counts still work for them.
- Graphics APIs come from a community wiki, so they're missing for many
  smaller games and can lag behind patches. Some games that offer both DX11
  and DX12 start in DX12 and need a setting or launch option (often `-dx11`)
  to switch.
- The ProtonDB export covers games with public reports. A game whose first
  report arrived after the latest export won't be listed until the next
  export, but `protonfilter show <appid>` adds it right away.

## Development

```bash
cargo test
```

### Running your changes

`scripts/launch.sh` runs `cargo build --release` and then opens the window.
When nothing has changed, the build check adds about 0.1 s. Any other
arguments go to the CLI instead, for example `scripts/launch.sh status`.

If a build takes longer than a moment, a notification says it's rebuilding.
If the build fails, the last good build opens and a notification points to
`target/launch-build.log`.

The desktop shortcut runs this script, so after an edit you only need to
click it. To make the shortcut again (for example after moving the project):

```bash
cat > ~/Desktop/ProtonFilter.desktop <<EOF
[Desktop Entry]
Type=Application
Name=ProtonFilter
Exec=$PWD/scripts/launch.sh
Path=$PWD
Icon=$PWD/assets/protonfilter.svg
Terminal=false
Categories=Game;
StartupWMClass=protonfilter
EOF
chmod +x ~/Desktop/ProtonFilter.desktop
# On Wayland the taskbar finds the icon through a .desktop file named after
# the window's app id. NoDisplay keeps it out of the app menu.
sed 's/^StartupWMClass=.*/&\nNoDisplay=true/' ~/Desktop/ProtonFilter.desktop \
  > ~/.local/share/applications/protonfilter.desktop
```

The shortcut points straight at the icon file rather than an icon theme name.
Plasma only scans icon theme folders that existed when it started, so an icon
installed into a new theme folder stays blank until you log out and back in.

The icon is `assets/protonfilter.svg`. The window icon built into the binary
is a PNG made from it; after editing the SVG, regenerate it with
`ksvgtopng 256 256 assets/protonfilter.svg assets/protonfilter-256.png`.

Debug builds can screenshot the window without anyone clicking through it.
`PROTONFILTER_CAPTURE` is the PNG to write, and `PROTONFILTER_CAPTURE_STATE`
is optional JSON with `filters`, `select` (an app id), `tag_query`, `dark`,
`width`, `height`, `refresh` and `sync`:

```bash
PROTONFILTER_CAPTURE=/tmp/shot.png PROTONFILTER_CAPTURE_STATE='{"filters": {"tiers": ["gold"], "include": ["RPG"]}, "select": 292030}' cargo run -- gui
```

## License

MIT. See [LICENSE](LICENSE).
