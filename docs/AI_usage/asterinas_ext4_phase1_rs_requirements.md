# Asterinas ext4 移植提示词（阶段1）

## 1. 任务目标

把 `ext4_rs` 集成到 `asterinas`，让内核支持基础 ext4 文件操作（阶段1），不做日志恢复。

项目路径：
- `asterinas`: `/home/lby/os_com_codex/asterinas`
- `ext4_rs`: `/home/lby/os_com_codex/ext4_rs`

## 2. 阶段1必须实现

1. `mount -t ext4` 可挂载 ext4 镜像。
2. `lookup`、`readdir` 可用（`ls` 正常）。
3. `read_at` 可用（`cat` 正常）。
4. `create`、`write_at` 可用（`touch`、`echo > file` 正常）。
5. `unlink` 可用（删除普通文件）。
6. `mkdir` 可用。
7. `rmdir` 可用（仅要求空目录）。
8. `/proc/filesystems` 中可看到 `ext4`。

## 3. 阶段1不做

1. journaling/jbd2 与崩溃恢复保证。
2. `rename`、硬链接、符号链接。
3. xattr/acl/quota/ioctl/fallocate。
4. 性能优化与细粒度并发优化。

## 4. 实现范围（应改这些地方）

1. 在 `asterinas/kernel/src/fs/` 新增 `ext4/` 模块。
2. 在 `asterinas/kernel/src/fs/mod.rs` 中注册 `ext4::init()`。
3. 实现：
- `Ext4Type`（实现 `FsType`）
- `Ext4Fs`（实现 `FileSystem`）
- `Ext4Inode`（实现 `Inode`/`InodeIo`）
4. 增加块设备适配器：把 `Arc<dyn aster_block::BlockDevice>` 适配成 `ext4_rs::BlockDevice`。

## 5. 实现约束（必须遵守）

1. 默认使用 4KiB ext4 镜像：`mkfs.ext4 -b 4096`。
2. 首版可用全局粗粒度锁保护 `ext4_rs::Ext4`。
3. 禁止把可恢复错误变成 panic，统一映射为 Asterinas `Errno`。
4. `FileSystem::sync()` 要下推到块设备 flush。
5. 不依赖 `simple_interface::ext4_file_open`（该路径存在已知行为风险），优先用稳定底层接口封装。

## 6. 交付物

1. 可编译运行的 ext4 集成代码。
2. 最小测试脚本，覆盖：挂载、`ls`、`cat`、`touch`、`echo`、`rm`、`mkdir`、`rmdir`。
3. 一份简短已知限制说明（明确“无 journaling 保证”）。

## 7. 验收标准

1. 内核编译通过并启动。
2. ext4 可挂载并完成阶段1全部操作。
3. 基础读写无内核 panic。
4. Linux 挂载同一镜像时可读到 Asterinas 写入的数据。
