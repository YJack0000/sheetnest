#!/usr/bin/env bash
# Build the `sheetnest` npm package (web + node targets) into pkg/.
#
# Everything this needs beyond rustup and a C++-less toolchain is downloaded
# into .tmp/ on first run, so CI and a laptop run the exact same steps:
#
#   - WASI SDK, because the geometry kernel (clipper2 -> clipper2c-sys) is C++
#     and wasm32-unknown-unknown has no libc++ of its own. Compiling the C++
#     for wasm32-wasip1 and linking it into a wasm32-unknown-unknown cdylib is
#     the recipe clipper2c-sys itself vendors.
#   - wasm-pack, as a prebuilt release binary. `cargo install wasm-pack` is
#     avoided on purpose: it takes minutes and, as of 0.15, its lockfile needs
#     a newer rustc than this workspace's MSRV toolchain.
#
# Usage: scripts/build.sh [--dev]
set -euo pipefail

WASI_SDK_VERSION=24
WASM_PACK_VERSION=0.15.0
PKG_NAME=sheetnest

cd "$(dirname "$0")/.."
CRATE_DIR="$PWD"
TMP="$CRATE_DIR/.tmp"
mkdir -p "$TMP/bin"

PROFILE=--release
[ "${1:-}" = "--dev" ] && PROFILE=--dev

# Keep out of the workspace's default target dir so a concurrent host build
# does not fight us for the lock.
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$CRATE_DIR/../../target/wasm}"

case "$(uname -s)-$(uname -m)" in
  Darwin-arm64)  WASI_ARCH=arm64-macos;  HOST=aarch64-apple-darwin ;;
  Darwin-x86_64) WASI_ARCH=x86_64-macos; HOST=x86_64-apple-darwin ;;
  Linux-x86_64)  WASI_ARCH=x86_64-linux; HOST=x86_64-unknown-linux-musl ;;
  Linux-aarch64) WASI_ARCH=arm64-linux;  HOST=aarch64-unknown-linux-musl ;;
  *) echo "unsupported host $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac

# wasm-pack refuses to run a wasm-bindgen whose version does not match the
# crate, so read the version cargo actually resolved rather than pinning a
# second copy of it here.
WASM_BINDGEN_VERSION=$(
  awk '/^name = "wasm-bindgen"$/ { getline; gsub(/[",]/, ""); print $3; exit }' \
    "$CRATE_DIR/../../Cargo.lock"
)
[ -n "$WASM_BINDGEN_VERSION" ] || { echo "cannot read wasm-bindgen version from Cargo.lock" >&2; exit 1; }

# Fetch a prebuilt release binary into .tmp/bin unless one is already on PATH.
# $1 repo, $2 tool, $3 version, $4 tag, $5 archive stem
fetch_tool() {
  local repo=$1 tool=$2 version=$3 tag=$4 stem=$5
  if command -v "$tool" >/dev/null 2>&1 &&
     "$tool" --version 2>/dev/null | grep -qw "$version"; then
    command -v "$tool"
    return
  fi
  if [ ! -x "$TMP/bin/$tool" ]; then
    echo "==> downloading $tool $version ($HOST)" >&2
    curl -fsSL "https://github.com/$repo/releases/download/$tag/$stem.tar.gz" \
      -o "$TMP/$tool.tar.gz"
    tar -xzf "$TMP/$tool.tar.gz" -C "$TMP"
    mv "$TMP/$stem/$tool" "$TMP/bin/$tool"
    rm -rf "$TMP/$tool.tar.gz" "$TMP/$stem"
  fi
  echo "$TMP/bin/$tool"
}

# ---- WASI SDK -------------------------------------------------------------
WASI_SDK="$TMP/wasi-sdk-${WASI_SDK_VERSION}.0-${WASI_ARCH}"
if [ ! -d "$WASI_SDK" ]; then
  echo "==> downloading WASI SDK ${WASI_SDK_VERSION} ($WASI_ARCH)"
  curl -fsSL "https://github.com/WebAssembly/wasi-sdk/releases/download/wasi-sdk-${WASI_SDK_VERSION}/wasi-sdk-${WASI_SDK_VERSION}.0-${WASI_ARCH}.tar.gz" \
    | tar -xz -C "$TMP"
fi

# ---- wasm-pack + wasm-bindgen ---------------------------------------------
# wasm-pack would otherwise `cargo install wasm-bindgen-cli`, which builds
# `ring` for the host and so trips over the WASI clang exported below (and
# takes minutes). Both are downloaded prebuilt instead.
WASM_PACK=$(fetch_tool rustwasm/wasm-pack wasm-pack "$WASM_PACK_VERSION" \
  "v$WASM_PACK_VERSION" "wasm-pack-v$WASM_PACK_VERSION-$HOST")
fetch_tool wasm-bindgen/wasm-bindgen wasm-bindgen "$WASM_BINDGEN_VERSION" \
  "$WASM_BINDGEN_VERSION" "wasm-bindgen-$WASM_BINDGEN_VERSION-$HOST" >/dev/null
PATH="$TMP/bin:$PATH"
export PATH

# ---- toolchain env --------------------------------------------------------
rustup target add wasm32-unknown-unknown >/dev/null

# Target-scoped on purpose: a bare CC/CXX/AR would also be used for host
# build scripts and proc-macros, which must keep using the system compiler.
export CC_wasm32_unknown_unknown="$WASI_SDK/bin/clang"
export CXX_wasm32_unknown_unknown="$WASI_SDK/bin/clang++"
export AR_wasm32_unknown_unknown="$WASI_SDK/bin/llvm-ar"
export CFLAGS_wasm32_unknown_unknown="--target=wasm32-wasip1"
# No exceptions, no RTTI: Clipper2 guards every `throw` behind
# __cpp_exceptions (DoError becomes a no-op), and without them the archive
# stops referencing libc++abi's typeinfo, vtables and unwinder — the whole
# class of symbols that otherwise has to be faked in src/cxx_abi.rs. libc++'s
# own abort path is a variadic function no stable Rust can define, so it is
# redirected to a trap at the preprocessor level.
export CXXFLAGS_wasm32_unknown_unknown="--target=wasm32-wasip1 -fno-exceptions -fno-rtti -D_LIBCPP_VERBOSE_ABORT(...)=__builtin_trap()"
# The `cc` crate emits `-lstdc++` for any target it does not recognise, and
# wasm32-unknown-unknown has no such library. Empty means "link no C++ stdlib":
# what clipper2c needs is already in the archive the WASI SDK produced.
export CXXSTDLIB_wasm32_unknown_unknown=""

# ---- build ----------------------------------------------------------------
rm -rf "$CRATE_DIR/pkg"
for target in web nodejs; do
  out=$([ "$target" = nodejs ] && echo node || echo web)
  echo "==> wasm-pack build --target $target"
  "$WASM_PACK" build "$PROFILE" \
    --target "$target" \
    --out-dir "pkg/$out" \
    --out-name "$PKG_NAME" \
    --no-pack
done

# ---- assemble the npm package --------------------------------------------
# npm/package.json is the published manifest; the crate-root package.json is
# just the dev harness (a second one named "sheetnest" up here would make Node
# self-reference resolve `import "sheetnest"` to the crate dir, not to pkg/).
# The published version is the workspace version; npm/package.json only
# carries a placeholder so the two can never drift apart.
VERSION=$(grep -m1 '^version = ' "$CRATE_DIR/../../Cargo.toml" | sed 's/.*"\(.*\)"/\1/')
[ -n "$VERSION" ] || { echo "cannot read workspace version" >&2; exit 1; }
node -e '
  const fs = require("fs");
  const [src, dst, version] = process.argv.slice(1);
  const pkg = JSON.parse(fs.readFileSync(src, "utf8"));
  pkg.version = version;
  fs.writeFileSync(dst, JSON.stringify(pkg, null, 2) + "\n");
' "$CRATE_DIR/npm/package.json" "$CRATE_DIR/pkg/package.json" "$VERSION"
cp "$CRATE_DIR/README.md" "$CRATE_DIR/pkg/README.md"
cp "$CRATE_DIR/../../LICENSE-MIT" "$CRATE_DIR/../../LICENSE-APACHE" "$CRATE_DIR/pkg/"
# wasm-pack drops a .gitignore in each out-dir that would make npm skip
# everything; `files` in our package.json is the allowlist that matters.
rm -f "$CRATE_DIR"/pkg/*/.gitignore

# Node resolves a file's module type from the *nearest* package.json, and the
# root one says "type": "module". The nodejs target is CommonJS, so it needs
# its own marker or `require()` of it would be parsed as ESM and blow up.
printf '{ "type": "commonjs" }\n' > "$CRATE_DIR/pkg/node/package.json"
printf '{ "type": "module" }\n'   > "$CRATE_DIR/pkg/web/package.json"

# Let `import 'sheetnest'` resolve through the real exports map in the tests
# without an npm install.
mkdir -p "$CRATE_DIR/node_modules"
rm -rf "$CRATE_DIR/node_modules/$PKG_NAME"
ln -s ../pkg "$CRATE_DIR/node_modules/$PKG_NAME"

# ---- guard: no unresolved C++ symbols ------------------------------------
# The geometry kernel is C++ compiled against libc++ headers with no libc++
# linked (see src/cxx_abi.rs). Anything it still needs lands as an import from
# the `env` module, which neither a browser nor Node can satisfy: the module
# would fail to instantiate at runtime. Catch it here instead.
node -e '
  const fs = require("fs");
  let bad = 0;
  for (const f of process.argv.slice(1)) {
    const m = new WebAssembly.Module(fs.readFileSync(f));
    const env = WebAssembly.Module.imports(m).filter((i) => i.module === "env");
    if (env.length) {
      bad = 1;
      console.error(`${f}: unresolved C++ symbols, add them to src/cxx_abi.rs:`);
      for (const i of env) console.error(`    ${i.name}`);
    }
  }
  process.exit(bad);
' "$CRATE_DIR"/pkg/*/*.wasm

echo
echo "==> pkg/"
find "$CRATE_DIR/pkg" -name '*.wasm' -exec ls -lh {} \; | awk '{print "    " $NF, $5}'
