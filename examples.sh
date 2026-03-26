#!/bin/bash

# 固定共享内存分配策略 - 使用示例

# ============================================
# 1. 基本使用（默认参数）
# ============================================

# 使用 95% 的系统共享内存，4 个 workers
../target/cp-with-usrbio \
    --source /data/input \
    --target /3fs/output \
    --mount-point /3fs

# ============================================
# 2. 查看共享内存状态
# ============================================

# 在执行前查看可用共享内存
../target/cp-with-usrbio --shm-status

# ============================================
# 3. 调整共享内存使用比例
# ============================================

# 保守策略：使用 50% 的系统共享内存
../target/cp-with-usrbio \
    --source /data/input \
    --target /3fs/output \
    --mount-point /3fs \
    --shm-usage-ratio 0.5

# 激进策略：使用 98% 的系统共享内存
../target/cp-with-usrbio \
    --source /data/input \
    --target /3fs/output \
    --mount-point /3fs \
    --shm-usage-ratio 0.98

# ============================================
# 4. 调整并发度
# ============================================

# 小文件场景：2 个 workers
../target/cp-with-usrbio \
    --source /data/small_files \
    --target /3fs/output \
    --mount-point /3fs \
    --workers 2 \
    --shm-usage-ratio 0.3

# 大文件场景：8 个 workers
../target/cp-with-usrbio \
    --source /data/large_files \
    --target /3fs/output \
    --mount-point /3fs \
    --workers 8

# ============================================
# 5. 调整 Pipeline 深度
# ============================================

# 增加块大小，减少 pipeline 深度
../target/cp-with-usrbio \
    --source /data/input \
    --target /3fs/output \
    --mount-point /3fs \
    --block-size 16777216  # 16MB

# 减小块大小，增加 pipeline 深度上限
../target/cp-with-usrbio \
    --source /data/input \
    --target /3fs/output \
    --mount-point /3fs \
    --block-size 524288  # 512KB \
    --max-pipeline-depth 16

# ============================================
# 6. 调试模式
# ============================================

# 启用详细日志
../target/cp-with-usrbio \
    --source /data/input \
    --target /3fs/output \
    --mount-point /3fs \
    --debug

# ============================================
# 7. 完整示例（生产环境）
# ============================================

# 推荐：平衡性能和资源使用
../target/cp-with-usrbio \
    --source /data/input \
    --target /3fs/output \
    --mount-point /3fs \
    --workers 4 \
    --block-size 1048576 \
    --shm-usage-ratio 0.9 \
    --recursive \
    --progress \
    --preserve-attrs

# ============================================
# 8. 内存受限环境
# ============================================

# 如果系统共享内存 < 16GB
../target/cp-with-usrbio \
    --source /data/input \
    --target /3fs/output \
    --mount-point /3fs \
    --workers 2 \
    --shm-usage-ratio 0.5 \
    --block-size 524288

# 或者临时增加共享内存
sudo mount -o remount,size=32G /dev/shm
../target/cp-with-usrbio \
    --source /data/input \
    --target /3fs/output \
    --mount-point /3fs \
    --workers 4

# ============================================
# 9. 验证分配
# ============================================

# 在另一个终端查看共享内存分配
ls -lh /dev/shm/cp_usrbio_*

# 监控共享内存使用
watch -n 1 'df -h /dev/shm'

# ============================================
# 10. 错误排查
# ============================================

# 如果遇到 "insufficient /dev/shm space"
# 方案 1：减少 workers
../target/cp-with-usrbio --workers 2 ...

# 方案 2：降低使用比例
../target/cp-with-usrbio --shm-usage-ratio 0.3 ...

# 如果遇到 "pipeline_depth exceeds maximum depth"
# 方案 1：减小 block_size
../target/cp-with-usrbio --block-size 524288 ...

# 方案 2：减少 workers（增加每个 worker 的共享内存）
../target/cp-with-usrbio --workers 2 ...

# 方案 3：增加共享内存使用比例
../target/cp-with-usrbio --shm-usage-ratio 0.98 ...
