# Building and installing Ferridis

This guide covers building Ferridis from source on every supported platform.
Ferridis uses `rustls` throughout, so there is **no OpenSSL dependency** on any platform.
The only platform-varying runtime requirement is the OS keychain backend used by the wallet.

---

## Prerequisites — all platforms

| Requirement | Version | Notes |
|---|---|---|
| [rustup](https://rustup.rs) | any | Installs and manages Rust toolchains |
| Rust toolchain | **1.95.0** | pinned in `rust/rust-toolchain.toml` — `rustup` picks it up automatically |
| Git | any | for cloning the repo |

```
git clone https://github.com/george-andrikopoulos/ferridis.git
cd ferridis
```

No further global tools are required to build the workspace.

---

## Linux — x86_64 (Debian / Ubuntu / Fedora / Arch)

### System libraries

Ferridis stores credentials in the OS Secret Service (GNOME Keyring or KWallet)
via D-Bus. Install the D-Bus development headers before building:

```bash
# Debian / Ubuntu / Raspberry Pi OS
sudo apt-get update
sudo apt-get install -y libdbus-1-dev pkg-config

# Fedora / RHEL / Rocky Linux
sudo dnf install -y dbus-devel pkg-config

# Arch Linux
sudo pacman -S dbus pkgconf
```

### Build

```bash
cd rust
cargo build --release --workspace
```

Release binaries land in `rust/target/release/`.

### Key binaries

| Binary | Purpose |
|---|---|
| `ferridis-mcp-server` | MCP publisher shim — exposes Ferridis capabilities as MCP tools |
| `ferridis-cli` | JSON-RPC 2.0 sidecar for non-Rust hosts (scripts). The VS Code extension does not use it: it talks to `ferridis-mcp-server` (see `editors/vscode/README.md`) |
| `ferridis-discovery-broker` | Local service registry on `127.0.0.1:7825` |
| `ferridis-adapter-fs` | Filesystem reference adapter |
| `ferridis-adapter-claude-cli` | Claude Code CLI adapter (stream-kind) |
| `ferridis-ptc` (library + `ptc_run` example) | Programmatic tool calling over the Claude Messages API. Try it with `ANTHROPIC_API_KEY=... cargo run -p ferridis-ptc --example ptc_run -- adapters.json "<task>"`; an unscoped key also needs `ANTHROPIC_WORKSPACE_ID` |

### Discovery broker — systemd user unit

The broker can run as a persistent user service:

```bash
# Copy the binary
cp rust/target/release/ferridis-discovery-broker ~/.local/bin/

# Create the unit file
mkdir -p ~/.config/systemd/user
cat > ~/.config/systemd/user/ferridis-discovery-broker.service << 'EOF'
[Unit]
Description=Ferridis discovery broker
After=network.target

[Service]
ExecStart=%h/.local/bin/ferridis-discovery-broker
Restart=on-failure
RestartSec=5
Environment=RUST_LOG=info

[Install]
WantedBy=default.target
EOF

systemctl --user daemon-reload
systemctl --user enable --now ferridis-discovery-broker
```

The broker persists pinned registrations to `~/.local/share/ferridis/discovery.json`
and restores them automatically on restart. Without `HOME` it uses
`%LOCALAPPDATA%\Ferridis\discovery.json`, and with neither set it refuses to start
until `--state-file <path>` is given.

### Keyring backend — Linux

The wallet uses the **Secret Service** protocol (GNOME Keyring or KWallet).

- GNOME desktop: GNOME Keyring is started automatically at login.
- Headless / CI: start a transient keyring with `gnome-keyring-daemon --start --components=secrets`
  or set `FERRIDIS_WALLET_MEMORY=1` to use an in-memory store (tests only — not persisted).

---

## Linux — ARM (aarch64 / armv7)

Tested on Raspberry Pi OS (64-bit) and Ubuntu 22.04 LTS on aarch64.

### Native build on the device

Install the same D-Bus headers as the x86_64 path above, then build natively:

```bash
# On the ARM device
sudo apt-get install -y libdbus-1-dev pkg-config
cd ferridis/rust
cargo build --release --workspace
```

A Raspberry Pi 4 (4 GB) takes roughly 10–15 minutes for a clean release build.
Subsequent incremental builds are much faster.

### Cross-compilation from an x86_64 host

Install [cross](https://github.com/cross-rs/cross) and Docker (or Podman):

```bash
cargo install cross --git https://github.com/cross-rs/cross

# aarch64 (Raspberry Pi 4 / 64-bit ARM servers)
cd ferridis/rust
cross build --release --workspace --target aarch64-unknown-linux-gnu

# armv7 (Raspberry Pi 3 / 32-bit)
cross build --release --workspace --target armv7-unknown-linux-gnueabihf
```

Binaries land in `rust/target/<target>/release/`. Copy them to the device with `scp` or `rsync`.

The D-Bus system library is bundled inside the `cross` container image — no host-side setup needed.

---

## macOS — Intel and Apple Silicon

### Prerequisites

Xcode Command Line Tools (provides the linker):

```bash
xcode-select --install
```

No other system libraries are required. Ferridis uses `rustls` (no OpenSSL) and the
macOS Keychain directly (no extra keychain libraries).

### Build

```bash
cd rust
cargo build --release --workspace
```

**Apple Silicon (M-series):** the workspace compiles natively for `aarch64-apple-darwin`
with no extra flags. If you installed Rust via rustup on an Apple Silicon Mac,
`cargo build` targets `aarch64-apple-darwin` by default.

**Intel Mac:** targets `x86_64-apple-darwin` by default.

**Universal binary** (both architectures in one file):

```bash
rustup target add x86_64-apple-darwin aarch64-apple-darwin
cargo build --release --target x86_64-apple-darwin --target aarch64-apple-darwin
lipo -create \
  rust/target/x86_64-apple-darwin/release/ferridis-mcp-server \
  rust/target/aarch64-apple-darwin/release/ferridis-mcp-server \
  -output ferridis-mcp-server-universal
```

### Discovery broker — launchd

To run the broker as a persistent background service on macOS:

```bash
cp rust/target/release/ferridis-discovery-broker /usr/local/bin/

cat > ~/Library/LaunchAgents/io.ferridis.discovery-broker.plist << 'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN"
  "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>io.ferridis.discovery-broker</string>
  <key>ProgramArguments</key>
  <array>
    <string>/usr/local/bin/ferridis-discovery-broker</string>
  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>StandardErrorPath</key>
  <string>/tmp/ferridis-broker.log</string>
  <key>EnvironmentVariables</key>
  <dict>
    <key>RUST_LOG</key>
    <string>info</string>
  </dict>
</dict>
</plist>
EOF

launchctl load ~/Library/LaunchAgents/io.ferridis.discovery-broker.plist
```

### Keychain backend — macOS

The wallet uses the **macOS Keychain** via the system `Security.framework`.
No configuration required — it works out of the box for any user session.

---

## Windows — x86_64

Ferridis builds on Windows with no dependency on OpenSSL or MSYS2.
The wallet uses **Windows Credential Manager** via the system `wincred` API.

### Prerequisites

**Option A — MSVC toolchain (recommended)**

1. Install [Visual Studio 2019 or later](https://visualstudio.microsoft.com/downloads/)
   with the **"Desktop development with C++"** workload (provides the linker `link.exe`).
   The free Community edition is sufficient.

2. Install Rust via [rustup.rs](https://rustup.rs). Accept the default MSVC toolchain
   (`x86_64-pc-windows-msvc`).

**Option B — GNU toolchain via MSYS2**

```powershell
winget install MSYS2.MSYS2
# In the MSYS2 shell:
pacman -S mingw-w64-x86_64-toolchain
```

Then set the GNU target as default:

```powershell
rustup toolchain install stable-x86_64-pc-windows-gnu
rustup default stable-x86_64-pc-windows-gnu
```

### Build

```powershell
cd ferridis\rust
cargo build --release --workspace
```

Release binaries land in `rust\target\release\` with a `.exe` suffix.

### Discovery broker — Windows Task Scheduler

To run the broker automatically at login:

```powershell
# Copy the binary first
Copy-Item rust\target\release\ferridis-discovery-broker.exe `
  "$env:LOCALAPPDATA\Ferridis\ferridis-discovery-broker.exe" -Force

# Register a scheduled task (run in an elevated PowerShell)
$action = New-ScheduledTaskAction `
  -Execute "$env:LOCALAPPDATA\Ferridis\ferridis-discovery-broker.exe"
$trigger = New-ScheduledTaskTrigger -AtLogOn
$principal = New-ScheduledTaskPrincipal -UserId $env:USERNAME -RunLevel Limited
$settings = New-ScheduledTaskSettingsSet -ExecutionTimeLimit 0
Register-ScheduledTask -TaskName "FerridisDiscoveryBroker" `
  -Action $action -Trigger $trigger `
  -Principal $principal -Settings $settings
```

Alternatively, use [NSSM](https://nssm.cc) to wrap the binary as a Windows Service.

### MCP server — Windows service (WinSW)

To serve one Ferridis federation to every client on the machine (Claude Code, the
VS Code extension's service mode, scripts), run `ferridis-mcp-server` with
`--http-bind` as a Windows service. [WinSW](https://github.com/winsw/winsw) wraps the
binary: its executable, renamed (e.g. `ferridis-service.exe`), sits beside an XML file
of the same name.

```xml
<service>
  <id>FerridisMCP</id>
  <name>Ferridis MCP</name>
  <executable>%BASE%\ferridis-mcp-server.exe</executable>
  <arguments>--adapters-config "%BASE%\adapters.json" --http-bind 127.0.0.1:7820 --bearer-token-file "%BASE%\bearer.token"</arguments>
  <env name="RUST_LOG" value="info" />
  <startmode>Automatic</startmode>
  <onfailure action="restart" delay="10 sec" />
  <log mode="roll" />
</service>
```

Install with `ferridis-service.exe install`, start with `ferridis-service.exe start`
(both elevated). Notes on the choices:

- **`%BASE%`, not `%LOCALAPPDATA%`.** WinSW expands `%BASE%` to the folder holding the
  wrapper. Environment variables expand in the *service's* environment, so under
  LocalSystem `%LOCALAPPDATA%` is SYSTEM's profile, not yours. Keep the wrapper, the
  binary, `adapters.json` and the token file in one folder, or write absolute paths.

- **`--bearer-token-file`, not the token in the XML.** WinSW XML is plain text, and
  `--bearer-token` or an `<env name="FERRIDIS_MCP_BEARER">` entry would put the token
  there. The file holds one token on one line; surrounding whitespace is trimmed.
  Restrict it to Administrators and SYSTEM (`icacls bearer.token /inheritance:r
  /grant:r Administrators:F SYSTEM:R`). The server refuses a file combined with
  `--bearer-token` or `FERRIDIS_MCP_BEARER`, so remove any old `<env>` entry when
  switching.
- **The account.** WinSW runs the service as LocalSystem unless the XML names another
  account. Choose it from the adapters: one that spawns a stdio MCP server needing
  elevation fails with `os error 740` under a user account, and its tools are
  skipped. Under LocalSystem, keychain entries live in SYSTEM's Credential Manager,
  not yours.
- **Loopback only.** Bind `127.0.0.1`. Put a TLS-terminating proxy or tunnel in front
  before exposing the port to another host.

**Clients.** Claude Code: `claude mcp add --transport http ferridis
http://127.0.0.1:7820/mcp --header "Authorization: Bearer <token>"`. VS Code: set
`ferridis.serverUrl` and run **Ferridis: Set Service Token** (see
[editors/vscode/README.md](./editors/vscode/README.md)). Every client holds the same
token, so rotating it means updating each one.

**Redeploying.** Build the release binaries, then:

1. Back up the current executables, the XML and the token file.
2. `ferridis-service.exe stop`, copy the new `ferridis-mcp-server.exe` in, then
   `ferridis-service.exe start`.
3. Probe: a request carrying the token gets `200`, and one without it gets `401`. A
   `tools/list` returns at least as many tools as before the swap.
4. If any probe fails, restore the backup and start the service again.

Keeping the token across a redeploy keeps every client connected. Rotate it only on
purpose, because each client then needs the new value.

### Keychain backend — Windows

The wallet uses **Windows Credential Manager** (`wincred.h`), which is built into every
Windows version since Vista. No configuration required.

---

## Environment variables

| Variable | Purpose | Default |
|---|---|---|
| `FERRIDIS_WALLET_MEMORY=1` | Use an in-memory wallet instead of the OS keychain (tests / CI only — not persisted) | unset |
| `FERRIDIS_KEYCHAIN_NAMESPACE` | Namespace prefix for wallet entries in the OS keychain | `ferridis` |
| `RUST_LOG` | Log level (`error`, `warn`, `info`, `debug`, `trace`) | `warn` |
| `FERRIDIS_MCP_BEARER` | Static bearer for `ferridis-mcp-server --http-bind`, instead of `--bearer-token`. Prefer `--bearer-token-file` for a service | unset |

---

## Verifying the build

```bash
bash scripts/check.sh
```

This runs `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings` and `cargo test --workspace` from `rust/`, reports each stage's exit status, and exits non-zero if any failed. It is the project's only gate: hosted CI is switched off. Expected: `PASS: rustfmt, clippy, test`. See [TODO.md](./TODO.md) for the authoritative
test count after each milestone.

---

## MCP server quick-start

Once built, register `ferridis-mcp-server` with any MCP-aware client.

**Claude Code (`~/.claude.json`):**
```jsonc
{
  "mcpServers": {
    "ferridis": {
      "command": "/path/to/ferridis-mcp-server",
      "args": ["--adapters-config", "/path/to/adapters.json"]
    }
  }
}
```

**Zed (`settings.json`):**
```jsonc
{
  "context_servers": {
    "ferridis-mcp": {
      "command": { "path": "/path/to/ferridis-mcp-server", "args": [] },
      "settings": {
        "binary_path": "/path/to/ferridis-mcp-server",
        "adapters_config": "/path/to/adapters.json"
      }
    }
  }
}
```

See [architecture.md](./architecture.md) for the full six-layer design and
[FEATURES.md](./FEATURES.md) for the current feature matrix.
