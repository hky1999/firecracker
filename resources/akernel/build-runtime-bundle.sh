#!/usr/bin/env bash

set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT_DIR="$(cd "${SCRIPT_DIR}/../.." && pwd)"
VERSIONS_FILE="${AKERNEL_RUNTIME_VERSIONS_FILE:-${SCRIPT_DIR}/runtime-versions.env}"
KERNEL_FRAGMENT="${SCRIPT_DIR}/kernel/akernel.config"

log() {
    printf '[akernel-bundle] %s\n' "$*"
}

fail() {
    printf '[akernel-bundle][error] %s\n' "$*" >&2
    exit 1
}

release_tag="${1:-}"
output_dir="${2:-}"

[[ "${release_tag}" =~ ^v[0-9]+\.[0-9]+\.[0-9]+-akernel\.[0-9]+$ ]] ||
    fail "release tag must match vX.Y.Z-akernel.N"
[ -n "${output_dir}" ] || fail "output directory is required"
[ ! -e "${output_dir}" ] ||
    fail "output directory already exists: ${output_dir}"
[ -f "${VERSIONS_FILE}" ] ||
    fail "missing runtime versions file: ${VERSIONS_FILE}"

# shellcheck source=runtime-versions.env
source "${VERSIONS_FILE}"

required_versions=(
    FIRECRACKER_VERSION
    FIRECRACKER_X86_64_ARCHIVE_SHA256
    FIRECRACKER_X86_64_BINARY_SHA256
    FIRECRACKER_RELEASE_BASE_URL
    GUEST_KERNEL_VERSION
    GUEST_KERNEL_TAG
    GUEST_KERNEL_SOURCE_BASE_URL
    GUEST_KERNEL_SOURCE_SHA256
)
for name in "${required_versions[@]}"; do
    [ -n "${!name:-}" ] || fail "${name} is empty in ${VERSIONS_FILE}"
done

[ "${release_tag%-akernel.*}" = "v${FIRECRACKER_VERSION}" ] ||
    fail "${release_tag} does not extend Firecracker v${FIRECRACKER_VERSION}"
[ "$(uname -m)" = "x86_64" ] ||
    fail "only x86_64 AKernel bundles are currently supported"

for command_name in curl file gcc gzip jq make sha256sum tar; do
    command -v "${command_name}" >/dev/null 2>&1 ||
        fail "missing command: ${command_name}"
done

base_kernel_config="${ROOT_DIR}/resources/guest_configs/microvm-kernel-ci-x86_64-${GUEST_KERNEL_VERSION}.config"
[ -f "${base_kernel_config}" ] ||
    fail "missing Firecracker guest kernel config: ${base_kernel_config}"
[ -f "${KERNEL_FRAGMENT}" ] ||
    fail "missing AKernel kernel fragment: ${KERNEL_FRAGMENT}"

work_dir="$(mktemp -d)"
cleanup() {
    rm -rf -- "${work_dir}"
}
trap cleanup EXIT

mkdir -p "${output_dir}" "${work_dir}/official" "${work_dir}/linux" \
    "${work_dir}/package"

official_archive="${work_dir}/firecracker-v${FIRECRACKER_VERSION}-x86_64.tgz"
official_url="${FIRECRACKER_RELEASE_BASE_URL}/v${FIRECRACKER_VERSION}/firecracker-v${FIRECRACKER_VERSION}-x86_64.tgz"
log "downloading official Firecracker v${FIRECRACKER_VERSION}"
curl -fSL --retry 10 --retry-delay 2 --retry-all-errors \
    "${official_url}" -o "${official_archive}"
printf '%s  %s\n' "${FIRECRACKER_X86_64_ARCHIVE_SHA256}" \
    "${official_archive}" | sha256sum --check -
tar -xzf "${official_archive}" -C "${work_dir}/official"

official_dir="${work_dir}/official/release-v${FIRECRACKER_VERSION}-x86_64"
official_binary="${official_dir}/firecracker-v${FIRECRACKER_VERSION}-x86_64"
[ -x "${official_binary}" ] ||
    fail "official Firecracker archive is missing its x86_64 binary"
printf '%s  %s\n' "${FIRECRACKER_X86_64_BINARY_SHA256}" \
    "${official_binary}" | sha256sum --check -
"${official_binary}" --version

kernel_archive="${work_dir}/amazon-linux-kernel.tar.gz"
kernel_url="${GUEST_KERNEL_SOURCE_BASE_URL}/${GUEST_KERNEL_TAG}"
log "downloading Amazon Linux guest kernel ${GUEST_KERNEL_TAG}"
curl -fSL --retry 10 --retry-delay 2 --retry-all-errors \
    "${kernel_url}" -o "${kernel_archive}"
printf '%s  %s\n' "${GUEST_KERNEL_SOURCE_SHA256}" \
    "${kernel_archive}" | sha256sum --check -
tar -xzf "${kernel_archive}" -C "${work_dir}/linux" --strip-components=1

log "building AKernel guest kernel"
cd "${work_dir}/linux"
cat "${base_kernel_config}" "${KERNEL_FRAGMENT}" >.config
make olddefconfig

required_kernel_config=(
    CONFIG_BLK_DEV_INITRD=y
    CONFIG_EROFS_FS=y
    CONFIG_EROFS_FS_XATTR=y
    CONFIG_EROFS_FS_POSIX_ACL=y
    CONFIG_EROFS_FS_SECURITY=y
    CONFIG_EROFS_FS_ZIP=y
    CONFIG_EXT4_FS=y
    CONFIG_OVERLAY_FS=y
    CONFIG_VIRTIO_BLK=y
    CONFIG_VIRTIO_NET=y
    CONFIG_VIRTIO_VSOCKETS=y
)
for config_value in "${required_kernel_config[@]}"; do
    grep -qx "${config_value}" .config ||
        fail "resolved kernel config is missing ${config_value}"
done
grep -qx '# CONFIG_EROFS_FS_ZIP_LZMA is not set' .config ||
    fail "resolved kernel config unexpectedly enables EROFS LZMA"

SOURCE_DATE_EPOCH=0 \
KBUILD_BUILD_TIMESTAMP='Thu Jan  1 00:00:00 UTC 1970' \
KBUILD_BUILD_USER=akernel \
KBUILD_BUILD_HOST=builder \
KBUILD_BUILD_VERSION=1 \
    make -j"$(nproc)" vmlinux

kernel_release="$(make -s kernelrelease)"
source_commit="$(git -C "${ROOT_DIR}" rev-parse HEAD)"
source_repository="${GITHUB_REPOSITORY:-akernel-dev/firecracker}"
workflow_run_id="${GITHUB_RUN_ID:-local}"
bundle_dir_name="release-${release_tag}-x86_64"
bundle_dir="${work_dir}/package/${bundle_dir_name}"
mkdir -p "${bundle_dir}/licenses/firecracker" \
    "${bundle_dir}/licenses/linux"

install -m 0755 "${official_binary}" "${bundle_dir}/firecracker"
install -m 0644 vmlinux "${bundle_dir}/vmlinux"
install -m 0644 .config "${bundle_dir}/kernel.config"
install -m 0644 COPYING "${bundle_dir}/licenses/linux/COPYING"
for license_file in LICENSE NOTICE THIRD-PARTY; do
    install -m 0644 "${official_dir}/${license_file}" \
        "${bundle_dir}/licenses/firecracker/${license_file}"
done

vmm_sha256="$(sha256sum "${bundle_dir}/firecracker" | cut -d ' ' -f 1)"
kernel_sha256="$(sha256sum "${bundle_dir}/vmlinux" | cut -d ' ' -f 1)"
kernel_config_sha256="$(sha256sum "${bundle_dir}/kernel.config" | cut -d ' ' -f 1)"
base_config_sha256="$(sha256sum "${base_kernel_config}" | cut -d ' ' -f 1)"
fragment_sha256="$(sha256sum "${KERNEL_FRAGMENT}" | cut -d ' ' -f 1)"
compiler_version="$(gcc --version | head -n 1)"

jq -n \
    --arg repository "${source_repository}" \
    --arg commit "${source_commit}" \
    --arg workflow_run_id "${workflow_run_id}" \
    --arg release_tag "${release_tag}" \
    --arg architecture x86_64 \
    --arg vmm_version "v${FIRECRACKER_VERSION}" \
    --arg vmm_url "${official_url}" \
    --arg vmm_archive_sha256 "${FIRECRACKER_X86_64_ARCHIVE_SHA256}" \
    --arg vmm_sha256 "${vmm_sha256}" \
    --arg kernel_repository https://github.com/amazonlinux/linux \
    --arg kernel_tag "${GUEST_KERNEL_TAG}" \
    --arg kernel_url "${kernel_url}" \
    --arg kernel_source_sha256 "${GUEST_KERNEL_SOURCE_SHA256}" \
    --arg kernel_release "${kernel_release}" \
    --arg kernel_sha256 "${kernel_sha256}" \
    --arg kernel_config_sha256 "${kernel_config_sha256}" \
    --arg base_config "resources/guest_configs/microvm-kernel-ci-x86_64-${GUEST_KERNEL_VERSION}.config" \
    --arg base_config_sha256 "${base_config_sha256}" \
    --arg fragment resources/akernel/kernel/akernel.config \
    --arg fragment_sha256 "${fragment_sha256}" \
    --arg compiler "${compiler_version}" \
    '{
        schema: 1,
        component: "akernel-firecracker-runtime",
        repository: $repository,
        commit: $commit,
        workflow_run_id: $workflow_run_id,
        release_tag: $release_tag,
        architecture: $architecture,
        vmm: {
            source: "official-release",
            version: $vmm_version,
            url: $vmm_url,
            archive_sha256: $vmm_archive_sha256,
            binary_sha256: $vmm_sha256
        },
        guest_kernel: {
            repository: $kernel_repository,
            source_tag: $kernel_tag,
            source_url: $kernel_url,
            source_sha256: $kernel_source_sha256,
            release: $kernel_release,
            image_sha256: $kernel_sha256,
            config_sha256: $kernel_config_sha256,
            base_config: $base_config,
            base_config_sha256: $base_config_sha256,
            fragment: $fragment,
            fragment_sha256: $fragment_sha256,
            compiler: $compiler
        }
    }' >"${bundle_dir}/manifest.json"

(
    cd "${bundle_dir}"
    find . -type f ! -name SHA256SUMS -print0 |
        LC_ALL=C sort -z |
        xargs -0 sha256sum >SHA256SUMS
)

archive_name="firecracker-${release_tag}-x86_64.tgz"
manifest_name="firecracker-${release_tag}-x86_64.manifest.json"
tar --sort=name --mtime='@0' --owner=0 --group=0 --numeric-owner \
    --format=gnu -C "${work_dir}/package" -cf - "${bundle_dir_name}" |
    gzip -n -9 >"${output_dir}/${archive_name}"
cp "${bundle_dir}/manifest.json" "${output_dir}/${manifest_name}"
(
    cd "${output_dir}"
    sha256sum "${archive_name}" >"${archive_name}.sha256"
)

log "created ${output_dir}/${archive_name}"
