{
  description = "rua";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    crane.url = "github:ipetkov/crane";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      crane,
      flake-utils,
      rust-overlay,
      ...
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };

        craneLib = (crane.mkLib pkgs).overrideToolchain (
          p:
          p.rust-bin.stable.latest.default.override {
            extensions = [ "rust-src" ];
            targets = [ "wasm32-unknown-unknown" ];
          }
        );
      in
      {
        devShells.default = craneLib.devShell {
          # dioxus-cli 0.7.10 与 rua-ui 的 dioxus = "=0.7.10" 精确对齐；
          # dx 0.7.10 只接受 wasm-bindgen-cli 0.2.121（nixpkgs 默认版本更新，
          # 会被 dx 拒绝），所以 pin _0_2_121。wasm32 target 由上面 toolchain
          # 的 targets 提供，rust-lld 走 nix 工具链，无需额外垫片。
          packages = [
            pkgs.dioxus-cli
            pkgs.wasm-bindgen-cli_0_2_121
          ];
        };
      }
    );
}
