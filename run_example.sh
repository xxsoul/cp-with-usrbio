#!/bin/bash
# 示例运行脚本

set -e

# 设置环境变量
export HF3FS_BUILD_DIR="${HF3FS_BUILD_DIR:-/root/code/3fs/build}"
export LD_LIBRARY_PATH="$HF3FS_BUILD_DIR/src/lib/api:$LD_LIBRARY_PATH"

# 示例参数（请根据实际情况修改）
SOURCE_DIR="${SOURCE_DIR:-/tmp/test_data}"
TARGET_DIR="${TARGET_DIR:-/3fs/test_cluster/data}"
MOUNT_POINT="${MOUNT_POINT:-/3fs/test_cluster}"
WORKERS="${WORKERS:-4}"

# 创建测试数据（如果不存在）
if [ ! -d "$SOURCE_DIR" ]; then
    echo "Creating test data in $SOURCE_DIR..."
    mkdir -p "$SOURCE_DIR"

    # 创建一些测试文件
    for i in {1..10}; do
        dd if=/dev/urandom of="$SOURCE_DIR/file_$i.bin" bs=1M count=10 2>/dev/null
    done

    echo "Created 10 test files (10MB each)"
fi

# 运行拷贝工具
echo "🚀 Running USRBIO copy tool..."
echo "Source: $SOURCE_DIR"
echo "Target: $TARGET_DIR"
echo "Mount point: $MOUNT_POINT"
echo "Workers: $WORKERS"
echo ""

./target/release/cp-with-usrbio \
    --source "$SOURCE_DIR" \
    --target "$TARGET_DIR" \
    --mount-point "$MOUNT_POINT" \
    --workers "$WORKERS" \
    --block-size 4194304 \
    --pipeline-depth 16 \
    --recursive \
    --progress

echo ""
echo "✨ Copy completed!"

# 验证
if [ -d "$TARGET_DIR" ]; then
    echo "Verifying..."
    SRC_COUNT=$(find "$SOURCE_DIR" -type f | wc -l)
    DST_COUNT=$(find "$TARGET_DIR" -type f | wc -l)

    echo "Source files: $SRC_COUNT"
    echo "Target files: $DST_COUNT"

    if [ "$SRC_COUNT" -eq "$DST_COUNT" ]; then
        echo "✅ Verification passed!"
    else
        echo "❌ Verification failed!"
        exit 1
    fi
fi
