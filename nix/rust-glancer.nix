{ lib, stdenv, stdenvNoCC, fetchurl, unzip, autoPatchelfHook }:

let
  version = "0.1.1";
  assets = {
    "x86_64-linux" = {
      platform = "linux-x64";
      sha256 = "a07df244d16e266aa661a75c1a72006b307a5ab35ce413f4ec8bf2ad1ba543d3";
    };
    "aarch64-linux" = {
      platform = "linux-arm64";
      sha256 = "bb3c91ae0b8b5984d91c16e27533030b94d96cbc3798277f4b3e502b16e4ea06";
    };
    "x86_64-darwin" = {
      platform = "darwin-x64";
      sha256 = "436e6451e8885e182e8a09ca09186a9b65217f725197b72909e93e6b31d88f65";
    };
    "aarch64-darwin" = {
      platform = "darwin-arm64";
      sha256 = "dac95f6a2ad7cef36c552fd14eb5cf475a6eb3093f4589ecc65aaa841a0339a3";
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
