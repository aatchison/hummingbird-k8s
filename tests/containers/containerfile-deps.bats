#!/usr/bin/env bats
#
# Static drift-fence tests for the bootc Containerfiles' package install
# layer. Codifies in-image dependencies that are load-bearing for operator
# scripts running on the bootc host — if someone removes them in a future
# layer-trim refactor, this test fires before the build silently regresses
# operator behavior.
#
# Current fences:
#   1. `jq` is present in the PRIMARY `dnf install -y` package list of
#      containers/k8s/Containerfile and containers/k8s-worker/Containerfile.
#      jq is required by:
#        - `hbird update-cluster`'s bootc_booted_digest() helper (post-#353
#          cutover; was scripts/update-cluster.sh), which pipes `bootc
#          status --json` to `jq -r '.status.booted.image.imageDigest // ...'`
#          to detect "no actual update available" and short-circuit the
#          bootID-changed gate. Without jq the digest reads empty, both
#          pre/post values look identical-but-<unknown>, and the bootID
#          gate runs the full READY_TIMEOUT before failing (issue #257).
#        - containers/shared/bootc-semver-update.sh, which uses jq to
#          parse skopeo tag-list output and `bootc status --json` (#181).
#      We assert it lives in the SAME `dnf install -y` line as
#      kubeadm/kubelet/kubectl so it can't accidentally end up in a
#      conditional/optional block again (the pre-#257 placement inside
#      the bootc-semver-update layer worked but was unobvious; this fence
#      keeps jq pinned to the primary package layer).
#   2. Fedora era pinning (#397/#399): the build-time Fedora repos are pinned
#      to the base image's release era via `ARG FEDORA_RELEASE`. These fences
#      assert (a) no Rawhide metalink sneaks back in, (b) both flavors agree
#      on one era, (c) the era matches a vendored GPG key, (d) the build-time
#      base-era assertion is present, and (e) every Fedora repo stanza keeps
#      the exclude list that shields the pkgs.k8s.io version locks
#      (cri-o, cri-tools — incl. Fedora's versioned names like cri-tools1.35,
#      hence the glob — and kubernetes-cni via containernetworking-plugins).
#
# Run via:
#   bats tests/containers/containerfile-deps.bats
# OR:
#   podman run --rm -v "$PWD:/repo:Z" -w /repo \
#     docker.io/bats/bats:latest tests/containers/

setup() {
  REPO_ROOT="$(cd "$BATS_TEST_DIRNAME/../.." && pwd)"
  CP_CONTAINERFILE="$REPO_ROOT/containers/k8s/Containerfile"
  WORKER_CONTAINERFILE="$REPO_ROOT/containers/k8s-worker/Containerfile"
}

# Extract the `RUN dnf install -y \` continuation block that includes
# kubeadm. Returns the joined-on-spaces single-line form so we can grep
# for `jq` as a standalone word.
_extract_primary_dnf_install() {
  local file="$1"
  awk '
    /^RUN dnf install -y/ { in_run=1; block=""; }
    in_run {
      block = block " " $0
      # End of the RUN block: line without trailing backslash.
      if ($0 !~ /\\[[:space:]]*$/) {
        # Only care about the block that installs kubeadm — that is the
        # canonical primary package layer for both Containerfiles.
        if (block ~ /kubeadm/) {
          print block
          exit
        }
        in_run = 0
        block = ""
      }
    }
  ' "$file"
}

@test "containers/k8s/Containerfile: jq is in the primary dnf install -y line (with kubeadm)" {
  block="$(_extract_primary_dnf_install "$CP_CONTAINERFILE")"
  [ -n "$block" ] || {
    echo "could not locate primary 'RUN dnf install -y' block containing kubeadm"
    return 1
  }
  # Word-boundary match: `jq` as its own token in the install list.
  # The trailing space avoids matching e.g. `jqfoo` or `libjq-1.6` (neither
  # exists today, but the fence is cheap).
  [[ " $block " == *" jq "* ]] || {
    echo "jq missing from primary dnf install in containers/k8s/Containerfile"
    echo "block was: $block"
    return 1
  }
}

@test "containers/k8s-worker/Containerfile: jq is in the primary dnf install -y line (with kubeadm)" {
  block="$(_extract_primary_dnf_install "$WORKER_CONTAINERFILE")"
  [ -n "$block" ] || {
    echo "could not locate primary 'RUN dnf install -y' block containing kubeadm"
    return 1
  }
  [[ " $block " == *" jq "* ]] || {
    echo "jq missing from primary dnf install in containers/k8s-worker/Containerfile"
    echo "block was: $block"
    return 1
  }
}

# Symmetry check — both Containerfiles should install the same set of
# operator-critical helpers in their primary layer. Today that's only jq;
# if a second helper joins this fence, extend the loop. Keeps CP + worker
# from drifting apart.
@test "containerfiles: CP and worker primary dnf install lines both include jq" {
  cp_block="$(_extract_primary_dnf_install "$CP_CONTAINERFILE")"
  worker_block="$(_extract_primary_dnf_install "$WORKER_CONTAINERFILE")"
  for tool in jq; do
    [[ " $cp_block " == *" $tool "* ]] || {
      echo "containers/k8s/Containerfile: $tool missing"
      return 1
    }
    [[ " $worker_block " == *" $tool "* ]] || {
      echo "containers/k8s-worker/Containerfile: $tool missing"
      return 1
    }
  done
}

# ---- Fedora era pin fences (#397/#399) ----------------------------------

# Extract the value of `ARG FEDORA_RELEASE=` from a Containerfile.
_fedora_release_arg() {
  sed -n 's/^ARG FEDORA_RELEASE=\([0-9][0-9]*\)$/\1/p' "$1" | head -1
}

@test "containerfiles: no Rawhide metalink (the #397 regression)" {
  for f in "$CP_CONTAINERFILE" "$WORKER_CONTAINERFILE"; do
    if grep -q 'metalink=[^ ]*repo=rawhide' "$f"; then
      echo "$f: found a Rawhide metalink — Fedora repos must stay pinned to the base's release era (#397)"
      return 1
    fi
  done
}

@test "containerfiles: CP and worker agree on one ARG FEDORA_RELEASE" {
  cp_era="$(_fedora_release_arg "$CP_CONTAINERFILE")"
  worker_era="$(_fedora_release_arg "$WORKER_CONTAINERFILE")"
  [ -n "$cp_era" ] || { echo "containers/k8s/Containerfile: ARG FEDORA_RELEASE missing"; return 1; }
  [ -n "$worker_era" ] || { echo "containers/k8s-worker/Containerfile: ARG FEDORA_RELEASE missing"; return 1; }
  [ "$cp_era" = "$worker_era" ] || {
    echo "FEDORA_RELEASE era skew: CP=$cp_era worker=$worker_era — bump both flavors together"
    return 1
  }
}

@test "containerfiles: vendored GPG key exists for the pinned era" {
  era="$(_fedora_release_arg "$CP_CONTAINERFILE")"
  key="$REPO_ROOT/containers/shared/gpg/RPM-GPG-KEY-fedora-${era}-primary"
  [ -f "$key" ] || {
    echo "missing $key — vendor the era's key when bumping FEDORA_RELEASE (#399)"
    return 1
  }
  head -1 "$key" | grep -q 'BEGIN PGP PUBLIC KEY BLOCK' || {
    echo "$key is not an ASCII-armored PGP key"
    return 1
  }
}

@test "containerfiles: build-time base-era assertion is present in both flavors" {
  for f in "$CP_CONTAINERFILE" "$WORKER_CONTAINERFILE"; do
    grep -q 'rpm -E %fedora' "$f" || {
      echo "$f: missing the rpm -E %fedora era assertion — it is the loud-failure fence for base/repo era drift (#397)"
      return 1
    }
  done
}

@test "containerfiles: every Fedora repo stanza carries the pkgs.k8s.io-shielding exclude" {
  # Each repo-writing printf line holds two stanzas (release + updates); the
  # exclude must appear once per metalink in that same line so neither stanza
  # can silently drop it.
  for f in "$CP_CONTAINERFILE" "$WORKER_CONTAINERFILE"; do
    while IFS= read -r line; do
      metalinks="$(printf '%s' "$line" | grep -o 'metalink=https://mirrors.fedoraproject.org/metalink' | wc -l)"
      excludes="$(printf '%s' "$line" | grep -o 'exclude=cri-o,cri-tools\*,containernetworking-plugins' | wc -l)"
      [ "$metalinks" -eq "$excludes" ] || {
        echo "$f: a Fedora repo printf has $metalinks metalink stanza(s) but $excludes exclude line(s)"
        echo "line: $line"
        return 1
      }
    done < <(grep 'metalink=https://mirrors.fedoraproject.org/metalink' "$f")
  done
}
