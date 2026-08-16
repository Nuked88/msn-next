# msnnext

A P2P desktop messenger inspired by MSN Messenger. Its goal is to keep text, nudges, custom emoticons, and media exchange free of a central account: identities, keys, contacts, and history belong to users.

> Current status: **development alpha**. The GUI starts and can be used to try connections and messages, but it is not yet a reliable release. Emoticons and attachments now have confirmations tied to the core result, but still require end-to-end testing between two installed applications.

## Current progress

### Desktop application

- single Tauri 2 + Svelte application with an integrated Rust core and no separate CLI process;
- MSN-inspired UI with online/offline contacts, chats, search, and unread counts;
- persistent Light, Dark, and System themes;
- main window closes to the tray, with commands to reopen or truly quit the app;
- signed updates checked at startup and every five hours, with downloads started only by the user;
- onboarding, adding contacts through `msnnext://add/...` links, rendered QR codes, and QR scanning from images;
- native file and image picker plus drag and drop;
- persistent group chats with up to 32 participants, routed through individual encrypted channels;
- separate conversations per Peer ID;
- contacts, history, and identity persist across restarts;
- multi-device linking through a one-time QR/code, with a distinct Peer ID for every installation;
- typed bridge between GUI and core through Tauri commands and events.

### Networking and security

- persistent Ed25519 identity, distinct per device and authorized by a shared account root;
- libp2p QUIC and TCP transport;
- LAN discovery through mDNS;
- Identify, Kademlia DHT, AutoNAT, DCUtR, and Circuit Relay v2;
- signed contact links and verification that the public key matches the Peer ID;
- hybrid X25519 + ML-KEM-768 application handshake;
- application messages encrypted with XChaCha20-Poly1305 and a symmetric ratchet;
- replay protection and limited support for out-of-order messages;
- backoff reconnection after a lost connection;
- encrypted P2P device synchronization, attempting direct DCUtR with Circuit Relay fallback;
- idle connection timeout increased beyond the previous incorrect 60-second limit.

### Local data and protocol

- versioned CBOR envelopes bound to device and conversation;
- SQLite history with locally encrypted content;
- persistent contacts;
- chunked core transfers with BLAKE3 hashes and resuming missing chunks;
- attachments up to 5 GB, without reconstructing the whole file in RAM;
- locally encrypted received-attachment archive per chunk, including partial chunks;
- profile-configurable sent and received image previews;
- limits and validation for PNG, JPEG, GIF, and WebP emoticons;
- encrypted, rate-limited nudges;
- desktop identity in the operating-system keystore, with automatic migration from the previous `identity.key`;
- encrypted incremental log with cursors, deduplication, and tombstones to synchronize contacts, history, and groups; attachments remain local;
- password-encrypted backups to recover account, contacts, and history on a new PC; restoring retains a distinct local Peer ID;
- Ed25519- and ML-DSA-65-signed v2 contact links; QR uses a compact Ed25519-signed v3 card because an ML-DSA-65 signature does not fit in one QR code, while retaining v1 card support;
- restrictive WebView CSP and a fingerprint displayed in the GUI;
- public mininode preconfigured as bootstrap and Circuit Relay v2 for clients behind CGNAT; it does not store messages.

## What works today

- GUI startup and identity creation;
- adding contacts through a link or QR code;
- direct connection on the same machine or LAN under the conditions already tested;
- secure-channel negotiation;
- sending and receiving text messages;
- text history and contact list after a restart;
- synchronizing contacts, messages, groups, renames, and deletions while at least two account devices are online;
- basic nudge with web-window animation;
- generating Windows MSI and NSIS installers.

Reconnection stability has improved and was manually tested with two local cores, including a restart, but still needs long-running testing with two installed applications on different real networks.

## Known issues

### Custom emoticons

The flow is present in the core and GUI, including creation, saving, shortcut renaming, and deletion:

- sending, previewing, saving on the recipient side, and reuse through a shortcut still need end-to-end testing between two installed applications;
- emoticon spans are retained in encrypted history and restored after restart;
- shortcut conflicts are rejected, but the GUI does not yet automatically suggest an alternative shortcut.

The outcome required by `GROUND.md` remains: choose an image or GIF, assign a shortcut, see it in the recipient’s message, and let them save it in a few seconds.

### Images, videos, and files

The chunked Rust protocol, drag and drop, and encrypted archive are integrated, but the experience is still incomplete:

- reliable end-to-end verification between two desktop applications is missing;
- images and videos up to the preview limit are displayed internally;
- every received file requires explicit acceptance and pending offers are limited;
- sending shows chunk progress and can be cancelled; transfer speed and cancellation of an already accepted download remain;
- files that cannot be previewed are explicitly exported by the user and are not automatically opened in plaintext.

The original file sent is not copied into the msnnext archive: it is read in chunks from the selected path, verified with BLAKE3, and sent over the encrypted channel. On the recipient, chunks remain encrypted in the app data directory and are decrypted only in memory for preview or streamed to a path chosen during export; the temporary directory is not used. The archive key derives from the local identity, now held in the operating-system keystore on desktop.

### Group chats

- creation, local history, messages, and attachments work by sending separately to each online participant;
- owner, administrators, and members have a persistent hierarchy; owners and administrators can apply mutes, temporary bans, and permanent bans;
- offline delivery does not yet exist: anyone disconnected at the time does not receive the message;
- nudges and subsequent participant editing are not yet available in group chats.

### Connection and presence

- automatic reconnection still needs long-running tests on two PCs and different networks;
- hole punching and relay fallback have not been tested across two real NATs;
- there is only one preconfigured public relay, so there is currently no redundancy if the VPS is unavailable;
- offline messages do not exist: at least one device must be reachable;
- the mininode coordinates reachability and relay but does not retain synchronized data; without overlapping online time between two devices, no synchronization occurs;
- multi-device pairing and synchronization still need testing on two real installations behind different NATs;
- avatar, personal name, groups, contact rename, and contact removal are available; personal status and blocking remain incomplete.

### Nudges and UX

- nudges move the native window, with WebView fallback, and have a disableable sound;
- settings, accessibility, and complete notification control are still missing.

### Security work remaining

- ML-DSA signature of the application-handshake transcript, in addition to the hybrid-signed v2 contact cards already implemented;
- explicit persistence of fingerprint/QR comparison results;
- independent cryptographic and security audit;
- migration/versioning protocol for cryptographic primitives.

## Next priorities

1. Test the complete custom-emoticon flow between two installed GUIs.
2. Add speed reporting and cancellation for already accepted downloads.
3. Run extended connection, disconnection, and reconnection tests with two installed applications.
4. Complete native nudge, sound, presence, avatar, and contact management.
5. Test multi-device pairing, bootstrap, relay, and hole punching on real networks.
6. Add device revocation with account-root rotation.
7. Strengthen key storage, CSP, post-quantum signatures, and identity verification.

## Structure

```text
apps/cli                 Rust core and CLI client
apps/desktop             Svelte GUI
apps/desktop/src-tauri   Tauri desktop backend
crates/protocol          shared events and formats
prototypes/web           old prototype, not a production application
```

## Starting the desktop app

Prerequisites: Rust, Node.js, npm, and the system dependencies required by Tauri 2.

```powershell
cd apps/desktop
npm install
npm run desktop
```

## Checks

```powershell
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings

cd apps/desktop
npm run check
npm run build
npx tauri build --config '{"bundle":{"createUpdaterArtifacts":false}}'
```

The current standard suite passes with 68 tests. A test using two real sockets is excluded from the automated suite because libp2p teardown can block the harness on Windows. Its scenario should be replaced with deterministic desktop end-to-end testing.

Installers are generated in:

```text
target/release/bundle/msi/msnnext_0.2.0_x64_en-US.msi
target/release/bundle/nsis/msnnext_0.2.0_x64-setup.exe
```

## Creating releases and installers

On Windows, from the repository root:

```powershell
.\scripts\release-windows.ps1
```

This produces the release executable and NSIS setup. To reuse installed dependencies: `.\scripts\release-windows.ps1 -SkipInstall`.

The MSI format uses the old WiX toolset and can be requested with `.\scripts\release-windows.ps1 -Msi`; NSIS remains the default and more reliable Windows installer.

On Linux:

```sh
./scripts/release-linux.sh
```

This produces the release binary, a DEB package, and an AppImage. Run it directly on Linux with the system dependencies required by Tauri; to reuse `node_modules`, pass `--skip-install`.

On macOS, from the repository root, double-click `BUILD.command` or run:

```sh
./scripts/release-macos.sh
```

This produces a native DMG for the current Mac. To create the same universal Intel/Apple Silicon DMG as the GitHub workflow, pass `--universal`; to reuse `node_modules`, pass `--skip-install`. If it finds `~/.tauri/msnnext-updater.key`, the script asks for the password to sign the updater archive too; pressing Enter still creates the DMG without that archive. Local builds are ad-hoc signed but not notarized by Apple.

Windows, Linux, and macOS releases must be built on their respective operating systems. Android is not yet included: it requires initializing the Tauri mobile project and adapting native desktop functionality.

### Cross-platform builds on GitHub

The `Build release bundles` workflow compiles NSIS on Windows, DEB/AppImage on Linux, and a universal Intel/Apple Silicon DMG on macOS. On pull requests it retains packages as artifacts without signing keys; when manually started from the **Actions** tab, it publishes a complete GitHub release with installers, signatures, and `latest.json` for the integrated updater.

Updates are verified by the client with the public key included in the app. The private key must not enter the repository: GitHub Actions reads it from the `TAURI_SIGNING_PRIVATE_KEY` secret. A recovery copy of the current key is stored locally in `~/.tauri/msnnext-updater.key` with user-only permissions. For a signed local build, set `TAURI_SIGNING_PRIVATE_KEY` to that file’s path; without the key, use the development override shown above.

The app version follows SemVer and is defined only once in `apps/desktop/package.json`; Tauri reads it from that file. While the project is alpha, use `0.x.0` for new features or incompatible changes and `0.x.y` for compatible fixes. Future releases will use `v0.x.y` Git tags.

## Definition of “usable”

msnnext will not be considered usable until two people can, without manual restarts:

1. add each other through QR or a link;
2. connect and remain connected;
3. exchange text and nudges;
4. create, send, and save static or animated emoticons;
5. send and receive images, videos, and files with clear feedback;
6. close and reopen the app while retaining contacts and conversations.
