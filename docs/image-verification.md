# Image verification (cosign keyless)

The OCI images published by this repo are signed with
[cosign](https://github.com/sigstore/cosign) using **keyless** OIDC signing:
no long-lived signing key exists. Each signature is bound to:

- the GitHub Actions workflow that produced it,
- the repository (`aatchison/hummingbird-k8s`),
- the Sigstore Fulcio CA + the public Rekor transparency log.

## Why this matters

A consumer running `bootc switch ghcr.io/aatchison/hummingbird-k8s:vX.Y.Z`
otherwise has no way to know that the image they pull was actually built
from this repo's `main` by this repo's workflows. Without verification, a
compromised registry credential or a typosquatted tag could substitute a
malicious image. Verifying the cosign signature gives a cryptographic
guarantee that the image came from a workflow in this repo that was issued
an OIDC token by GitHub.

## Trust model caveat

Keyless signing trusts **any** workflow in this repo that has
`id-token: write`. Today only `.github/workflows/build-*.yml` request that
permission, and they only run on tag pushes (which are gated to commits
reachable from `main`). If you add a new workflow that requests
`id-token: write`, that workflow can also produce signatures attributed to
this repo. Reviewers: treat new `id-token: write` grants as
security-sensitive.

If you need a stricter binding, you can extend the
`--certificate-identity-regexp` below to pin the workflow path, e.g.
`^https://github.com/aatchison/hummingbird-k8s/\.github/workflows/build-k8s\.yml@` (CP) or `build-worker\.yml@` (worker).

## Verifying an image manually

```sh
# Example: verify hummingbird-k8s vX.Y.Z
COSIGN_EXPERIMENTAL=1 cosign verify \
  --certificate-identity-regexp '^https://github.com/aatchison/hummingbird-k8s/' \
  --certificate-oidc-issuer 'https://token.actions.githubusercontent.com' \
  ghcr.io/aatchison/hummingbird-k8s:vX.Y.Z
```

Substitute the real flavor (`hummingbird-k8s` or `hummingbird-k8s-worker`)
and version tag. The command exits non-zero if
the image is unsigned, signed by a different identity, or not present in
the Rekor transparency log.

You can also verify by digest, which is what the publish workflow signs:

```sh
COSIGN_EXPERIMENTAL=1 cosign verify \
  --certificate-identity-regexp '^https://github.com/aatchison/hummingbird-k8s/' \
  --certificate-oidc-issuer 'https://token.actions.githubusercontent.com' \
  ghcr.io/aatchison/hummingbird-k8s@sha256:<digest>
```

## Single-arch caveat

Today's builds are single-arch (`linux/amd64`), so a single digest covers
the only manifest that exists. If multi-arch publishing is added later,
the publish workflow must sign **both** the manifest index AND each child
manifest, and verifiers must check whichever one they pull.

## Enforcing verification in a `bootc switch` consumer

To make a host refuse unsigned or wrongly-signed images, drop a sigstore
policy under `/etc/containers/policy.json` (or a `registries.d/` snippet
that points at it). Minimal example:

```json
{
  "default": [{"type": "reject"}],
  "transports": {
    "docker": {
      "ghcr.io/aatchison/hummingbird-k8s": [
        {
          "type": "sigstoreSigned",
          "fulcio": {
            "caData": "<base64 Fulcio root CA>",
            "oidcIssuer": "https://token.actions.githubusercontent.com",
            "subjectRegExp": "^https://github.com/aatchison/hummingbird-k8s/"
          },
          "rekorPublicKeyData": "<base64 Rekor public key>"
        }
      ]
    }
  }
}
```

With this in place, `bootc switch ghcr.io/aatchison/hummingbird-k8s:vX.Y.Z`
will fail closed if the image is missing a valid signature from this repo's
workflows. The Fulcio root CA and Rekor public key are available from
<https://github.com/sigstore/root-signing>.
