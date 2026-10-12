{ lib, stdenv, stdenvNoCC, fetchurl, unzip, makeWrapper, autoPatchelfHook, rustToolchain }:

# Verus ships prebuilt binaries whose `rust_verify` links the exact rustc
# driver of the toolchain it was built against (1.98.1 for this release).
# The upstream launcher finds that toolchain through rustup; here nix supplies
# it instead (`rustToolchain`, from rust-overlay's official binaries, so the
# driver's hash matches) and VERUS_USE_RUSTUP=0 tells the launcher to trust it.
let
  version = "0.2026.10.04.426d8b0";
  assets = {
    "aarch64-darwin" = {
      platform = "arm64-macos";
      sha256 = "83cafc50284caea07fa8762df04b00cc0af441f25cdf1631d6c30987c65af446";
    };
    "x86_64-darwin" = {
      platform = "x86-macos";
      sha256 = "9b00357ee298a3b052038abc85c0be3129ab2821762fe8be5eb8dcbe37349f9c";
    };
    "x86_64-linux" = {
      platform = "x86-linux";
      sha256 = "624018662868563a7bec0dd07be5e4dd0c3d61124b4b55105765884bf2d4d1a0";
    };
  };
  asset = assets.${stdenv.hostPlatform.system}
    or (throw "verus is unsupported on ${stdenv.hostPlatform.system}");
  libraryPathVar =
    if stdenv.hostPlatform.isDarwin then "DYLD_FALLBACK_LIBRARY_PATH" else "LD_LIBRARY_PATH";
in
stdenvNoCC.mkDerivation {
  pname = "verus";
  inherit version;

  src = fetchurl {
    url = "https://github.com/verus-lang/verus/releases/download/release/${version}/verus-${version}-${asset.platform}.zip";
    inherit (asset) sha256;
  };

  nativeBuildInputs = [ unzip makeWrapper ] ++ lib.optionals stdenv.hostPlatform.isLinux [ autoPatchelfHook ];
  buildInputs = [ rustToolchain ] ++ lib.optionals stdenv.hostPlatform.isLinux [ stdenv.cc.cc.lib ];
  # The rustc driver is found at run time through the toolchain's lib dir.
  autoPatchelfIgnoreMissingDeps = [ "librustc_driver-*.so" "libLLVM*.so*" "libstd-*.so" ];
  dontUnpack = true;
  # Stripping removes the .rmeta section from libvstd.rlib, which breaks every
  # `use vstd::...`.
  dontStrip = true;

  installPhase = ''
    runHook preInstall
    mkdir -p "$out/libexec" "$out/bin"
    unzip -q "$src" -d "$out/libexec"
    mv "$out/libexec/verus-${asset.platform}" "$out/libexec/verus"
    # `cargo-verus` invokes its sibling `verus` as RUSTC_WRAPPER, so both
    # wrappers carry the same environment and the inner calls inherit it.
    for exe in verus cargo-verus; do
      makeWrapper "$out/libexec/verus/$exe" "$out/bin/$exe" \
        --set VERUS_USE_RUSTUP 0 \
        --prefix ${libraryPathVar} : "${rustToolchain}/lib"
    done
    runHook postInstall
  '';

  meta = {
    description = "Verified Rust for low-level systems code";
    homepage = "https://github.com/verus-lang/verus";
    license = lib.licenses.mit;
    mainProgram = "cargo-verus";
    platforms = builtins.attrNames assets;
  };
}
