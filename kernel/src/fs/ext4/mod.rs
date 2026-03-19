// SPDX-License-Identifier: MPL-2.0

mod fs;
mod inode;

use fs::Ext4Type;

pub(super) fn init() {
    super::registry::register(&Ext4Type).unwrap();
}
