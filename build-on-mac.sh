#!/bin/bash
# usageBar 在 Mac 上的一键 build 脚本
# 用法：cd 到 usageBar 根目录后运行 ./build-on-mac.sh

set -e

echo "=== usageBar Mac build ==="
echo ""

# 1. 检查 / 装 Tauri CLI
if ! cargo tauri --version >/dev/null 2>&1; then
    echo "[1/3] 安装 Tauri CLI（约 3-5 分钟）..."
    cargo install tauri-cli --version "^2.0" --locked
else
    echo "[1/3] Tauri CLI 已装：$(cargo tauri --version)"
fi

# 2. 构建
echo ""
echo "[2/3] 构建 .app（约 3-5 分钟首次，之后增量 30s-2min）..."
cd src-tauri
cargo tauri build --target aarch64-apple-darwin

# 3. 安装到 /Applications
APP_PATH="target/aarch64-apple-darwin/release/bundle/macos/usageBar.app"
if [ -d "$APP_PATH" ]; then
    echo ""
    echo "[3/3] 安装到 /Applications..."
    cp -R "$APP_PATH" /Applications/
    echo ""
    echo "=== 完成 ==="
    echo "App 路径：/Applications/usageBar.app"
    echo ""
    echo "首次启动：Finder → /Applications → 右键 usageBar → 打开"
    echo "（之后会正常出现 Gatekeeper 提示，允许即可）"
else
    echo "❌ 未找到 $APP_PATH，构建可能失败"
    exit 1
fi