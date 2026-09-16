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
rm -f web/msdos_bg.*.wasm web/msdos_bg.*.wasm.gz web/msdos_bg.wasm.d.ts web/msdos.[0-9a-f]*.js
gzip -9 -f web/msdos_bg.wasm
HASH=$(md5 -q web/msdos_bg.wasm.gz | cut -c1-8)
mv web/msdos_bg.wasm.gz "web/msdos_bg.$HASH.wasm.gz"
# js 同样内容寻址：浏览器对 js 的缓存头不可靠（CF Pages 实测返回 max-age=14400），
# 曾导致部署后用户长时间停留在旧逻辑
mv web/msdos.js "web/msdos.$HASH.js"
# 替换 index.html 中任意旧 hash/旧格式引用（支持重复构建）
sed -i '' -E "s#/*msdos_bg\.[a-z0-9.]+\.wasm(\.gz)?#/msdos_bg.$HASH.wasm.gz#g; s#\.?/*msdos(\.[a-z0-9]+)?\.js(\?v=[a-z0-9.]*)?#/msdos.$HASH.js#g" web/index.html

if [[ "${1:-}" == "--deploy" ]]; then
  echo "==> 部署到 Cloudflare Pages"
  npx wrangler pages deploy web --project-name ms-dos --commit-dirty=true
else
  echo "跳过部署：加 --deploy 参数"
fi
ls -lh web/
