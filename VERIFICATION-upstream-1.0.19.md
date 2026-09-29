# v1.0.16–v1.0.19 port verification

Branch: `port/upstream-useful-1.0.19`, HEAD unchanged `8da8727`. No commit/push/install/app launch or production Apple/notification request.

## Included
- App-only persistent query profile, explicit exclusive owner lock, owned-process shutdown before unlock; retains Cookie data, never copies personal/buyer data or scans/deletes legacy directories.
- Upstream real Chromium identity and CDP preparation; nearby city/model planning <=20 parts, exact store fallback; pickup/delivery endpoint isolation and per-address delivery caches.
- Clash/mihomo explicit opt-in dedicated group/port, pinned/manual/rotating nodes, latency check and per-site route cooldown; local controller only, rejects common global groups. No system proxy/Shadowrocket configuration writes.
- Store/product refresh and restart cache, partial failure retention, China store city display and target hydration; folded preferences/network settings in existing design.
- Backoff default ON (existing failed-cycle/region protection); explicit OFF gives exact user interval without jitter. Per-cycle short circuit, two-second request gate and route cooldown remain. Repeated blocked requests may extend limits.

## Deliberately not included
- Upstream automatic legacy Chromium-process reclamation (may kill an unowned process), wholesale UI/brand/version changes, route-specific catalog probing/fallback (catalog keeps system HTTP), education bag, auto-start.
- Shadowrocket has no direct Clash control API; routes are implemented and offline tested but NOT configured or tested against the user's real proxy/Apple service.

## Actual final acceptance
- `cargo test --workspace`: 319 passed / 0 failed / 19 ignored. Ignored live tests were NOT run. Final rerun after the quit barrier fix.
- `pnpm test`: 21 passed / 0 failed.
- `pnpm exec tsc --noEmit`: exit 0.
- `python3 tests/query-baseline-contract.py`: 4 tests OK.
- `python3 tests/query-baseline-mutation.py`: PASS (its deliberate in-memory browser-path mutation produces expected FAIL, script exits 0).
- `python3 tests/manual-start-contract.py`: 2 tests OK.
- `pnpm tauri build --bundles app`: exit 0; ad-hoc signed. Notarization skipped because Apple developer credentials are not configured.
- `codesign --verify --deep --strict --verbose=2`: valid on disk / satisfies Designated Requirement.
- App: `/Users/fangyang/Projects/fruit-radar/target/release/bundle/macos/水果雷达.app`
- Final signed bundle executable SHA256: `42dfadd108eb9dc25ba1606f9d0a33249d9e77907e2acbaf9d2cdd8e6f467a34`.
- Standalone release executable SHA256: `1af864cea4a4ce13f4535ae3577b0849aab93119a81fdad4362ad948341be40f`. Bundling signs the copied executable, so do not require standalone/bundle byte equality; signed bundle hash above is the deliverable.
- Bundle: com.amtmac.fruitradar, version 1.1.1 unchanged.
- Backend binary and frontend dist marker checks passed; no real UI launch claimed.

## Contract change rationale
User expanded authorization supersedes old temporary-profile/no-persistence/403-destruction assertions. Browser discovery still exactly matches `3d7c56e`; CDP/preparation/request-plan portions exactly match `f86fee4`. New local-CDP + actual owned-child tests cover persistent rejection sessions, shutdown/reap/unlock and Cookie retention. Existing cooldown escalation, retry-now and UA fork tests retained. Settings schema exact-field count expanded with explicit network/backoff assertions, not relaxed.

## Logs and working tree
Earlier port logs remain at `/Users/fangyang/.hermes/cache/scratch/fruit-radar-port/`. Final acceptance and RED/GREEN logs after the quit fix are at `/Users/fangyang/.hermes/cache/scratch/fruit-radar-exit-barrier/`:
- `red.log`: targeted behavior regression, exit 101, synchronous prevention assertion expected 1 but observed 0 with a no-op seam representing the old unhandled native exit. No compile-error/source-text test.
- `green.log`: targeted regression passes after the barrier implementation.
- `cargo-final.log`, `pnpm-final.log`, `tsc-final.log`: final workspace Rust, frontend and type checks, all exit 0.
- `query-contract-final.log`, `query-mutation-final.log`, `manual-start-final.log`: all three final contracts exit 0. Mutation log's deliberate FAIL is expected, followed by PASS.
- `build-final.log`, `codesign-final.log`, `sha256-final.log`: final app build, strict signature verification and executable SHA256s.
- `fix-only.diff` and `*.before`: isolated changes compared with the pre-fix dirty working tree; `VERIFICATION-auto-bag.md` is byte-identical to its before snapshot.

## P1 native Quit/Cmd-Q cleanup fix
Only this defect and its evidence were changed in this follow-up: `src-tauri/src/lib.rs`, new `src-tauri/src/exit_barrier.rs`, a behavior regression in `src-tauri/src/chromium_fetcher.rs`, and this report. All earlier uncommitted port work is preserved. HEAD remains `8da8727de4caa5e5be6ac1f3d0fc5cd48a3cd622`; no new commit was made.

- Replace `Builder::run` with `build(...).run(callback)` to handle `RunEvent::ExitRequested` from the macOS default menu/Cmd-Q as well as tray `app.exit(0)`.
- Shared atomic `RUNNING -> STOPPING -> READY` barrier calls `api.prevent_exit()` synchronously while cleanup is outstanding. Compare/exchange starts exactly one asynchronous task; repeated requests during STOPPING are prevented without another cleanup/exit task.
- Cleanup holds the existing control lock, cancels pending automation admission, then awaits the shared `shutdown_monitor`: `watcher.stop()` followed by `fetcher.shutdown()`. Existing owned-session destruction reaps only its child/process group before unlocking the persistent profile. No system directory scan, unknown PID kill or profile migration was added.
- Start commands reject once quitting has begun, preventing a late restart from recreating a query session. Only after shutdown completes is READY published and the original exit code (native None -> 0) passed to `app.exit`; the resulting ExitRequested reentry is allowed, avoiding recursion.
- Regression: `chromium_fetcher::tests::native_quit_barrier_waits_for_owned_child_and_allows_only_completed_reentry`. Uses the real Watcher, fetcher/session shutdown, local CDP peer, real owned `sleep` child and exclusive ProfileOwner in a test-only TempDir. Blocks cleanup deterministically with Notify, proves no early exit and profile remains locked/child alive, injects a repeated quit, then proves exactly one cleanup/exit, stopped watcher, closed CDP, child gone, profile immediately reclaimable and Cookie fixture preserved. The production shutdown helper and barrier are exercised, not source-code strings.
- Native GUI menu dispatch itself was not exercised: app launch is expressly forbidden. The Tauri ExitRequested API wiring compiles in the final signed app; offline behavior coverage must not be represented as a real Cmd-Q UI run.
