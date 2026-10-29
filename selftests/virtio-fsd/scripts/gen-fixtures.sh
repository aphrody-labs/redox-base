#!/usr/bin/env bash
# Build fixtures/ on the virtiofsd share.
# Usage: ./scripts/gen-fixtures.sh /path/to/host/share
#
set -euo pipefail

ROOT="${1:-}"
if [[ -z "${ROOT}" || ! -d "${ROOT}" ]]; then
  echo "usage: $0 <host-test-root>" >&2
  echo "  host-test-root is the virtiofsd shared directory." >&2
  exit 1
fi

ROOT="$(cd "${ROOT}" && pwd)"
FIX="${ROOT}/fixtures"

echo "virtio-fsd-selftest: generating fixtures under ${FIX}"

rm -rf "${FIX}"
mkdir -p "${FIX}/empty"
mkdir -p "${FIX}/nested/a/b/c"
mkdir -p "${FIX}/files"
mkdir -p "${FIX}/many"
mkdir -p "${FIX}/deletable/a" "${FIX}/deletable/b" "${FIX}/deletable/once_dir"
mkdir -p "${FIX}/cp"
mkdir -p "${FIX}/links"

printf 'virtio-fsd-selftest:known\n' > "${FIX}/known.txt"
printf 'leaf\n' > "${FIX}/nested/a/b/c/leaf.txt"
printf 'small\n' > "${FIX}/files/small.txt"

printf 'virtio-fsd-selftest:cp-src\n' > "${FIX}/cp/src.txt"
chmod 0640 "${FIX}/cp/src.txt"
printf 'virtio-fsd-selftest:cp-600\n' > "${FIX}/cp/src_600.txt"
chmod 0600 "${FIX}/cp/src_600.txt"
if ! touch -t 202001151200.00 "${FIX}/cp/src.txt" 2>/dev/null; then
  echo "note: could not pin cp/src.txt mtime; meta_cp_p_from_host still compares src vs dst" >&2
fi

printf 'host-once\n' > "${FIX}/deletable/once_file.txt"
printf 'shared-inode\n' > "${FIX}/deletable/a/foo"
if ! ln "${FIX}/deletable/a/foo" "${FIX}/deletable/b/bar"; then
  echo "note: hard links not supported here; unlink_hardlink_other_name will SKIP" >&2
fi

printf 'link-target\n' > "${FIX}/links/target.txt"
ln -s target.txt "${FIX}/links/to_file"
ln -s ./target.txt "${FIX}/links/to_file_dot"
ln -s ../files/small.txt "${FIX}/links/to_nested"
ln -s ../nested "${FIX}/links/to_dir"
ln -s no_such_name "${FIX}/links/dangling"
ln -s chain_b "${FIX}/links/chain_a"
ln -s target.txt "${FIX}/links/chain_b"
ln -s loop_b "${FIX}/links/loop_a"
ln -s loop_a "${FIX}/links/loop_b"
ln -s target.txt "${FIX}/links/with spaces"
ln -s ../links/target.txt "${FIX}/deletable/once_symlink"

for i in $(seq 0 199); do
  n=$(printf '%03d' "${i}")
  printf 'file %s\n' "${n}" > "${FIX}/many/file_${n}"
done

echo "done: $(find "${FIX}" | wc -l) paths under fixtures/"
