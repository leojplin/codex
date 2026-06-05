#!/usr/bin/env bash
set -euo pipefail

repo="${CODEX_FORK_REPO:-leojplin/codex}"
version="${CODEX_FORK_VERSION:-latest}"
install_dir="${INSTALL_DIR:-${HOME}/.local/bin}"
bin_name="codex-fork"
asset="codex-fork-aarch64-apple-darwin.tar.gz"

case "$(uname -s):$(uname -m)" in
  Darwin:arm64|Darwin:aarch64)
    ;;
  *)
    echo "codex-fork release installer only supports macOS ARM64." >&2
    exit 1
    ;;
esac

for command in curl tar shasum; do
  if ! command -v "${command}" >/dev/null 2>&1; then
    echo "Missing required command: ${command}" >&2
    exit 1
  fi
done

if [[ "${version}" == "latest" ]]; then
  download_base="https://github.com/${repo}/releases/latest/download"
else
  download_base="https://github.com/${repo}/releases/download/${version}"
fi

tmp_dir="$(mktemp -d)"
cleanup() {
  rm -rf "${tmp_dir}"
}
trap cleanup EXIT

archive="${tmp_dir}/${asset}"
checksums="${tmp_dir}/SHA256SUMS"

curl -fsSL "${download_base}/${asset}" -o "${archive}"

if curl -fsSL "${download_base}/SHA256SUMS" -o "${checksums}"; then
  checksum_line="$(grep -E "[[:space:]]${asset}$" "${checksums}" || true)"
  if [[ -n "${checksum_line}" ]]; then
    (
      cd "${tmp_dir}"
      printf '%s\n' "${checksum_line}" | shasum -a 256 -c -
    )
  else
    echo "Warning: SHA256SUMS did not include ${asset}; skipping checksum verification." >&2
  fi
else
  echo "Warning: could not download SHA256SUMS; skipping checksum verification." >&2
fi

tar -xzf "${archive}" -C "${tmp_dir}"

if [[ ! -f "${tmp_dir}/${bin_name}" ]]; then
  echo "Release archive did not contain ${bin_name}." >&2
  exit 1
fi

mkdir -p "${install_dir}"
install -m 0755 "${tmp_dir}/${bin_name}" "${install_dir}/${bin_name}"

echo "Installed ${bin_name} to ${install_dir}/${bin_name}"

case ":${PATH}:" in
  *":${install_dir}:"*)
    ;;
  *)
    echo "Add ${install_dir} to PATH to run ${bin_name} without a full path."
    ;;
esac
