# Changelog

All notable changes to RetinaTag (Windows side). Newest at the top.

## v1.5.103 — 2026-05-11
**REAL P0 modal fix.** v1.5.97 ("backdrop-filter regression" theory) was
wrong — diagnostic in v1.5.99 confirmed modal had `opacity:1
display:flex` in CSS BUT was still invisible on screen. Chromium 148 /
WebView2 148.x has **two stacking compositor bugs**:

1. **Opacity transition**: CSS reaches `opacity:1` but the GPU layer
   texture stays at `0` for the entire animation. `getComputedStyle`
   reports the correct value; the screen disagrees.
2. **`display:none → display:flex` toggle**: even with explicit
   `width:100vw;height:100vh;top:0;left:0;right:0;bottom:0`, the
   element gets `display:flex` in computed style but `getBoundingClientRect`
   returns `0×0`. The layout never propagates from the display change.

Both end the same way: `.modal-overlay` "opens" with `pointer-events:all`
active, but is invisible. Every click after Settings / Tag Manager /
Import / Vault / Cluster / Welcome modal opens gets eaten by the
invisible overlay → user reports "full kilit".

Fix that actually paints: drop opacity transitions AND display toggle.
Keep the modal in flow with `display:flex` permanently, gate visibility
with `visibility:hidden ↔ visible`. Verified in v1.5.102 — `rect
x=0 y=0 w=1440 h=900`, visibility=visible, and the Settings panel
visually appeared on screen for the first time since v1.5.76.

## v1.5.97 — 2026-05-11
**P0 freeze fix — kanıtlandı.** Bütün önceki teoriler (LAN sync infra,
DB-lock contention, spawn_blocking boşlukları) YANLIŞTI. v1.5.94/96'da
shipped olan canlı diagnostic overlay ile WebView2 içinden `openSettings`
çağrıldığında modal'ın gerçekten `class="modal-overlay open"`
+`display:flex` alıyor ama `computed opacity:0`'da kaldığı kanıtlandı.

Root cause: **WebView2 148.x compositor regression** —
`backdrop-filter:blur(6px)` aynı element üzerinde transitioned
`opacity` ile birlikte olunca compositor opacity'i 1'e geçirmiyor.
Modal "açılıyor" (class flipliyor, `pointer-events:all` aktif) ama
görünmez kalıyor. Kullanıcı Settings / Tag Manager / Import / People /
herhangi bir folder'a tıkladığında — modal görünmeden tüm tıklamaları
yutuyor → "full kilit" raporu.

Fix tek satır CSS:
- `.modal-overlay`'den `backdrop-filter:blur(6px)` kaldırıldı
  (background zaten `rgba(8,4,4,.85)` ile karartıyor)
- `will-change:opacity` eklendi → compositor transition'ı fast-path'te
  tutuyor

v1.5.96 diagnostic'i `opacity=1 display=flex *** CSS FIX WORKED ***`
yazdı; v1.5.97 temiz binary (diagnostic kaldırıldı).

LAN sync revert'i (v1.5.92) yerinde kalıyor — onlar yanlış teşhise
göre yapılmıştı; gerçek freeze WebView2 CSS bug'ıydı, bağlantısız.

## v1.5.91 — 2026-05-11
**Diagnostic invoke timing wrapper.** v1.5.89+v1.5.90'da DB komutları
spawn_blocking ile sarıldı ama kullanıcı hâlâ "full kilitleniyor"
diyor. Hangi komutun yavaş olduğunu bulmak için global invoke wrap'i
eklendi: 600 ms'den uzun süren her IPC çağrısı `console.warn` +
kullanıcıya görünür toast ile bildiriliyor (`Slow: get_photos took
3.2s`). Hata atan invoke `console.error` ile log'lanıyor. Böylece
freeze pattern'i atomik komuta indirgenebiliyor.

## v1.5.90 — 2026-05-11
**Settings sonrası sidebar freeze fix** — v1.5.89 sadece Settings'i açtı
ama sidebar tıklamaları (People listesindeki bir isim, Tag Manager,
Import from Device) hâlâ kilitleniyordu. `get_persons` + `get_all_tags`
de spawn_blocking ile sarıldı. Ayrıca production build'de WebView2
DevTools (sağ-tık → Inspect veya F12) artık aktif — kalan herhangi bir
yavaş çağrı kullanıcı tarafında doğrudan görülebiliyor.

## v1.5.89 — 2026-05-11
**P0 Settings freeze fix** — `Settings`'e basıldığında program kilitleniyordu.
v1.5.72'de `get_photos` / `get_stats` / `get_folders` için yapılmış olan
`spawn_blocking` sarmalı `get_settings`, `get_provider_statuses`,
`get_watch_folders`, `get_budget_status`, `sync_get_state`,
`sync_set_device_name`, `sync_list_peers` komutlarına uygulanmamıştı —
`std::sync::Mutex::lock` async runtime worker thread'i park ediyor, Tauri
IPC dispatcher'ı bloke oluyor, Settings modal'ı açılmıyordu (kullanıcı
gözünden "freeze"). LAN sync etkinleştirilince mDNS broadcaster ve HTTP
server'ın DB lock'una ek talebi durumu kötüleştiriyordu. Hepsi
`tauri::async_runtime::spawn_blocking` ile sarıldı.

## v1.5.88 — 2026-05-11
Settings → About now has direct links to the project repo, the LAN sync
wire-protocol doc, and the GitHub issue tracker.

## v1.5.87 — 2026-05-11
Each paired peer in Settings → Network Sync gets a **Test** button that
pings the device and shows inline reachable/unreachable status.

## v1.5.86 — 2026-05-11
Disabling Network Sync while peers are paired now prompts a confirmation
dialog explaining the consequence ("They stay paired but cannot reach
you until you re-enable").

## v1.5.85 — 2026-05-11
Pair code now shows a live TTL countdown ("Expires in 4:23") and turns
red on expiry. Timer is cleared when Settings closes.

## v1.5.84 — 2026-05-11
Pair code and address fields in Settings → Network Sync are now
click-to-copy with a toast confirmation. Saves the user from selecting
6 digits manually.

## v1.5.83 — 2026-05-11
Settings → Network Sync "Nearby on Wi-Fi" list auto-refreshes every 5
seconds while the pane is visible. Stopped on Settings close / disable.

## v1.5.82 — 2026-05-11
`sync_get_state` now enumerates non-loopback IPv4 interfaces and ships
them as `local_ips`. The UI surfaces the actual LAN address
(`192.168.1.42:43210`) instead of the `<your local IP>` placeholder.

## v1.5.81 — 2026-05-11
**Full LAN Sync (Phase-1)** — freeze regression from v1.5.76 RESOLVED
via a 5-round bisect. Root cause was build cache corruption, not the
code. v1.5.81 ships:

- Ed25519 device identity (mint-on-first-enable, secret stays local)
- mDNS-SD broadcast on `_retinatag._tcp.local.`
- axum HTTP server with `/ping` + `/pair` endpoints
- 6-digit pair code (5-min TTL, single-use, constant-time compare)
- `sync_identity` + `sync_peers` tables
- 10 Tauri commands (`sync_get_state`, `sync_enable`, …)
- Settings → Network Sync (Beta) UI

## v1.5.80, v1.5.79, v1.5.78, v1.5.77 — 2026-05-11
Intermediate bisect steps that landed parts of the LAN sync stack
incrementally to identify which v1.5.76 change broke the WebView. Each
was verified UI-responsive before publishing.

## v1.5.76 — abandoned
Shipped LAN sync in one big commit. Froze the WebView on launch.
Rolled back; see v1.5.81 for the rebuilt, working version.

## v1.5.75 — 2026-05-10
Audit cleanup:

- `watcher.rs`: poison-tolerant `Mutex::lock` so a sibling thread panic
  can't kill file-watching for the session.
- `auto_hide_nsfw`: bounds-check `prompt_embs[0]` before indexing —
  CLIP model missing / GPU OOM now surfaces a clear error.
- Delete-to-recycle: PowerShell failures logged via `eprintln!` instead
  of swallowed by `let _ = .output()`.
- `fix_health_issues`: wipes orphan thumbnail + `.rtenc` files
  alongside the DB row delete.
- `scanner.rs`: skips walker entries with empty `file_name()` (drive
  roots like `D:\`).
- Cleanup-delete modal: keydown listener detached on every close path.
- Face-popup polls: 60 s hard cap so a popup race doesn't leave a
  timer spinning forever.
- Gallery shortcuts (p/n/g/s/r/f/x/[/]/0–5/…) gated on input-focus
  check. Typing in the description editor / filename rename / search
  no longer navigates the gallery. Modifier chords (Ctrl+S/Z/K) still
  pass through.
- `lbNav`: drops the previous photo's `<img src>` before loading the
  next so the WebView doesn't hold base64 data URLs from
  `get_private_photo_data` across vault-lightbox navigation.

## v1.5.74 — 2026-05-10
P1 freeze + data-integrity fixes:

- `recognize_all_faces`: wrapped in `spawn_blocking`. The cosine-sim
  fan-out + per-match `db.lock()` + `insert_tags` + emit used to run
  inline on the async worker thread; UI froze for minutes on 50K
  libraries.
- `watcher::process_new_files`: only holds the db mutex around the
  dedup query and final insert. `image_dimensions` / EXIF / ffprobe /
  thumbnail generation run lock-free.
- `apply_rename`: wrapped in one transaction with per-row on-disk
  rollback. Eliminates the "photo disappeared from gallery" outcome
  of a half-applied batch rename.
- `vault_files::decrypt_to_bytes`: propagates `read_to_end` errors
  verbatim instead of swallowing them and surfacing the disk error as
  a generic "auth tag mismatch / vault corrupt" message.
- Search pipeline: monotonic `_searchSeq` guard inserted after every
  await. Fast typing in a 60K library no longer lands on results for
  a query the user has already typed past.
- `toast` / `toastAction`: HTML-escape `msg` + `label` before
  innerHTML interpolation.

## v1.5.73 — 2026-05-10
**Vault P0 leaks + single-instance + XSS hardening.**

- New helper `move_photo_private(db, photo_id, new_private, kek,
  thumbs_dir)` — single source of truth for the encrypt + DB-flip
  pattern; rolls back the on-disk rename if the DB write fails.
- Patched call sites (all P0 leaks where `set_photo_private(true)`
  was called without encrypting):
  - `batch_set_private` (the "🔒 Vault" multi-select button)
  - `toggle_photo_private` (now hard-fails if vault locked)
  - `auto_hide_nsfw`
- `tauri-plugin-single-instance` 2.4.2 added; second launches
  activate the existing window instead of spawning a parallel process
  that fights over the SQLite WAL.
- Frontend XSS escape: filenames, tags, descriptions, meta fields
  HTML-escaped before innerHTML interpolation.
- `selectPhoto` race guard: bail out if `selectedId !== id` after the
  await — clicking photo A then B no longer lets A's late detail
  response overwrite B's panel.
- `_loadDescriptionFor`: auto-saves the previous photo's dirty
  description instead of silently dropping it.
- `created_at` null guard in detail panel render.

## v1.5.72 — 2026-05-10
**Startup-freeze fix on large libraries.**

- Frontend `init()`: `await loadPhotos()` first, then sidebar widgets
  (refreshStats / refreshFolders / refreshProviders /
  refreshCollections) fire on staggered `setTimeout(0, 80, 160, 240)`
  instead of `Promise.all` — saturating Tauri's async worker pool
  with 4 parallel `state.db.lock()` calls used to wedge IPC for 5–10
  seconds on launch.
- Backend `get_photos`, `get_photo_detail`, `get_stats`,
  `get_folders`, `get_folders_with_status`, `get_collections` wrapped
  in `tauri::async_runtime::spawn_blocking`.
- `tauri.conf.json`: `bundle.createUpdaterArtifacts: true` so `.sig`
  files are emitted next to the installer.

## v1.5.71 — 2026-05-09
Search overhaul and Trending Tags removal:

- `search_photos` English path rewritten. Stop-word filter + per-
  concept synonym groups + intersection of group result sets
  (AND-of-ORs) instead of a single OR over every synonym. Fixes
  "couple on the boat" returning everything tagged `the`, `on`,
  `love`, or `gemi`.
- Path/description search uses `content_words` (not the full synonym
  set), so stop words don't contribute to OR-match noise.
- `quick_translate_contextual`: added Turkish vehicle terms
  (`tekne`→boat, `gemi`→ship, `yat`→yacht, …) with sea/ocean/marina/
  harbor context tags.
- Removed Trending Tags panel (UI request).

---

For older versions, see git history.
