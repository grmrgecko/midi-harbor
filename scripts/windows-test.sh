#!/bin/bash
# Cross-compiles test binaries for Windows, copies them to a Windows machine over ssh, and runs
# them there. The machine needs OpenSSH server with cmd as its shell, and nothing else: no Rust.
# Scratch goes in C:\mh-win on that machine.
#
# Usage: MH_WINDOWS_HOST=user@host scripts/windows-test.sh <cargo test selection> [-- <test args>]
set -uo pipefail
host=${MH_WINDOWS_HOST:?set MH_WINDOWS_HOST to the Windows machine, as user@host}
cd "$(dirname "$0")/.."
sel=(); pass=()
seen=0
for a in "$@"; do
  if [ "$a" = "--" ]; then seen=1; continue; fi
  if [ $seen = 1 ]; then pass+=("$a"); else sel+=("$a"); fi
done
exes=$(cargo test --no-run --target x86_64-pc-windows-gnu --no-default-features "${sel[@]}" --message-format=json 2>/dev/null \
  | python3 -c 'import sys,json
for l in sys.stdin:
    try: m=json.loads(l)
    except: continue
    if m.get("reason")=="compiler-artifact" and m.get("executable") and m.get("profile",{}).get("test"): print(m["executable"])')
if [ -z "$exes" ]; then echo "build failed"; cargo test --no-run --target x86_64-pc-windows-gnu --no-default-features "${sel[@]}" 2>&1 | grep -E "^error" -A5 | head -40; exit 1; fi
ssh $host "if not exist C:\\mh-win\\tests mkdir C:\\mh-win\\tests" >/dev/null
# Tests find the binary and the sources at paths compiled in on this machine. Those paths start at
# a drive root on Windows, so the tree is mirrored under C:\mh-win\root and tests run from a drive
# substituted onto it.
repo=$(pwd)
winrepo="C:\\mh-win\\root$(echo "$repo" | tr '/' '\\')"
ssh $host "if not exist $winrepo mkdir $winrepo" >/dev/null
git ls-files -co --exclude-standard | COPYFILE_DISABLE=1 tar --no-xattrs -cf - -T - | ssh $host "tar -xf - -C $winrepo"
if [ -f target/x86_64-pc-windows-gnu/debug/midi-harbor.exe ]; then
  ssh $host "if not exist $winrepo\\target\\x86_64-pc-windows-gnu\\debug mkdir $winrepo\\target\\x86_64-pc-windows-gnu\\debug" >/dev/null
  scp -q target/x86_64-pc-windows-gnu/debug/midi-harbor.exe "$host:$(echo "$winrepo" | sed 's/\\\\/\//g')/target/x86_64-pc-windows-gnu/debug/midi-harbor.exe"
fi
rc=0
for e in $exes; do
  b=$(basename "$e")
  scp -q "$e" "$host:C:/mh-win/tests/$b"
  out=$(ssh $host "subst M: C:\\mh-win\\root >nul 2>&1 & M: & cd \\ & C:\\mh-win\\tests\\$b ${pass[*]:-} 2>&1")
  code=$?
  echo "== $b (exit $code)"
  echo "$out" | grep -E "^test result|FAILED|panicked|failures:|^---- " | head -30
  [ $code -ne 0 ] && rc=1
done
exit $rc
