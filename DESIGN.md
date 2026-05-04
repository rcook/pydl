# Design: how `pydl` verifies assets

This document captures the trade-offs behind `pydl`'s current verification approach so future maintainers understand *why* the bake-at-build-time model was chosen over alternatives that might look simpler on paper. The short version: the current design optimises for tamper-resistance against a hostile network, trading that for a "stale set" maintenance cost whenever upstream cuts a new release.

## Current design: baked-in checksums

Every `.sha256sums` file under `./checksums/` is embedded verbatim into the `pydl` binary at build time. At runtime, `pydl download` / `pydl install` look up the expected hash by `(tag, asset_name)` and refuse any asset whose bytes don't match. A missing checksum is a hard error, not a pass-through.

**Strengths:**

1. **Tamper-resistance is strong.** The checksum set is captured at build time from a reviewed `./checksums/` directory and turned into a `&'static str` literal inside the binary. To tamper with it, an attacker needs to either modify the committed `.sha256sums` files (visible in git history) or compromise the build pipeline that produced the binary the user is running. Neither is possible with just the in-flight network.
2. **Air-gapped / offline.** No runtime network round-trip to fetch checksums before verifying a download. If GitHub is down or the user is behind a captive portal, verification still works.
3. **Fast path is simple.** The `expected_hash()` lookup is a `HashMap::get` against embedded strings — no caching, revalidation or "is this checksum stale?" logic to get wrong.
4. **Single trust root.** The only thing a user needs to trust is the `pydl` binary itself. If they got it from a signed source (a GitHub release artifact, a Homebrew bottle), the checksum set inherits that trust transitively.
5. **Auditable.** `./checksums/` is a small, human-readable directory. A reviewer can diff it across releases. Commits that add a checksum for a not-yet-released tag would stand out.

**Weaknesses:**

1. **Stale set problem.** A `pydl` binary from April can't install a build tagged in July. The `get-checksums` workflow mitigates it but requires human effort every time upstream cuts a release, and creates a silent cliff for old binaries users may have installed.
2. **Binary bloat.** ~80 × ~21 KB files ≈ 1.6 MB of UTF-8 literals baked in today, growing linearly with upstream's release cadence. Not huge, but most of `pydl`'s binary size.
3. **Coupling of release cadence.** `pydl`'s effective "supported set" of Python builds is frozen at its build time. Shipping any fix *and* keeping `pydl` useful against fresh upstream releases collapses two independent cycles into one.
4. **No way to add trust incrementally.** If someone reports a new tag is missing, the only user-visible answer is "rebuild from source" or "wait for a new `pydl` release".
5. **Commit-log noise weakens "commits get reviewed".** Every upstream release produces a PR adding one `<tag>.sha256sums` file. The diffs are SHA-256 noise and nobody spot-checks them in practice. The formal control is weaker than it looks. *This is what the standalone [`check-checksums`](./check-checksums/README.md) binary exists to mitigate — it replaces human review with CI-enforced bit-exact agreement with upstream.*

## Alternatives considered

All of these keep the core requirement — *the checksum must be trusted* — but move or change where that trust lives. Each was evaluated and **not** adopted; the reasons are recorded here so future proposals don't re-tread the same ground.

### A. Fetch `SHA256SUMS` at runtime, verify a signature over it

Upstream `python-build-standalone` signs releases. Instead of baking per-release `SHA256SUMS`, bake a **single** thing: the upstream signer's public key. At install time, fetch `SHA256SUMS` and `SHA256SUMS.sigstore` for the target release, verify the signature against the baked key, then proceed with the per-asset hash check as today.

- **Pros:** binary never goes stale — any signed upstream release is verifiable; the trust root is ~500 bytes, not 1.6 MB and growing; upstream source cadence decouples from `pydl` release cadence; the signature check is a cryptographic primitive with a clear threat model, not an ad-hoc "we committed this after reviewing it" argument.
- **Cons:** runtime dep on Sigstore / cosign verification (adds multiple MB and a non-trivial audit surface); offline use needs the `SHA256SUMS` and `.sigstore` files cached alongside the assets; the trust root is still baked in, just smaller — key rotations still require a rebuild; some users don't trust Sigstore's log infrastructure; depends on upstream signing reliably, which today they don't do in a format `pydl` could consume cleanly.
- **Verdict:** the cleanest long-term design *if* upstream signs releases reliably. Revisit when `astral-sh/python-build-standalone` ships consistent Sigstore signatures.

### B. Bake only a root-of-trust hash; fetch `SHA256SUMS` at runtime

Keep `./checksums/` in the repo, but commit one small file: a manifest listing each tag with the SHA-256 of *its* `SHA256SUMS`. Embed that manifest. At install time, fetch the upstream `SHA256SUMS` for the target release, hash it, compare against the embedded manifest entry, then do the per-asset check.

- **Pros:** binary footprint is much smaller (64 hex bytes × N releases vs. full `SHA256SUMS` bodies); still offline after first fetch (the per-tag `SHA256SUMS` gets cached in `~/.pydl/cache/`); same review surface as today.
- **Cons:** still has the stale-set problem — every upstream release still needs a PR to add a manifest entry. Effectively the same workflow as today with an extra indirection step; doesn't solve the core complaint.
- **Verdict:** a size optimisation over the current design, not a semantic improvement. Not worth the complexity unless binary bloat becomes acute.

### C. Maintain a signed `checksums.json` as a separate artefact

The `pydl` maintainer signs a `checksums.json` published at a well-known URL (or a `gh-pages` branch populated by `get-checksums`). `pydl` fetches it, verifies the signature against a baked public key and caches it.

- **Pros:** decoupled from upstream's signing practices — works whether upstream signs or not; maintainer controls refresh cadence independently of `pydl` binary releases; clear trust model (users trust *this project's* signing key); simple recovery story for key rotation (ship a `pydl` update with the new key).
- **Cons:** key management becomes an operational responsibility — the current design has zero running secrets, this adds one; a stolen signing key is a supply-chain attack vector (hardware keys mitigate but don't eliminate); users now have two things to trust (binary + signing key) instead of one; requires signing pipeline, key revocation story, a published `checksums.json`.
- **Verdict:** appropriate if `pydl` wants to keep working across many upstream releases without rebuilds, and the maintainer is willing to take on signing-infrastructure ownership. For a small project this is a lot of operational surface.

### D. TOFU (trust-on-first-use) with per-asset fetch

On first install of a tag, fetch `SHA256SUMS`, prompt the user to confirm, record it to `~/.pydl/checksums-cache/<tag>.sha256sums`. Subsequent installs of the same tag use the cached file. Like SSH known-hosts.

- **Pros:** zero maintenance — fresh releases Just Work without any `pydl` or `./checksums/` changes; binary stays small and version-independent; clear audit trail in the user's home directory.
- **Cons:** moves the trust decision to the user, who's typically a bad judge ("press y to continue" is almost always pressed); no protection on the very first install — a MITM during the first fetch is undetectable; changes the UX semantics meaningfully (sometimes prompts, sometimes doesn't).
- **Verdict:** worse than the current design for this threat model. Prioritises fresh-release ergonomics over tamper-resistance, which is the opposite trade from the one being made.

### E. No runtime verification; rely on TLS + GitHub as the trust anchor

Skip the checksum step entirely. Use HTTPS fetching (with certificate pinning if paranoid) and trust GitHub's asset hosting as the source of truth.

- **Pros:** trivially simple; no checksums to manage anywhere.
- **Cons:** exactly the scenario this design exists to prevent. Anyone who can compromise the transport — a rogue root CA, a misbehaving CDN, a GitHub account breach via OAuth — can silently ship tampered bytes.
- **Verdict:** not a real alternative for this threat model.

## Where the current design sits

The bake-at-build-time approach buys strong tamper-resistance and pays for it with maintenance cost (`get-checksums` must be run periodically) and binary size (growing ~21 KB per upstream release). The standalone [`check-checksums`](./check-checksums/README.md) binary, run in CI, closes the weakest link in the "commits get reviewed" argument by turning it into bit-exact agreement with upstream, verified on every PR and nightly. If upstream's signing practices stabilise in a form `pydl` can consume, **alternative A** is the cleanest upgrade path; until then, the current design is the honest choice for the threat model.
