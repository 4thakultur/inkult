# Installing Inkult

This guide takes you from a fresh machine to a running Inkult server, on
Linux, macOS, Windows or a Raspberry Pi. Inkult runs in Docker with Docker
Compose on every system, so after the first step the instructions are the
same everywhere.

You do not need Rust, yt-dlp, ffmpeg or anything else installed: the Docker
image builds Inkult from source and bundles every tool it needs.

## Contents

1. [Before you start](#1-before-you-start)
2. [Install Docker and Git](#2-install-docker-and-git)
   - [Linux](#linux)
   - [Raspberry Pi and other ARM boards](#raspberry-pi-and-other-arm-boards)
   - [macOS](#macos)
   - [Windows](#windows)
   - [NAS and home servers](#nas-and-home-servers)
3. [Download Inkult](#3-download-inkult)
4. [Configure it](#4-configure-it)
5. [Create the music and data folders](#5-create-the-music-and-data-folders)
6. [Build and start](#6-build-and-start)
7. [Create the first account](#7-create-the-first-account)
8. [Add the rest of your crew](#8-add-the-rest-of-your-crew)
9. [Reach Inkult from other devices](#9-reach-inkult-from-other-devices)
10. [Open the Soulseek ports](#10-open-the-soulseek-ports)
11. [Everyday operation](#11-everyday-operation)
12. [Updating](#12-updating)
13. [Backups](#13-backups)
14. [Account recovery](#14-account-recovery)
15. [Troubleshooting](#15-troubleshooting)
16. [Uninstalling](#16-uninstalling)

---

## 1. Before you start

**Hardware**

| | Minimum | Comfortable |
|---|---|---|
| CPU | 64-bit x86 (`amd64`) or ARM (`arm64`) | 4 cores |
| Memory | 2 GB (add swap for the build, see below) | 4 GB or more |
| Disk | 3 GB for Docker images and build cache, plus your music | SSD for the data folder |

Only 64-bit systems are supported. On a Raspberry Pi this means a 64-bit
operating system, even on a Pi that could run a 32-bit one.

The first build compiles Inkult from source. It takes a few minutes on a
desktop computer and considerably longer on a Raspberry Pi or a low-power
NAS. Later rebuilds reuse the compile cache and are much faster.

**Network**

- Internet access for the build (it downloads Rust crates, Debian packages,
  yt-dlp and deno) and, of course, for downloading music.
- A correct system clock. Two-factor codes are time-based and fail when the
  server's clock is more than about 30 seconds off. Every modern OS keeps
  time with NTP by default; just don't turn it off.

**An authenticator app** on your phone, for the two-factor login every
account sets up: Google Authenticator, Microsoft Authenticator, Aegis, 2FAS,
Authy, 1Password, Bitwarden, KeePassXC or any other TOTP app.

---

## 2. Install Docker and Git

You need:

- **Docker Engine** 23 or newer (or Docker Desktop), with
- **Docker Compose v2**, the `docker compose` command (with a space; the old
  `docker-compose` v1 is not supported), and
- **Git**, to download Inkult and update it later.

Pick your system below, then check the result with:

```sh
docker --version
docker compose version
git --version
```

All three must print a version number before you continue.

### Linux

Docker's official packages include Compose v2. The convenience script below
works on Debian, Ubuntu, Raspberry Pi OS, Fedora, CentOS Stream, RHEL and
their derivatives:

```sh
curl -fsSL https://get.docker.com -o get-docker.sh
sh get-docker.sh          # read it first if you like; it asks for sudo
```

If you prefer to add Docker's repository by hand, follow
<https://docs.docker.com/engine/install/> for your distribution.

On **Arch Linux** and **Manjaro**:

```sh
sudo pacman -S docker docker-compose docker-buildx git
sudo systemctl enable --now docker
```

On Debian/Ubuntu, install Git with `sudo apt install git`; on Fedora,
`sudo dnf install git`.

Then let your user run Docker without `sudo`, and log out and back in (or
reboot) for it to take effect:

```sh
sudo usermod -aG docker "$USER"
```

> Membership of the `docker` group is equivalent to root access on that
> machine. On a shared server, keep using `sudo docker …` instead.

Make sure Docker starts at boot, so Inkult comes back after a reboot:

```sh
sudo systemctl enable --now docker
```

### Raspberry Pi and other ARM boards

Inkult supports the Raspberry Pi 4 and 5 and other `arm64` boards.

1. Install **Raspberry Pi OS (64-bit)**, *Lite* is enough, with Raspberry
   Pi Imager. Check with `uname -m`: it must print `aarch64`. If it prints
   `armv7l`, the system is 32-bit and must be reinstalled with the 64-bit
   image.
2. Follow the [Linux](#linux) steps above.
3. On boards with 2 GB of memory or less, give the first build some swap so
   the compiler is not killed for lack of memory:

   ```sh
   sudo dphys-swapfile swapoff
   sudo sed -i 's/^CONF_SWAPSIZE=.*/CONF_SWAPSIZE=2048/' /etc/dphys-swapfile
   sudo dphys-swapfile setup
   sudo dphys-swapfile swapon
   ```

   On other distributions, create a 2 GB swap file with `fallocate`,
   `mkswap` and `swapon`.
4. Keep your music on a USB SSD or a NAS rather than the SD card, which is
   slow and wears out (see [`MUSIC_DIR`](#4-configure-it)).

### macOS

1. Install **Docker Desktop** from
   <https://www.docker.com/products/docker-desktop/> (pick *Apple Silicon* or
   *Intel* to match your Mac), open it once and accept the terms. Compose v2
   is included.

   [OrbStack](https://orbstack.dev) and [Colima](https://github.com/abiosoft/colima)
   (`brew install colima docker docker-compose docker-buildx`) work as well.
2. Install Git. macOS offers it when you first type `git` in Terminal
   (it installs the Xcode command line tools), or use
   `brew install git`.
3. In Docker Desktop, open **Settings → General** and enable *Start Docker
   Desktop when you sign in*, so Inkult restarts with your Mac.
4. If your music lives outside your home folder (an external drive under
   `/Volumes`), add that location under **Settings → Resources → File
   sharing**.

Run the commands in this guide in **Terminal**.

### Windows

Inkult runs on Windows 10 (22H2) and Windows 11 through Docker Desktop and
WSL 2 (the Windows Subsystem for Linux).

1. Open **PowerShell as administrator** and install WSL 2 with Ubuntu:

   ```powershell
   wsl --install
   ```

   Restart when asked. Ubuntu then opens and asks you to choose a Linux
   username and password.
2. Install **Docker Desktop** from
   <https://www.docker.com/products/docker-desktop/>. During setup keep
   *Use WSL 2 instead of Hyper-V* selected.
3. In Docker Desktop, open **Settings → Resources → WSL integration** and
   enable integration for **Ubuntu**. Under **Settings → General**, enable
   *Start Docker Desktop when you sign in*.
4. Open **Ubuntu** from the Start menu and install Git there:

   ```sh
   sudo apt update && sudo apt install -y git
   ```

**Run every command in this guide inside the Ubuntu (WSL) terminal**, not in
PowerShell or Command Prompt, and keep the Inkult folder inside the Linux
file system (for example `~/inkult`), not under `/mnt/c/…`. Folders on the
Windows drive are much slower through WSL and do not keep Linux file
permissions, which Inkult relies on to protect its account data.

Your music can still be reached from Windows: in File Explorer, open
`\\wsl$\Ubuntu\home\<your-linux-user>\inkult\music`, or pin it to Quick
access. If you would rather keep the library on a Windows drive, set
`MUSIC_DIR=/mnt/d/Music` (for example) in step 4, but keep `DATA_DIR` inside
the Linux file system.

### NAS and home servers

Any NAS or home-server OS that runs Docker Engine with Compose v2 on a
64-bit CPU can run Inkult: Synology DSM 7.2+ (*Container Manager*),
QNAP (*Container Station*), Unraid, TrueNAS SCALE, OpenMediaVault, Proxmox
(in a Debian VM or LXC container) and others.

The simplest route is to connect to the NAS over SSH and follow this guide
from [step 3](#3-download-inkult) in a folder on one of your volumes. If
Git is not available on the NAS, download the repository as a ZIP from
GitHub and unpack it there instead.

Use the user and group IDs of the account that owns your music share for
`PUID` and `PGID` (run `id <user>` over SSH to find them).

---

## 3. Download Inkult

Pick a folder for Inkult, for example your home directory, and clone the
repository:

```sh
cd ~
git clone https://github.com/<owner>/inkult.git
cd inkult
```

Replace `<owner>` with the GitHub account or organisation that hosts
Inkult. Every remaining command in this guide is run from inside this
`inkult` folder.

> **Windows users with Git for Windows:** if you cloned from PowerShell
> instead of WSL, Git may have converted line endings, which breaks the
> Docker build. Clone again from the Ubuntu terminal, or use
> `git clone -c core.autocrlf=false …`.

---

## 4. Configure it

Every setting has a working default, so this step is optional, but it is
worth checking the first four lines. Copy the example file:

```sh
cp .env.example .env
```

and open `.env` in any text editor (`nano .env` works everywhere).

| Setting | What to put there |
|---|---|
| `PUID`, `PGID` | Your user and group IDs, so the music files Inkult writes belong to you. Run `id -u` and `id -g` to find them. The default `1000` matches the first user on most Linux systems and WSL; **on macOS it is usually `501` and `20`**. |
| `MUSIC_DIR` | Where your library is stored on this machine. Default `./music`, inside the Inkult folder. Can be an absolute path such as `/mnt/nas/music` or `/Volumes/Music`. |
| `DATA_DIR` | Where accounts, the library index and other state live. Default `./data`. Keep it on a local disk, not a network share. |
| `INKULT_HTTP_BIND` | `127.0.0.1` (default) = reachable only from this machine. Leave it until [step 9](#9-reach-inkult-from-other-devices). |
| `INKULT_HTTP_PORT` | Port of the web UI, default `9876`. Change it if something else already uses that port. |

The remaining settings are explained in the file itself and in the
[Configuration](README.md#configuration) section of the README. An invalid
value stops Inkult at startup with a message naming the setting.

> `.env` is ignored by Git and by the Docker build, so your settings never
> end up in a commit or in the image.

---

## 5. Create the music and data folders

Create both folders **before** the first start:

```sh
mkdir -p music data
```

If you set custom `MUSIC_DIR` / `DATA_DIR` paths, create those instead,
owned by the `PUID`/`PGID` user. For example:

```sh
sudo mkdir -p /mnt/nas/music
sudo chown 1000:1000 /mnt/nas/music     # your PUID:PGID
```

Why this matters: if a folder does not exist, Docker creates it owned by
`root`, Inkult (which runs as your user, never as root) cannot write to it,
and it stops at startup with:

```text
INKULT_LIBRARY_DIR: directory /music is not usable: Permission denied
```

---

## 6. Build and start

```sh
docker compose up -d --build
```

This builds the image (slow the first time, see
[Before you start](#1-before-you-start)) and starts Inkult in the
background. Check that it is running:

```sh
docker compose ps
```

The `STATUS` column should read `Up … (healthy)` within a minute of the
build finishing. `(health: starting)` just means it is still starting.

---

## 7. Create the first account

On its very first start Inkult has no accounts and prints a one-time setup
code to its log:

```sh
docker compose logs inkult | grep "setup code"
```

```text
WARN inkult::auth: no accounts yet: open the web UI and enter setup code 49e0-6333-3b53 to appoint the first captain
```

1. Open <http://localhost:9876> in a browser on the same machine (on
   Windows and macOS, the browser on your desktop works).
2. Enter the setup code, a **handle** (username) and a **password**. The
   password needs at least 12 characters and must be hard to guess; three
   unrelated words pass easily.
3. Set up two-factor login: scan the QR code with your authenticator app,
   then confirm with a code from the app and your password.
4. **Save the ten recovery codes** somewhere safe, such as a password
   manager. Each one logs you in once if you lose your phone.

You are now the first **captain** (administrator). The setup code stops
working as soon as this account exists.

---

## 8. Add the rest of your crew

In **[7] ADMIN**, a captain adds an account by choosing only a handle and a
rank:

- **Captain**: administrator, manages accounts and server rules.
- **Pirate**: regular user; searches, listens, downloads and chats, and
  sees only their own downloads and library.

Inkult shows a temporary password **once**. Hand it over privately. It
expires after 72 hours if unused. At first login the new user chooses their
own password and sets up two-factor login.

Make at least **two captains**. Accounts lock after 10 failed logins in a
row, and only a captain can unlock them; with a second captain you never
need the command line for that (see [Account recovery](#14-account-recovery)).

Server-wide rules (whether two-factor login is required, the name shown in
authenticator apps, the longest track allowed) are in
**[7] ADMIN → SHIP RULES**.

---

## 9. Reach Inkult from other devices

Out of the box, Inkult only answers on the machine it runs on. Inkult speaks
plain HTTP, so passwords and session cookies would cross the network
unencrypted. Choose one of the two options below.

### Option A: on your home network only

For a home network you trust, publish the web UI on every interface. In
`.env`:

```ini
INKULT_HTTP_BIND=0.0.0.0
```

Apply the change and find the server's address:

```sh
docker compose up -d
hostname -I        # Linux; on macOS: ipconfig getifaddr en0
```

Then open `http://<server-ip>:9876` from your phone or laptop.

- **Windows:** use the Windows PC's own address (`ipconfig` in PowerShell,
  *IPv4 Address*), not the WSL one. When Windows asks whether Docker
  Desktop may accept connections, allow it on **private** networks.
- **Linux with a firewall** (`ufw`, `firewalld`): Docker's published ports
  normally bypass these, but check that your network setup allows port
  `9876` from the LAN.

Do **not** forward port 9876 on your router. For access from the internet,
use option B.

### Option B: over HTTPS with a domain name (needed for internet access)

Put Inkult behind a reverse proxy that handles HTTPS, such as Caddy,
Traefik, Nginx Proxy Manager or SWAG. You need a domain name pointing at
your public IP address, and ports 80 and 443 forwarded on your router to
the machine running the proxy.

With **Caddy** installed on the same machine
(<https://caddyserver.com/docs/install>), this `Caddyfile` is all it takes;
Caddy obtains and renews the certificate itself:

```text
music.example.com {
    reverse_proxy 127.0.0.1:9876
}
```

Keep `INKULT_HTTP_BIND=127.0.0.1` in this setup, so Inkult is only
reachable through the proxy. Then tell Inkult about the domain and the
proxy in `.env`:

```ini
INKULT_ALLOWED_HOSTS=music.example.com
INKULT_TRUSTED_PROXIES=
```

and restart with `docker compose up -d`.

- **`INKULT_ALLOWED_HOSTS`**: Inkult refuses requests for host names it
  does not know (answering `421 Misdirected Request`), which protects you
  against DNS-rebinding attacks. List every domain you use, comma
  separated, without `https://` or a port.
- **`INKULT_TRUSTED_PROXIES`**: sign-in limits apply per visitor, but
  behind a proxy every connection seems to come from the proxy. After your
  first visit through the proxy, look for this warning in the log:

  ```sh
  docker compose logs inkult | grep -i proxies
  ```

  It names the address to add, for example `172.18.0.1` when Docker
  forwards the connection. Put that address in `INKULT_TRUSTED_PROXIES`
  and restart.

**Proxy running in Docker as well?** Attach it and Inkult to a shared Docker
network and proxy to `inkult:9876` instead of `127.0.0.1:9876`. Create a
`docker-compose.override.yml` next to `docker-compose.yml` (Compose reads
it automatically, and it survives updates):

```yaml
services:
  inkult:
    networks: [default, proxy]
networks:
  proxy:
    external: true   # the network your proxy container is on
```

Set `INKULT_TRUSTED_PROXIES` to the proxy container's address, or to that
network's range, for example `172.18.0.0/16` (`docker network inspect proxy`
shows it).

Other proxies need no special settings: they pass the `Host` header through
and set `X-Forwarded-Proto` by default. Leave HSTS to the proxy.

---

## 10. Open the Soulseek ports

Inkult includes a Soulseek client. Each signed-in Soulseek account listens
on one TCP port from **2234 to 2238**. Forward that range on your router to
the machine running Inkult (TCP, same port numbers outside and inside), so
other users can connect to you.

This is optional: without it Soulseek still works through outgoing
connections, but you cannot reach users who are behind a firewall too, and
many of them are. These ports carry only Soulseek peer traffic, never the
web UI.

- **Windows:** allow Docker Desktop through Windows Defender Firewall on
  private and public networks, or add an inbound rule for TCP 2234–2238.
- **Linux firewalls:** allow TCP 2234–2238 if your setup filters Docker's
  published ports.
- If another program already uses these ports, stop that program rather
  than remapping them: the host and container port numbers must stay
  identical, because peers are told this number.

---

## 11. Everyday operation

Run these from the Inkult folder.

| Task | Command |
|---|---|
| Status and health | `docker compose ps` |
| Follow the logs | `docker compose logs -f inkult` (Ctrl+C to stop following) |
| Stop | `docker compose stop` |
| Start again | `docker compose start` |
| Restart after editing `.env` | `docker compose up -d` |
| Stop and remove the container | `docker compose down` (your music and data stay) |

Inkult restarts on its own after a crash or a reboot (`restart:
unless-stopped`), as long as Docker itself starts at boot. Container logs
are rotated automatically (3 × 10 MB).

Restarting Inkult signs everyone out, and downloads still in the queue must
be queued again (the Soulseek transfer queue survives restarts). On
`docker compose stop` or `down`, running downloads get 30 seconds to cancel
cleanly.

---

## 12. Updating

```sh
cd ~/inkult        # your Inkult folder
git pull
docker compose up -d --build
```

Your `.env`, music and data are not touched by an update. Rebuilds reuse the
compile cache, so only Inkult itself is recompiled.

**YouTube downloads suddenly failing?** YouTube changes often, and the
yt-dlp bundled in the image may fall behind before a new Inkult release
updates it. You can build with a newer yt-dlp yourself: take the version
number from <https://github.com/yt-dlp/yt-dlp/releases> and the two
checksums from that release's `SHA2-256SUMS` file (the lines for
`yt-dlp_linux` and `yt-dlp_linux_aarch64`):

```sh
docker compose build \
  --build-arg YTDLP_VERSION=<version> \
  --build-arg YTDLP_SHA256_AMD64=<sha256 of yt-dlp_linux> \
  --build-arg YTDLP_SHA256_ARM64=<sha256 of yt-dlp_linux_aarch64>
docker compose up -d
```

The build stops if a checksum does not match, so a tampered or truncated
download can never reach your server.

**Freeing disk space:** old images and build cache pile up after many
updates. `docker image prune` removes unused images, and
`docker builder prune` the build cache (the next build is then slower).

---

## 13. Backups

Back up **both** folders, together:

| Folder | Contents | Size |
|---|---|---|
| `MUSIC_DIR` (`./music`) | Your library | Large |
| `DATA_DIR` (`./data`) | Accounts, two-factor secrets, library index, chat rooms, Soulseek state, server rules | Small |

The library index in `data` records who owns each track and where it came
from, so a `music` backup without the matching `data` loses that
information. `data/staging/` and `data/cache/` are temporary and can be left
out.

For a consistent copy, stop Inkult first:

```sh
docker compose stop
tar -czf inkult-data-$(date +%F).tar.gz data
docker compose start
```

`data` holds password hashes, two-factor secrets and saved Soulseek
passwords. Store its backups somewhere only you can read.

**Moving to another machine:** install Docker and Inkult there (steps 2 to
4), copy `.env`, `music` and `data` across with their permissions intact
(`rsync -a` or `tar`), then start with `docker compose up -d --build`.

---

## 14. Account recovery

| Problem | Fix |
|---|---|
| Lost phone, still have a recovery code | Log in with the recovery code, then **[6] SETTINGS → [NEW PHONE]**. |
| Lost phone and recovery codes | Another captain uses **[RESET 2FA]** in **[7] ADMIN**. |
| Account locked after failed logins | Another captain uses **[UNLOCK]** in **[7] ADMIN**. |
| Forgotten password | Another captain uses **[RESET PW]** in **[7] ADMIN**. |

If no other captain can help, use the command line. Inkult must be stopped
while you do this:

```sh
docker compose stop

# lift a lock caused by failed logins (keeps the password)
docker compose run --rm -T inkult unlock <handle>

# set a new password
echo 'a new long password' | docker compose run --rm -T inkult passwd <handle>

# remove two-factor login (set it up again at the next login)
docker compose run --rm -T inkult reset-2fa <handle>

docker compose start
```

To avoid leaving the new password in your shell history, type a space
before the `echo` line (most shells then skip saving it), or change the
password again from the web UI right after.

---

## 15. Troubleshooting

Start with the log; Inkult explains what went wrong there:

```sh
docker compose logs --tail 100 inkult
```

**`permission denied while trying to connect to the Docker daemon socket`**
Your user is not in the `docker` group yet. Run
`sudo usermod -aG docker "$USER"` and log out and back in, or prefix
commands with `sudo`.

**`docker: 'compose' is not a docker command`**
Compose v2 is missing. Install the `docker-compose-plugin` package (Debian,
Ubuntu, Fedora with Docker's repository) or `docker-compose` (Arch), or
update Docker Desktop.

**`… is not usable: Permission denied` at startup**
The music or data folder is owned by another user than `PUID`/`PGID`. Fix
it with your own IDs:

```sh
sudo chown -R "$(id -u):$(id -g)" music data
```

**The build is killed, or fails with `signal: 9` / out of memory**
The compiler ran out of memory. Add swap (see
[Raspberry Pi](#raspberry-pi-and-other-arm-boards)), or give Docker Desktop
more memory under **Settings → Resources**, then run the build again; it
continues where it stopped.

**`unsupported architecture` during the build**
The system is 32-bit or not `amd64`/`arm64`. On a Raspberry Pi, install the
64-bit OS.

**The Windows build fails with errors such as `/bin/sh: 1: …: not found`
or `\r`**
Line endings were converted when cloning. Clone again from the Ubuntu
terminal (see [step 3](#3-download-inkult)).

**`port is already allocated` / `address already in use`**
Another program uses port 9876 or one of 2234–2238. For the web UI, set
another `INKULT_HTTP_PORT` in `.env`. For the Soulseek ports, stop the
other program (often another Soulseek client such as Nicotine+ or slskd
running on the same machine).

**No setup code in the log**
The code is only printed while no account exists. If you already created
the first captain, sign in instead. If you never did and the log has
rotated, restart with `docker compose restart` and look again.

**The page does not load from another device**
By default Inkult only answers on the machine it runs on; see
[step 9](#9-reach-inkult-from-other-devices).

**`421 Misdirected Request`**
You reached Inkult by a domain name it does not know. Add it to
`INKULT_ALLOWED_HOSTS` in `.env` and run `docker compose up -d`.

**Two-factor codes are always rejected**
The server's clock is off. Check it with `date` (on Linux, `timedatectl`
should show `System clock synchronized: yes`). On Windows and macOS,
restarting Docker Desktop resynchronises its clock after the computer was
asleep.

**Everyone gets locked out after a few attempts behind a reverse proxy**
Sign-in limits are counting the proxy as one visitor. Set
`INKULT_TRUSTED_PROXIES` (see [option B](#option-b-over-https-with-a-domain-name-needed-for-internet-access)).

**YouTube downloads fail but SoundCloud works**
yt-dlp is out of date; see [Updating](#12-updating).

**The container is `unhealthy`**
The health check could not reach the web UI inside the container. The log
says why; usually it is one of the startup errors above.

---

## 16. Uninstalling

```sh
cd ~/inkult
docker compose down --rmi local     # removes the container and the Inkult image
docker builder prune                # optional: removes the build cache
```

This leaves your `music` and `data` folders in place. Delete them yourself
only once you are sure you no longer need them; Inkult never deletes them
for you.
