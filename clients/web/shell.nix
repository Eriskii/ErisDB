# A C compiler for wasm32, which ring needs: `nix-shell clients/web/shell.nix`. The unwrapped
# clang has no builtin headers in its resource directory; they live in its `lib` output.
{ pkgs ? import <nixpkgs> { } }:
let
  llvm = pkgs.llvmPackages_19;
in
pkgs.mkShell {
  packages = [ llvm.clang-unwrapped llvm.lld llvm.libllvm ];
  CC_wasm32_unknown_unknown = "clang";
  AR_wasm32_unknown_unknown = "llvm-ar";
  CFLAGS_wasm32_unknown_unknown = "-isystem ${llvm.clang-unwrapped.lib}/lib/clang/19/include";
}
