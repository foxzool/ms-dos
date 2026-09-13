#!/usr/bin/env bash
# 重新下载珍珠港样例地图（Overpass API，约 30MB）
# 数据 © OpenStreetMap contributors (ODbL)
set -euo pipefail
cd "$(dirname "$0")/.."
curl -s -o data/pearl_harbor.osm \
  --data-urlencode "data@scripts/pearl_harbor.overpassql" \
  https://overpass-api.de/api/interpreter
ls -lh data/pearl_harbor.osm
