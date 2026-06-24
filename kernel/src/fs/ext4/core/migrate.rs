// SPDX-License-Identifier: MPL-2.0
//! ext2 → ext4 inode 块映射迁移接缝（report §5.9）——**预留空模块**，本 Task 不实现。
//!
//! 当一个 legacy（间接映射）inode 首次以 extent 路径增长时，ext4 会把其直接/间接块映射
//! 迁移成 extent 树（对照 ext4_rs `file.rs` 中 `EXT4_INODE_FLAG_EXTENTS` 的置位与初始化路径，
//! file.rs:617；以及 Linux `fs/ext4/migrate.c`）。本项目把 ext2 兼容 + 迁移作为后续 phase 的
//! 接缝预留；当前真镜像创建即 extent inode，无迁移触发。
//!
//! 本模块当前仅占位（无导出符号），等 ext2 间接路径落地后再填迁移逻辑。
