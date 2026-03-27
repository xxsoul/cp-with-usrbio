# USRBIO Copy Tool - 开发指南

## 项目概述

高性能文件拷贝工具，专门用于将数据从本地磁盘或NFS拷贝到3FS分布式文件系统。使用USRBIO（用户态I/O）技术实现零拷贝数据传输。

### 核心特性

- **USRBIO零拷贝**: 利用3FS的USRBIO特性，避免数据在用户空间和内核空间之间多次拷贝
- **固定共享内存分配**: 每个Worker线程使用固定大小的共享内存，支持灵活配置
- **动态Pipeline并发**: 根据文件大小自动调整I/O深度（2-8），优化性能
- **多线程并发**: 支持可配置的并发工作线程数（默认4个）
- **实时进度监控**: 显示拷贝进度、速度和预计完成时间
- **断点续传**: 文件数>100时自动启用，支持中断恢复
- **文件校验**: 支持比对source和target的文件一致性
- **错误容错**: 单个文件失败不影响整体拷贝，支持重试机制
- **Ctrl+C优雅退出**: 中断时自动清理资源，保存进度

## 技术架构

### 依赖关系

```
cp-with-usrbio (Rust)
├── hf3fs-usrbio-sys (本地子项目)
│   ├── include/hf3fs_usrbio.h (C API头文件)
│   ├── src/lib.rs (Rust封装实现)
│   ├── build.rs (bindgen生成绑定)
│   └── 依赖: shared_memory, uuid, bindgen
├── clap (CLI解析)
├── crossbeam (并发原语)
├── indicatif (进度条)
├── walkdir (目录遍历)
├── libc (系统调用)
├── serde (序列化)
├── serde_json (JSON)
└── ctrlc (信号处理)
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

### 模块结构

```
src/
├── main.rs           - 主程序入口，任务调度，统计汇总
├── cli.rs            - CLI参数定义 (Args struct)
├── worker_context.rs - Worker线程上下文和Pipeline处理
├── copier.rs         - USRBIO拷贝协调器（已废弃，逻辑迁移至worker_context）
├── managed_fd.rs     - 文件描述符自动管理
├── shm_manager.rs    - 共享内存工具函数
├── progress.rs       - 断点续传管理
├── task.rs           - 任务收集和定义
└── utils.rs          - 工具函数（块大小检测）
```

## 核心组件详解

### 1. WorkerContext (src/worker_context.rs)

核心Worker线程上下文，负责管理固定共享内存资源和文件拷贝。

**关键职责**:
- 管理固定大小的共享内存资源 (ShmManager)
- Pipeline I/O处理逻辑
- 动态计算Pipeline深度
- 文件注册/注销
- 进度跟踪和统计

**资源生命周期**:
```
WorkerThread::new()
    ├─ ShmManager::new() → 创建共享内存 + Iov
    └─ 固定资源，跨文件复用

每个文件:
    ├─ ManagedFd::from_raw_fd() → hf3fs_reg_fd()
    ├─ Ior::create() → 创建I/O请求队列
    ├─ Pipeline循环: read → prepare → submit → poll
    ├─ fsync() + ioctl() → 确保数据持久化
    └─ Drop: ManagedFd自动注销 + Ior销毁

WorkerThread退出:
    └─ ShmManager::Drop → 清理符号链接 + 释放共享内存
```

**固定共享内存策略**:
- 每个Worker启动时分配固定大小的共享内存
- 大小 = `(总共享内存 × shm_usage_ratio) / workers`
- 文件之间复用，不再根据文件大小动态调整
- 避免频繁的内存分配/释放开销

### 2. 动态Pipeline深度调整 (src/worker_context.rs:184-248)

根据文件大小和共享内存容量自动优化I/O并发度。

**策略**:
- 小文件 (<10MiB): 深度 min_pipeline_depth (默认2)
- 中等文件 (10-100MiB): 深度线性缩放 (2-4)
- 大文件 (100MiB-1GiB): 深度线性缩放 (4-6)
- 超大文件 (>1GiB): 深度 max_pipeline_depth (默认8)

**约束**:
```
实际深度 = min(理想深度, max_pipeline_depth, iov_size/block_size)
```

**示例** (iov_size=1GiB, block_size=1MiB):
```
文件大小: 500MiB
理论最大深度: 1024
理想深度: 5 (根据算法)
实际深度: min(5, 8, 1024) = 5
```

### 3. 自动块大小检测 (src/utils.rs:1-40)

使用 `statvfs` 系统调用自动检测目标文件系统的块大小。

**检测策略**:
1. 尝试 `statvfs(target_path)`
2. 失败 → 尝试 `statvfs(parent_dir)`
3. 都失败 → 回退到 1MiB
4. 限制范围: 4KiB ~ 16MiB

### 4. Worker线程模型 (src/main.rs:305-337)

多生产者-多消费者模型，每个Worker独立处理文件。

**并发模型**:
```
Main Thread:
  ├─ 收集任务 (walk_and_send_tasks)
  ├─ channel::bounded(workers * 2)
  ├─ spawn workers × N
  ├─ 等待完成
  └─ 统计和重试逻辑

Worker Thread (src/worker_context.rs:679-697):
  ├─ recv() from channel (try_recv + sleep)
  ├─ 检查中断标志 (running.load)
  ├─ WorkerContext::copy_file()
  └─ update atomic stats
```

**优雅中断**:
```
Ctrl+C → ctrlc::set_handler
    ↓
running.store(false)
    ↓
Workers检测到停止标志
    ↓
完成当前文件，退出循环
    ↓
触发Drop清理:
  ├─ ManagedFd::Drop (注销fd)
  ├─ ShmManager::Drop (清理符号链接 + 释放共享内存)
  └─ ProgressManager::Drop (保存进度)
```

### 5. ManagedFd (src/managed_fd.rs)

RAII封装，自动管理3FS文件描述符的注册和注销。

**特性**:
- 创建时自动调用 `hf3fs_reg_fd()`
- Drop时自动调用 `hf3fs_dereg_fd()`
- 避免资源泄露

### 6. ProgressManager (src/progress.rs)

断点续传管理器，持久化拷贝进度。

**功能**:
- 记录已完成和失败的文件列表
- 支持JSON格式持久化
- 程序重启时自动加载进度
- 避免重复拷贝已完成文件

**进度文件位置**: `{target_dir}/.cp-with-usrbio-progress.json`

### 7. 任务收集 (src/task.rs)

支持两种任务收集模式：

**正常模式**:
- 使用 `walk_and_send_tasks()` 边遍历边发送
- 支持递归目录遍历
- 实时统计文件数和总大小

**校验重传模式**:
- 使用 `verify_files()` 比对source和target
- 仅比较文件大小，发现不一致或缺失
- 生成重传任务列表

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
- 足够内存: `(总共享内存 × shm_usage_ratio)`

## 关键参数说明

### 性能参数

| 参数 | 默认值 | 说明 | 调优建议 |
|------|--------|------|----------|
| `--workers` | 4 | 并发工作线程数 | CPU核心数到2倍核心数 |
| `--block-size` | 1048576 | I/O块大小（字节） | 小文件: 512KiB-1MiB, 大文件: 4-16MiB |
| `--pipeline-depth` | 2 | 最小Pipeline深度 | 保持默认即可 |
| `--max-pipeline-depth` | 8 | 最大Pipeline深度 | SSD+RDMA: 16-32 |
| `--shm-usage-ratio` | 0.95 | 共享内存使用比例 | 内存受限时降低 |
| `--recursive` | false | 递归拷贝目录 | 目录拷贝时必需 |
| `--resume` | true | 启用断点续传 | 文件数>100自动启用 |
| `--verify` | false | 校验模式 | 检查文件一致性 |
| `--preserve-attrs` | true | 保留文件属性 | 权限、时间戳等 |
| `--debug` | false | 详细调试信息 | 排查问题时启用 |

### 约束关系

**共享内存容量**:
```
每个Worker的iov_size = (总共享内存 × shm_usage_ratio) / workers
pipeline_depth × block_size ≤ iov_size_per_worker
```

**内存占用**:
```
总共享内存占用 = 总共享内存 × shm_usage_ratio
```

**示例配置**:
```bash
# 小文件优化 (内存充足，64GB共享内存)
--workers 8 --block-size 524288 --shm-usage-ratio 0.3

# 大文件优化 (高性能)
--workers 4 --block-size 16777216 --shm-usage-ratio 0.95

# 内存受限 (4GB共享内存)
--workers 2 --shm-usage-ratio 0.5
```

## 代码结构详解

### main.rs (655行)

```
1-27:     导入和模块声明
28-38:    Ctrl+C处理器设置
39-51:    共享内存状态显示逻辑
52-99:    参数验证
100-148:  共享内存计算和验证
149-175:  启动信息打印
176-203:  创建进度管理器
204-291:  校验模式处理
292-596:  主循环（初次拷贝 + 重试）
597-609:  最终结果返回
```

### worker_context.rs (677行)

```
1-18:     导入
19-36:    WorkerContext struct定义
37-73:    ShmManager struct + Drop实现
74-147:   WorkerContext::new() - 初始化
148-182:  calculate_pipeline_depth() - 动态深度计算
183-677:  copy_file() - 文件拷贝核心逻辑
           ├─ 打开源文件
           ├─ 创建ManagedFd
           ├─ 创建Ior
           ├─ Pipeline循环
           ├─ fsync + ioctl
           ├─ 保留文件属性
           └─ 错误处理
```

### shm_manager.rs (376行)

```
1-32:     导入和工具函数
33-126:   verify_shm_space() - 共享内存空间验证
127-221:  ShmManager::new() - 创建共享内存和Iov
222-376:  工具函数:
           ├─ get_total_shm_size()
           ├─ get_shm_usage()
           ├─ get_shm_available_space()
           ├─ show_shm_status()
           └─ cleanup_stale_shm()
```

### progress.rs (310行)

```
1-29:     ProgressState struct定义
30-119:   ProgressState方法:
           ├─ new()
           ├─ load()
           ├─ save()
           └─ 工具方法
120-310:  ProgressManager struct + 方法
```

### task.rs (317行)

```
1-45:     CopyTask, CopyStats, VerifyResult struct定义
46-96:    collect_tasks() - 任务收集（已废弃）
97-214:   walk_and_send_tasks() - 边遍历边发送
215-317:  verify_files() - 文件校验
```

## 关键函数说明

### WorkerContext::copy_file()

执行单个文件的USRBIO拷贝。

**步骤**:
1. 打开源文件 (普通read)
2. 创建目标文件并注册fd (`ManagedFd::from_raw_fd`)
3. 创建Ior (I/O请求队列)
4. Pipeline循环:
   - read: 从源文件读取数据到共享内存
   - prepare: 准备I/O请求
   - submit: 提交I/O请求
   - poll: 等待I/O完成
5. 同步文件长度 (ioctl)
6. fsync确保数据持久化
7. 保留文件属性
8. 清理资源 (Drop trait自动触发)

**错误处理**:
- 详细错误信息 + 可选backtrace (debug模式)
- 单文件失败不中断整体流程
- 失败统计在最终报告中显示
- 支持重试机制（最多3次）

### WorkerContext::calculate_pipeline_depth()

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
    < 10MiB  => min_pipeline_depth,
    < 100MiB => scale_linearly(10-100),
    < 1GiB   => scale_linearly(100-1000),
    _        => max_pipeline_depth,
};
min(ideal, max_pipeline_depth, max_by_shm)
```

## 开发指南

### 添加新功能

**1. 添加新CLI参数**:
```rust
// src/cli.rs - Args struct
#[arg(short, long, default_value = "...")]
pub new_param: Type,
```

**2. 传递到WorkerContext**:
```rust
// src/main.rs - worker_thread调用
worker_thread(
    ...,
    args.new_param,
    ...
);
```

**3. 更新WorkerContext**:
```rust
// src/worker_context.rs
pub fn new(..., new_param: Type) {
    // 使用新参数
}
```

**4. 更新文档**:
- README.md 参数表
- CLAUDE.md 性能调优章节
- 使用示例

### 调试技巧

**启用详细日志**:
```bash
./cp-with-usrbio ... --debug
```

**输出包含**:
- Worker初始化信息
- 共享内存创建详情
- Pipeline深度计算
- 每个文件的缓冲区使用情况
- 错误堆栈跟踪

**Rust backtrace**:
```bash
RUST_BACKTRACE=full ./cp-with-usrbio ... --debug
```

**检查共享内存状态**:
```bash
./cp-with-usrbio --shm-status
```

### 性能分析

**基准测试场景**:
1. 小文件密集 (10KiB × 10000个)
2. 中等文件 (100MiB × 100个)
3. 大文件 (10GiB × 10个)
4. 混合场景 (各种大小混合)

**关键指标**:
- 吞吐量: MiB/s
- 文件处理速率: files/s
- 内存占用: 总共享内存 × shm_usage_ratio
- CPU利用率: top/htop

**性能瓶颈定位**:
- `strace -c`: 系统调用统计
- `perf record`: CPU性能分析
- `iotop`: I/O监控
- `/proc/meminfo`: 内存使用
- `watch -n 1 'df -h /dev/shm'`: 共享内存监控

## 常见问题

### 1. 找不到动态库

**错误**: `error while loading shared libraries: libhf3fs_api_shared.so`

**解决**:
```bash
export HF3FS_BUILD_DIR=/root/code/3fs/build
export LD_LIBRARY_PATH=$HF3FS_BUILD_DIR/src/lib/api:$LD_LIBRARY_PATH
```

### 2. 共享内存不足

**错误**: `Insufficient /dev/shm space`

**解决方案**:

**方案A**: 减少并发
```bash
--workers 2
```

**方案B**: 降低使用比例
```bash
--shm-usage-ratio 0.5
```

**方案C**: 清理共享内存
```bash
rm -f /dev/shm/cp_usrbio_*
# 或使用工具清理
./cp-with-usrbio --shm-status
```

**方案D**: 增加共享内存
```bash
# 临时增加
sudo mount -o remount,size=8G /dev/shm

# 永久增加（编辑 /etc/fstab）
tmpfs /dev/shm tmpfs defaults,size=8G 0 0
```

### 3. Pipeline depth过小

**错误**: `pipeline_depth exceeds maximum depth allowed by iov_size_per_worker/block_size ratio`

**解决方案**:

**方案A**: 减小块大小
```bash
--block-size 524288  # 512KiB
```

**方案B**: 减少workers
```bash
--workers 2
```

**方案C**: 增加共享内存使用比例
```bash
--shm-usage-ratio 0.98
```

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
- [ ] 共享内存使用比例是否合理？
- [ ] 网络带宽是否饱和？
- [ ] 目标存储是否成为瓶颈？

### 6. Ctrl+C中断后资源未清理

**症状**: `/dev/shm` 中残留 `cp_usrbio_*` 文件

**解决**:
```bash
# 手动清理
./cp-with-usrbio --shm-status

# 或直接删除
rm -f /dev/shm/cp_usrbio_*
```

## 已知问题和修复

### 并发Bug (已修复)

**问题**: 多进程执行时偶发文件复制失败

**根本原因**: 缓冲区重置竞态条件

**修复方案**:
1. 移除危险的缓冲区重置逻辑
2. 添加文件级别同步 (fsync + ioctl)
3. 改进缓冲区wrap-around逻辑
4. 添加缓冲区使用监控

详见: `BUGFIX_CONCURRENCY.md`

### Ctrl+C中断保护 (已实施)

**问题**: 中断时可能造成资源泄露和文件损坏

**解决方案**:
1. 添加Ctrl+C信号处理器
2. Worker线程优雅退出
3. 资源自动清理 (Drop trait)
4. 进度自动保存

详见: `CTRLC_FIX_REPORT.md`

## 未来改进方向

### 短期优化

- [ ] 增量同步功能 (类似rsync)
- [ ] 符号链接处理选项
- [ ] 目录权限保留
- [ ] 并发控制优化

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
- 资源管理使用RAII模式 (Drop trait)

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

建议添加单元测试框架：

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

    #[test]
    fn test_shm_manager_lifecycle() {
        // 测试共享内存创建和清理
    }

    #[test]
    fn test_progress_manager() {
        // 测试进度保存和加载
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
- [ ] 大文件拷贝 (>10GiB)
- [ ] 小文件密集 (>10000个)
- [ ] 错误恢复 (单个文件失败)
- [ ] Debug模式输出
- [ ] 进度显示
- [ ] 文件权限保留
- [ ] 断点续传
- [ ] Ctrl+C中断和恢复
- [ ] 校验模式
- [ ] 重试机制
- [ ] 多进程并发

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

## 相关文档

- [README.md](README.md) - 用户指南

## 联系方式

- 项目路径: `/root/code/cp-with-usrbio`
- 二进制路径: `/root/code/target/cp-with-usrbio`
- 依赖项目: `/root/code/3fs/`
