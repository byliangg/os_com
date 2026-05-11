# SPDX-License-Identifier: MPL-2.0

MKFILE_PATH := $(abspath $(lastword $(MAKEFILE_LIST)))
CUR_DIR := $(patsubst %/,%,$(dir $(MKFILE_PATH)))
INITRAMFS ?= $(CUR_DIR)/../../../build/initramfs
TARGET_DIR := $(INITRAMFS)/opt/xfstests
RUN_BASH := $(CUR_DIR)/run_xfstests_test.sh
TESTCASES_DIR := $(CUR_DIR)/testcases
BLOCKED_DIR := $(CUR_DIR)/blocked
XFSTESTS_PREBUILT_DIR ?=

.PHONY: all
all: $(TARGET_DIR)

$(TARGET_DIR): $(RUN_BASH) $(TESTCASES_DIR) $(BLOCKED_DIR)
	@if [ -z "$(XFSTESTS_PREBUILT_DIR)" ]; then \
		echo "Error: XFSTESTS_PREBUILT_DIR is empty."; \
		echo "Please set XFSTESTS_PREBUILT_DIR to a directory containing xfstests payload."; \
		exit 1; \
	fi
	@rm -rf $@ && mkdir -p $@
	@# Copy phase3 runner and test lists
	@cp -f $(RUN_BASH) $@
	@chmod +x $@/run_xfstests_test.sh
	@cp -r $(TESTCASES_DIR) $@
	@cp -r $(BLOCKED_DIR) $@
	@# Copy xfstests payload
	@if [ -d "$(XFSTESTS_PREBUILT_DIR)/xfstests-dev" ]; then \
		cp -a "$(XFSTESTS_PREBUILT_DIR)/xfstests-dev" "$@/"; \
	elif [ -x "$(XFSTESTS_PREBUILT_DIR)/check" ]; then \
		mkdir -p "$@/xfstests-dev"; \
		cp -a "$(XFSTESTS_PREBUILT_DIR)"/* "$@/xfstests-dev/"; \
	else \
		echo "Error: xfstests payload not found in $(XFSTESTS_PREBUILT_DIR)."; \
		exit 1; \
	fi
	@# Optional helper toolchain used by xfstests scripts
	@if [ -d "$(XFSTESTS_PREBUILT_DIR)/tools" ]; then \
		cp -a "$(XFSTESTS_PREBUILT_DIR)/tools" "$@/"; \
	fi

.PHONY: clean
clean:
	@rm -rf $(TARGET_DIR)
