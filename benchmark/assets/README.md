# benchmark 运行资产

该目录存放 benchmark/xfstests/lmbench 流程所需的运行资产，目标是让流程默认不依赖 `.local`。

## 目录说明

- `initramfs/`
  - `initramfs_phase3.cpio.gz`：测试基底 initramfs（必需）
  - `initramfs_phase4_part3.cpio.gz`：phase4 运行时使用的可重打包镜像（可自动生成）
- `xfstests-prebuilt/`
  - `xfstests-dev/` 与 `tools/`：xfstests 运行时预构建资产（建议必需）
- `linux_vdso/`
  - `vdso_x86_64.so`、`vdso_riscv64.so`：构建内核时的 vDSO 依赖
- `xfstests-src/`
  - xfstests 源码快照，用于离线重建 prebuilt 或同步样例数据

## 使用说明

当前 ext4 测试脚本默认读取本目录资产：

- `tools/ext4/run_phase4_in_docker.sh`
- `tools/ext4/run_phase4_part1.sh`
- `tools/ext4/run_phase4_part2.sh`
- `tools/ext4/run_phase4_part3.sh`
- `tools/ext4/prepare_phase4_part*_initramfs.sh`
- `tools/ext4/prepare_xfstests_prebuilt.sh`

## 更新方式

若你在 `.local` 更新了依赖，可重新复制到这里后再提交。推荐同步后执行：

```bash
./benchmark/sync_dataset.sh
```
