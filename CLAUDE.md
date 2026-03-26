# USRBIO Copy Tool - 开发指南

## 项目概述

高性能文件拷贝工具，专门用于将数据从本地磁盘或NFS拷贝到3FS分布式文件系统。使用USRBIO（用户态I/O）技术实现零拷贝数据传输。

### 核心特性

- **USRBIO零拷贝**: 利用3FS的USRBIO特性，避免数据在用户空间和内核空间之间多次拷贝
- **动态Pipeline并发**: 根据文件大小自动调整I/O深度，优化性能
- **多线程并发**: 支持可配置的并发工作线程数
- **自动块大小检测**: 自动检测目标文件系统的块大小
- **实时进度监控**: 显示拷贝进度、速度和预计完成时间
- **错误容错**: 单个文件失败不影响整体拷贝

## 技术架构

### 依赖关系

```
cp-with-usrbio (Rust)
├── hf3fs-usrbio-sys (本地路径依赖)
│   ├── 绑定 hf3fs_usrbio.h (C API)
│   ├── 链接 libhf3fs_api_shared.so
│   └── 依赖: shared_memory, uuid
├── clap (CLI解析)
├── crossbeam (并发原语)
├── indicatif (进度条)
├── walkdir (目录遍历)
└── libc (系统调用)
```

### 外部依赖

**必需**:
- Rust 1.75.0+
- 3FS构建目录: `/root/code/3fs/build`
- 动态库: `libhf3fs_api_shared.so` (位于 `$HF3FS_BUILD_DIR/src/lib/api/`)

**运行时依赖**:
```bash
export HF3FS_BUILD_DIR=/root/code/3fs/build
export LD_LIBRARY_PATH=$HF3FS_BUILD_DIR/src/lib/api:$LD_LIBRARY_PATH
```

### 核心组件

#### 1. USRBIOCopier (src/main.rs:96-624)

核心拷贝引擎，负责单个文件的USRBIO拷贝。

**关键职责**:
- 管理USRBIO资源 (Iov, Ior)
- Pipeline并发控制
- 共享内存管理
- 文件注册/注销
- 进度跟踪

**资源生命周期**:
```
File.create() → hf3fs_reg_fd() → Iov::wrap() → Ior::create()
    ↓
Pipeline: read → prepare → submit → poll
    ↓
Drop: hf3fs_dereg_fd() + Iov/Iov cleanup
```

#### 2. 动态Pipeline深度调整 (src/main.rs:108-142)

根据文件大小和共享内存容量自动优化I/O并发度。

**策略**:
- 小文件 (<10MB): 深度 2
- 中等文件 (10-100MB): 深度 4-20
- 大文件 (100MB-1GB): 深度 16-32
- 超大文件 (>1GB): 深度 32 (max)

**约束**:
```
实际深度 = min(理想深度, max_pipeline_depth, iov_size/block_size)
```

#### 3. 自动块大小检测 (src/main.rs:26-56)

使用 `statvfs` 系统调用自动检测目标文件系统的块大小。

**检测策略**:
1. 尝试 `statvfs(target_path)`
2. 失败 → 尝试 `statvfs(parent_dir)`
3. 都失败 → 回退到 1MB
4. 限制范围: 4KB ~ 16MB

#### 4. Worker线程模型 (src/main.rs:679-697)

多生产者-单消费者模型，每个worker独立处理文件。

**并发模型**:
```
Main Thread:
  └─ collect_tasks() → Vec<CopyTask>
  └─ channel::bounded(workers * 2)
  └─ spawn workers × N

Worker Thread:
  └─ recv() from channel
  └─ USRBIOCopier::copy_file()
  └─ update atomic stats
```

## 构建与部署

### 构建命令

```bash
# 标准构建（推荐使用构建脚本）
./build.sh

# 输出位置: ../target/cp-with-usrbio

# 或手动构建
export HF3FS_BUILD_DIR=/root/code/3fs/build
export LD_LIBRARY_PATH=$HF3FS_BUILD_DIR/src/lib/api:$LD_LIBRARY_PATH
cargo build --release
cp target/release/cp-with-usrbio ../target/
```

### 部署位置

```bash
# 编译输出（自动复制到父目录）
../target/cp-with-usrbio
# 即: /root/code/target/cp-with-usrbio
```

### 运行要求

**环境变量** (必需):
```bash
export HF3FS_BUILD_DIR=/root/code/3fs/build
export LD_LIBRARY_PATH=$HF3FS_BUILD_DIR/src/lib/api:$LD_LIBRARY_PATH
```

**系统要求**:
- 3FS FUSE挂载点已激活
- 共享内存支持 (`/dev/shm`)
- 足够内存: workers × iov_size (默认: 4 × 1GB = 4GB)

## 关键参数说明

### 性能参数

| 参数 | 默认值 | 说明 | 调优建议 |
|------|--------|------|----------|
| `--workers` | 4 | 并发工作线程数 | CPU核心数到2倍核心数 |
| `--block-size` | 自动检测 | I/O块大小 | 小文件: 512KB-1MB, 大文件: 4-16MB |
| `--pipeline-depth` | 2 | 最小Pipeline深度 | 保持默认即可 |
| `--max-pipeline-depth` | 32 | 最大Pipeline深度 | SSD+RDMA: 32-64 |
| `--iov-size` | 1GB | 共享内存大小 | 大文件: 2-4GB |

### 约束关系

**共享内存容量**:
```
pipeline_depth * block_size <= iov_size
```

**内存占用**:
```
总内存 = workers * iov_size
```

**示例**:
```bash
# 小文件优化 (内存充足)
--workers 8 --block-size 524288 --max-pipeline-depth 64 --iov-size 536870912

# 大文件优化 (高性能)
--workers 4 --block-size 16777216 --max-pipeline-depth 128 --iov-size 4294967296

# 内存受限
--workers 2 --block-size 1048576 --max-pipeline-depth 16 --iov-size 268435456
```

## 代码结构

### main.rs 结构

```
1-25:     导入和依赖
26-56:    get_filesystem_block_size() - 自动块大小检测
58-70:    Args struct - CLI参数定义
72-77:    CopyTask struct - 拷贝任务定义
79-94:    CopyStats struct - 统计信息
96-624:   USRBIOCopier - 核心拷贝引擎
          ├─ new() - 构造函数
          ├─ calculate_pipeline_depth() - 动态深度计算
          └─ copy_file() - 文件拷贝逻辑
626-677:  collect_tasks() - 任务收集
679-697:  worker_thread() - 工作线程
699-815:  main() - 主函数
```

### 关键函数说明

#### `USRBIOCopier::copy_file()`

执行单个文件的USRBIO拷贝。

**步骤**:
1. 打开源文件 (普通read)
2. 创建目标文件并注册fd (`hf3fs_reg_fd`)
3. 创建共享内存和Iov
4. 创建Ior (I/O请求队列)
5. Pipeline循环: read → prepare → submit → poll
6. 同步文件长度 (ioctl)
7. 保留文件属性
8. 清理资源 (Drop trait)

**错误处理**:
- 详细错误信息 + 可选backtrace (debug模式)
- 单文件失败不中断整体流程
- 失败统计在最终报告中显示

#### `USRBIOCopier::calculate_pipeline_depth()`

根据文件大小和资源限制计算最优深度。

**输入**:
- `file_size`: 文件大小 (字节)
- `min_pipeline_depth`: 用户指定的最小深度
- `max_pipeline_depth`: 用户指定的最大深度
- `iov_size`: 共享内存大小
- `block_size`: 块大小

**输出**:
- 实际pipeline深度

**算法**:
```rust
let max_by_shm = iov_size / block_size;
let ideal = match file_size {
    < 10MB  => min_depth.max(2),
    < 100MB => scale_linearly(),
    < 1GB   => scale_aggressively(),
    _       => max_pipeline_depth,
};
min(ideal, max_pipeline_depth, max_by_shm)
```

## 开发指南

### 添加新功能

**1. 添加新CLI参数**:
```rust
// src/main.rs - Args struct
#[arg(short, long, default_value = "...")]
new_param: Type,
```

**2. 传递到USRBIOCopier**:
```rust
// src/main.rs - main()
let copier = Arc::new(USRBIOCopier::new(
    ...,
    args.new_param,
    ...
));
```

**3. 更新文档**:
- README.md 参数表
- 性能调优章节
- 使用示例

### 调试技巧

**启用详细日志**:
```bash
./cp-with-usrbio ... --debug
```

**输出包含**:
- 文件注册/注销
- 共享内存创建详情
- Pipeline深度计算
- 每个I/O操作
- 错误堆栈跟踪

**Rust backtrace**:
```bash
RUST_BACKTRACE=full ./cp-with-usrbio ... --debug
```

### 性能分析

**基准测试场景**:
1. 小文件密集 (10KB × 10000个)
2. 中等文件 (100MB × 100个)
3. 大文件 (10GB × 10个)
4. 混合场景 (各种大小混合)

**关键指标**:
- 吞吐量: MB/s
- 文件处理速率: files/s
- 内存占用: workers × iov_size
- CPU利用率: top/htop

**性能瓶颈定位**:
- `strace -c`: 系统调用统计
- `perf record`: CPU性能分析
- `iotop`: I/O监控
- `/proc/meminfo`: 内存使用

## 常见问题

### 1. 找不到动态库

**错误**: `error while loading shared libraries: libhf3fs_api_shared.so`

**解决**:
```bash
export HF3FS_BUILD_DIR=/root/code/3fs/build
export LD_LIBRARY_PATH=$HF3FS_BUILD_DIR/src/lib/api:$LD_LIBRARY_PATH
```

### 2. 共享内存不足

**错误**: `Failed to create shared memory`

**解决**:
- 减少 `--iov-size`
- 减少 `--workers`
- 检查 `/dev/shm` 可用空间

### 3. Pipeline深度超限

**错误**: `pipeline_depth exceeds maximum depth allowed by iov_size/block_size ratio`

**解决**:
- 增加 `--iov-size`
- 减少 `--block-size`
- 减少 `--max-pipeline-depth`

### 4. 文件注册失败

**错误**: `Failed to register fd`

**可能原因**:
- 3FS挂载点不正确
- 目标路径不在3FS上
- fd数量超过限制

**调试**:
```bash
# 检查挂载点
mount | grep 3fs

# 检查fd限制
ulimit -n
```

### 5. 性能不佳

**诊断清单**:
- [ ] Pipeline深度是否合理？(debug模式查看)
- [ ] 块大小是否匹配文件特征？
- [ ] 工作线程数是否合适？
- [ ] 网络带宽是否饱和？
- [ ] 目标存储是否成为瓶颈？

## 未来改进方向

### 短期优化

- [ ] 增量同步功能 (类似rsync)
- [ ] 符号链接处理选项
- [ ] 目录权限保留
- [ ] 进度持久化 (断点续传)

### 中期优化

- [ ] 静态链接选项 (减少动态依赖)
- [ ] 分布式拷贝 (多机并行)
- [ ] 压缩传输选项
- [ ] 带宽限制选项

### 长期优化

- [ ] Web界面监控
- [ ] 任务队列管理
- [ ] 与3FS管理工具集成
- [ ] 自动性能调优

## 代码规范

### Rust规范

- 遵循 `rustfmt` 格式化
- 使用 `clippy` 静态分析
- 错误处理使用 `anyhow` + `thiserror`
- 关键路径添加debug日志

### 提交规范

```
feat: 添加新功能
fix: 修复bug
docs: 文档更新
perf: 性能优化
refactor: 重构
test: 测试相关
chore: 构建/工具相关
```

### 注释规范

```rust
/// 公共API文档注释
/// 包含: 功能说明、参数、返回值、示例

// 内部实现注释
// 说明复杂逻辑的原因和思路

// TODO: 待办事项
// FIXME: 已知问题
// HACK: 临时解决方案
```

## 测试

### 单元测试

目前项目暂无单元测试框架，建议添加：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calculate_pipeline_depth() {
        // 测试小文件
        // 测试中等文件
        // 测试大文件
        // 测试边界条件
    }
}
```

### 集成测试

建议添加集成测试脚本：

```bash
#!/bin/bash
# tests/integration_test.sh

# 准备测试数据
# 运行各种场景
# 验证结果
# 性能基准
```

### 手动测试清单

- [ ] 单个文件拷贝
- [ ] 目录递归拷贝
- [ ] 空文件处理
- [ ] 大文件拷贝 (>10GB)
- [ ] 小文件密集 (>10000个)
- [ ] 错误恢复 (单个文件失败)
- [ ] Debug模式输出
- [ ] 进度显示
- [ ] 文件权限保留

## 参考资料

### 3FS文档

- 3FS源码: `/root/code/3fs/`
- USRBIO头文件: `/root/code/3fs/src/lib/api/hf3fs_usrbio.h`
- API文档: 待补充

### Rust文档

- Rust Book: https://doc.rust-lang.org/book/
- Cargo Guide: https://doc.rust-lang.org/cargo/
- anyhow: https://docs.rs/anyhow/
- clap: https://docs.rs/clap/

### 系统编程

- Linux系统调用: `man 2 statvfs`, `man 2 ioctl`
- 共享内存: `man 7 shm_overview`
- 零拷贝I/O: 相关论文和文档

## 联系方式

- 项目路径: `/root/code/cp-with-usrbio`
- 二进制路径: `/root/code/target/cp-with-usrbio`
- 依赖项目: `/root/code/3fs/`
