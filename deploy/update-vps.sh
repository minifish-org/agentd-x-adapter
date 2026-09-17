#!/usr/bin/env bash
set -euo pipefail
host=${1:-singapore}
root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"
test -z "$(git status --porcelain)" || { echo 'Commit and push changes before deploying.' >&2; exit 1; }
test "$(git branch --show-current)" = main || { echo 'Deploy from main.' >&2; exit 1; }
git fetch origin main
revision=$(git rev-parse HEAD)
test "$revision" = "$(git rev-parse origin/main)" || { echo 'Local main must equal origin/main.' >&2; exit 1; }
remote=$(git remote get-url origin)
# Pass values as shell-quoted positional arguments, never interpolate untrusted shell code.
printf -v remote_args '%q %q' "$revision" "$remote"
ssh "$host" "bash -s -- $remote_args" <<'REMOTE'
set -euo pipefail
revision=$1
remote=$2
root=/opt/agentd-x-adapter
sudo test -s /etc/agentd-x-adapter.env || { echo 'Provision /etc/agentd-x-adapter.env first.' >&2; exit 1; }
if ! test -d "$root/.git"; then
  sudo mkdir -p "$root"
  sudo chown "$(id -un):$(id -gn)" "$root"
  git clone "$remote" "$root"
fi
cd "$root"
test -z "$(git status --porcelain)" || { echo 'Remote checkout is dirty.' >&2; exit 1; }
git fetch origin main
test "$(git rev-parse origin/main)" = "$revision" || { echo 'Remote main changed; retry deployment.' >&2; exit 1; }
git checkout main
git merge --ff-only "$revision"
export PATH="$HOME/.cargo/bin:$PATH"
cargo build --locked --release --jobs 1
sudo mkdir -p "$root/bin"
sudo install -m 755 target/release/agentd-x-adapter "$root/bin/$revision"
old=$(readlink "$root/bin/current" || true)
unit=/etc/systemd/system/agentd-x-adapter.service
backup=$(mktemp)
had_unit=false
if sudo test -f "$unit"; then sudo cat "$unit" > "$backup"; had_unit=true; fi
rollback() {
  if test -n "$old"; then
    sudo ln -sfn "$old" "$root/bin/rollback"
    sudo mv -Tf "$root/bin/rollback" "$root/bin/current"
  fi
  if "$had_unit"; then sudo install -m 644 "$backup" "$unit"; else sudo rm -f "$unit"; fi
  sudo systemctl daemon-reload
  if test -n "$old"; then sudo systemctl restart agentd-x-adapter; else sudo systemctl stop agentd-x-adapter || true; fi
}
trap 'rollback; rm -f "$backup"' ERR
sudo install -m 644 deploy/agentd-x-adapter.service "$unit"
sudo ln -sfn "$revision" "$root/bin/next"
sudo mv -Tf "$root/bin/next" "$root/bin/current"
sudo systemctl daemon-reload
sudo systemctl restart agentd-x-adapter
sleep 5
sudo systemctl is-active --quiet agentd-x-adapter
sudo systemctl enable agentd-x-adapter
trap - ERR
rm -f "$backup"
echo "Deployed $revision. Check the journal and preview output before enabling publishing."
REMOTE
