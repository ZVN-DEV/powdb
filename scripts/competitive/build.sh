#!/usr/bin/env bash
# Private SDK installs and separate worker binaries. Nothing is installed into
# system directories; every fetched archive is verified before extraction.
set -euo pipefail
root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$root"
work="${1:?usage: build.sh /absolute/work-directory}"
[[ "$work" == /* ]] || { echo 'work directory must be absolute' >&2; exit 2; }
[[ "$(uname -s)/$(uname -m)" == Linux/x86_64 ]] || { echo 'v1 SDK pins target Linux x86_64 only' >&2; exit 2; }
mkdir -p "$work"/{bin,lib/pkgconfig,downloads,tmp}
python3 - "$root/scripts/competitive/engines.json" "$work" <<'PY'
import hashlib,json,sys,tarfile,urllib.request,zipfile
from pathlib import Path
manifest=json.loads(Path(sys.argv[1]).read_text())
work=Path(sys.argv[2])
for engine,name,algorithm in [('sqlite','sqlite.zip','sha3_256'),('duckdb','duckdb.tar.gz','sha256')]:
    entry=manifest[engine]
    destination=work/'downloads'/name
    if not destination.exists():
        request=urllib.request.Request(entry['url'],headers={'User-Agent':'PowDB-comparative-benchmark'})
        with urllib.request.urlopen(request,timeout=90) as response:
            destination.write_bytes(response.read())
    if hashlib.new(algorithm,destination.read_bytes()).hexdigest()!=entry[algorithm]:
        raise SystemExit(f'{engine} archive digest mismatch')
with zipfile.ZipFile(work/'downloads/sqlite.zip') as archive:
    if any('..' in Path(n).parts or Path(n).is_absolute() for n in archive.namelist()):
        raise SystemExit('unsafe SQLite archive member')
    archive.extractall(work/'downloads')
with tarfile.open(work/'downloads/duckdb.tar.gz') as archive:
    archive.extractall(work/'lib',filter='data')
(work/'lib/pkgconfig/sqlite3.pc').write_text('prefix='+str(work)+'\nlibdir=${prefix}/lib\nincludedir=${prefix}/downloads/sqlite-amalgamation-3540100\nName: SQLite\nDescription: Verified private benchmark SQLite\nVersion: 3.54.1\nLibs: -L${libdir} -lsqlite3\nCflags: -I${includedir}\n')
PY
cc -O3 -fPIC -DSQLITE_THREADSAFE=1 -DSQLITE_ENABLE_FTS5 -DSQLITE_ENABLE_RTREE \
  -DSQLITE_ENABLE_COLUMN_METADATA -shared "$work/downloads/sqlite-amalgamation-3540100/sqlite3.c" \
  -o "$work/lib/libsqlite3.so" -pthread -ldl -lm
export LIBSQLITE3_SYS_USE_PKG_CONFIG=1
export PKG_CONFIG_PATH="$work/lib/pkgconfig"
export DUCKDB_LIB_DIR="$work/lib"
export LD_LIBRARY_PATH="$work/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
export CARGO_TARGET_DIR="$work/target"
export TMPDIR="$work/tmp"
for backend in powdb sqlite duckdb postgres mysql wire; do
  feature="competitive-$backend"
  cargo tree --locked -p powdb-compare --features "$feature" -e normal,build \
    --prefix none --format '{p} features={f}' > "$work/$backend-features.txt"
  python3 - "$work/$backend-features.txt" <<'PYGUARD'
import re,sys
from pathlib import Path
text=Path(sys.argv[1]).read_text()
if re.search(r'powdb-(query|storage).*features=.*\btesting\b',text):
    raise SystemExit('test instrumentation is forbidden in measured workers')
PYGUARD
  cargo build --release --locked -p powdb-compare --bin powdb-competitive-worker --features "$feature"
  name="$backend"
  [[ "$backend" == wire ]] && name=powdb-server
  cp "$CARGO_TARGET_DIR/release/powdb-competitive-worker" "$work/bin/$name"
  ldd "$work/bin/$name" > "$work/$name-linked-libraries.txt"
done
python3 - "$work" <<'PY'
import hashlib,json,sys,os,subprocess
from pathlib import Path
work=Path(sys.argv[1])
artifacts={}
for p in [*work.joinpath('bin').iterdir(),*work.joinpath('lib').glob('*.so')]:
    artifacts[str(p.relative_to(work))]=hashlib.sha256(p.read_bytes()).hexdigest()
(work/'build-manifest.json').write_text(json.dumps({'artifacts_sha256':artifacts, 'rustc':subprocess.check_output(['rustc','-Vv'],text=True), 'cc':subprocess.check_output(['cc','--version'],text=True), 'rustflags':os.environ.get('RUSTFLAGS',''), 'source_sha':subprocess.check_output(['git','rev-parse','HEAD'],text=True).strip(),
  'sqlite_cc_flags':'-O3 -fPIC -DSQLITE_THREADSAFE=1 -DSQLITE_ENABLE_FTS5 -DSQLITE_ENABLE_RTREE -DSQLITE_ENABLE_COLUMN_METADATA'},indent=2)+'\n')
PY
