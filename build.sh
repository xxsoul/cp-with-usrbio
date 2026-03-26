#!/bin/bash
set -e

echo "🏗️  Building USRBIO Copy Tool..."

# 设置环境变量
export HF3FS_BUILD_DIR="${HF3FS_BUILD_DIR:-/root/code/3fs/build}"
export LD_LIBRARY_PATH="$HF3FS_BUILD_DIR/src/lib/api:$LD_LIBRARY_PATH"

echo "HF3FS_BUILD_DIR: $HF3FS_BUILD_DIR"
echo "LD_LIBRARY_PATH: $LD_LIBRARY_PATH"

# 检查依赖
if [ ! -f "$HF3FS_BUILD_DIR/src/lib/api/libhf3fs_api_shared.so" ]; then
    echo "❌ Error: libhf3fs_api_shared.so not found"
    echo "Please build 3FS first or set HF3FS_BUILD_DIR correctly"
    exit 1
fi

# 构建
cargo build --release

echo "✅ Build successful!"
echo "Binary: target/release/cp-with-usrbio"
echo ""
echo "To run:"
echo "export LD_LIBRARY_PATH=$LD_LIBRARY_PATH"
echo "./target/release/cp-with-usrbio --help"
