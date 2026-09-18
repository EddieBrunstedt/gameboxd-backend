#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
[[ $# == 1 && ! -e $1 ]] || { echo 'Usage: scripts/build-acceptance.sh NEW_OUTPUT_DIRECTORY' >&2; exit 2; }
mkdir -p "$1"
acceptance_out=$(realpath "$1")
tsc --target es2022 --module es2022 --moduleResolution bundler --strict --skipLibCheck --outDir "$acceptance_out" contract/auth.ts contract/errors.ts
# Runtime has just one relative import; generated DTO imports are type-only.
python3 - "$acceptance_out/auth.js" <<'PY'
from pathlib import Path
import sys
p = Path(sys.argv[1])
p.write_text(p.read_text().replace('"./errors"', '"./errors.js"'))
PY
cp ops/acceptance/index.html ops/acceptance/main.js "$acceptance_out/"
echo "Built acceptance harness: $acceptance_out"
