# cp-with-usrbio

高性能 USRBIO 文件拷贝工具，专门用于将数据从本地磁盘或 NFS 拷贝到 3FS 分布式文件系统。使用 USRBIO（用户态 I/O）技术实现零拷贝数据传输。

## 核心特性

- 🚀 **USRBIO 零拷贝** - 利用 3FS 的 USRBIO 特性，避免数据在用户空间和内核空间之间多次拷贝
- 📊 **固定共享内存分配** - 每个工作线程使用固定大小的共享内存，占总共享内存的 95%（可配置）
- ⚡ **动态 Pipeline 并发** - 根据文件大小自动调整 I/O 深度，优化性能
- 🔄 **多线程并发** - 支持可配置的并发工作线程数
- 📈 **实时进度监控** - 显示拷贝进度、速度和预计完成时间
- 💾 **断点续传** - 文件数 > 100 自动启用，支持中断恢复
- 🛡️ **健壮性增强** - 共享内存空间检查，防止资源耗尽崩溃

## 快速开始

### 环境要求

**必需**:
- Rust 1.75.0+
- 3FS 构建目录: `/root/code/3fs/build`
- 动态库: `libhf3fs_api_shared.so`

**运行时依赖**:
```bash
export HF3FS_BUILD_DIR=/root/code/3fs/build
export LD_LIBRARY_PATH=$HF3FS_BUILD_DIR/src/lib/api:$LD_LIBRARY_PATH
```

**注意**: 本项目包含了独立的 `hf3fs-usrbio-sys` Rust绑定，不依赖3FS源码树中的绑定代码。

⚠️ **版本匹配检查（重要）**

本项目包含独立的 `hf3fs-usrbio-sys` Rust绑定，其中的头文件来自3FS源码快照。

**在首次使用或更新3FS版本后，必须确认头文件版本匹配**：

1. 对比您的3FS源码头文件与本项目的头文件
2. 如有差异，将您的3FS头文件复制到 `hf3fs-usrbio-sys/include/`
3. 检查 `hf3fs-usrbio-sys/src/lib.rs` 是否需要相应调整
4. 重新构建

详细说明请参见 [hf3fs-usrbio-sys/README.md](hf3fs-usrbio-sys/README.md)。

### 构建

```bash
# 使用构建脚本（推荐）
./build.sh

# 二进制输出位置
../target/cp-with-usrbio
```

### 基础使用

```bash
# 单文件拷贝
../target/cp-with-usrbio \
    --source /data/file.txt \
    --target /3fs/backup/file.txt \
    --mount-point /3fs

# 目录递归拷贝
../target/cp-with-usrbio \
    --source /data \
    --target /3fs/backup \
    --mount-point /3fs \
    --recursive
```

### 查看共享内存状态

```bash
../target/cp-with-usrbio --shm-status
```

---

## 参数说明

### 核心参数

| 参数 | 默认值 | 说明 |
|------|--------|------|
| `--source` | - | 源文件或目录路径（必需） |
| `--target` | - | 目标路径（3FS）（必需） |
| `--mount-point` | - | 3FS 挂载点（必需） |
| `--workers` | 4 | 并发工作线程数 |
| `--shm-usage-ratio` | 0.95 | 共享内存使用比例（占总共享内存的百分比） |
| `--block-size` | 1048576 | I/O 块大小（字节），默认 1MB |
| `--pipeline-depth` | 2 | 最小 Pipeline 深度 |
| `--max-pipeline-depth` | 8 | 最大 Pipeline 深度 |
| `--recursive` | false | 递归拷贝目录 |
| `--resume` | true | 启用断点续传（文件数>100 自动启用） |
| `--debug` | false | 详细调试信息 |

---

## 共享内存分配策略

### 固定大小分配

每个工作线程在启动时分配固定大小的共享内存，不再根据文件大小动态调整。

**计算公式**：
```
每个 Worker 的共享内存 = floor(总共享内存 × shm_usage_ratio / workers数量)
```

**示例**（64GB 系统共享内存，默认参数）：
```
总共享内存 = 64GB
可用共享内存 = 64GB × 0.95 = 60.8GB
每个 Worker = 60.8GB / 4 = 15.2GB

实际分配：
/dev/shm/cp_usrbio_w0 → 15.2GB
/dev/shm/cp_usrbio_w1 → 15.2GB
/dev/shm/cp_usrbio_w2 → 15.2GB
/dev/shm/cp_usrbio_w3 → 15.2GB
```

### 约束关系

```bash
# 核心约束
pipeline_depth × block_size ≤ iov_size_per_worker

# 如果遇到错误：
# "pipeline_depth exceeds maximum depth"
# 解决方案：
--block-size 524288        # 减小块大小
--workers 2                # 减少 workers
--shm-usage-ratio 0.98     # 增加共享内存使用比例
```

---

## 使用场景

### 场景 1：小文件密集（< 1MB）

```bash
../target/cp-with-usrbio \
    --source /data/small_files \
    --target /3fs/backup \
    --mount-point /3fs \
    --workers 2 \
    --shm-usage-ratio 0.3 \
    --recursive
```

### 场景 2：大文件（> 10GB）

```bash
../target/cp-with-usrbio \
    --source /data/large_files \
    --target /3fs/backup \
    --mount-point /3fs \
    --workers 8 \
    --block-size 16777216  # 16MB
```

### 场景 3：内存受限环境

```bash
../target/cp-with-usrbio \
    --source /data \
    --target /3fs/backup \
    --mount-point /3fs \
    --workers 2 \
    --shm-usage-ratio 0.5
```

---

## 监控和调试

### 查看共享内存状态

```bash
../target/cp-with-usrbio --shm-status
```

**输出**:
```
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
📊 /dev/shm Memory Status
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
Total:         67.11 MB
Used:           0.00 MB (0.0%)
Available:     67.11 MB
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
```

### 实时监控

```bash
# 监控内存占用
watch -n 1 'ps aux | grep cp-with-usrbio'

# 监控共享内存
watch -n 1 'df -h /dev/shm'

# 查看活跃文件
watch -n 1 'ls /dev/shm/cp_usrbio_* | wc -l'
```

---

## 故障排除

### 问题 0: 头文件版本不匹配

**症状**:
- 链接错误：符号未定义
- 运行时段错误或崩溃
- 编译错误：类型不匹配

**原因**: 本项目的头文件与部署的3FS版本不一致

**解决**:
```bash
# 1. 检查差异
diff /root/code/3fs/src/lib/api/hf3fs_usrbio.h \
     hf3fs-usrbio-sys/include/hf3fs_usrbio.h

# 2. 更新头文件
cp /root/code/3fs/src/lib/api/hf3fs_usrbio.h \
   hf3fs-usrbio-sys/include/hf3fs_usrbio.h

# 3. 检查Rust封装代码是否需要调整
#    查看 hf3fs-usrbio-sys/src/lib.rs
#    特别关注API签名、结构体字段等

# 4. 重新构建
cargo clean
./build.sh
```

**预防措施**:
- 每次更新3FS后都检查头文件
- 在 `hf3fs-usrbio-sys/README.md` 中记录版本信息

---

### 问题 1: 找不到动态库

**错误**: `error while loading shared libraries: libhf3fs_api_shared.so`

**解决**:
```bash
export HF3FS_BUILD_DIR=/root/code/3fs/build
export LD_LIBRARY_PATH=$HF3FS_BUILD_DIR/src/lib/api:$LD_LIBRARY_PATH
```

---

### 问题 2: 共享内存不足

**错误**: `Insufficient /dev/shm space`

**解决方案**:

**方案 A**: 减少并发
```bash
--workers 2
```

**方案 B**: 降低使用比例
```bash
--shm-usage-ratio 0.5
```

**方案 C**: 清理共享内存
```bash
rm -f /dev/shm/cp_usrbio_*
# 或使用工具清理
../target/cp-with-usrbio --shm-status
```

**方案 D**: 增加共享内存
```bash
# 临时增加
sudo mount -o remount,size=8G /dev/shm

# 永久增加（编辑 /etc/fstab）
tmpfs /dev/shm tmpfs defaults,size=8G 0 0
```

---

### 问题 3: Pipeline depth 过小

**错误**: `pipeline_depth exceeds maximum depth allowed by iov_size_per_worker/block_size ratio`

**解决方案**:

**方案 A**: 减小块大小
```bash
--block-size 524288  # 512KB
```

**方案 B**: 减少 workers
```bash
--workers 2
```

**方案 C**: 增加共享内存使用比例
```bash
--shm-usage-ratio 0.98
```

---

## 技术架构

### 依赖关系

```
cp-with-usrbio (Rust)
├── hf3fs-usrbio-sys (本地子项目)
│   ├── include/hf3fs_usrbio.h (C API头文件)
│   ├── src/lib.rs (Rust封装实现)
│   ├── build.rs (bindgen生成绑定)
│   └── 依赖: shared_memory, uuid, bindgen
├── clap (CLI 解析)
├── crossbeam (并发原语)
├── indicatif (进度条)
├── walkdir (目录遍历)
├── libc (系统调用)
├── serde (序列化)
└── serde_json (JSON)
```

### 核心组件

- **WorkerContext** - Worker 线程上下文，持有固定大小的共享内存资源
- **PipelineProcessor** - Pipeline I/O 处理器
- **ProgressManager** - 断点续传管理器
- **ManagedFd** - 文件描述符管理
- **ShmManager** - 共享内存管理

---

## 资源管理

### 所有资源自动管理

| 资源 | 创建 | 释放 | Drop 实现 |
|------|------|------|----------|
| ManagedFd | `hf3fs_reg_fd` | `hf3fs_dereg_fd` | ✅ |
| Iov | `hf3fs_iovwrap` | `hf3fs_iovdestroy` | ✅ |
| Ior | `hf3fs_iorcreate4` | `hf3fs_iordestroy` | ✅ |
| Shmem | `ShmemConf::create` | 自动 | ✅ |

---

## 开发指南

详细开发文档请参见 `CLAUDE.md`。

### 关键文件

```
src/
├── main.rs           - 主程序入口
├── cli.rs            - CLI 参数定义
├── worker_context.rs - Worker 上下文（固定共享内存）
├── pipeline.rs       - Pipeline I/O 处理
├── managed_fd.rs     - 文件描述符管理
├── shm_manager.rs    - 共享内存工具函数
├── progress.rs       - 断点续传管理
├── task.rs           - 任务收集和定义
└── utils.rs          - 工具函数
```

### 二进制输出位置

```
构建脚本: ./build.sh
输出位置: ../target/cp-with-usrbio
即: /root/code/target/cp-with-usrbio
```

---

## 常见问题

**Q: 如何清除断点重新开始？**

```bash
rm /3fs/target/.cp-with-usrbio-progress.json
../target/cp-with-usrbio ...
```

**Q: 如何调整并发度？**

根据 CPU 核心数和网络带宽调整 `--workers`，建议 4-16。

**Q: 内存占用还是很高怎么办？**

检查 `--shm-usage-ratio` 设置，降低到 0.5 或更小，或减少 `--workers`。

**Q: 如何验证资源是否正确释放？**

```bash
# 运行后检查
../target/cp-with-usrbio --shm-status
# 应该看到 0 个活跃文件
```

**Q: 如何查看每个 Worker 的共享内存分配？**

```bash
../target/cp-with-usrbio --debug --source ... --target ... --mount-point ...

# 输出示例：
# [Worker 0] Initialized with fixed shared memory: 15200.00 MB
# [Worker 1] Initialized with fixed shared memory: 15200.00 MB
```

---

## 详细文档

- [开发指南](CLAUDE.md) - 项目架构和开发说明
- [共享内存策略](FIXED_SHM_STRATEGY.md) - 固定共享内存分配详细说明
- [更新日志](CHANGELOG.md) - 版本变更记录
- [使用示例](examples.sh) - 完整的使用示例

---

## License

内部项目，仅供 3FS 团队使用。

---

## 相关链接

- [3FS GitHub](https://github.com/deepseek-ai/3FS)
