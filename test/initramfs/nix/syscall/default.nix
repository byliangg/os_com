{ lib, stdenvNoCC, callPackage, testSuite ? "ltp", workDir ? "/tmp", smp ? 1,
}:
let
  envPathOrNull = envName:
    let
      envVal = builtins.getEnv envName;
    in
    if envVal == "" then null else builtins.path {
      name = "${envName}-prebuilt";
      path = envVal;
    };
in rec {
  inherit testSuite;
  ltp = callPackage ./ltp.nix { };
  # FIXME: Build gvisor syscall test with nix.
  gvisor = envPathOrNull "GVISOR_PREBUILT_DIR";
  # NOTE: This currently expects a prepared xfstests tree from the host env.
  # The tree must contain `xfstests-dev/` and may optionally contain `tools/bin/`.
  xfstests = envPathOrNull "XFSTESTS_PREBUILT_DIR";

  package = stdenvNoCC.mkDerivation {
    pname = "syscall_test";
    version = "0.1.0";
    src = lib.fileset.toSource {
      root = ./../../src;
      fileset = ./../../src/syscall;
    };
    buildCommand = ''
      cd $src/syscall
      mkdir -p $out
      export INITRAMFS=$out
      export LTP_PREBUILT_DIR=${ltp}
      export SYSCALL_TEST_SUITE=${testSuite}
      export SYSCALL_TEST_WORKDIR=${workDir}
      export SMP=${toString smp}
      if [ "${testSuite}" = "gvisor" ]; then
        if [ -z "${toString gvisor}" ]; then
          echo "Error: GVISOR_PREBUILT_DIR is not set"
          exit 1
        fi
        export GVISOR_PREBUILT_DIR=${toString gvisor}
      fi
      if [ "${testSuite}" = "xfstests" ]; then
        if [ -z "${toString xfstests}" ]; then
          echo "Error: XFSTESTS_PREBUILT_DIR is not set"
          exit 1
        fi
        export XFSTESTS_PREBUILT_DIR=${toString xfstests}
      fi
      make
    '';
  };
}
