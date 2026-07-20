#!/usr/bin/env bash
# deploy-local-brew.sh — 本机构建 im-agentproc → 经 brew 安装到 /opt/homebrew/bin，零 CI。
#
# 用途：本机快速调试 bridge / bridge-manager 改动。产物仅在本机，不上传 GitHub，
# 公共 tap 不变（脚本结束自动还原 formula）。
#
# 流程：
#   1. 本地 cargo build --release（host arch）出 im-agentproc
#   2. ad-hoc 重签（避免 macOS AMFI 运行时 SIGKILL / "killed: 9"）
#   3. 临时把 jeffkit/tap 的 formula 改成 file:// 指向本地产物 + 本地 sha256
#   4. brew reinstall（从本地文件装到 Cellar，并 relink /opt/homebrew/bin）
#   5. 还原 tap formula（git checkout / 备份还原），保持 tap 干净
#
# 用法：
#   scripts/deploy-local-brew.sh
#
# 注意：这是「本机调试」路径。要让其它机器 / 公共 tap 拿到新版本，需打 release tag
# 并更新 formula 的 url/sha256（见 ilink-hub 的发布规范）。

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$ROOT"

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "本脚本仅用于 macOS 本地 brew 部署。" >&2
  exit 1
fi

VERSION="$(grep '^version' Cargo.toml | head -1 | sed 's/.*= *"\(.*\)"/\1/')"
TAP_DIR="$(brew --repository jeffkit/tap 2>/dev/null || true)"
FORMULA="$TAP_DIR/Formula/im-agentproc.rb"

if [[ -z "$TAP_DIR" || ! -f "$FORMULA" ]]; then
  echo "!! 未找到 jeffkit/tap formula（$FORMULA）。先执行：brew tap jeffkit/tap" >&2
  exit 1
fi

ARCH="$(uname -m)"
case "$ARCH" in
  arm64) SUFFIX="macos-aarch64" ;;
  x86_64) SUFFIX="macos-x86_64" ;;
  *) echo "!! 不支持的架构：$ARCH" >&2; exit 1 ;;
esac

echo "==> 本地构建 release（im-agentproc）v$VERSION ($ARCH)"
cargo build --release --bin im-agentproc

STAGE="$(mktemp -d)"
ASSET="im-agentproc-$SUFFIX"
cp target/release/im-agentproc "$STAGE/$ASSET"

echo "==> ad-hoc 重签（修复 AMFI 运行时拒签）"
xattr -cr "$STAGE/$ASSET" 2>/dev/null || true
codesign --remove-signature "$STAGE/$ASSET" 2>/dev/null || true
codesign --force --sign - "$STAGE/$ASSET"

SHA="$(shasum -a 256 "$STAGE/$ASSET" | awk '{print $1}')"

BACKUP="$(mktemp)"
cp "$FORMULA" "$BACKUP"
cleanup() {
  # 还原 tap formula（优先 git，保持 tap 干净），清理临时文件
  if ! git -C "$TAP_DIR" checkout -- Formula/im-agentproc.rb 2>/dev/null; then
    cp "$BACKUP" "$FORMULA"
  fi
  rm -f "$BACKUP"
  rm -rf "$STAGE"
}
trap cleanup EXIT

echo "==> 临时改写 tap formula 指向本地文件"
cat > "$FORMULA" <<RUBY
# typed: false
# frozen_string_literal: true
# !! 本地调试覆盖（deploy-local-brew.sh 生成）—— 切勿提交；脚本结束会自动还原。
class ImAgentproc < Formula
  desc "IM-side runtime for the agentproc ecosystem (local dev build)"
  homepage "https://github.com/jeffkit/im-agentproc"
  version "$VERSION"
  license "MIT"

  on_macos do
    url "file://$STAGE/$ASSET"
    sha256 "$SHA"
  end

  def install
    bin.install "$ASSET" => "im-agentproc"
  end

  test do
    assert_match "im-agentproc", shell_output("#{bin}/im-agentproc --version")
  end
end
RUBY

echo "==> brew reinstall（从本地文件）"
brew reinstall --formula "$FORMULA"

# 二次重签 Cellar 内的二进制，确保 brew 拷贝/重定位后签名仍有效
CELLAR="$(brew --cellar im-agentproc 2>/dev/null)/$VERSION/bin"
if [[ -d "$CELLAR" ]]; then
  codesign --force --sign - "$CELLAR/im-agentproc" 2>/dev/null || true
fi

echo "==> 安装结果"
/opt/homebrew/bin/im-agentproc --version || true

echo "本地 brew 部署完成 v${VERSION} 。tap formula 已还原（保持 jeffkit/tap 干净）。"
