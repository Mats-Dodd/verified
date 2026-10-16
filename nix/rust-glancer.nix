{ lib, stdenv, stdenvNoCC, fetchurl, unzip, autoPatchelfHook }:

let
  version = "0.3.0";
  assets = {
    "x86_64-linux" = {
      platform = "linux-x64";
      sha256 = "aef20d0e0ccf70cb1643caafd4c6321ea95cb786d1a413cca963079f5f8184c2";
    };
    "aarch64-linux" = {
      platform = "linux-arm64";
      sha256 = "e829e382b3274f870c559547568536bf3bdc8b268880e3b8e38f27d5e5e0e504";
    };
    "x86_64-darwin" = {
      platform = "darwin-x64";
      sha256 = "c9b3222978d875210b2c22a1a22b3dd4ed5a5b083aa814b4887386acce26eeb3";
    };
    "aarch64-darwin" = {
      platform = "darwin-arm64";
      sha256 = "0b05e2e29edb262590eb6d470ddb7b3264e72620271310f12b9ece87f395ffb6";
    };
  };
  asset = assets.${stdenv.hostPlatform.system}
    or (throw "rust-glancer is unsupported on ${stdenv.hostPlatform.system}");
in
stdenvNoCC.mkDerivation {
  pname = "rust-glancer";
  inherit version;

  src = fetchurl {
    url = "https://github.com/rust-glancer/rust-glancer/releases/download/v${version}/rust-glancer-${version}-${asset.platform}.vsix";
    inherit (asset) sha256;
  };

  nativeBuildInputs = [ unzip ] ++ lib.optionals stdenv.hostPlatform.isLinux [ autoPatchelfHook ];
  buildInputs = lib.optionals stdenv.hostPlatform.isLinux [ stdenv.cc.cc.lib ];
  dontUnpack = true;

  installPhase = ''
    runHook preInstall
    mkdir -p "$out/bin"
    unzip -p "$src" extension/server/rust-glancer > "$out/bin/rust-glancer"
    chmod +x "$out/bin/rust-glancer"
    runHook postInstall
  '';

  meta = {
    description = "Lightweight Rust LSP optimized for low memory usage";
    homepage = "https://github.com/rust-glancer/rust-glancer";
    license = lib.licenses.mit;
    mainProgram = "rust-glancer";
    platforms = builtins.attrNames assets;
  };
}
