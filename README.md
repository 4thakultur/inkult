# Inkult

**A self-hosted music downloader and library manager for DJs and music
collectors.**

Search YouTube Music and SoundCloud from your browser, dig through other
collectors' shares on Soulseek, listen before you download, and get the best
quality each source offers as properly tagged files with cover art, filed into
a clean library. One server, several users, a single shared music folder.

- **Best available quality, honestly labelled.** Inkult inspects every format
  a source offers, picks the best one and never re-encodes unless you ask.
  Quality is measured from the file you actually got, and a lossy source is
  never called lossless.
- **Metadata that DJ software can use.** Title, artists, album, label,
  release date, ℗ line, genre, source URL and embedded artwork, normalized
  across providers. The comment tag is left alone for your cue notes.
- **A library you'd organize yourself.** `Artist/Album/03 - Title.ext`, no
  duplicates, and existing files are never overwritten.
- **Soulseek built in.** A native client: search, browse, queue whole
  folders, share your library. No other client needed. **Quality upgrade**
  finds a lossless copy of a track you got from search and adds it next to it.
- **Made to be exposed to the internet**, behind your reverse proxy:
  mandatory two-factor login, account locking, per-client limits, and
  admin-only server settings.

A single Rust binary with an embedded web UI (green-phosphor terminal look,
with an AMOLED theme and colour variants), usable on desktop, tablet and
phone.

## Contents

- [Quick start](#quick-start)
- [Features](#features)
- [Audio quality and formats](#audio-quality-and-formats)
- [Soulseek](#soulseek)
- [Messages](#messages)
- [Accounts and two-factor login](#accounts-and-two-factor-login)
- [Installation](#installation)
- [Configuration](#configuration)
- [Security](#security)
- [Development](#development)
- [Roadmap](#roadmap)
- [Disclaimer](#disclaimer)
- [How Inkult was made](#how-inkult-was-made)
- [License](#license)

## Quick start

Requires Docker with Compose v2. The image bundles `yt-dlp`, `ffmpeg` and
`deno` and builds for `amd64` and `arm64` (e.g. a Raspberry Pi 4/5).

```sh
git clone https://github.com/<owner>/inkult.git && cd inkult
cp .env.example .env        # optional: user, paths, port, settings
mkdir -p music data         # or the MUSIC_DIR / DATA_DIR from .env
docker compose up -d --build
docker compose logs inkult  # shows the one-time setup code
```

Open <http://localhost:9876>, enter the setup code from the log with a handle
and password to become the first **captain** (administrator), and set up
two-factor login with any authenticator app. Then add the rest of your crew
from **[7] ADMIN**.

Before opening Inkult to other machines, read
[Exposing Inkult](#exposing-inkult): it speaks plain HTTP and belongs behind
an HTTPS reverse proxy.

## Features

### Search and discovery

- **Search** YouTube Music (official audio tracks, with artist, album,
  duration and cover) and SoundCloud, or paste a track, playlist, album or set
  link.
- **Filters** (**[+] FILTERS** under the search bar): release year range,
  record label, words to exclude (`remix live "sped up"`) and track length.
  Words match whole words in any case. Filters are strict: tracks whose year
  or label is unknown are left out and counted, with a button to show them.
  - *YouTube Music*: release year and label from each track's release info
    (the `℗` line, which names the label rather than the distributor). This
    costs one small request per result, made only when a year or label
    filter is set.
  - *SoundCloud*: only the upload year is known (shown as `up. 2017`); a
    label matches when the tags, uploader or description name it.
  - With only a label and no search words, the label itself is searched for.
  - Filters apply to searches, not to pasted links.
- **Artist pages.** Artist names in results open the artist's page inside the
  search area: biography, a link to the original page, and every track they
  posted (up to `INKULT_PLAYLIST_LIMIT`).
  - *YouTube Music*: the artist's page and its full song list. For a channel
    that is not a music artist, its uploads instead.
  - *SoundCloud*: the uploader's tracks plus their reposts, interleaved by
    date as on the profile's "All" tab and marked `↻ REPOST`. **[x] REPOSTS**
    toggles reposts (remembered per browser, on by default). When the
    uploader is not a credited artist (a label, an edit, a repost), the track
    shows "via <uploader>".
- **Tracks, not mixes.** Anything longer than 10:00 is almost always a DJ
  mix, live set or full album in one file, so it is hidden from search
  results, artist pages, playlists and Soulseek results (with a count of what
  was hidden) and refused if downloaded anyway. Tracks without a listed
  length are checked against their metadata, or the received file, before
  reaching the library. Captains can change or disable the limit in
  **[7] ADMIN**.

### Listening

Every track in search results, collections, the download queue and the
library has a play button on its artwork. The player bar has play/pause,
volume/mute (remembered per browser) and a SoundCloud-style **waveform**
measured from the decoded audio, which fills in from the left while you can
already seek anywhere. Space toggles playback, and the player can be folded
away. It always says where the audio comes from and its quality, e.g.
"Streaming from YouTube · Opus · 48 kHz · 106 kbps" or "From your library".

Listening never writes to disk and never re-encodes lossy audio:

- progressive streams (YouTube) are proxied through Inkult as-is, with
  seeking;
- HLS streams (SoundCloud) are fetched once into memory (a few MB per track,
  the last four kept) and re-wrapped without re-encoding, so seeking is
  instant;
- library AIFF/ALAC files, which most browsers cannot decode, are converted
  losslessly to FLAC on the fly.

Tracks you already downloaded play from the library file.

### Downloads and library

- **Download queue** with live progress, a concurrency limit, retries with
  backoff for transient errors, cancellation, and clear error messages.
- **Organized library** with safe, portable filenames:
  - `Artist/Album/03 - Title.opus`
  - `Artist/Singles/Title.m4a`
  - `Various Artists/Album/07 - Artist - Title.mp3`
- **No duplicates, no overwrites.** Tracks are identified by provider ID, so a
  track already in the library is not downloaded again. Existing files are
  never replaced: name collisions get a ` (2)` suffix.
- **One library, separate views.** All users share one music directory, so a
  track is stored once however many people want it. Requesting a track that
  is already on disk adds it to your library instantly; if someone is
  downloading it right now, you join their download. Users only see the
  tracks they downloaded; captains see everything and who owns what.
- **Formats side by side.** The same track can exist in several formats;
  each library row has a format picker that decides which file it plays,
  saves, shares or converts.
- **Save to device**: send any library file to the browser (phone, laptop)
  with its proper filename.
- **Delete** a track from your library. The file is only removed from disk
  once no other user owns it; a captain can delete it for everyone. Empty
  artist and album folders are cleaned up.

### Metadata

Each file gets title, artists, album, album artist, track number, release
date, genre, label, copyright (℗), source URL and an embedded front cover.
Each file also stores `PURL` (source URL) and `INKULT_TRACK_KEY` (e.g.
`youtube:lXc8EHmFXOM`), so its origin is always traceable.

Precedence, highest first:

1. Structured provider metadata (YouTube Music track/artist/album fields,
   SoundCloud publisher metadata).
2. Data parsed from YouTube's auto-generated "Provided to YouTube by …"
   description: label, ℗ line, release date and the full artist list.
3. Inferred: "Artist - Title" parsed from the upload title, then the
   uploader name. Inferred fields are flagged in the UI.

Video noise such as "(Official Video)" is stripped; mix names such as
"(Extended Mix)" are always kept. The upload description goes into the
description tag, never the comment tag, which DJ software uses for cue notes.

### Themes and devices

**[6] SETTINGS** switches between *Pipeboy* (the original green phosphor
look), *AMOLED* (pure black background) and the AMOLED colour variants
*White*, *Blue*, *Purple*, *Orange* and *Red*, remembered per browser. On
phones the menu becomes a slide-in drawer, dialogs become bottom sheets, and
the chat fits the screen without page scrolling.

## Audio quality and formats

Downloads are never transcoded unless you pick a converting format, so the
default output quality equals the source quality. Format selection, in
order:

1. Never pick YouTube's dynamic-range-compressed (`-drc`) variants.
2. Lossless (FLAC/ALAC/WAV/AIFF) beats any lossy format, e.g. a SoundCloud
   uploader's "original file" download.
3. Lossy formats are compared by codec-weighted bitrate, not raw bitrate:
   Opus 160k beats AAC 128k, and AAC 256k beats Opus 160k.
4. Audio-only streams are preferred over muxed video.

After download, quality is **measured from the file** and shown as *Source*
(what the provider advertised) vs *File* (what you got). A file is only
marked lossless when the delivered codec is lossless; Inkult cannot know
whether an uploader made a lossless file from a lossy master.

Typical results: YouTube Music gives Opus ~130–160 kbps / 48 kHz. SoundCloud
gives AAC 160k, MP3 128k, or the lossless original when the uploader enabled
downloads.

### Choosing a format

Clicking download opens a format menu; Inkult remembers your last choice.

| Format | Re-encoded? | Notes |
|---|---|---|
| Best quality (default) | No | The best source stream, exactly as delivered (usually Opus or AAC) |
| AAC (M4A) | No | Best AAC stream the source offers. Avoids Opus with no extra loss. YouTube's AAC is ~128 kbps |
| MP3 320 kbps | Yes | For players or DJ gear that can't read Opus. **Not better sound**: a 320 kbps MP3 made from a ~135 kbps stream only adds size |
| AIFF | Decoded | Uncompressed for CDJs/Rekordbox. No further loss beyond the source, ~10 MB/min. Not marked lossless unless the source was |

Conversions always start from the best source stream, and the file records
what it was converted from ("converted from …" in the UI). Only the same track
*in the same format* counts as a duplicate.

### Converting library tracks

**[CONV]** on a library track converts it without downloading it again. The
menu lists every format and greys out impossible ones, with the reason:

- **MP3 320** and **AIFF** can be made from any library file except one
  already in that codec (re-encoding MP3 to MP3 only loses quality).
- **Best quality** and **AAC (no re-encoding)** mean the provider's own
  stream, which no local file can be turned back into; download them instead.
- If the format is already on disk (someone else converted or downloaded it),
  it is added to your library instantly.

A conversion always starts from the best copy on disk, so an AIFF is never
made from an MP3 when the Opus original is there. Tags, artwork and anything
you edited in your DJ software (comments, BPM, key) are copied from the
source file, and the result records its history, e.g. "converted from MP3 ·
320 kbps ← Opus · 48 kHz · 135 kbps". The new file is written next to the
source; the source is never modified. Conversions run in the download queue
and can be cancelled there.

### Quality upgrade

For tracks downloaded from search (YouTube, SoundCloud), **[CONV] → Quality
upgrade** looks for a lossless copy on Soulseek and adds it to the library as
another format of the same track (**Lossless**, listed first in the row's
format menu). It needs you to be signed in to Soulseek, and uses your account.

**[GET] → Quality upgrade** on a search result does the same without
downloading the YouTube/SoundCloud stream first: Inkult looks the track up at
its source (title, artists, length, artwork, release details; no audio), then
searches Soulseek. The file is tagged from the source's metadata and cover
(the shared file's own tags fill whatever the source lacks) and filed under
that track, so a later download of the stream joins it as another format. If
you already have the track, it is tagged like your copy instead.

1. Inkult searches Soulseek for the artist and title (without "Official
   Video" and featured artists) and collects replies for 10–30 seconds.
2. Only FLAC, WAV and AIFF files count. A file must contain every word of the
   title and of one artist in its path, must not name another version
   ("Remix", "Live", "Extended Mix"…; "Original Mix" is fine), and, when the
   user's client reports a length, must be within a few seconds of yours.
3. The best match is downloaded: confirmed length first, then a free upload
   slot, then bit depth and sample rate, then FLAC over AIFF over WAV, then the
   shorter queue and faster user. If it fails, is refused, or has not started
   after 10 minutes, the next one is tried (up to 5).
4. The received file must really be lossless (codec *and* [spectrum
   check](#spectrum-check)) and as long as your copy, or it is rejected and
   the next match is tried. It is tagged like your copy (your
   edits, artwork and Inkult's identity carry over); its own tags only fill
   gaps such as album, track number or ISRC.

The upgrade runs in **Downloads**, and its transfer also shows in the
Soulseek tab. The file is written next to your copy, which is never touched,
and records where it came from ("via Soulseek"). Conversions of the track then
start from the lossless copy.

### Spectrum check

A FLAC made from an MP3 is the size of a FLAC and the quality of the MP3.
Lossy encoders remove the top of the spectrum with a steep cut (MP3 128
kbps at about 16–17 kHz, 192 at about 19 kHz, Opus and AAC near 20 kHz), and
decoding keeps that hole. Inkult decodes every lossless file it receives
from Soulseek or downloads as a lossless original (e.g. a SoundCloud WAV),
averages its spectrum over the whole track and looks for that cliff:

| Verdict | Shown as | Meaning |
|---|---|---|
| Full band | `spectrum ok` | Content up to the top of the range; no sign of lossy encoding. |
| Lossy | `fake lossless` | A cliff below 20.3 kHz: made from a lossy file. Not marked lossless; ranked as the lossy file it was (so it is not preferred as a conversion source). Quality upgrades reject it and try the next copy. |
| Upsampled | `upsampled` | A 88.2 kHz+ file with nothing above CD range: not real high resolution. Still lossless. |
| No verdict | `spectrum ?` | Too quiet or too dull at the top to tell (old recordings, some ambient). |

It is a heuristic, like checking a spectrogram by eye: MP3 at 320 kbps and
encoders that keep the full band (e.g. ffmpeg's own AAC at 256 kbps) are not
caught, and a track low-passed on purpose for its whole length would look
lossy. To check files by hand: `inkult spectrum <file>…`.

## Soulseek

Soulseek is a peer-to-peer network where people share their own music
collections: the place for out-of-print records, white labels, vinyl rips and
lossless versions nobody streams. Inkult speaks the protocol itself.

**Signing in.** Each Inkult user signs in with their own Soulseek account in
**[2] SOULSEEK**. There is no separate registration: a username that does not
exist yet is created with the password you enter, and Soulseek has no
password reset. *Stay signed in* stores the password in `data/soulseek.json`
(readable by the owner only) so Inkult reconnects after a restart, once you
log in to Inkult again; nothing connects before that. Signing in with the
same account in another client (e.g. Nicotine+) ends the Inkult session, and
Inkult does not fight back by reconnecting.

**Digging.**

- **Search** the network, or one user's shares. Results arrive over a minute
  or so and are grouped by user and folder, with the format each peer reports
  (`FLAC 24-BIT 96 KHZ`, `MP3 320`, `MP3 245 VBR`), size, length, free upload
  slot / queue length and speed. Filter by words (`-word` excludes), format,
  minimum lossy bitrate and free slots; sort by fastest start, quality, speed
  or folder size. Previous searches stay one click away.
- **[GET]** queues a file; **[GET FOLDER]** asks the user for the complete
  folder (including CD1/CD2 subfolders) and queues every audio file plus the
  best cover image, which is embedded into tracks that have no artwork.
- **Browse** a user's whole share as a filterable folder list, with their
  info card: description, picture, speed, shared files, free slots and queue.
- **Messages**: private chat with other users, kept in memory for the
  session.
- **Wishlist**: terms searched again automatically at the pace the server
  allows (about every 12 minutes, one term at a time). New finds collect
  under the term in the search history.

**Transfers.** A user uploads when they have a free slot, so a download first
waits in *their* queue. Inkult shows your place in it, asks again
periodically, and re-sends the request if the user restarted and forgot it.
Unreachable users are retried after 30 s, 1, 5, 15, 30 and 60 minutes before
the download is marked failed (and can be retried by hand). Interrupted
transfers resume where they stopped, and the queue survives restarts.

**Into the library.** A finished file is checked and filed like any other
download:

1. The codec and quality are **measured from the file**, never taken from the
   peer's claims. Files that are not readable audio are rejected. Accepted:
   FLAC, MP3, M4A/AAC/ALAC, Ogg, Opus, WAV, AIFF; playlists, logs and other
   files in a folder are skipped.
2. **Metadata precedence:** the file's own tags first, then what the folder
   and file name suggest (`Artist/Album (2017) [FLAC]/03 - Title.flac`,
   `Artist - Album/CD2/…`, `Artist - Title.mp3`), flagged as *guessed* in the
   UI. Rip descriptions such as `[FLAC]` or `(24-96)` are dropped from album
   names; real ones like `(Deluxe Edition)` are kept.
3. The file is tagged (`PURL` = `slsk://user/path`), placed with the usual
   layout and naming rules, and added to your library. The same file from the
   same user is never downloaded twice.

Every received FLAC, WAV or AIFF gets a [spectrum check](#spectrum-check). A
file found to be made from lossy audio is kept (you chose it) but is not
called lossless, and says why.

**Sharing** (off by default, **SETUP**). When on, the tracks in *your* Inkult
library are shared read-only under a virtual `inkult\Artist\Album\…` folder:
other users can browse them, search them by user search, and download them
through `INKULT_SOULSEEK_UPLOAD_SLOTS` upload slots (one per user at a time,
at most 100 queued files per user). Real paths are never revealed. Many
Soulseek users only serve people who share, so sharing also helps your own
downloads.

**Connectivity.** Each signed-in account listens on its own port from
`INKULT_SOULSEEK_PORTS` (default 2234–2238, so up to five accounts). Forward
those TCP ports on your router so users behind firewalls can reach you;
without it Inkult still works through outgoing connections, but cannot reach
users who are firewalled too. Inkult does not take part in the distributed
search network, so other users' network-wide searches do not find your
shares (browsing and user searches do).

Not included: Soulseek network chat rooms, buddy lists, user bans and speed
limits.

## Messages

**[5] MESSAGES** holds chat rooms for everyone with an account on your Inkult
server, laid out like an IRC client: rooms on the left, the log in the
middle, who is in the room on the right. These are Inkult rooms, not Soulseek
network rooms.

- Anyone can **create** a room (`#name`: 2–32 of `a-z 0-9 . _ -`, case
  insensitive). The creator is its **owner** (`@name`) and can set its topic
  or **delete** it; captains can too, as moderators.
- **Private rooms**: give a password when creating the room. Private rooms
  don't appear in the room list; others join with **[ JOIN PRIVATE ]** by
  name and password. A private room is deleted when its last member leaves.
- **Join** a room to read and talk; joining shows the recent backlog.
- **Share tracks**: **[SEND]** on a library track posts it to one of your
  rooms as a card with its title, artists and file quality. Members can play
  it right away, and **[ + MY LIBRARY ]** adds it to their library instantly,
  with nothing downloaded or re-encoded. Access lasts as long as the message
  stays in the room's history.
- Commands: `/me <action>`, `/topic [text]`, `/join <room>`, `/part`. Lines
  mentioning your handle are highlighted, and unread messages show on the
  tab.
- Limits: 64 rooms per server, 8 owned per account, 1000 characters and 20
  lines per message, 8 messages per 10 s; each room keeps its last 500
  messages.

Rooms are stored in `data/rooms/`, one file per room, readable by the owner
only.

## Accounts and two-factor login

Inkult always requires a login. There are two ranks:

| Rank        | Can |
|-------------|-----|
| **Captain** | everything below, plus manage the crew (add, promote/demote, reset passwords and two-factor login, unlock, delete), change server rules; their own downloads and library by default, with a SHOW switch to every download and the whole library with who owns each track |
| **Pirate**  | search, listen, download, chat; sees only their own downloads and library |

**First start.** With no accounts yet, Inkult prints a one-time setup code to
its log:

```text
WARN inkult::auth: no accounts yet: open the web UI and enter setup code 49e0-6333-3b53 to appoint the first captain
```

Enter it in the web UI with a handle and password to become the first
captain. The code is only valid until that captain exists, so nobody else who
can reach the server can claim it.

**New crew members.** A captain only picks the handle and rank; Inkult
generates a strong temporary password (`k7mq-3xtz-…`, about 99 bits) and
shows it once, to be handed over privately. It expires after 72 hours if
unused. At their first login the new user must choose their own password and
then set up two-factor login (while the server requires it, which by default
it does for everyone) before they can do anything else. **[RESET PW]** works
the same way, so a captain never knows anyone's real password.

**Two-factor login** works with any standard TOTP app: Google Authenticator,
Microsoft Authenticator, Aegis, 2FAS, Authy, 1Password, Bitwarden, KeePassXC…
(RFC 6238: SHA-1, 6 digits, 30 seconds). Scan the QR code (or type the key,
or tap *Open it in your authenticator app* on a phone), confirm with a code
and your password, and save the ten **recovery codes**. Each recovery code
logs you in once if the phone is lost. The server's clock must be right
(within about 30 s; use NTP).

- **New phone?** **[6] SETTINGS → [NEW PHONE]**: scan the new QR code, then
  enter a code from the old app (or a recovery code), a code from the new
  app and your password. The old app stops working only once the new one is
  proven.
- **Lost phone and recovery codes?** A captain uses **[RESET 2FA]** in
  **[7] ADMIN**. If it's the only captain, stop Inkult and run
  `inkult reset-2fa <handle>`.
- **Is this file really lossless?** `inkult spectrum <file>…` prints the
  [spectrum check](#spectrum-check) verdict for each file (needs only ffmpeg).
- **Locked out?** Stop Inkult and run
  `echo 'new password' | inkult passwd <handle>`. If the account was only
  locked by failed logins, `inkult unlock <handle>` lifts the lock and keeps
  the password.

Under Docker, run these as `docker compose run --rm -T inkult <command>`
while the `inkult` service is stopped.

Accounts live in `data/users.json`. Sessions are kept in memory, so
restarting Inkult logs everyone out.

### Server rules

Captains set server-wide rules in **[7] ADMIN → SHIP RULES**. They apply at
once and are saved in `data/settings.json`.

| Rule                  | Default               | What it does |
|-----------------------|-----------------------|--------------|
| Two-factor login      | Required for everyone | `REQUIRED FOR EVERYONE`, `CAPTAINS ONLY` or `OPTIONAL`. Whoever it applies to cannot turn 2FA off, and must set it up right after logging in. |
| Authenticator name    | `Inkult`              | The name authenticator apps show next to the code, e.g. `Inkult (home)`. Only apps set up afterwards show a new name. |
| Longest track         | 10:00                 | Longer uploads are treated as mixes/sets; `0` = no limit. Files already in the library are not touched. |

Relaxing the two-factor rule asks for the captain's password and a code, so
a hijacked captain session cannot quietly switch it off. An unreadable
`settings.json` stops startup instead of being reset to defaults.

## Installation

The full step-by-step guide for Linux, macOS, Windows, Raspberry Pi and
NAS, including HTTPS, backups and troubleshooting, is in
**[INSTALL.md](INSTALL.md)**. A summary follows.

### Docker Compose (recommended)

See [Quick start](#quick-start). **Create the `music` and `data` directories
first**, owned by the user in `PUID`/`PGID` (default `1000:1000`; check yours
with `id`). Otherwise Docker creates them owned by root and Inkult stops at
startup with `INKULT_LIBRARY_DIR: directory /music is not usable: Permission
denied`. Fix with `sudo chown -R 1000:1000 music data` (your IDs).

| Host (`.env`)           | Container | Contents | Back up? |
|-------------------------|-----------|----------|----------|
| `MUSIC_DIR` (`./music`) | `/music`  | Your library | Yes |
| `DATA_DIR` (`./data`)   | `/data`   | Accounts, library index, rooms, Soulseek state, server rules; `staging/` and `cache/` are disposable | Yes (small) |

The library index in `data` ties files to their owners and sources, so back
up both directories together. A NAS share works for `MUSIC_DIR` as long as
the `PUID` user can write to it; keep `DATA_DIR` on local disk.

The compose file drops all Linux capabilities, runs a tiny init to reap
child processes, rotates container logs (3 × 10 MB) and gives running
downloads 30 seconds to cancel cleanly on `docker compose down`.

### Exposing Inkult

By default the web UI is published on `127.0.0.1` only. Inkult speaks plain
HTTP, so passwords and session cookies must not cross a network
unencrypted. Either:

- put it behind an HTTPS reverse proxy (Caddy, Traefik, Nginx Proxy Manager,
  …) and set `INKULT_ALLOWED_HOSTS` to the domain name you use. The proxy
  must pass the `Host` header through and set `X-Forwarded-Proto` (all
  common proxies do). A minimal Caddyfile:

  ```text
  music.example.com {
      reverse_proxy 127.0.0.1:9876
  }
  ```

  If your proxy runs in Docker, attach both containers to a shared network
  and proxy to `inkult:9876`; the published port is then not needed.

  Also set **`INKULT_TRUSTED_PROXIES`** to the proxy's address as Inkult sees
  it. Sign-in and connection limits are per client, and behind a proxy every
  connection comes from the proxy; only a listed proxy may name the real
  client (`X-Forwarded-For`). Until it is listed, Inkult logs a warning
  naming the address to add (e.g. `172.18.0.1` for Docker).
- or, on a home network you trust, set `INKULT_HTTP_BIND=0.0.0.0` and open
  `http://<server-ip>:9876`.

**Soulseek ports** (TCP 2234–2238) are published on all interfaces and should
be forwarded on your router (see [Soulseek](#soulseek)). Host and container
port numbers must stay identical.

### Updating

```sh
git pull
docker compose up -d --build
```

Rebuilds reuse the compile cache, so only Inkult itself is recompiled.
YouTube changes often: when downloads start failing before a new Inkult
release bumps it, rebuild with a newer yt-dlp by passing its version and the
checksums from that release's `SHA2-256SUMS` file:

```sh
docker compose build \
  --build-arg YTDLP_VERSION=<version> \
  --build-arg YTDLP_SHA256_AMD64=<sha256 of yt-dlp_linux> \
  --build-arg YTDLP_SHA256_ARM64=<sha256 of yt-dlp_linux_aarch64>
docker compose up -d
```

The download queue is not persisted yet: downloads interrupted by a restart
must be queued again (the Soulseek transfer queue does survive restarts).

### From source

Requirements: Rust 1.88+, plus these on your `PATH` (or configured below):

- [`yt-dlp`](https://github.com/yt-dlp/yt-dlp) (a recent release)
- [`ffmpeg`](https://ffmpeg.org)
- [`deno`](https://deno.com), which yt-dlp needs for YouTube

```sh
cargo run --release
# open http://127.0.0.1:9876
```

## Configuration

All settings are environment variables, validated at startup; an invalid
value stops Inkult with a clear error. With Docker, set them in `.env` (see
`.env.example`). Server rules are set in the web UI instead (see
[Server rules](#server-rules)).

| Variable                          | Default          | Description |
|-----------------------------------|------------------|-------------|
| `INKULT_BIND`                     | `127.0.0.1:9876` | Listen address |
| `INKULT_LIBRARY_DIR`              | `./music`        | Music library root |
| `INKULT_DATA_DIR`                 | `./data`         | Library index and internal state |
| `INKULT_STAGING_DIR`              | `<data>/staging` | In-progress downloads |
| `INKULT_MAX_CONCURRENT_DOWNLOADS` | `2` (1–16)       | Parallel downloads |
| `INKULT_MAX_QUEUED_JOBS`          | `1000`           | Cap on unfinished jobs |
| `INKULT_MAX_RETRIES`              | `3` (0–10)       | Retries for transient failures |
| `INKULT_SEARCH_LIMIT`             | `20` (1–50)      | Results per source |
| `INKULT_PLAYLIST_LIMIT`           | `500`            | Max tracks taken from one playlist or artist |
| `INKULT_YTDLP`                    | `yt-dlp`         | yt-dlp executable |
| `INKULT_FFMPEG`                   | `ffmpeg`         | ffmpeg executable |
| `INKULT_ALLOWED_HOSTS`            | (none)           | Domain names Inkult is reached by (see below) |
| `INKULT_TRUSTED_PROXIES`          | (none)           | Reverse proxies allowed to report the client address (IPs or CIDR ranges, comma separated) |
| `INKULT_SOULSEEK_SERVER`          | `server.slsknet.org:2242` | Soulseek server |
| `INKULT_SOULSEEK_PORTS`           | `2234-2238`      | Ports peers connect to, one per signed-in Soulseek account (`off` for none) |
| `INKULT_SOULSEEK_UPLOAD_SLOTS`    | `2` (1–20)       | Users served at once when sharing |
| `INKULT_MAX_TRACK_SECS`           | `600` (0–86400)  | First start only: initial longest track, in seconds |
| `INKULT_TOTP_ISSUER`              | `Inkult`         | First start only: initial authenticator name (no `:`) |
| `RUST_LOG`                        | `inkult=info`    | Log filter |

When you reach Inkult by a domain name, list it in `INKULT_ALLOWED_HOSTS`
(comma separated, no scheme or port). IP addresses and `localhost` always
work. Requests for any other host name are refused with `421`, which stops
DNS-rebinding attacks, where a malicious website points its own domain at
your Inkult and scripts it from your browser. `*` disables the check.

## Security

Inkult is meant to be reachable from the internet behind an HTTPS reverse
proxy, and treats every provider, peer and visitor as untrusted.

**Sign-in**

- Passwords are hashed with Argon2id. New passwords need at least 12
  characters and must be hard to guess, rated with
  [zxcvbn](https://github.com/dropbox/zxcvbn) (common passwords, dictionary
  words, keyboard patterns, dates and the handle itself are rejected; three
  unrelated words pass easily).
- **Two-factor login** is required for everyone by default. A correct
  password only yields a 5-minute challenge token, never a session. Codes
  are compared in constant time, and each accepted code is recorded before
  the session is issued, so it cannot be replayed, even across a restart.
  Recovery codes have 80 random bits and are stored hashed. Changing 2FA
  requires the password and a code again.
- **Account lock** after 10 failed logins in a row (wrong codes included),
  captains too, until a captain unlocks it. Only someone who enters the right
  password is told the account is locked. Anyone who knows a handle can lock
  that account, so keep at least two captains.
- **Throttling.** Failed logins are slowed down per handle after five
  attempts, and each client address (IPv6: each /64) gets 20 failed sign-ins
  before waiting longer and longer (up to 15 minutes). Each client may hold
  64 connections (1024 in all), must send headers within 15 seconds and
  finish a sign-in request within 20 seconds, so slowloris-style connections
  can't starve the server. When too many sign-ins wait for a password hash,
  new ones get `503` at once.
- **No account enumeration.** Unknown handles, wrong passwords and locked
  accounts all answer `401` with the same message and take the same time.
- **fail2ban.** Every failed sign-in is logged with the client address:
  `WARN inkult::web::client: failed sign-in attempt client=203.0.113.9 path="/api/auth/login" status=401`.
- **Sessions** use a random 256-bit token in an `HttpOnly`,
  `SameSite=Strict` cookie (`Secure` when the proxy reports HTTPS), expire
  after 7 days idle or 30 days at most, and end on password change or
  account deletion.

**Authorization and isolation**

- Admin actions are checked on the server for every request, with the role
  read live, so a demoted captain's open sessions lose access at once.
- Pirates are told nothing about the server itself (directories, tool paths
  or versions); as a last filter, server paths in any JSON response to a
  pirate are replaced by placeholders.
- Library files and streams are only served to their owners (and captains).
  Soulseek sessions, searches, messages and transfers belong to one user and
  are never visible to others, captains included.

**Web**

- Cross-origin write requests are rejected (by `Origin` and
  `Sec-Fetch-Site`) and only configured host names are served (CSRF, DNS
  rebinding). Responses carry `nosniff`, `X-Frame-Options: DENY`, a strict
  Content-Security-Policy and `no-referrer`; API answers are never cached.
  HSTS is left to the reverse proxy.
- Internal errors are logged in full but answered with a generic message.

**Files, tools and peers**

- All provider and peer metadata is sanitized before use in paths: path
  traversal, reserved names, control and bidi characters and overlong names
  are neutralized.
- Only URLs of supported providers are accepted, so arbitrary URLs are never
  fetched. Signed CDN URLs are never logged, and the browser only sees opaque
  stream ids.
- External tools run without a shell, with every untrusted value passed as a
  separate argument after `--`. ffmpeg may only open local files, with only
  the demuxers Inkult needs.
- Soulseek messages have size limits per type, compressed data is inflated
  with an output cap, and all lists and queues are bounded. Received files go
  through a staging directory and must parse as audio before reaching the
  library.
- The Docker image verifies yt-dlp against pinned checksums and pins its
  deno image.

**At rest.** `data/users.json` holds password hashes, TOTP secrets (stored
as-is, since TOTP needs the secret to compute codes) and hashed recovery
codes; `data/soulseek.json` holds Soulseek passwords for *Stay signed in*.
Both are readable by their owner only. Protect the data directory and its
backups accordingly.

## Development

```sh
cargo fmt --check
cargo clippy
cargo test
```

The unit tests need no network. `cargo test --test admin_access` starts the
real binary and runs authorization attacks against it (pirates at every
onboarding step, demoted and deleted accounts, route/header/JSON tricks,
cross-site requests).

```text
web (HTTP server + embedded UI, session cookie)
 └─ app            application services + permission checks
     ├─ auth       accounts, password hashing, 2FA, sessions, throttling
     ├─ providers  YouTube / SoundCloud (URL parsing, search, metadata quirks)
     │   └─ tools  yt-dlp / ffmpeg process wrappers
     ├─ jobs       queue, lifecycle, retries, pipeline
     ├─ playback   listening: prepared streams, CDN proxy, HLS capture, waveforms
     ├─ rooms      chat rooms, history, membership, flood control
     ├─ soulseek   P2P client: protocol, sessions, peers, search, browse,
     │             chat, downloads → import into library, sharing/uploads
     ├─ metadata   normalization + tag writing (lofty)
     └─ library    layout, filename sanitization, no-clobber placement, index
domain             TrackMetadata, AudioFormat/quality selection, ProviderKind
```

New sources are added as providers behind the `MusicProvider` trait in
`src/providers`, which own URL recognition, resolution, metadata extraction
and format discovery; downloading, tagging and filing are shared.

## Roadmap

- Spotify and Deezer links (resolve their metadata and match it to a
  downloadable source)
- Configurable path templates
- Persisting the download queue across restarts
- YouTube cookies / Premium support (AAC 256k)

## Disclaimer

Inkult is a tool for managing your own music library. Downloading may be
restricted by the terms of service of the platforms involved and by copyright
law where you live. You are responsible for what you download and share;
support the artists you listen to.

## How Inkult was made

Inkult was vibecoded: its code was written by the AI models
[Claude](https://claude.ai) (Anthropic) and [Qwen](https://qwen.ai)
(Alibaba), directed, reviewed and tested by a human who manages a music
library. Review it like any other code before trusting it with your
library, and report anything that looks wrong.

## License

Copyright © 2026 the Inkult contributors.

Inkult is free software: you can redistribute it and/or modify it under the
terms of the [GNU Affero General Public License](LICENSE) as published by the
Free Software Foundation, either version 3 of the License, or (at your
option) any later version.

In short: you may use, study, share and modify Inkult, but anything you
distribute based on it, including a modified version you let others use over
a network, must be released under the same license with its source code.

Inkult is distributed in the hope that it will be useful, but WITHOUT ANY
WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS FOR
A PARTICULAR PURPOSE. See the [LICENSE](LICENSE) file for details.
