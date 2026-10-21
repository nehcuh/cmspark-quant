#!/usr/bin/env bash
# 一键部署（单机）：PostgreSQL/TimescaleDB + 审计签名进程（默认）+ 可选 Redis（--hot）
# + 工作区初始化 + 可选锚点仓库定时推送（--anchor-repo URL）。可重复执行（幂等）。
#
#   scripts/deploy.sh [--hot] [--anchor-repo https://github.com/<you>/<anchors>] [--home DIR]
#
# 结果写入 <secrets>/cmsq.env：之后 `set -a; . ~/.config/cmsq/cmsq.env; set +a` 即可使用 cmsq。
# 已有工作区若还在用应用侧 audit.key，会自动迁移进签名进程（导入→登记→经 socket 全链验证→粉碎旧密钥）。
# 不涉及任何真实下单；不读取/打印 Tushare token。
set -euo pipefail
cd "$(dirname "$0")/.."
HOT=0; ANCHOR=""; HOME_DIR=${CMSQ_HOME:-$HOME/cmsq-home}
while [ $# -gt 0 ]; do case $1 in
  --hot) HOT=1;; --anchor-repo) ANCHOR=$2; shift;; --home) HOME_DIR=$2; shift;;
  -h|--help) sed -n 2,9p "$0"; exit 0;; *) echo "unknown option $1"; exit 2;; esac; shift; done
docker info >/dev/null 2>&1 || { echo "当前用户无法访问 Docker：sudo usermod -aG docker \$USER 后重新登录（或 sg docker -c \"$0 $*\"）"; exit 1; }

export CMSQ_SECRETS_DIR=${CMSQ_SECRETS_DIR:-$HOME/.config/cmsq}
export CMSQ_HOST_UID=$(id -u) CMSQ_HOST_GID=$(id -g "$(id -un)")   # primary group, also under `sg docker`
export CMSQ_RUN_DIR=${CMSQ_RUN_DIR:-$HOME/.local/state/cmsq/run}
mkdir -p "$CMSQ_SECRETS_DIR" "$CMSQ_RUN_DIR" "$HOME/.local/bin" && chmod 700 "$CMSQ_SECRETS_DIR" && chgrp "$CMSQ_HOST_GID" "$CMSQ_RUN_DIR" && chmod 770 "$CMSQ_RUN_DIR"
C="docker compose -f deploy/docker-compose.yml"

echo "== 1/6 编译 cmsq（release）"
cargo build -q --release -p cmsq-cli
install -m 755 target/release/cmsq "$HOME/.local/bin/cmsq"; BIN=$HOME/.local/bin/cmsq

echo "== 2/6 PostgreSQL/TimescaleDB"
scripts/db_up.sh
[ -s "$HOME/.config/cmsq/runner_image_id" ] || scripts/build_runner.sh

echo "== 3/6 审计签名进程（密钥只在 cmsq-signer 卷里，uid 10001，无网络）"
mkdir -p deploy/.signer-build && cp target/release/cmsq deploy/Dockerfile.signer deploy/.signer-build/
$C build -q signer
$C up -d --wait signer
SOCK=$CMSQ_RUN_DIR/sock
SX="$C exec -T signer cmsq signer"

if [ $HOT = 1 ]; then echo "== 3b Redis 热层（profile hot）"; scripts/redis_up.sh; fi

echo "== 4/6 工作区 $HOME_DIR 与审计链登记"
export CMSQ_HOME=$HOME_DIR
unset CMSQ_AUDIT_SIGNER CMSQ_AUDIT_KEY
KF=${CMSQ_AUDIT_KEY_FILE:-$CMSQ_SECRETS_DIR/audit.key}
if [ -s "$KF" ] && [ -e "$HOME_DIR/cmsq.db" -o -d "$HOME_DIR/strategies" ]; then
  # 同一 secrets 目录下的其他工作区之后各自执行 `cmsq --home <dir> audit signer-enroll`（旧条目仍用 key 0 验证）
  echo "发现应用侧 audit.key：迁移到签名进程"
  read -r CH SEQ MAC < <("$BIN" audit signer-enroll --print)
  $SX import --dir /var/lib/cmsq-signer --key-file /dev/stdin < "$KF"
  $SX enroll --dir /var/lib/cmsq-signer --chain "$CH" --seq "$SEQ" --mac "$MAC"
  $C restart signer >/dev/null && $C up -d --wait signer
  CMSQ_AUDIT_SIGNER=$SOCK "$BIN" audit verify   # 失败则 set -e 中止，旧密钥保留
  n=$(stat -c %s "$KF"); head -c "$n" /dev/zero > "$KF"; sync; rm -f "$KF"
  echo "迁移完成：链 $CH（$SEQ 条）经签名进程验证通过，应用侧密钥已覆写并删除"
fi
export CMSQ_AUDIT_SIGNER=$SOCK
read -r CH SEQ MAC < <("$BIN" audit signer-enroll --print)
$SX enroll --dir /var/lib/cmsq-signer --chain "$CH" --seq "$SEQ" --mac "$MAC" >/dev/null 2>&1 \
  || $SX enroll --dir /var/lib/cmsq-signer --chain "$CH" --seq "$SEQ" --mac "$MAC"
"$BIN" db migrate >/dev/null
"$BIN" init >/dev/null
"$BIN" audit verify

echo "== 5/6 环境文件"
umask 077
cat > "$CMSQ_SECRETS_DIR/cmsq.env" <<ENV
CMSQ_HOME=$HOME_DIR
CMSQ_SECRETS_DIR=$CMSQ_SECRETS_DIR
CMSQ_AUDIT_SIGNER=$SOCK
PATH=$HOME/.local/bin:\$PATH
ENV

echo "== 6/6 锚点仓库"
if [ -n "$ANCHOR" ]; then
  AD=$HOME/.local/share/cmsq/anchors
  [ -d "$AD/.git" ] || git clone -q "$ANCHOR" "$AD"
  git -C "$AD" config user.name >/dev/null || git -C "$AD" config user.name "cmsq-anchor"
  git -C "$AD" config user.email >/dev/null || git -C "$AD" config user.email "cmsq-anchor@localhost"
  SK=$CMSQ_SECRETS_DIR/anchor_signing_ed25519; SIGN=""
  if command -v ssh-keygen >/dev/null; then
    [ -f "$SK" ] || ssh-keygen -q -t ed25519 -N '' -C cmsq-anchor -f "$SK"
    git -C "$AD" config gpg.format ssh; git -C "$AD" config user.signingkey "$SK.pub"
    echo "$(git -C "$AD" config user.email) $(cat "$SK.pub")" > "$AD/.git/allowed_signers"
    git -C "$AD" config gpg.ssh.allowedSignersFile "$AD/.git/allowed_signers"; SIGN=--sign
  fi
  "$BIN" audit anchor-git --repo "$AD" $SIGN --push && "$BIN" audit verify-git --repo "$AD"
  JOB="set -a; . $CMSQ_SECRETS_DIR/cmsq.env; set +a; cmsq audit anchor-git --repo $AD $SIGN --push && cmsq audit verify-git --repo $AD"
  if command -v crontab >/dev/null; then
    ( crontab -l 2>/dev/null | grep -v 'cmsq audit anchor-git'; echo "7 * * * * bash -c '$JOB' >>$HOME/.local/state/cmsq/anchor.log 2>&1" ) | crontab -
    echo "已安装每小时锚点任务（crontab）"
  else
    echo "无 crontab：请用 deploy/systemd/cmsq-anchor.timer，或手动定时执行：$JOB"
  fi
fi
echo
echo "部署完成。使用：set -a; . $CMSQ_SECRETS_DIR/cmsq.env; set +a; cmsq list"
