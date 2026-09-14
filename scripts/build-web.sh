#!/usr/bin/env bash
# 构建 web 版并部署到 Cloudflare Pages
# 依赖：rustup wasm32-unknown-unknown、wasm-bindgen-cli 0.2.128、wasm-opt
set -euo pipefail
cd "$(dirname "$0")/.."

BINDGEN="${WASM_BINDGEN:-wasm-bindgen}"
echo "==> cargo build (wasm-release)"
cargo build --target wasm32-unknown-unknown --profile wasm-release

echo "==> wasm-bindgen"
"$BINDGEN" --out-dir web --out-name msdos --target web \
  target/wasm32-unknown-unknown/wasm-release/ms-dos.wasm

echo "==> wasm-opt"
wasm-opt -Oz \
  --enable-bulk-memory --enable-nontrapping-float-to-int --enable-sign-ext \
  --enable-simd --enable-multivalue --enable-reference-types \
  web/msdos_bg.wasm -o web/msdos_bg.wasm.opt
mv web/msdos_bg.wasm.opt web/msdos_bg.wasm

echo "==> 内容寻址文件名"
rm -f web/msdos_bg.*.wasm web/msdos_bg.wasm.d.ts web/msdos.d.ts
gzip -9 -f web/msdos_bg.wasm
HASH=$(md5 -q web/msdos_bg.wasm.gz | cut -c1-8)
mv web/msdos_bg.wasm.gz "web/msdos_bg.$HASH.wasm.gz"
# 替换 index.html 中任意旧 hash（支持重复构建）
sed -i '' -E "s#msdos_bg\.[a-f0-9]+\.wasm(\.gz)?#msdos_bg.$HASH.wasm.gz#g; s#msdos.js\?v=[a-f0-9]*#msdos.js?v=$HASH#g" web/index.html

if [[ "${1:-}" == "--deploy" ]]; then
  echo "==> 部署到 Cloudflare Pages"
  npx wrangler pages deploy web --project-name ms-dos --commit-dirty=true
else
  echo "跳过部署：加 --deploy 参数"
fi
ls -lh web/
