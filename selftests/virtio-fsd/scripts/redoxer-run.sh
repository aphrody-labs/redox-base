#!/usr/bin/env bash
# Run virtio-fsd-selftest in redoxer with a vhost-user-fs share.
#
# Missing virtiofsd is fetched from gitlab.com/virtio-fs/virtiofsd (static
# x86_64 musl artifact on main).

set -euo pipefail

# Must match the share_tag passed to virtio-fsd-selftest below.
SHARE_TAG=virtiofs-tests

VIRTIOFSD_URL="${VIRTIOFSD_URL:-https://gitlab.com/virtio-fs/virtiofsd/-/jobs/artifacts/main/raw/target/x86_64-unknown-linux-musl/release/virtiofsd?job=publish}"

find_virtiofsd() {
  local c
  for c in "${VIRTIOFSD:-}" virtiofsd /usr/libexec/virtiofsd /usr/lib/qemu/virtiofsd; do
    if [[ -n "${c}" && -x "${c}" ]]; then
      echo "${c}"
      return 0
    fi
    if [[ -n "${c}" ]] && command -v "${c}" >/dev/null 2>&1; then
      command -v "${c}"
      return 0
    fi
  done
  return 1
}

fetch_virtiofsd() {
  local cache dest tmp
  cache="${XDG_CACHE_HOME:-${HOME:-/tmp}/.cache}/virtiofsd-redoxer"
  dest="${cache}/virtiofsd"
  if [[ -x "${dest}" && -z "${VIRTIOFSD_REFRESH:-}" ]]; then
    echo "${dest}"
    return 0
  fi
  if [[ "$(uname -m)" != x86_64 ]]; then
    echo "test-virtio-fs: no virtiofsd on PATH; auto-fetch is x86_64-only" >&2
    return 1
  fi
  mkdir -p "${cache}"
  tmp="$(mktemp "${cache}/virtiofsd.XXXXXX")"
  echo "test-virtio-fs: fetching virtiofsd from gitlab.com/virtio-fs/virtiofsd" >&2
  if command -v curl >/dev/null 2>&1; then
    curl -fL --retry 3 --retry-delay 1 -o "${tmp}" "${VIRTIOFSD_URL}"
  elif command -v wget >/dev/null 2>&1; then
    wget -O "${tmp}" "${VIRTIOFSD_URL}"
  else
    echo "test-virtio-fs: need curl or wget to fetch virtiofsd" >&2
    rm -f "${tmp}"
    return 1
  fi
  if [[ "$(head -c 4 "${tmp}")" != $'\x7fELF' ]]; then
    echo "test-virtio-fs: download is not an ELF binary (see ${VIRTIOFSD_URL})" >&2
    rm -f "${tmp}"
    return 1
  fi
  chmod +x "${tmp}"
  mv -f "${tmp}" "${dest}"
  echo "${dest}"
}

if ! VIRTIOFSD_BIN="$(find_virtiofsd || fetch_virtiofsd)"; then
  echo "test-virtio-fs: virtiofsd not found and fetch failed" >&2
  exit 1
fi

if ! command -v redoxer >/dev/null 2>&1; then
  echo "test-virtio-fs: redoxer not found" >&2
  exit 1
fi

if [[ -z "${REDOXER_SYSROOT:-}" || ! -d "${REDOXER_SYSROOT}" ]]; then
  echo "test-virtio-fs: REDOXER_SYSROOT must be the installed sysroot" >&2
  exit 1
fi
REDOXER_SYSROOT="$(cd "${REDOXER_SYSROOT}" && pwd)"

if [[ ! -x "${REDOXER_SYSROOT}/usr/bin/virtio-fsd-selftest" ]]; then
  echo "test-virtio-fs: missing ${REDOXER_SYSROOT}/usr/bin/virtio-fsd-selftest" >&2
  exit 1
fi
if [[ ! -x "${REDOXER_SYSROOT}/usr/lib/drivers/virtio-fsd" ]]; then
  echo "test-virtio-fs: missing ${REDOXER_SYSROOT}/usr/lib/drivers/virtio-fsd" >&2
  exit 1
fi

mkdir -p "${REDOXER_SYSROOT}/root"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SELFTEST_ARGS="${SELFTEST_ARGS---skip-perf}"
WORKDIR="$(mktemp -d /tmp/virtio-fs-selftest.XXXXXX)"
SHARE="${WORKDIR}/share"
SOCK="${WORKDIR}/vhost.sock"
WRAP="${WORKDIR}/qemu-wrapper"
cleanup() {
  if [[ -n "${VIRTIOFSD_PID:-}" ]]; then
    kill "${VIRTIOFSD_PID}" 2>/dev/null || true
    wait "${VIRTIOFSD_PID}" 2>/dev/null || true
  fi
  rm -rf "${WORKDIR}"
}
trap cleanup EXIT

mkdir -p "${SHARE}"
"${SCRIPT_DIR}/gen-fixtures.sh" "${SHARE}"

"${VIRTIOFSD_BIN}" \
  --socket-path="${SOCK}" \
  --shared-dir "${SHARE}" \
  --sandbox none \
  --translate-uid="map:0:$(id -u):1" \
  --translate-gid="map:0:$(id -g):1" \
  &
VIRTIOFSD_PID=$!

for _ in $(seq 1 50); do
  if [[ -S "${SOCK}" ]]; then
    break
  fi
  sleep 0.1
done
if [[ ! -S "${SOCK}" ]]; then
  echo "test-virtio-fs: virtiofsd did not create ${SOCK}" >&2
  exit 1
fi
sleep 0.2

QEMU_BIN="$(command -v qemu-system-x86_64)"
cat > "${WRAP}" <<EOF
#!/usr/bin/env bash
set -euo pipefail
# Shared RAM has to exist before -machine or vhost-user-fs DMA uses private memory.
filtered=()
skip=
for a in "\$@"; do
  case "\$skip" in
    m|machine) skip=; continue ;;
  esac
  case "\$a" in
    -m) skip=m; continue ;;
    -machine) skip=machine; continue ;;
    -m[0-9]*) continue ;;
  esac
  filtered+=("\$a")
done
cmd=(
  "${QEMU_BIN}"
  -object memory-backend-file,id=mem,size=2G,mem-path=/dev/shm,share=on
  -numa node,memdev=mem
  -m 2G
  -machine q35
  "\${filtered[@]}"
  -chardev socket,id=char0,path=${SOCK}
  -device vhost-user-fs-pci,queue-size=1024,chardev=char0,tag=${SHARE_TAG}
)
{
  printf 'test-virtio-fs qemu command:\\n '
  printf ' %q' "\${cmd[@]}"
  printf '\\n'
} >&2
exec "\${cmd[@]}"
EOF
chmod +x "${WRAP}"

# --folder overlays sysroot onto /. Unset REDOXER_SYSROOT to avoid a second overlay.
env -u REDOXER_SYSROOT REDOXER_QEMU_BINARY="${WRAP}" \
  redoxer exec --folder "${REDOXER_SYSROOT}/:/" /usr/bin/virtio-fsd-selftest "${SHARE_TAG}" ${SELFTEST_ARGS}
