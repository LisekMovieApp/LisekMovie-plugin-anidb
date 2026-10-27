# LisekMovie-plugin-anidb

aniDB metadata provider extension for [LisekMovie](https://github.com/LisekMovieApp/LisekMovie-app), the self-hosted video-on-demand server.

| | |
|---|---|
| Provider key | `anidb` |
| Covers | Anime |
| Status | Basic — works, parts still missing |
| API key | Required — a registered client (https://anidb.net/software/add), entered as `client` or `client:version` |
| Fix match ids | `anidb:69`, `https://anidb.net/anime/69`, or `69` with aniDB picked |

## What it does

aniDB HTTP API (gzip XML): lookup by aniDB id with titles in many languages, synopsis, permanent rating, weighted tags and the main picture. Requests are spaced at least 2.5 s apart process-wide (aniDB bans faster clients).

**TODO:** title search. The HTTP API has none; it needs a local index of aniDB's daily `anime-titles.xml.gz` dump. Until then automatic matching and title search return nothing (a warning is logged once).

## How it plugs in

The crate implements `lisek_plugin_api::MetadataProvider` and is compiled into `lisek-server` as a Cargo path dependency — there is no runtime plugin loading. Clone it **next to** `LisekMovie-app`:

```
LisekMovie/
  LisekMovie-app/
  LisekMovie-plugin-anidb/
  ...
```

The server lists the provider under **Admin → Plugins** (disabled by default). Turn it on, add the key if it needs one, and set its priority — globally, or per library (for example AniList or aniDB first in an anime library). `install-linux.sh` and `update-linux.sh` clone and update it with the other modules.

## Development

```bash
cargo clippy --all-targets -- -D warnings
cargo test
```

Tests run against a local `wiremock` server; nothing calls the real service.

## License

[MIT](LICENSE), like LisekMovie itself.
