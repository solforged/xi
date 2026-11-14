# rustup and the xi-install command. `direnv allow` loads the shell; outside
# it, run `direnv exec . xi-install`. rustup takes Rust from
# rust-toolchain.toml.
{ pkgs, options, ... }:

{
  # No Nix compiler, Apple SDK or xcrun shim: cargo links with the host's
  # compiler, as it does under a plain rustup.
  stdenv = pkgs.stdenvNoCC;
  apple.sdk = null;
  # stdenv sets these for Nix builds; they would pin the binaries' minimum
  # macOS to the SDK's and stamp build outputs with 1980.
  unsetEnvVars = options.unsetEnvVars.default ++ [
    "SOURCE_DATE_EPOCH"
    "MACOSX_DEPLOYMENT_TARGET"
    "ZERO_AR_DATE"
  ];

  packages = [ pkgs.rustup ];

  scripts.xi-install = {
    description = "Build release xi and its sandbox helper into $XDG_BIN_HOME or ~/.local/bin";
    # Builds the checkout that contains the working directory, rather than
    # $DEVENV_ROOT, so a shell inherited from another worktree builds its own.
    exec = ''
      set -euo pipefail
      root=$PWD
      until [ -f "$root/devenv.nix" ] && [ -d "$root/crates/xi-sandbox" ]; do
        if [ "$root" = / ]; then
          echo "xi-install: run inside an xi checkout" >&2
          exit 1
        fi
        root=$(dirname "$root")
      done
      cd "$root"
      cargo build --release --locked -p xi-cli -p xi-sandbox
      target="''${CARGO_TARGET_DIR:-$root/target}/release"
      bin="''${XDG_BIN_HOME:-$HOME/.local/bin}"
      mkdir -p "$bin"
      # xi runs tools through the xi-sandbox-exec beside its own binary.
      install -m 755 "$target/xi" "$target/xi-sandbox-exec" "$bin/"
      echo "installed xi and xi-sandbox-exec in $bin"
    '';
  };
}
