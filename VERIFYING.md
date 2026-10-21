# Verifying random.cash Releases

Docker images published by CI are cryptographically signed, so you can check
that the image you are about to run was built by this repository's pipeline from
a known commit — not substituted somewhere between here and your registry pull.

**Images are the only signed artifact today.** Binary tarballs and signed git
tags are not published; see [What is not signed](#what-is-not-signed) below, which
is there so you know what you are *not* getting rather than assuming.

## Prerequisites

Install [cosign](https://docs.sigstore.dev/cosign/system_config/installation/):

```sh
# Arch Linux
pacman -S cosign

# macOS
brew install cosign

# Binary
curl -sLO https://github.com/sigstore/cosign/releases/latest/download/cosign-linux-amd64
chmod +x cosign-linux-amd64 && sudo mv cosign-linux-amd64 /usr/local/bin/cosign
```

## Docker images

Signed with cosign in keyless (OIDC) mode: there is no private key: the signing
identity is the GitHub Actions job itself, and the signature is recorded in the
public Sigstore transparency log (Rekor).

```sh
TAG=sha-a1b2c3d   # or a release tag such as v0.1.0

# Server (API + migrations)
cosign verify \
  --certificate-identity-regexp "^https://github.com/randomcash/ethpayserver/\.github/workflows/ci\.yml@refs/(heads|tags)/.*$" \
  --certificate-oidc-issuer "https://token.actions.githubusercontent.com" \
  ghcr.io/randomcash/ethpayserver:$TAG

# EVM monitor
cosign verify \
  --certificate-identity-regexp "^https://github.com/randomcash/ethpayserver/\.github/workflows/ci\.yml@refs/(heads|tags)/.*$" \
  --certificate-oidc-issuer "https://token.actions.githubusercontent.com" \
  ghcr.io/randomcash/ethpayserver/evmmonitor:$TAG
```

The checkout client is published from its own repository,
[payserver-client](https://github.com/gcannelloni/payserver-client), and signed
the same way — note the different identity:

```sh
cosign verify \
  --certificate-identity-regexp "^https://github.com/randomcash/payserver-client/\.github/workflows/ci\.yml@refs/(heads|tags)/.*$" \
  --certificate-oidc-issuer "https://token.actions.githubusercontent.com" \
  ghcr.io/randomcash/payserver-client:$TAG
```

The tag this payserver **tests against** is pinned in `ops/client-image.pin`.
It is not necessarily the tag serving production — the frontend deploys
independently.

### Pinning the identity harder

The regexes above accept any branch or tag. To require a specific one — which is
what you want if you only ever run releases — drop the regex and match exactly:

```sh
cosign verify \
  --certificate-identity "https://github.com/randomcash/ethpayserver/.github/workflows/ci.yml@refs/tags/v0.1.0" \
  --certificate-oidc-issuer "https://token.actions.githubusercontent.com" \
  ghcr.io/randomcash/ethpayserver:v0.1.0
```

### Tags, digests, and what is actually signed

Signatures are made against the image **digest**, not the tag. A tag is a
movable pointer, so "this tag was signed" is not a claim about the bytes you
pull. `cosign verify` resolves a tag to its current digest and checks the
signature on that, so verifying by tag is safe — but if you want to remove all
doubt, verify and deploy by digest:

```sh
DIGEST=$(crane digest ghcr.io/randomcash/ethpayserver:$TAG)
cosign verify ... ghcr.io/randomcash/ethpayserver@$DIGEST
```

Tags follow `sha-<short-sha>` (immutable, one per commit) or a release tag.
`testnet-latest` / `main-latest` move and nothing should deploy from them.

## What the signature proves

A successful `cosign verify` tells you the image was built and pushed by the
`ci.yml` workflow in the named repository, on a commit reachable from the branch
or tag embedded in the certificate, and that this was recorded publicly in Rekor
at the time of signing.

It does **not** tell you the source code is free of defects, that the commit was
reviewed, or that the person who pushed that commit was authorised. It binds
artifact to pipeline, nothing more.

## What is not signed

Listed explicitly, because a verification page that quietly omits things invites
the assumption that everything is covered.

| Artifact | Status |
|----------|--------|
| Docker images | **Signed** (keyless cosign, GitHub Actions OIDC) |
| Binary tarballs | **Not published.** Releases ship images only. Any tarball claiming to be a random.cash release binary did not come from this pipeline. |
| Git tags | **Not signed.** Tags are lightweight and carry no GPG signature. Verify the commit through the image signature instead. |
