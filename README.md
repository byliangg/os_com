# EXT4 on Asterinas · 面向 RustOS 的高性能强一致性文件系统

在 **星绽(Asterinas)** framekernel 上,用 Rust 从零实现的 **ext4** 文件系统。不移植 Linux 的 C 代码,磁盘格式和算法对齐 Linux 6.6,代码风格贴 Asterinas 里已有的 ext2。

> **2026 年全国大学生计算机系统能力大赛 · 操作系统设计赛 · OS 功能挑战赛道**
> 赛题:面向 RustOS 的高性能强一致性文件系统研究(Research on High-Performance and Strong-Consistency File System for RustOS)
>
> 本仓库基于 [Asterinas](https://github.com/asterinas/asterinas)(上游 README 在 git 历史里)。ext4 的全部代码在 [`kernel/src/fs/fs_impls/ext4/`](kernel/src/fs/fs_impls/ext4/),约 3.9 万行 Rust。

## 现在做到哪一步

赛题分基础 / 进阶 / 优秀三档。功能面已经覆盖到优秀档(完整 JBD2 日志 + 多文件并发),正确性达标,性能对标留到收尾阶段:

| 维度 | 优秀档门槛 | 现状 |
|---|---|---|
| 功能 | JBD2 完整 + 并发 | ✅ revoke / 组提交 / 懒 checkpoint / 日志校验和 / 64bit 全接线 |
| 正确性 | xfstests ≥ 95% | ✅ **63 / 66 = 95.5%** |
| 崩溃一致性 | 100% 一致 | ✅ 160 个 workload 掉电,e2fsck 全 CLEAN |
| 性能 | ≥ Linux 90% + 一项 ≥5% 优化 | ◐ buffered 顺序写 21% / 暖读 75%,优化在收尾阶段 |

## 实现了哪些功能

**POSIX 接口**:create、open、close、read、write、truncate、lseek、mkdir、rmdir、unlink、rename、stat、mknod、symlink、link、readdir、fsync、fdatasync、fallocate、statfs 等。

**ext4 核心特性**:

- **Extent 块映射** —— depth≤2 的 extent 树;写洞时先分配成 unwritten(读出来是零),写完再在同一个事务里转成 written,避免崩溃后暴露别的文件释放掉的陈旧数据。
- **JBD2 完整日志** —— ordered 数据模式;事务 / 组提交 / 懒 checkpoint / revoke;崩溃恢复三遍扫描(SCAN / REVOKE / REPLAY);日志校验和 v2/v3;64bit tag;精确的信用记账 + 空间背压;abort 后转只读;`EXT4_IOC_SHUTDOWN`。
- **崩溃一致性** —— RECOVER 位的整个生命周期;孤儿链回收,以及把被中断的 truncate 续做完。
- **目录** —— 线性目录的全套命名空间操作;htree 索引读 + 插入前降级为线性;目录哈希与 e2fsprogs 逐位对拍。
- **块 / inode 分配** —— 位图 first-fit;flex_bg;BLOCK_UNINIT / INODE_UNINIT 惰性初始化;ENOSPC 后提交再重试。
- **现代特性** —— metadata_csum(crc32c,校验超级块 / 组描述符 / inode / 位图 / extent / 目录六种结构)、64bit、flex_bg、HUGE_FILE。
- **持久化** —— fsync / fdatasync(tid 级区分)、O_SYNC / O_DSYNC、整卷 sync。

镜像特性接近默认 `mke2fs`(`has_journal,extent,filetype,metadata_csum,dir_index,64bit,flex_bg`),块大小 4K、inode 256 字节。

## 性能(fio / SQLite)

对标 Linux 6.16,同一个容器、同一套 QEMU、同一块盘、同轮交替测,buffered(`-direct=0`),**串行跑**(并发会污染吞吐数):

| 用例 | 本实现 | Linux 6.16 | 比值 |
|---|---:|---:|---:|
| fio 顺序写(1M) | 162 MiB/s | 766 MiB/s | **21%** |
| fio 顺序读(暖,命中页缓存) | 5020 MiB/s | 6693 MiB/s | **75%** |
| fio 顺序读(冷) | 30.7 MiB/s | 5044 MiB/s | 口径污染,不采信 |
| SQLite `speedtest1 --size 1000` | 未跑完 | 57.3s | — |

两点要说清楚。冷读那一档,host 的 `drop_caches` 在容器里没权限,两侧都被宿主的页缓存污染,所以只有写(21%)和暖读(75%)的比值是干净的。SQLite 跑不完,是崩在 CREATE INDEX:带索引的插入会把 extent 树打碎,每插一条就重写整棵树(O(n²)),这正是下面"还没实现"里排第一的那笔。这些数字是**性能优化之前的地板基线**。

## 还没实现

按赛题路线,下面这些留给收尾的两个阶段(崩溃穷举验证 / 性能优化 + 文档):

- **extent 树就地增删** —— 现在每次插入都重写整棵树,高碎片或带索引的负载会崩(SQLite 的 CREATE INDEX 就是)。改成 Linux 那样就地改树,是最要紧的一笔。
- **O_DIRECT** —— 写会返回 `EOPNOTSUPP`,读则静默走页缓存。ext2 已经有、我们还没做,算是对 ext2 的一处功能回退。
- **块分配器优化** —— 现在只是 first-fit 骨架,Orlov 铺散和局部性都没做(这是赛题点名要的那项 ≥5% 优化)。
- 其它:htree 的构建(让大目录插入保持 O(log n),现在是插入前降级线性)、冷读 readahead、mmap 写洞的回写、`minixdf` 版 statfs。

**明确不做**(带这些特性的镜像会被直接拒挂):fast_commit、bigalloc、casefold、加密 / verity、resize / migrate、mmp、inline_data、外部日志、`data=journal`。

## 怎么测

所有构建和测试都在固定版本的 Docker 容器里跑,换台机器拉同一个镜像就能复现同样的结果:

```bash
docker run -it --privileged --network=host -v /dev:/dev \
  -v $(pwd)/asterinas:/root/asterinas \
  asterinas/asterinas:0.18.0-20260618
# 进容器后,下面的命令都在 /root/asterinas 下跑
```

测试分四层,职责不重叠:

| 层 | 测什么 | 怎么跑 | 结果 |
|---|---|---|---|
| 单元(ktest) | extent 树、目录项、位图分配、日志、校验和等内部逻辑 | `make ktest` | **510** 全过 |
| xfstests | POSIX 一致性,真实镜像端到端 | 由 Makefile 的 `XFSTESTS_RUNLIST` 指定清单(在 [`test/initramfs/src/conformance/xfstests/`](test/initramfs/src/conformance/xfstests/)),在 guest 里跑官方 `./check` | **63 / 66 = 95.5%** |
| 崩溃一致性 | 掉电后日志重放是否一致 | `bash test/crash/run_matrix.sh` | 160 workload × 掉电点,e2fsck 全 CLEAN |
| 性能 | 对标 Linux ext4 | `bash test/initramfs/src/benchmark/run_ext4_bench.sh <suite/job>` | 见上表 |

- **崩溃 harness**:QEMU 用 `blklogwrites` 录下写流,在每个 FLUSH 点做前缀重放,再用 e2fsck 严格模式(`-fy`)判一致。
- **xfstests 口径**:清单继承自上游 Asterinas 的 ext2 CI。通过率 = PASS /(PASS + 真 FAIL),NOTRUN 和环境崩溃不计入分母,并逐条写明原因。95.5% 里唯一"跑不了"的一批,全是 O_DIRECT 用例,正好对应上面那处功能回退。

## 代码结构

```
kernel/src/fs/fs_impls/ext4/
├── super_block.rs / feature.rs   挂载、特性门、几何校验
├── block_group.rs                组描述符、位图、first-fit 分配、flex_bg
├── inode/                        inode 编解码、读写、truncate、fallocate
│   ├── extent_manager/           extent 树(逻辑块↔物理块映射)
│   └── dir/                      目录项、htree 读、目录哈希
├── journal/                      自研 JBD2(事务 / 提交 / 恢复 / revoke / checkpoint / 格式)
└── impl_for_vfs/                 VFS trait 实现(FileOps / Inode / FileSystem)
```

## 说明

本项目是在 [Asterinas](https://github.com/asterinas/asterinas)(星绽,一个 Rust framekernel)之上从零实现的 ext4,遵循 Asterinas 的代码规范,目标是能合入上游。除 ext4 之外的内核部分都是 Asterinas 上游代码。许可证沿用 Asterinas:MPL-2.0。
