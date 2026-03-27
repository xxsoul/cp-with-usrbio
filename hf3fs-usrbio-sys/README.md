# hf3fs-usrbio-sys

3FS USRBIO API的Rust绑定，从3FS项目中独立出来维护。

## ⚠️ 重要：版本匹配检查

**在使用本绑定之前，必须检查头文件版本是否匹配！**

本项目的 `include/hf3fs_usrbio.h` 是从3FS源码复制的快照，可能与您部署的3FS版本存在差异。

### 检查步骤

1. **找到您的3FS源码头文件**：
   ```bash
   # 通常位于3FS源码目录
   YOUR_3FS_HEADER="/root/code/3fs/src/lib/api/hf3fs_usrbio.h"
   ```

2. **对比头文件**：
   ```bash
   # 查看差异
   diff -u "$YOUR_3FS_HEADER" include/hf3fs_usrbio.h

   # 或使用更详细的对比
   git diff --no-index "$YOUR_3FS_HEADER" include/hf3fs_usrbio.h
   ```

3. **处理差异**：

   **情况A：无差异或仅有注释差异**
   - ✅ 可以直接使用

   **情况B：有API差异**
   - ❌ **不要直接使用**
   - 复制您的3FS头文件到本项目：
     ```bash
     cp "$YOUR_3FS_HEADER" include/hf3fs_usrbio.h
     ```
   - 检查 `src/lib.rs` 中的Rust封装代码是否需要调整
   - 重新构建：`cargo build --release`

### 常见版本不匹配问题

| 症状 | 可能原因 | 解决方法 |
|------|----------|----------|
| 链接错误：符号未定义 | API签名变更 | 更新头文件，调整Rust封装 |
| 运行时段错误 | 结构体字段变更 | 更新头文件，重新生成绑定 |
| 编译错误：类型不匹配 | 类型定义变更 | 更新头文件和Rust代码 |

### 版本记录

当前绑定的头文件来自：
- **3FS版本/提交**: （请根据实际情况更新）
- **复制时间**: 2026-03-26
- **源路径**: `/root/code/3fs/src/lib/api/hf3fs_usrbio.h`

如果更新了头文件，请更新上述记录。

---

## 构建

在构建之前，确保：

1. ✅ **已完成版本匹配检查**（见上方）
2. 设置环境变量 `HF3FS_BUILD_DIR` 指向3FS构建目录
3. 3FS的CMake目标 `hf3fs_api_shared` 已经构建完成

```bash
export HF3FS_BUILD_DIR=/root/code/3fs/build
cargo build
```

## 结构

- `include/hf3fs_usrbio.h` - 3FS USRBIO头文件（从3FS源码复制）
- `src/lib.rs` - Rust封装实现
- `build.rs` - 构建脚本，使用bindgen生成绑定

## 依赖

- `libhf3fs_api_shared.so` - 来自3FS构建产物
- `bindgen` - 构建时自动生成C绑定

## 与上游的区别

本绑定基于3FS项目的 `src/lib/rs/hf3fs-usrbio-sys`，但包含以下改进：

1. 独立的头文件管理（不再依赖3FS源码树）
2. 更清晰的构建配置
3. 适合特定项目需求的自定义修改

## License

本项目作为 [cp-with-usrbio](https://github.com/xxsoul/cp-with-usrbio) 的一部分，采用 MIT 许可证开源。详见项目根目录的 [LICENSE](../LICENSE) 文件。
