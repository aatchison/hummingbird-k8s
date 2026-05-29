# Security

## Reporting

Open a private security advisory via GitHub (Security → Advisories → Report a vulnerability), or file an issue with the `hardening` label for non-sensitive items.

## Known burned credential — legacy VM sudo password

Early bootc VM builds (before commit `9e1bf94`, "feat(sudoless): default to no-wheel, no-password") baked a hardcoded sudo password into `bib-config.toml` via the `build*.sh` scripts. That password was publicly disclosed in issue #2 and is preserved in the immutable history of the `k3s/v0.1.0` tag. **Treat it as compromised.**

### Current state (remediated)

- `VM_PASSWORD` is unset by default; `lib/build-common.sh` only emits the `password =` block in `bib-config.toml` when `VM_PASSWORD` is explicitly set.
- Images default to SSH-key-only auth: `wheel` dropped from default groups, `PermitRootLogin prohibit-password`, and a `99-no-passwords.conf` sshd drop-in disable password authentication.

### If you restore or run a VM built before `9e1bf94`

Such an image may still accept the burned password for `sudo` and/or SSH. On every affected VM:

```bash
# Rotate or lock the build user's password
sudo passwd "${VM_USER:-core}"      # set a fresh value, OR
sudo passwd -l "${VM_USER:-core}"   # lock password auth entirely

# Confirm SSH password auth is disabled
sudo sshd -T | grep -i passwordauthentication   # expect: passwordauthentication no
```

The history at `k3s/v0.1.0` is intentionally **not** rewritten — the credential is already public/indexed, and rewriting a tagged public release is more disruptive than the leak. Rotation on affected hosts is the correct mitigation.
